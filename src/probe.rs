//! ウィンドウを開かずに 1 フレームを描き、画素を書き出す。
//!
//! 画面キャプチャに頼らずに描画結果を確かめるための経路である。
//! 実際の起動と同じ描画関数を通すので、PTY からグリッド、
//! 左ペインの割り付け、字形の配置までをまとめて検査できる。

use std::path::Path;
use std::sync::mpsc::channel;
use std::time::{Duration, Instant};

use crate::config::{Config, HOST_PROFILE};
use crate::history::History;
use crate::render::Renderer;
use crate::session::Manager;
use crate::term::{TermSize, UiEvent, UiSender};
use crate::theme::Theme;

const WIDTH: u32 = 1280;
const HEIGHT: u32 = 800;

/// 端末の挙動をひととおり出す台本。色、太字、全角、OSC 133 を含む。
const SCRIPT: &str = r#"
printf '\033]7;file://probe/Users/tkc/github/terminal_tex\007'
printf '\033]133;A\007'
printf '\033[32m~/github/terminal_tex\033[0m $ '
printf '\033]133;B\007'
printf 'cargo test\r\n'
printf '\033]133;C\007'
printf '   Compiling tex v0.1.0\r\n'
printf 'test result: \033[32mok\033[0m. 55 passed; 0 failed\r\n'
printf '\033]133;D;0\007'
printf '\033]133;A\007'
printf '\033[32m~/github/terminal_tex\033[0m $ '
printf '\033]133;B\007'
printf 'git status\r\n'
printf '\033]133;C\007'
printf 'On branch \033[1mmain\033[0m\r\n'
printf '\033[31m 変更されたファイル\033[0m: src/render.rs 全角の桁送りを確かめる\r\n'
printf '\033[34m青\033[0m \033[35m紫\033[0m \033[36m水\033[0m \033[1m太字\033[0m \033[7m反転\033[0m \033[4m下線 underline\033[0m\r\n'
printf '\033]133;D;0\007'
printf '\033]133;A\007'
printf '\033[32m~/github/terminal_tex\033[0m $ '
printf '\033]133;B\007'
sleep 60
"#;

pub fn run(out_path: &str) {
    let (tx, rx) = channel::<UiEvent>();
    let sender = UiSender::Channel(tx);

    let renderer = pollster::block_on(Renderer::offscreen(WIDTH, HEIGHT, "Menlo", 13.0));
    let cell = renderer.cell();
    let (cols, rows) = renderer.grid_size();

    let mut config = Config::default();
    config.shell.program = Some("/bin/sh".into());
    config.shell.args = vec!["-c".into(), SCRIPT.into()];

    let sidebar_cols = config.window.sidebar_cols.min(cols.saturating_sub(20));
    let term_col = sidebar_cols + 1;
    let term_cols = cols.saturating_sub(term_col).max(2);
    let term_rows = rows.saturating_sub(1).max(1);

    let mut manager = Manager::new(
        TermSize::new(term_cols, term_rows),
        (cell.width as u16, cell.height as u16),
        sender,
    );
    let cwd = std::env::current_dir().unwrap_or_else(|_| "/".into());
    manager
        .spawn_new(&config, HOST_PROFILE, &cwd)
        .expect("最初のセッションを起動できる");
    // 分岐を 2 段作り、系統樹の表示を確かめる。
    manager.fork(&config, 0, None).expect("分岐できる");
    manager.fork(&config, 1, None).expect("孫を分岐できる");
    manager.select(0);

    let history = History::open_memory().ok();

    // 出力が落ち着くまでイベントを受ける。
    let deadline = Instant::now() + Duration::from_millis(2500);
    let mut records = Vec::new();
    while Instant::now() < deadline {
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(UiEvent::Osc(id, ev)) => {
                if let Some(s) = manager.get_mut(id) {
                    s.on_osc(&ev);
                }
            }
            Ok(UiEvent::Command(_, record)) => records.push(record),
            Ok(UiEvent::ChildExit(id, code)) => manager.mark_exited(id, code),
            Ok(_) => {}
            Err(_) => {}
        }
    }
    eprintln!("[probe] 取り出したコマンドの記録 {} 件", records.len());
    if let Some(h) = &history {
        for r in &records {
            let _ = h.record(r);
        }
    }

    let theme = Theme::default();
    let mut state = crate::State {
        renderer,
        manager,
        history,
        theme,
        sidebar: true,
        search: None,
        picker: None,
        mouse: Default::default(),
        mods: Default::default(),
        status: None,
        recent: Vec::new(),
        recent_for: None,
        pending_since: None,
        window: None,
    };
    let id = state.manager.selected().map(|s| s.id).unwrap_or(0);
    state.recent = state
        .history
        .as_ref()
        .map(|h| h.recent(id, 10).unwrap_or_default())
        .unwrap_or_default();
    state.recent_for = Some(id);

    let layout = crate::Layout {
        cols,
        rows,
        sidebar_cols,
        term_col,
        term_cols,
        term_rows,
    };

    // 重ねた一覧の描画も確かめられるようにする。
    match std::env::var("TEX_PROBE_OVERLAY").as_deref() {
        Ok("picker") => {
            state.picker = Some(crate::PickerState {
                names: vec!["host".into(), "sandbox".into(), "no-network".into()],
                selected: 1,
            })
        }
        Ok("search") => {
            let results = state
                .history
                .as_ref()
                .map(|h| {
                    h.search("", crate::history::Scope::All, 1, "", 10)
                        .unwrap_or_default()
                })
                .unwrap_or_default();
            state.search = Some(crate::SearchState {
                query: "car".into(),
                scope: crate::history::Scope::All,
                results,
                selected: 0,
            });
        }
        _ => {}
    }

    state.renderer.begin();
    crate::draw_sidebar(&mut state, &layout, &theme);
    for row in 0..layout.rows {
        state
            .renderer
            .put_char(layout.sidebar_cols, row, '│', theme.fg_tertiary, false, false);
    }
    crate::draw_terminal(&mut state, &layout, &theme);
    crate::draw_bottom(&mut state, &layout, &theme);
    let pixels = state.renderer.render_to_pixels(theme.bg);

    write_png(Path::new(out_path), WIDTH, HEIGHT, &pixels).expect("画像を書けない");
    println!(
        "{out_path} に {WIDTH}x{HEIGHT} を書いた（{cols}桁 {rows}行、セル {:.2}x{:.2}）",
        cell.width, cell.height
    );

    // 子プロセスを片付ける。
    while !state.manager.is_empty() {
        state.manager.select(0);
        state.manager.close_selected();
        state.manager.select(0);
        state.manager.close_selected();
        break;
    }
}

// ------------------------------------------------------------------ PNG 出力

/// 圧縮なしの deflate ブロックで PNG を書く。画像確認のためだけに使う。
fn write_png(path: &Path, width: u32, height: u32, rgba: &[u8]) -> std::io::Result<()> {
    use std::io::Write;

    let mut raw = Vec::with_capacity((width as usize * 4 + 1) * height as usize);
    for y in 0..height as usize {
        raw.push(0u8); // フィルタなし
        let start = y * width as usize * 4;
        raw.extend_from_slice(&rgba[start..start + width as usize * 4]);
    }

    let mut idat = Vec::new();
    idat.extend_from_slice(&[0x78, 0x01]); // zlib ヘッダ
    for (i, chunk) in raw.chunks(65535).enumerate() {
        let last = (i + 1) * 65535 >= raw.len();
        idat.push(u8::from(last));
        idat.extend_from_slice(&(chunk.len() as u16).to_le_bytes());
        idat.extend_from_slice(&(!(chunk.len() as u16)).to_le_bytes());
        idat.extend_from_slice(chunk);
    }
    idat.extend_from_slice(&adler32(&raw).to_be_bytes());

    let mut out = Vec::new();
    out.extend_from_slice(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]);
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]); // 8bit RGBA
    push_chunk(&mut out, b"IHDR", &ihdr);
    push_chunk(&mut out, b"IDAT", &idat);
    push_chunk(&mut out, b"IEND", &[]);

    let mut f = std::fs::File::create(path)?;
    f.write_all(&out)
}

fn push_chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    let mut crc_input = Vec::with_capacity(4 + data.len());
    crc_input.extend_from_slice(kind);
    crc_input.extend_from_slice(data);
    out.extend_from_slice(&crc32(&crc_input).to_be_bytes());
}

fn adler32(data: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for &byte in data {
        a = (a + byte as u32) % 65521;
        b = (b + a) % 65521;
    }
    (b << 16) | a
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for &byte in data {
        crc ^= byte as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xedb8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adler_と_crc_が既知の値を返す() {
        // zlib と PNG の仕様に載っている検算値。
        assert_eq!(adler32(b"abc"), 0x024d0127);
        assert_eq!(crc32(b"123456789"), 0xcbf43926);
    }
}

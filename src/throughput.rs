//! PTY から読んだバイト列を処理する速さを測る。窓は開かない。
//!
//! alacritty の vtebench は端末の中で走らせて「PTY を読む速さ」だけを測る。
//! ここではその負荷を真似た列を自分で作り、実際の読み取り経路
//! （`OscScanner` の走査 → `vte` の解釈 → グリッドへの書き込み）を通す。
//! 走査と解釈を別々に計るので、どちらを削ればよいかが分かる。

use std::sync::Arc;
use std::time::{Duration, Instant};

use alacritty_terminal::event::WindowSize;
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::vte::ansi::Processor;

use crate::osc::OscScanner;
use crate::term::{new_term, EventProxy, TermSize, UiSender};

/// PTY の読み取りと同じ大きさで区切って流す。
const CHUNK: usize = 65536;
const COLS: usize = 163;
const LINES: usize = 47;
const SCROLLBACK: usize = 10_000;
/// 1 種類あたりに流す量。多すぎると測るだけで時間がかかる。
const BYTES: usize = 16 << 20;

/// 負荷の作り方。名前と、列を作る関数の組。
type Case = (&'static str, fn() -> Vec<u8>);

pub fn run() {
    println!(
        "PTY を読む速さ（{COLS} 桁 x {LINES} 行、履歴 {SCROLLBACK} 行、{} MiB ずつ）",
        BYTES >> 20
    );
    println!("vtebench の負荷に倣った列を自前で作り、実際の読み取り経路を通す。\n");
    let cases: &[Case] = &[
        ("light_cells", light_cells),
        ("medium_cells", medium_cells),
        ("dense_cells", dense_cells),
        ("scrolling", scrolling),
        ("scrolling_region", scrolling_region),
        ("cursor_motion", cursor_motion),
        ("unicode", unicode),
        ("sync_cells", sync_cells),
        ("osc_heavy", osc_heavy),
    ];
    println!(
        "  {:<17} {:>9} {:>9} {:>9} {:>7}",
        "負荷", "合計", "走査", "解釈", "速さ"
    );
    for (name, gen) in cases {
        let payload = gen();
        let (scan, parse) = feed(&payload);
        let total = scan + parse;
        let mib = payload.len() as f64 / (1 << 20) as f64;
        println!(
            "  {name:<17} {:>7.1}ms {:>7.1}ms {:>7.1}ms {:>5.0} MiB/s",
            total.as_secs_f64() * 1000.0,
            scan.as_secs_f64() * 1000.0,
            parse.as_secs_f64() * 1000.0,
            mib / total.as_secs_f64(),
        );
    }
    println!("\n走査は OSC を拾うための前段である。解釈は vte とグリッドへの書き込み。");
}

/// 実際の読み取り経路を通し、走査と解釈の時間を別々に返す。
fn feed(payload: &[u8]) -> (Duration, Duration) {
    let (tx, rx) = std::sync::mpsc::channel();
    let (ptx, prx) = std::sync::mpsc::channel();
    // 受け側を捨てると送信が失敗して経路が変わる。生かしておく。
    let ws = Arc::new(FairMutex::new(WindowSize {
        num_lines: LINES as u16,
        num_cols: COLS as u16,
        cell_width: 8,
        cell_height: 17,
    }));
    let proxy = EventProxy::new(1, ptx, UiSender::Channel(tx), ws);
    let mut term = new_term(TermSize::new(COLS, LINES), SCROLLBACK, proxy);
    let mut parser: Processor = Processor::new();
    let mut scanner = OscScanner::new();
    let mut scan = Duration::ZERO;
    let mut parse = Duration::ZERO;
    for chunk in payload.chunks(CHUNK) {
        let t = Instant::now();
        let events = scanner.feed(chunk);
        scan += t.elapsed();
        let t = Instant::now();
        let mut pos = 0usize;
        for (end, _ev) in events {
            parser.advance(&mut term, &chunk[pos..end]);
            pos = end;
        }
        parser.advance(&mut term, &chunk[pos..]);
        parse += t.elapsed();
    }
    drop(rx);
    drop(prx);
    (scan, parse)
}

/// 決まった並びの疑似乱数。外部の乱数に頼らずに同じ負荷を作る。
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

fn fill(mut f: impl FnMut(&mut Vec<u8>)) -> Vec<u8> {
    let mut out = Vec::with_capacity(BYTES + 4096);
    while out.len() < BYTES {
        f(&mut out);
    }
    out
}

/// 短い行を延々と流す。字の数が少なく、送りが多い。
fn light_cells() -> Vec<u8> {
    let mut rng = Rng(1);
    fill(move |out| {
        for _ in 0..8 {
            out.push(b'a' + rng.below(26) as u8);
        }
        out.extend_from_slice(b"\r\n");
    })
}

/// 半分の幅を埋める行。
fn medium_cells() -> Vec<u8> {
    let mut rng = Rng(2);
    fill(move |out| {
        for _ in 0..COLS / 2 {
            out.push(b'a' + rng.below(26) as u8);
        }
        out.extend_from_slice(b"\r\n");
    })
}

/// 全幅を埋め、コマごとに色を変える。SGR が字の数だけ挟まる。
fn dense_cells() -> Vec<u8> {
    let mut rng = Rng(3);
    fill(move |out| {
        for _ in 0..COLS {
            let (r, g, b) = (rng.below(256), rng.below(256), rng.below(256));
            out.extend_from_slice(format!("\x1b[38;2;{r};{g};{b}m").as_bytes());
            out.push(b'a' + rng.below(26) as u8);
        }
        out.extend_from_slice(b"\x1b[0m\r\n");
    })
}

/// 送りだけを起こす。履歴への押し出しが主な費用になる。
fn scrolling() -> Vec<u8> {
    fill(|out| out.extend_from_slice(b"\n"))
}

/// 画面の下部だけを送り領域にする。履歴には入らない。
fn scrolling_region() -> Vec<u8> {
    let mut out = Vec::with_capacity(BYTES + 4096);
    out.extend_from_slice(format!("\x1b[{};{}r", LINES / 2, LINES).as_bytes());
    out.extend_from_slice(format!("\x1b[{};1H", LINES).as_bytes());
    while out.len() < BYTES {
        out.extend_from_slice(b"\n");
    }
    out
}

/// カーソルを飛ばしながら 1 字ずつ置く。
fn cursor_motion() -> Vec<u8> {
    let mut rng = Rng(4);
    fill(move |out| {
        let (l, c) = (rng.below(LINES) + 1, rng.below(COLS) + 1);
        out.extend_from_slice(format!("\x1b[{l};{c}H").as_bytes());
        out.push(b'a' + rng.below(26) as u8);
    })
}

/// 多バイト文字を並べる。UTF-8 の組み立てが加わる。
fn unicode() -> Vec<u8> {
    let chars = ['あ', 'い', '中', '文', '한', '글', '🦀', 'é'];
    let mut rng = Rng(5);
    fill(move |out| {
        let mut s = [0u8; 4];
        for _ in 0..COLS / 2 {
            let c = chars[rng.below(chars.len())];
            out.extend_from_slice(c.encode_utf8(&mut s).as_bytes());
        }
        out.extend_from_slice(b"\r\n");
    })
}

/// 同期更新（DECSET 2026）で囲む。囲みの費用を見る。
fn sync_cells() -> Vec<u8> {
    let mut rng = Rng(6);
    fill(move |out| {
        out.extend_from_slice(b"\x1b[?2026h");
        for _ in 0..COLS / 2 {
            out.push(b'a' + rng.below(26) as u8);
        }
        out.extend_from_slice(b"\x1b[?2026l\r\n");
    })
}

/// シェル統合の通知が頻繁に混じる場合。走査の側に効く。
fn osc_heavy() -> Vec<u8> {
    let mut rng = Rng(7);
    fill(move |out| {
        out.extend_from_slice(b"\x1b]133;A\x07");
        out.extend_from_slice(b"\x1b]7;file:///Users/x/work\x1b\\");
        for _ in 0..COLS / 2 {
            out.push(b'a' + rng.below(26) as u8);
        }
        out.extend_from_slice(b"\r\n\x1b]133;D;0\x07");
    })
}

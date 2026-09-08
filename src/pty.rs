//! PTY の生成と、読み取り・書き込み・終了待ちの各スレッド。
//!
//! `alacritty_terminal` の `event_loop` は使わない。
//! OSC 133 と OSC 1337 を拾うために、PTY の生バイト列が必要だからである。

use std::io::{Read, Write};
use std::path::Path;
use std::time::SystemTime;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;

use alacritty_terminal::event::WindowSize;
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::vte::ansi::{Processor, StdSyncHandler};
use portable_pty::{native_pty_system, ChildKiller, CommandBuilder, MasterPty, PtySize};

use crate::osc::{OscEvent, OscScanner};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line, Point};

use crate::session::{grid_text, CommandRecord, SessionId};
use crate::term::{EventProxy, TermSize, UiEvent, UiSender};

/// 起動した PTY への操作口。
pub struct PtyHandle {
    master: Box<dyn MasterPty + Send>,
    killer: Box<dyn ChildKiller + Send + Sync>,
    writer_tx: Sender<Vec<u8>>,
}

impl PtyHandle {
    pub fn write(&self, bytes: Vec<u8>) {
        let _ = self.writer_tx.send(bytes);
    }

    pub fn resize(&self, size: TermSize, cell_w: u16, cell_h: u16) {
        let _ = self.master.resize(PtySize {
            rows: size.lines as u16,
            cols: size.cols as u16,
            pixel_width: size.cols as u16 * cell_w,
            pixel_height: size.lines as u16 * cell_h,
        });
    }

    pub fn kill(&mut self) {
        let _ = self.killer.kill();
    }
}

/// 起動結果。
pub struct Spawned {
    pub handle: PtyHandle,
    pub term: Arc<FairMutex<alacritty_terminal::Term<EventProxy>>>,
    pub window_size: Arc<FairMutex<WindowSize>>,
    /// 前回の描画以降に画面が変わったか。通知の氾濫を抑えるために使う。
    pub dirty: Arc<AtomicBool>,
}

#[derive(Debug)]
pub enum SpawnError {
    OpenPty(String),
    Spawn(String, String),
}

impl std::fmt::Display for SpawnError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SpawnError::OpenPty(e) => write!(f, "PTY を開けない: {e}"),
            SpawnError::Spawn(prog, e) => write!(f, "{prog} を起動できない: {e}"),
        }
    }
}

/// 引数列を PTY 上で起動し、読み書きのスレッドを立てる。
pub fn spawn(
    id: SessionId,
    argv: &[String],
    cwd: &Path,
    size: TermSize,
    cell: (u16, u16),
    scrollback: usize,
    ui_tx: UiSender,
) -> Result<Spawned, SpawnError> {
    let pty_size = PtySize {
        rows: size.lines as u16,
        cols: size.cols as u16,
        pixel_width: size.cols as u16 * cell.0,
        pixel_height: size.lines as u16 * cell.1,
    };
    let pair = native_pty_system()
        .openpty(pty_size)
        .map_err(|e| SpawnError::OpenPty(e.to_string()))?;

    let mut cmd = CommandBuilder::new(&argv[0]);
    for a in &argv[1..] {
        cmd.arg(a);
    }
    cmd.cwd(cwd);
    // シェル統合とエージェント側のフックはこれらを見て動作を変える。
    cmd.env("TERM", "xterm-256color");
    cmd.env("COLORTERM", "truecolor");
    cmd.env("TERM_PROGRAM", "tex");
    cmd.env("TERM_PROGRAM_VERSION", env!("CARGO_PKG_VERSION"));
    cmd.env("TEX_SESSION", id.to_string());
    cmd.env("TEX_SHELL_INTEGRATION", "1");

    let child = pair
        .slave
        .spawn_command(cmd)
        .map_err(|e| SpawnError::Spawn(argv[0].clone(), e.to_string()))?;
    drop(pair.slave);

    let reader = pair
        .master
        .try_clone_reader()
        .map_err(|e| SpawnError::OpenPty(e.to_string()))?;
    let writer = pair
        .master
        .take_writer()
        .map_err(|e| SpawnError::OpenPty(e.to_string()))?;

    let window_size = Arc::new(FairMutex::new(WindowSize {
        num_lines: size.lines as u16,
        num_cols: size.cols as u16,
        cell_width: cell.0,
        cell_height: cell.1,
    }));

    let (writer_tx, writer_rx) = channel::<Vec<u8>>();
    spawn_writer(writer, writer_rx);

    let proxy = EventProxy::new(id, writer_tx.clone(), ui_tx.clone(), window_size.clone());
    let term = Arc::new(FairMutex::new(crate::term::new_term(
        size, scrollback, proxy,
    )));

    let dirty = Arc::new(AtomicBool::new(true));
    spawn_reader(
        id,
        reader,
        term.clone(),
        ui_tx.clone(),
        dirty.clone(),
        cwd.to_string_lossy().to_string(),
    );

    let mut killer = child.clone_killer();
    spawn_waiter(id, child, ui_tx);
    let _ = &mut killer;

    Ok(Spawned {
        handle: PtyHandle {
            master: pair.master,
            killer,
            writer_tx,
        },
        term,
        window_size,
        dirty,
    })
}

fn spawn_writer(mut writer: Box<dyn Write + Send>, rx: Receiver<Vec<u8>>) {
    thread::Builder::new()
        .name("tex-pty-write".into())
        .spawn(move || {
            while let Ok(bytes) = rx.recv() {
                if writer.write_all(&bytes).is_err() {
                    break;
                }
                if writer.flush().is_err() {
                    break;
                }
            }
        })
        .expect("書き込みスレッドを作れない");
}

/// OSC 133 の通知からコマンド 1 件を組み立てる。
///
/// 画面の状態は通知が届いた瞬間にしか正しく読めないため、
/// 組み立ては PTY を読むスレッドの中で行う。
#[derive(Default)]
struct CommandTracker {
    cwd: String,
    agent_id: Option<String>,
    /// 入力の開始位置と、その時点のスクロールバック行数。
    ///
    /// Enter を押すと画面がスクロールし、同じ行番号が別の行を指すようになる。
    /// 行数の増加分がスクロール量なので、それで行番号を戻す。
    input_start: Option<(alacritty_terminal::index::Point, usize)>,
    text: Option<String>,
    started_at: Option<SystemTime>,
}

impl CommandTracker {
    fn on_event(
        &mut self,
        session_id: SessionId,
        ev: &OscEvent,
        term: &alacritty_terminal::Term<EventProxy>,
    ) -> Option<CommandRecord> {
        match ev {
            OscEvent::Cwd(path) => {
                self.cwd = path.clone();
                None
            }
            OscEvent::AgentId(id) => {
                self.agent_id = Some(id.clone());
                None
            }
            OscEvent::PromptStart => {
                self.input_start = None;
                self.text = None;
                self.started_at = None;
                None
            }
            OscEvent::CommandStart => {
                self.input_start = Some((term.grid().cursor.point, term.grid().history_size()));
                None
            }
            OscEvent::CommandExecuted => {
                let end = term.grid().cursor.point;
                let start = match self.input_start {
                    Some((point, history)) => {
                        let scrolled = term.grid().history_size().saturating_sub(history) as i32;
                        let line = Line(point.line.0 - scrolled);
                        let topmost = Line(-(term.grid().history_size() as i32));
                        if line < topmost {
                            // 履歴からも押し出された。実行直前の行だけを読む。
                            Point::new(end.line, Column(0))
                        } else {
                            Point::new(line, point.column)
                        }
                    }
                    None => end,
                };
                self.text = Some(grid_text(term, start, end));
                self.started_at = Some(SystemTime::now());
                None
            }
            OscEvent::CommandFinished(code) => {
                let text = self.text.take()?;
                let started_at = self.started_at.take()?;
                self.input_start = None;
                let command = text.trim().to_string();
                if command.is_empty() {
                    return None;
                }
                let duration_ms = SystemTime::now()
                    .duration_since(started_at)
                    .ok()
                    .map(|d| d.as_millis() as i64);
                Some(CommandRecord {
                    session_id,
                    agent_id: self.agent_id.clone(),
                    cwd: self.cwd.clone(),
                    command,
                    exit_code: *code,
                    started_at,
                    duration_ms,
                })
            }
        }
    }
}

fn spawn_reader(
    id: SessionId,
    mut reader: Box<dyn Read + Send>,
    term: Arc<FairMutex<alacritty_terminal::Term<EventProxy>>>,
    ui_tx: UiSender,
    dirty: Arc<AtomicBool>,
    cwd: String,
) {
    thread::Builder::new()
        .name("tex-pty-read".into())
        .spawn(move || {
            let mut parser: Processor<StdSyncHandler> = Processor::new();
            let mut scanner = OscScanner::new();
            let mut tracker = CommandTracker {
                cwd,
                ..CommandTracker::default()
            };
            let mut buf = vec![0u8; 65536];
            let diag = std::env::var("TEX_FRAME_LOG").is_ok();
            let mut read_bytes = 0u64;
            let mut reads = 0u64;
            let mut sent = 0u64;
            let mut skipped = 0u64;
            let mut last = std::time::Instant::now();
            loop {
                let n = match reader.read(&mut buf) {
                    Ok(0) | Err(_) => {
                        if diag {
                            log::info!("[read {id}] 読み取り終了 reads={reads} bytes={read_bytes}");
                        }
                        break;
                    }
                    Ok(n) => n,
                };
                if diag {
                    reads += 1;
                    read_bytes += n as u64;
                    if last.elapsed().as_millis() >= 1000 {
                        last = std::time::Instant::now();
                        log::info!(
                            "[read {id}] 1 秒: 読み {reads} 回 {read_bytes} B, wakeup 送信 {sent}, 抑制 {skipped}"
                        );
                        reads = 0;
                        read_bytes = 0;
                        sent = 0;
                        skipped = 0;
                    }
                }
                let bytes = &buf[..n];
                let events = scanner.feed(bytes);
                let mut out: Vec<UiEvent> = Vec::new();
                {
                    let mut term = term.lock();
                    let mut pos = 0usize;
                    // OSC の終端ごとに区切って流し、その時点の画面を読む。
                    for (end, ev) in events {
                        parser.advance(&mut *term, &bytes[pos..end]);
                        pos = end;
                        if let Some(record) = tracker.on_event(id, &ev, &term) {
                            out.push(UiEvent::Command(id, record));
                        }
                        out.push(UiEvent::Osc(id, ev));
                    }
                    parser.advance(&mut *term, &bytes[pos..]);
                }
                for ev in out {
                    if ui_tx.send_event(ev).is_err() {
                        return;
                    }
                }
                // 直前の通知がまだ描画されていなければ、重ねて送らない。
                if dirty.swap(true, Ordering::AcqRel) {
                    skipped += 1;
                } else {
                    sent += 1;
                    if ui_tx
                        .send_event(UiEvent::Wakeup(id, std::time::Instant::now()))
                        .is_err()
                    {
                        if diag {
                            log::info!("[read {id}] wakeup の送信に失敗。読み取りを終える");
                        }
                        return;
                    }
                }
            }
        })
        .expect("読み取りスレッドを作れない");
}

fn spawn_waiter(
    id: SessionId,
    mut child: Box<dyn portable_pty::Child + Send + Sync>,
    ui_tx: UiSender,
) {
    thread::Builder::new()
        .name("tex-pty-wait".into())
        .spawn(move || {
            let code = match child.wait() {
                Ok(status) => status.exit_code() as i32,
                Err(_) => -1,
            };
            let _ = ui_tx.send_event(UiEvent::ChildExit(id, code));
        })
        .expect("終了待ちスレッドを作れない");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::term::TermSize;
    use alacritty_terminal::grid::Dimensions;
    use alacritty_terminal::index::{Column, Line};
    use std::sync::mpsc::channel;
    use std::time::{Duration, Instant};

    /// PTY で子プロセスを起動し、出力がグリッドに現れ、終了が通知されることを確かめる。
    #[test]
    fn 子プロセスの出力がグリッドに現れる() {
        let (tx, rx) = channel();
        let size = TermSize::new(40, 8);
        let spawned = spawn(
            7,
            &["/bin/echo".to_string(), "hello".to_string()],
            Path::new("/"),
            size,
            (8, 16),
            100,
            crate::term::UiSender::Channel(tx),
        )
        .expect("PTY を起動できる");

        let deadline = Instant::now() + Duration::from_secs(10);
        let mut exit_code = None;
        while Instant::now() < deadline && exit_code.is_none() {
            match rx.recv_timeout(Duration::from_millis(200)) {
                Ok(UiEvent::ChildExit(id, code)) => {
                    assert_eq!(id, 7);
                    exit_code = Some(code);
                }
                Ok(_) => {}
                Err(_) => {}
            }
        }
        assert_eq!(exit_code, Some(0), "子プロセスが終了コード 0 で終わる");

        // 出力の読み取りが終わるまで少し待つ。
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut found = false;
        while Instant::now() < deadline && !found {
            {
                let term = spawned.term.lock();
                let grid = term.grid();
                let mut text = String::new();
                for line in 0..grid.screen_lines() {
                    for col in 0..grid.columns() {
                        text.push(grid[Line(line as i32)][Column(col)].c);
                    }
                }
                found = text.contains("hello");
            }
            if !found {
                std::thread::sleep(Duration::from_millis(50));
            }
        }
        assert!(found, "グリッドに hello が現れる");
    }
}

#[cfg(test)]
mod shell_integration_tests {
    use super::*;
    use crate::term::TermSize;
    use std::sync::mpsc::channel;
    use std::time::{Duration, Instant};

    /// 同梱するシェル統合を実際の zsh に読み込ませ、コマンドが記録されるか確かめる。
    #[test]
    fn zsh_のシェル統合からコマンドを記録できる() {
        if !Path::new("/bin/zsh").exists() {
            eprintln!("zsh がないため飛ばす");
            return;
        }
        let (tx, rx) = channel();
        let spawned = spawn(
            11,
            &[
                "/bin/zsh".to_string(),
                "-f".to_string(),
                "-i".to_string(),
            ],
            Path::new("/tmp"),
            TermSize::new(80, 24),
            (8, 16),
            200,
            crate::term::UiSender::Channel(tx),
        )
        .expect("zsh を起動できる");

        // 統合を読み込ませてから、記録したいコマンドを打つ。
        let mut script = crate::osc::ZSH_INTEGRATION.replace('\n', "\n");
        script.push_str("\nprint -n ''\n");
        spawned.handle.write(script.into_bytes());
        std::thread::sleep(Duration::from_millis(400));
        spawned.handle.write(b"echo hello-from-zsh\n".to_vec());

        let deadline = Instant::now() + Duration::from_secs(15);
        let mut found = None;
        while Instant::now() < deadline && found.is_none() {
            match rx.recv_timeout(Duration::from_millis(200)) {
                Ok(UiEvent::Command(id, record)) => {
                    assert_eq!(id, 11);
                    if record.command.contains("echo hello-from-zsh") {
                        found = Some(record);
                    }
                }
                _ => {}
            }
        }
        let record = found.expect("echo の記録が届く");
        assert_eq!(record.command, "echo hello-from-zsh");
        assert_eq!(record.exit_code, Some(0));
        // macOS では /tmp は /private/tmp へ解決される。
        assert!(
            record.cwd.ends_with("/tmp"),
            "OSC 7 から作業ディレクトリを取れる: {}",
            record.cwd
        );

        let mut handle = spawned.handle;
        handle.kill();
    }
}

#[cfg(test)]
mod tracker_tests {
    use super::*;
    use crate::term::{EventProxy, TermSize, UiSender};
    use alacritty_terminal::event::WindowSize;
    use std::sync::mpsc::channel;

    fn make_term(cols: usize, lines: usize) -> alacritty_terminal::Term<EventProxy> {
        let (pty_tx, _pty_rx) = channel();
        let (ui_tx, _ui_rx) = channel();
        let size = TermSize::new(cols, lines);
        let ws = Arc::new(FairMutex::new(WindowSize {
            num_lines: lines as u16,
            num_cols: cols as u16,
            cell_width: 8,
            cell_height: 16,
        }));
        let proxy = EventProxy::new(1, pty_tx, UiSender::Channel(ui_tx), ws);
        crate::term::new_term(size, 100, proxy)
    }

    fn run(prelude: &[u8], typed: &[u8], cols: usize, lines: usize) -> Option<CommandRecord> {
        let mut term = make_term(cols, lines);
        let mut parser: Processor<StdSyncHandler> = Processor::new();
        let mut tracker = CommandTracker::default();
        parser.advance(&mut term, prelude);
        tracker.on_event(1, &OscEvent::CommandStart, &term);
        parser.advance(&mut term, typed);
        tracker.on_event(1, &OscEvent::CommandExecuted, &term);
        tracker.on_event(1, &OscEvent::CommandFinished(Some(0)), &term)
    }

    #[test]
    fn 改行で画面がスクロールしてもコマンドを読み出せる() {
        // 3 行の画面を埋めてから入力するので、Enter で必ずスクロールする。
        let rec = run(b"one\r\ntwo\r\n$ ", b"echo hi\r\n", 20, 3).expect("記録が作られる");
        assert_eq!(rec.command, "echo hi");
        assert_eq!(rec.exit_code, Some(0));
    }

    #[test]
    fn スクロールしない位置でもコマンドを読み出せる() {
        let rec = run(b"$ ", b"cargo test\r\n", 40, 10).expect("記録が作られる");
        assert_eq!(rec.command, "cargo test");
    }

    #[test]
    fn 画面幅を超える入力を折り返して読み出せる() {
        // 20 桁の画面に、プロンプトを含めて 2 行ぶんの入力を打つ。
        let long = "echo 123456789012345678901234567890";
        let typed = format!("{long}\r\n");
        let rec = run(b"$ ", typed.as_bytes(), 20, 6).expect("記録が作られる");
        assert_eq!(rec.command, long);
    }

    #[test]
    fn 空の入力は記録しない() {
        assert!(run(b"$ ", b"\r\n", 40, 10).is_none());
    }

    #[test]
    fn 作業ディレクトリと_エージェント_id_を保つ() {
        let mut term = make_term(40, 10);
        let mut parser: Processor<StdSyncHandler> = Processor::new();
        let mut tracker = CommandTracker::default();
        tracker.on_event(1, &OscEvent::Cwd("/repo".into()), &term);
        tracker.on_event(1, &OscEvent::AgentId("abc-123".into()), &term);
        parser.advance(&mut term, b"$ ");
        tracker.on_event(1, &OscEvent::CommandStart, &term);
        parser.advance(&mut term, b"ls\r\n");
        tracker.on_event(1, &OscEvent::CommandExecuted, &term);
        let rec = tracker
            .on_event(1, &OscEvent::CommandFinished(Some(2)), &term)
            .expect("記録が作られる");
        assert_eq!(rec.cwd, "/repo");
        assert_eq!(rec.agent_id.as_deref(), Some("abc-123"));
        assert_eq!(rec.exit_code, Some(2));
    }
}

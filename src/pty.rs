//! PTY の生成と、読み取り・書き込み・終了待ちの各スレッド。
//!
//! `alacritty_terminal` の `event_loop` は使わない。
//! OSC 133 と OSC 1337 を拾うために、PTY の生バイト列が必要だからである。

use std::io::{Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;
use std::thread;
use std::time::SystemTime;

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
    /// 直接起動した子の pid。前面のプロセス群が分からないときに使う。
    child_pid: Option<u32>,
}

impl PtyHandle {
    /// いま前面にいるプロセスの pid。
    ///
    /// シェルが別のプログラムを起動していれば、そちらの pid になる。
    /// 作業ディレクトリを尋ねる相手として、これが最も実態に近い。
    pub fn foreground_pid(&self) -> Option<i32> {
        #[cfg(unix)]
        if let Some(pid) = self.master.process_group_leader() {
            return Some(pid);
        }
        self.child_pid.map(|p| p as i32)
    }
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
            SpawnError::OpenPty(e) => write!(f, "cannot open pty: {e}"),
            SpawnError::Spawn(prog, e) => write!(f, "cannot spawn {prog}: {e}"),
        }
    }
}

/// 起動に必要なもの一式。
pub struct SpawnOptions<'a> {
    pub id: SessionId,
    pub argv: &'a [String],
    pub cwd: &'a Path,
    pub size: TermSize,
    /// 1 セルの幅と高さ（画素）。PTY へ窓の大きさとして伝える。
    pub cell: (u16, u16),
    pub scrollback: usize,
    /// 再起動をまたいで残るセッションの鍵。記録に付ける。
    pub session_key: String,
}

/// 引数列を PTY 上で起動し、読み書きのスレッドを立てる。
pub fn spawn(opts: SpawnOptions<'_>, ui_tx: UiSender) -> Result<Spawned, SpawnError> {
    let SpawnOptions {
        id,
        argv,
        cwd,
        size,
        cell,
        scrollback,
        session_key,
    } = opts;
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
    cmd.env("TERM_PROGRAM", "termit");
    cmd.env("TERM_PROGRAM_VERSION", env!("CARGO_PKG_VERSION"));
    cmd.env("TERMIT_SESSION", id.to_string());
    cmd.env("TERMIT_SHELL_INTEGRATION", "1");

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
        session_key,
    );

    let mut killer = child.clone_killer();
    let child_pid = child.process_id();
    spawn_waiter(id, child, ui_tx);
    let _ = &mut killer;

    Ok(Spawned {
        handle: PtyHandle {
            master: pair.master,
            killer,
            writer_tx,
            child_pid,
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
    session_key: String,
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
                    session_key: self.session_key.clone(),
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
    session_key: String,
) {
    thread::Builder::new()
        .name("termit-pty-read".into())
        .spawn(move || {
            let mut parser: Processor<StdSyncHandler> = Processor::new();
            let mut scanner = OscScanner::new();
            let mut tracker = CommandTracker {
                cwd,
                session_key,
                ..CommandTracker::default()
            };
            let mut buf = vec![0u8; 65536];
            let diag = std::env::var("TERMIT_FRAME_LOG").is_ok();
            let mut read_bytes = 0u64;
            let mut reads = 0u64;
            let mut sent = 0u64;
            let mut skipped = 0u64;
            let mut last = std::time::Instant::now();
            loop {
                let n = match reader.read(&mut buf) {
                    Ok(0) | Err(_) => {
                        if diag {
                            log::info!("[read {id}] eof reads={reads} bytes={read_bytes}");
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
                            "[read {id}] 1s: reads={reads} bytes={read_bytes} wakeup_sent={sent} coalesced={skipped}"
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
                            log::info!("[read {id}] wakeup send failed; stopping reader");
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
        let spawned = spawn(
            SpawnOptions {
                id: 7,
                // 出力を読み終える前に子が終わると、擬似端末に残った
                // ぶんが捨てられることがある。読む間だけ生かしておく。
                argv: &[
                    "/bin/sh".to_string(),
                    "-c".to_string(),
                    "echo hello; sleep 1".to_string(),
                ],
                cwd: Path::new("/"),
                size: TermSize::new(40, 8),
                cell: (8, 16),
                scrollback: 100,
                session_key: "test-key".to_string(),
            },
            crate::term::UiSender::Channel(tx),
        )
        .expect("PTY を起動できる");

        // 先に画面を見る。子はまだ生きている。
        let deadline = Instant::now() + Duration::from_secs(10);
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

        // 読み終えてから、終わり方を見る。
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
    }
}

/// OSC 52 でクリップボードへ書けることを確かめる。
///
/// コンテナの中で走るエージェントは `pbcopy` に手が届かない。
/// 外へコピーする道はこれだけなので、経路が生きているかを見張る。
#[cfg(test)]
mod osc52_tests {
    use super::*;
    use crate::term::TermSize;
    use std::sync::mpsc::channel;
    use std::time::{Duration, Instant};

    /// 台本を走らせ、届いたクリップボードの中身を返す。
    fn stored(script: &str) -> Option<String> {
        let (tx, rx) = channel();
        let _spawned = spawn(
            SpawnOptions {
                id: 1,
                argv: &["/bin/sh".into(), "-c".into(), script.into()],
                cwd: Path::new("/"),
                size: TermSize::new(80, 24),
                cell: (8, 16),
                scrollback: 100,
                session_key: "osc52".into(),
            },
            crate::term::UiSender::Channel(tx),
        )
        .expect("PTY を起動できる");
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if let Ok(UiEvent::ClipboardStore(_, text)) =
                rx.recv_timeout(Duration::from_millis(200))
            {
                return Some(text);
            }
        }
        None
    }

    #[test]
    fn bel_で終わる_osc52_を受け取る() {
        if !std::path::Path::new("/bin/sh").exists() {
            return;
        }
        assert_eq!(
            stored("printf '\\033]52;c;aGVsbG8=\\007'; sleep 1").as_deref(),
            Some("hello")
        );
    }

    #[test]
    fn st_で終わる_osc52_を受け取る() {
        if !std::path::Path::new("/bin/sh").exists() {
            return;
        }
        assert_eq!(
            stored("printf '\\033]52;c;aGVsbG8=\\033\\\\'; sleep 1").as_deref(),
            Some("hello")
        );
    }

    #[test]
    fn 大きな_osc52_も切り詰めない() {
        if !std::path::Path::new("/bin/sh").exists() {
            return;
        }
        // 画面ぶんを超える貼り付けが切れないこと。
        let script = "b=$(head -c 65536 /dev/zero | tr '\\0' a | base64 | tr -d '\\n'); \
                      printf '\\033]52;c;%s\\007' \"$b\"; sleep 2";
        assert_eq!(stored(script).map(|s| s.len()), Some(65536));
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
            SpawnOptions {
                id: 11,
                argv: &["/bin/zsh".to_string(), "-f".to_string(), "-i".to_string()],
                cwd: Path::new("/tmp"),
                size: TermSize::new(80, 24),
                cell: (8, 16),
                scrollback: 200,
                session_key: "test-key".to_string(),
            },
            crate::term::UiSender::Channel(tx),
        )
        .expect("zsh を起動できる");

        // 統合を読み込ませてから、記録したいコマンドを打つ。
        let mut script = crate::osc::ZSH_INTEGRATION.to_string();
        script.push_str("\nprint -n ''\n");
        spawned.handle.write(script.into_bytes());
        std::thread::sleep(Duration::from_millis(400));
        spawned.handle.write(b"echo hello-from-zsh\n".to_vec());

        let deadline = Instant::now() + Duration::from_secs(15);
        let mut found = None;
        while Instant::now() < deadline && found.is_none() {
            if let Ok(UiEvent::Command(id, record)) = rx.recv_timeout(Duration::from_millis(200)) {
                assert_eq!(id, 11);
                if record.command.contains("echo hello-from-zsh") {
                    found = Some(record);
                }
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

#[cfg(test)]
mod clear_tests {
    use super::*;
    use crate::term::TermSize;
    use alacritty_terminal::grid::Dimensions;
    use alacritty_terminal::index::{Column, Line};
    use alacritty_terminal::vte::ansi::{ClearMode, Handler as _};
    use std::sync::mpsc::channel;
    use std::time::{Duration, Instant};

    fn grid_text(term: &alacritty_terminal::Term<EventProxy>) -> String {
        let grid = term.grid();
        let mut out = String::new();
        for l in 0..grid.screen_lines() {
            for c in 0..grid.columns() {
                out.push(grid[Line(l as i32)][Column(c)].c);
            }
        }
        out
    }

    fn wait_for(spawned: &crate::pty::Spawned, needle: &str, present: bool, secs: u64) -> bool {
        let deadline = Instant::now() + Duration::from_secs(secs);
        while Instant::now() < deadline {
            {
                let term = spawned.term.lock();
                if grid_text(&term).contains(needle) == present {
                    return true;
                }
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    /// 画面消去が、スクロールバックと表示中の内容の両方を片付けることを確かめる。
    #[test]
    fn 画面消去で内容とスクロールバックが消える() {
        if !Path::new("/bin/zsh").exists() {
            eprintln!("zsh がないため飛ばす");
            return;
        }
        let (tx, _rx) = channel();
        let spawned = spawn(
            SpawnOptions {
                id: 21,
                argv: &["/bin/zsh".to_string(), "-f".to_string(), "-i".to_string()],
                cwd: Path::new("/tmp"),
                size: TermSize::new(80, 10),
                cell: (8, 16),
                scrollback: 500,
                session_key: "test-key".to_string(),
            },
            crate::term::UiSender::Channel(tx),
        )
        .expect("zsh を起動できる");
        std::thread::sleep(Duration::from_millis(400));

        // 画面の高さを超える量を出し、スクロールバックにも積む。
        spawned
            .handle
            .write(b"for i in $(seq 1 40); do echo MARKER-$i; done\n".to_vec());
        assert!(
            wait_for(&spawned, "MARKER-40", true, 10),
            "出力が画面に現れる"
        );

        // 端末側でスクロールバックを捨て、シェルへ Ctrl+L を送る。
        {
            let mut term = spawned.term.lock();
            term.clear_screen(ClearMode::Saved);
            assert_eq!(term.grid().history_size(), 0, "スクロールバックが空になる");
        }
        spawned.handle.write(vec![0x0c]);

        // 本体と同じく、押し出しが終わるまで履歴を捨て続ける。
        let until = Instant::now() + Duration::from_millis(250);
        while Instant::now() < until {
            spawned.term.lock().clear_screen(ClearMode::Saved);
            std::thread::sleep(Duration::from_millis(5));
        }

        assert!(
            wait_for(&spawned, "MARKER-", false, 10),
            "画面から古い内容が消える"
        );
        {
            let term = spawned.term.lock();
            let grid = term.grid();
            let hist = grid.history_size();
            let mut back = String::new();
            for l in 1..=hist {
                for c in 0..grid.columns() {
                    back.push(grid[Line(-(l as i32))][Column(c)].c);
                }
            }
            assert!(
                !back.contains("MARKER-"),
                "スクロールバックに古い出力が残っている: {}",
                back.trim()
            );
        }

        let mut handle = spawned.handle;
        handle.kill();
    }
}

#[cfg(test)]
mod mode_tests {
    use super::*;
    use crate::mouse;
    use crate::term::TermSize;
    use alacritty_terminal::term::TermMode;
    use std::sync::mpsc::channel;
    use std::time::{Duration, Instant};
    use winit::keyboard::ModifiersState;

    fn wait_mode(spawned: &crate::pty::Spawned, want: TermMode, secs: u64) -> TermMode {
        let deadline = Instant::now() + Duration::from_secs(secs);
        loop {
            let mode = *spawned.term.lock().mode();
            if mode.contains(want) || Instant::now() > deadline {
                return mode;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// 実際のバイト列から、マウスとフォーカスの要求が旗として立つことを確かめる。
    /// エージェントの全画面 UI が出すものと同じ並びを使う。
    #[test]
    fn マウスとフォーカスの要求が旗として立つ() {
        if !Path::new("/bin/sh").exists() {
            return;
        }
        let (tx, _rx) = channel();
        let spawned = spawn(
            SpawnOptions {
                id: 31,
                argv: &[
                    "/bin/sh".to_string(),
                    "-c".to_string(),
                    "printf '\\033[?1000h\\033[?1002h\\033[?1003h\\033[?1004h\\033[?1006h'; sleep 5"
                        .to_string(),
                ],
                cwd: Path::new("/tmp"),
                size: TermSize::new(80, 24),
                cell: (8, 16),
                scrollback: 100,
                session_key: "test-key".to_string(),
            },
            crate::term::UiSender::Channel(tx),
        )
        .expect("起動できる");

        // 1000 と 1002 と 1003 は排他で、後から設定したものが前を置き換える。
        // 三つを順に設定するエージェントは、結果として「全移動を報告」になる。
        let want = TermMode::MOUSE_MOTION | TermMode::FOCUS_IN_OUT | TermMode::SGR_MOUSE;
        let mode = wait_mode(&spawned, want, 10);
        assert!(mode.contains(want), "要求した旗が立つ: {mode:?}");
        assert!(
            !mode.contains(TermMode::MOUSE_REPORT_CLICK),
            "後から設定した 1003 が 1000 を置き換える: {mode:?}"
        );
        assert!(mouse::wants_mouse(mode), "マウスの報告が要求されている");

        // 旗が立っていれば、押下が SGR の形で送れる。
        let bytes = mouse::encode(
            mouse::Kind::Press,
            mouse::Button::Left,
            4,
            9,
            ModifiersState::empty(),
            mode,
        )
        .expect("報告の列ができる");
        assert_eq!(bytes, b"\x1b[<0;5;10M");

        let mut handle = spawned.handle;
        handle.kill();
    }

    /// 押下だけを要求する場合の旗を確かめる。
    #[test]
    fn クリックだけの要求も旗として立つ() {
        if !Path::new("/bin/sh").exists() {
            return;
        }
        let (tx, _rx) = channel();
        let spawned = spawn(
            SpawnOptions {
                id: 33,
                argv: &[
                    "/bin/sh".to_string(),
                    "-c".to_string(),
                    "printf '\\033[?1000h\\033[?1006h'; sleep 5".to_string(),
                ],
                cwd: Path::new("/tmp"),
                size: TermSize::new(80, 24),
                cell: (8, 16),
                scrollback: 100,
                session_key: "test-key".to_string(),
            },
            crate::term::UiSender::Channel(tx),
        )
        .expect("起動できる");

        let want = TermMode::MOUSE_REPORT_CLICK | TermMode::SGR_MOUSE;
        let mode = wait_mode(&spawned, want, 10);
        assert!(mode.contains(want), "押下の要求が立つ: {mode:?}");
        // 押下だけの要求では、移動を送らない。
        assert_eq!(
            mouse::encode(
                mouse::Kind::Move,
                mouse::Button::Left,
                0,
                0,
                ModifiersState::empty(),
                mode
            ),
            None
        );

        let mut handle = spawned.handle;
        handle.kill();
    }

    /// 代替画面へ入ると、車輪が矢印キーに変わることを確かめる。
    #[test]
    fn 代替画面では車輪が矢印になる() {
        if !Path::new("/bin/sh").exists() {
            return;
        }
        let (tx, _rx) = channel();
        let spawned = spawn(
            SpawnOptions {
                id: 32,
                argv: &[
                    "/bin/sh".to_string(),
                    "-c".to_string(),
                    "printf '\\033[?1049h'; sleep 5".to_string(),
                ],
                cwd: Path::new("/tmp"),
                size: TermSize::new(80, 24),
                cell: (8, 16),
                scrollback: 100,
                session_key: "test-key".to_string(),
            },
            crate::term::UiSender::Channel(tx),
        )
        .expect("起動できる");

        let mode = wait_mode(&spawned, TermMode::ALT_SCREEN, 10);
        assert!(mode.contains(TermMode::ALT_SCREEN), "代替画面に入る");
        assert!(
            mode.contains(TermMode::ALTERNATE_SCROLL),
            "代替スクロールは既定で有効"
        );
        assert_eq!(
            mouse::alternate_scroll(3, mode).expect("矢印に変わる"),
            b"\x1b[A\x1b[A\x1b[A"
        );

        let mut handle = spawned.handle;
        handle.kill();
    }
}

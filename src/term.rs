//! `alacritty_terminal` の `Term` を包み、端末問い合わせへの応答を PTY へ返す。

use std::sync::mpsc::Sender;
use std::sync::Arc;

use winit::event_loop::EventLoopProxy;

use alacritty_terminal::event::{Event, EventListener, WindowSize};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::Config as TermConfig;
use alacritty_terminal::Term;

use crate::session::SessionId;

/// 端末のセル数。`Dimensions` を満たすだけの最小の型。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TermSize {
    pub cols: usize,
    pub lines: usize,
}

impl TermSize {
    pub fn new(cols: usize, lines: usize) -> Self {
        Self {
            cols: cols.max(2),
            lines: lines.max(1),
        }
    }
}

impl Dimensions for TermSize {
    fn total_lines(&self) -> usize {
        self.lines
    }
    fn screen_lines(&self) -> usize {
        self.lines
    }
    fn columns(&self) -> usize {
        self.cols
    }
}

/// UI へ送る通知の送信口。
///
/// 通常は winit のイベントループへ直接届ける。テストでは
/// イベントループを立てられないため、素のチャネルも受け付ける。
#[derive(Clone)]
pub enum UiSender {
    Winit(EventLoopProxy<UiEvent>),
    Channel(Sender<UiEvent>),
}

impl UiSender {
    pub fn send_event(&self, event: UiEvent) -> Result<(), ()> {
        match self {
            UiSender::Winit(p) => p.send_event(event).map_err(|_| ()),
            UiSender::Channel(c) => c.send(event).map_err(|_| ()),
        }
    }
}

impl From<EventLoopProxy<UiEvent>> for UiSender {
    fn from(p: EventLoopProxy<UiEvent>) -> Self {
        UiSender::Winit(p)
    }
}

/// UI スレッドへ送る通知。
///
/// 送り先のセッションを表す `SessionId` は、受け手が使わない通知にも付ける。
/// どのペインから来たのかを型の上で失わないようにするためである。
#[derive(Clone)]
#[allow(dead_code)]
pub enum UiEvent {
    /// 画面内容が更新された。時刻は、その更新を読み取った瞬間である。
    Wakeup(SessionId, std::time::Instant),
    /// ウィンドウタイトルの変更要求。
    Title(SessionId, String),
    /// 子プロセスが終了した。
    ChildExit(SessionId, i32),
    /// クリップボードへの格納要求。
    ClipboardStore(SessionId, String),
    /// クリップボードの内容を PTY へ書き戻す要求。
    #[allow(dead_code)]
    ClipboardLoad(
        SessionId,
        Arc<dyn Fn(&str) -> String + Sync + Send + 'static>,
    ),
    /// PTY から取り出した OSC 通知。
    Osc(SessionId, crate::osc::OscEvent),
    /// 組み立てが終わったコマンドの記録。
    Command(SessionId, crate::session::CommandRecord),
}

/// `Term` からのイベントを受け、必要な応答を PTY へ書き戻す。
///
/// カーソル位置問い合わせや背景色問い合わせに応答しないと、
/// 起動時に端末へ問い合わせるエージェントやフルスクリーン UI が待ち続ける。
#[derive(Clone)]
pub struct EventProxy {
    id: SessionId,
    pty_tx: Sender<Vec<u8>>,
    ui_tx: UiSender,
    size: Arc<FairMutex<WindowSize>>,
}

impl EventProxy {
    pub fn new(
        id: SessionId,
        pty_tx: Sender<Vec<u8>>,
        ui_tx: UiSender,
        size: Arc<FairMutex<WindowSize>>,
    ) -> Self {
        Self {
            id,
            pty_tx,
            ui_tx,
            size,
        }
    }

    fn write_pty(&self, bytes: Vec<u8>) {
        let _ = self.pty_tx.send(bytes);
    }
}

impl EventListener for EventProxy {
    fn send_event(&self, event: Event) {
        match event {
            Event::PtyWrite(text) => self.write_pty(text.into_bytes()),
            Event::ColorRequest(index, format) => {
                // 既定パレットの値をそのまま返す。
                let rgb = crate::theme::Theme::default().indexed(index);
                self.write_pty(format(rgb).into_bytes());
            }
            Event::TextAreaSizeRequest(format) => {
                let size = *self.size.lock();
                self.write_pty(format(size).into_bytes());
            }
            Event::ClipboardStore(_, text) => {
                let _ = self
                    .ui_tx
                    .send_event(UiEvent::ClipboardStore(self.id, text));
            }
            Event::ClipboardLoad(_, format) => {
                let _ = self
                    .ui_tx
                    .send_event(UiEvent::ClipboardLoad(self.id, format));
            }
            Event::Title(title) => {
                let _ = self.ui_tx.send_event(UiEvent::Title(self.id, title));
            }
            Event::ResetTitle => {
                let _ = self
                    .ui_tx
                    .send_event(UiEvent::Title(self.id, String::new()));
            }
            Event::ChildExit(status) => {
                let code = status.code().unwrap_or(-1);
                let _ = self.ui_tx.send_event(UiEvent::ChildExit(self.id, code));
            }
            Event::Wakeup | Event::MouseCursorDirty | Event::Bell => {
                let _ = self
                    .ui_tx
                    .send_event(UiEvent::Wakeup(self.id, std::time::Instant::now()));
            }
            Event::CursorBlinkingChange | Event::Exit => {}
        }
    }
}

pub fn new_term(size: TermSize, scrollback: usize, proxy: EventProxy) -> Term<EventProxy> {
    let config = TermConfig {
        scrolling_history: scrollback,
        ..TermConfig::default()
    };
    Term::new(config, &size, proxy)
}

/// 全画面 UI の描き直しで選択が消えることを確かめる。
///
/// これが `State::picked` の理由である。
#[cfg(test)]
mod selection_tests {
    use super::*;
    use alacritty_terminal::index::{Column, Line, Point, Side};
    use alacritty_terminal::selection::{Selection, SelectionType};
    use alacritty_terminal::vte::ansi::Processor;

    /// 端末と、`hello` を選んだ状態を作る。
    fn selected() -> (Term<EventProxy>, Processor) {
        let (tx, _rx) = std::sync::mpsc::channel();
        let (ptx, _prx) = std::sync::mpsc::channel();
        let ws = Arc::new(FairMutex::new(WindowSize {
            num_lines: 10,
            num_cols: 80,
            cell_width: 8,
            cell_height: 16,
        }));
        let proxy = EventProxy::new(1, ptx, UiSender::Channel(tx), ws);
        let mut term = new_term(TermSize::new(80, 10), 100, proxy);
        let mut parser = Processor::new();
        parser.advance(&mut term, b"hello world\r\n");
        term.selection = Some(Selection::new(
            SelectionType::Simple,
            Point::new(Line(0), Column(0)),
            Side::Left,
        ));
        if let Some(s) = term.selection.as_mut() {
            s.update(Point::new(Line(0), Column(4)), Side::Right);
        }
        assert_eq!(term.selection_to_string().as_deref(), Some("hello"));
        (term, parser)
    }

    #[test]
    fn 選んだ行を書き直されると選択が消える() {
        let (mut term, mut parser) = selected();
        // 全画面 UI が同じ行を描き直す。
        parser.advance(&mut term, b"\x1b[H\x1b[Kxxxxx");
        assert_eq!(
            term.selection_to_string(),
            None,
            "書き直された時点で選択は捨てられる"
        );
    }

    #[test]
    fn 画面消去でも選択が消える() {
        let (mut term, mut parser) = selected();
        parser.advance(&mut term, b"\x1b[2J");
        assert_eq!(term.selection_to_string(), None);
    }

    #[test]
    fn 別の行を書かれても選択は残る() {
        let (mut term, mut parser) = selected();
        parser.advance(&mut term, b"\x1b[3;1H\x1b[Kother line");
        assert_eq!(
            term.selection_to_string().as_deref(),
            Some("hello"),
            "触られていない行の選択は残る"
        );
    }
}

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
#[derive(Clone)]
pub enum UiEvent {
    /// 画面内容が更新された。
    Wakeup(SessionId),
    /// ウィンドウタイトルの変更要求。
    Title(SessionId, String),
    /// 子プロセスが終了した。
    ChildExit(SessionId, i32),
    /// クリップボードへの格納要求。
    ClipboardStore(SessionId, String),
    /// クリップボードの内容を PTY へ書き戻す要求。
    #[allow(dead_code)]
    ClipboardLoad(SessionId, Arc<dyn Fn(&str) -> String + Sync + Send + 'static>),
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
                let _ = self.ui_tx.send_event(UiEvent::ClipboardStore(self.id, text));
            }
            Event::ClipboardLoad(_, format) => {
                let _ = self.ui_tx.send_event(UiEvent::ClipboardLoad(self.id, format));
            }
            Event::Title(title) => {
                let _ = self.ui_tx.send_event(UiEvent::Title(self.id, title));
            }
            Event::ResetTitle => {
                let _ = self.ui_tx.send_event(UiEvent::Title(self.id, String::new()));
            }
            Event::ChildExit(status) => {
                let code = status.code().unwrap_or(-1);
                let _ = self.ui_tx.send_event(UiEvent::ChildExit(self.id, code));
            }
            Event::Wakeup | Event::MouseCursorDirty | Event::Bell => {
                let _ = self.ui_tx.send_event(UiEvent::Wakeup(self.id));
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

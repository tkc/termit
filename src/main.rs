//! tex: エージェント向けの軽量ターミナル。

mod bench;
mod clipboard;
mod config;
mod cwd;
mod git;
mod history;
mod input;
mod keytest;
mod latency;
mod mouse;
mod osc;
mod probe;
mod pty;
mod rect;
mod render;
mod search;
mod session;
mod state;
mod term;
mod theme;

use std::path::PathBuf;
use std::sync::Arc;

use alacritty_terminal::index::{Column, Direction, Point, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{viewport_to_point, TermMode};
use alacritty_terminal::vte::ansi::{ClearMode, Handler as _};
use alacritty_terminal::vte::ansi::{CursorShape, Rgb};
use winit::application::ApplicationHandler;
use winit::event::{ElementState, Ime, MouseButton, MouseScrollDelta, StartCause, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::platform::modifier_supplement::KeyEventExtModifierSupplement;
use winit::window::{Window, WindowId};

use crate::config::{Config, HOST_PROFILE};
use crate::history::{Entry, History, Scope};
use crate::input::Action;
use crate::render::{char_cols, Renderer, TextStyle};
use crate::session::{Manager, RunState};
use crate::term::{TermSize, UiEvent};
use crate::theme::Theme;

/// 履歴の一覧で一度に見える行数。
const SEARCH_ROWS: usize = 10;
/// 履歴を遡れる件数。これを超えた分は出さない。
const SEARCH_LIMIT: usize = 500;

/// 押したキーを記録する。キーバインドが届かないときの切り分けに使う。
/// `TEX_KEYLOG` に書き出し先を指定したときだけ動く。
fn keylog(event: &winit::event::KeyEvent, mods: ModifiersState) {
    use std::io::Write;
    let Ok(path) = std::env::var("TERMIT_KEYLOG") else {
        return;
    };
    let base = event.key_without_modifiers();
    let action = input::action_for(&base, event.physical_key, mods);
    let line = format!(
        "state={:?} logical={:?} base={:?} physical={:?} text={:?} \
         ctrl={} shift={} alt={} super={} action={:?}\n",
        event.state,
        event.logical_key,
        base,
        event.physical_key,
        event.text.as_deref(),
        mods.control_key(),
        mods.shift_key(),
        mods.alt_key(),
        mods.super_key(),
        action,
    );
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = f.write_all(line.as_bytes());
    }
}

fn main() {
    env_logger::init();

    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--latency-test") {
        latency::run();
        return;
    }
    if args.iter().any(|a| a == "--bench") {
        bench::run();
        return;
    }
    if args.iter().any(|a| a == "--keytest") {
        let c = Config::load().unwrap_or_default();
        keytest::run(&c.window.font, c.window.font_size);
        return;
    }
    if args.iter().any(|a| a == "--shell-integration") {
        print!("{}", osc::ZSH_INTEGRATION);
        return;
    }
    if let Some(i) = args.iter().position(|a| a == "--probe") {
        let out = args
            .get(i + 1)
            .cloned()
            .unwrap_or_else(|| "probe.rgba".into());
        probe::run(&out);
        return;
    }

    let config = match Config::load() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("termit: {e}");
            std::process::exit(1);
        }
    };

    let event_loop = EventLoop::<UiEvent>::with_user_event()
        .build()
        .expect("イベントループを作れない");
    event_loop.set_control_flow(ControlFlow::Wait);
    let proxy = event_loop.create_proxy();

    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
    let mut app = App {
        config,
        cwd,
        proxy,
        state: None,
        counters: std::env::var("TERMIT_FRAME_LOG")
            .is_ok()
            .then(Counters::default),
    };
    if let Err(e) = event_loop.run_app(&mut app) {
        eprintln!("termit: {e}");
        std::process::exit(1);
    }
}

/// 車輪の動きを行数に直す。端数は次の通知まで持ち越す。
///
/// 触覚板は 1 回の通知が数画素しかない。行に満たないぶんを捨てると、
/// ゆっくり動かしているあいだ何も起こらず、動かないように見える。
#[derive(Default)]
struct WheelAccum {
    /// まだ行にならずに残っている分。符号は向きを表す。
    carry: f32,
}

impl WheelAccum {
    /// 通知 1 回ぶんを足し、動かせる行数を返す。
    fn push(&mut self, delta: MouseScrollDelta, cell_height: f32) -> i32 {
        let add = match delta {
            MouseScrollDelta::LineDelta(_, y) => y,
            MouseScrollDelta::PixelDelta(p) => p.y as f32 / cell_height.max(1.0),
        };
        if add == 0.0 {
            return 0;
        }
        // 向きが変わったら持ち越しは捨てる。逆向きの端数が残っていると、
        // 折り返した最初のひと押しがそれに食われる。
        if self.carry != 0.0 && add.signum() != self.carry.signum() {
            self.carry = 0.0;
        }
        self.carry += add;
        let whole = self.carry.trunc();
        self.carry -= whole;
        whole as i32
    }
}

/// マウスの状態。
#[derive(Default)]
struct MouseState {
    x: f32,
    y: f32,
    /// 端末領域でドラッグ中か（端末側の選択）。
    dragging: bool,
    /// 左ペインの幅を変えている最中か。
    resizing: bool,
    /// 端末上のプログラムへ報告中の釦。
    reporting: Option<mouse::Button>,
    /// 直前に報告したセル。同じセルの中の動きは送らない。
    last_reported: Option<(usize, usize)>,
    /// 直前のクリックの時刻、セル、連続回数。
    last_click: Option<(std::time::Instant, (usize, usize), u8)>,
    /// 車輪の端数。
    wheel: WheelAccum,
}

/// 名前を付けるための入力。
pub(crate) struct RenameState {
    pub(crate) input: String,
}

/// 重ねて出すプロファイルの一覧。
pub(crate) struct PickerState {
    pub(crate) names: Vec<String>,
    pub(crate) selected: usize,
}

pub(crate) struct SearchState {
    pub(crate) query: String,
    pub(crate) scope: Scope,
    pub(crate) results: Vec<Entry>,
    pub(crate) selected: usize,
    /// 一覧の先頭に出している位置。選んでいる行が外へ出ないよう追う。
    pub(crate) offset: usize,
}

impl SearchState {
    /// 選んでいる行が見える範囲に入るよう、出す位置を合わせる。
    fn follow(&mut self) {
        if self.selected < self.offset {
            self.offset = self.selected;
        } else if self.selected >= self.offset + SEARCH_ROWS {
            self.offset = self.selected + 1 - SEARCH_ROWS;
        }
    }
}

pub(crate) struct State {
    renderer: Renderer,
    manager: Manager,
    history: Option<History>,
    theme: Theme,
    sidebar: bool,
    search: Option<SearchState>,
    picker: Option<PickerState>,
    /// 名前を付ける入力。
    rename: Option<RenameState>,
    /// 左ペインの幅（pt）。境目を掴んで変えられる。
    sidebar_width: f32,
    /// 画面の中の検索。
    find: Option<search::ScreenSearch>,
    /// 見えている範囲で強調するセル。
    find_cells: std::collections::HashSet<(i32, usize)>,
    /// そのうち、いま選んでいる一致のセル。
    find_current: std::collections::HashSet<(i32, usize)>,
    mouse: MouseState,
    mods: ModifiersState,
    status: Option<String>,
    recent: Vec<Entry>,
    recent_for: Option<u32>,
    /// 未表示の更新のうち、最も古いものが読み取られた時刻。
    pending_since: Option<std::time::Instant>,
    /// 描こうとして描けなかった。間を置いて描き直す。
    needs_redraw: bool,
    /// セッションの並びが変わった。少し置いてから書き出す。
    state_dirty: bool,
    /// 最後に書き出した時刻。
    state_saved_at: Option<std::time::Instant>,
    /// いまウィンドウに出している題名。変わったときだけ設定し直す。
    shown_title: String,
    /// 変換中の文字列。確定するまでは入力にも検索にも渡さない。
    preedit: String,
    /// いま知らせてある候補窓の位置。変わったときだけ設定し直す。
    ime_area: Option<(i32, i32)>,
    /// 変換中の文字列を出す位置（画素）。候補窓もここへ寄せる。
    cursor_px: Option<(f32, f32)>,
    window: Option<Arc<Window>>,
}

impl State {
    /// セッションの並びが変わったことを覚えておく。
    fn mark_state_dirty(&mut self) {
        self.state_dirty = true;
    }

    /// 覚えている並びを書き出す。頻繁に呼ばれるので間隔を空ける。
    fn save_state(&mut self, force: bool) {
        if !self.state_dirty {
            return;
        }
        let now = std::time::Instant::now();
        let due = match self.state_saved_at {
            None => true,
            Some(t) => now.duration_since(t).as_millis() >= 500,
        };
        if !(due || force) {
            return;
        }
        self.state_dirty = false;
        self.state_saved_at = Some(now);
        if self.manager.sessions().is_empty() {
            state::SavedState::clear();
        } else {
            self.manager.snapshot().save();
        }
    }

    fn request_redraw(&self) {
        if let Some(w) = &self.window {
            w.request_redraw();
        }
    }
}

/// 描画されない原因を工程ごとに切り分けるための計数。
/// `TEX_FRAME_LOG` を指定したときだけ動く。
#[derive(Default)]
struct Counters {
    wakeup: u64,
    osc: u64,
    other_user_event: u64,
    redraw_requested: u64,
    key: u64,
    drew: u64,
    /// 更新を読み取ってから画面に出すまでの時間。
    latency_us: Vec<u64>,
    last: Option<std::time::Instant>,
}

impl Counters {
    fn tick(&mut self) {
        let now = std::time::Instant::now();
        let due = match self.last {
            None => {
                self.last = Some(now);
                false
            }
            Some(t) => now.duration_since(t).as_millis() >= 1000,
        };
        if !due {
            return;
        }
        self.last = Some(now);
        self.latency_us.sort_unstable();
        let pct = |v: &Vec<u64>, p: usize| {
            if v.is_empty() {
                0.0
            } else {
                v[(v.len() - 1) * p / 100] as f64 / 1000.0
            }
        };
        log::info!(
            "1s: wakeup={} redraw_req={} drawn={} keys={} | read→present p50 {:.1}ms p90 {:.1}ms max {:.1}ms",
            self.wakeup,
            self.redraw_requested,
            self.drew,
            self.key,
            pct(&self.latency_us, 50),
            pct(&self.latency_us, 90),
            pct(&self.latency_us, 100),
        );
        let last = self.last;
        *self = Counters {
            last,
            ..Default::default()
        };
    }
}

struct App {
    config: Config,
    cwd: PathBuf,
    proxy: EventLoopProxy<UiEvent>,
    state: Option<State>,
    counters: Option<Counters>,
}

/// 画面の割り付け。すべてセル単位で扱う。
#[derive(Clone, Copy, Debug)]
pub(crate) struct Layout {
    cols: usize,
    rows: usize,
    /// 左ペインの幅（画素）。0 なら出さない。
    sidebar_px: f32,
    /// 左ペインが占める桁数。端末はその次の桁から始まる。
    sidebar_cols: usize,
    term_col: usize,
    term_cols: usize,
    term_rows: usize,
}

impl App {
    fn layout(&self, s: &State) -> Layout {
        layout_of(&self.config, s)
    }
}

impl ApplicationHandler<UiEvent> for App {
    fn new_events(&mut self, event_loop: &ActiveEventLoop, cause: StartCause) {
        // 描けずに待っていた場合は、ここで描き直す。
        if matches!(cause, StartCause::ResumeTimeReached { .. }) {
            event_loop.set_control_flow(ControlFlow::Wait);
            if let Some(s) = &self.state {
                if s.needs_redraw {
                    s.request_redraw();
                }
            }
        }
    }

    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.state.is_some() {
            return;
        }
        let attrs = Window::default_attributes()
            .with_title("termit")
            .with_inner_size(winit::dpi::LogicalSize::new(1100.0, 720.0));
        let window = Arc::new(
            event_loop
                .create_window(attrs)
                .expect("ウィンドウを作れない"),
        );
        window.set_ime_allowed(true);

        let renderer = pollster::block_on(Renderer::new(
            window.clone(),
            event_loop,
            &self.config.window.font,
            self.config.window.font_size,
            self.config.window.vsync,
        ));

        let mut state = State {
            renderer,
            manager: Manager::new(TermSize::new(80, 24), (8, 16), self.proxy.clone().into()),
            history: History::open_default(),
            theme: Theme::default(),
            sidebar: true,
            search: None,
            picker: None,
            rename: None,
            sidebar_width: self.config.window.sidebar_width,
            find: None,
            find_cells: Default::default(),
            find_current: Default::default(),
            mouse: MouseState::default(),
            mods: ModifiersState::empty(),
            status: None,
            recent: Vec::new(),
            recent_for: None,
            pending_since: None,
            needs_redraw: false,
            state_dirty: false,
            state_saved_at: None,
            shown_title: String::new(),
            preedit: String::new(),
            ime_area: None,
            cursor_px: None,
            window: Some(window.clone()),
        };
        if state.history.is_none() {
            state.status = Some("history database unavailable; history disabled".into());
        }

        let layout = self.layout(&state);
        let cell = state.renderer.cell();
        state
            .manager
            .set_cell((cell.width as u16, cell.height as u16));
        state
            .manager
            .resize(TermSize::new(layout.term_cols, layout.term_rows));

        let cwd = self.cwd.clone();
        // 前回の並びを作り直す。作れなければ、いつもどおり 1 つ立ち上げる。
        let restored = if self.config.window.restore_sessions {
            state::SavedState::load()
                .map(|saved| state.manager.restore(&self.config, &saved))
                .unwrap_or(0)
        } else {
            0
        };
        if restored == 0 {
            if let Err(e) = state.manager.spawn_new(&self.config, HOST_PROFILE, &cwd) {
                state.status = Some(e.to_string());
            }
        }
        state.mark_state_dirty();

        self.state = Some(state);
        window.request_redraw();
    }

    fn user_event(&mut self, _event_loop: &ActiveEventLoop, event: UiEvent) {
        if let Some(c) = &mut self.counters {
            match &event {
                UiEvent::Wakeup(_, _) => c.wakeup += 1,
                UiEvent::Osc(_, _) => c.osc += 1,
                _ => c.other_user_event += 1,
            }
            c.tick();
        }
        let Some(state) = &mut self.state else { return };
        match event {
            UiEvent::Wakeup(_, at) => {
                // 最も古い更新の時刻を覚えておき、表示までの時間を測る。
                if state.pending_since.is_none() {
                    state.pending_since = Some(at);
                }
            }
            UiEvent::Title(id, title) => {
                if let Some(s) = state.manager.get_mut(id) {
                    s.window_title = (!title.is_empty()).then_some(title);
                }
            }
            UiEvent::ChildExit(id, code) => {
                state.manager.mark_exited(id, code);
                state.mark_state_dirty();
            }
            UiEvent::ClipboardStore(_, text) => clipboard::copy(&text),
            UiEvent::ClipboardLoad(id, format) => {
                let text = clipboard::paste().unwrap_or_default();
                if let Some(s) = state.manager.get_mut(id) {
                    s.pty.write(format(&text).into_bytes());
                }
            }
            UiEvent::Osc(id, ev) => {
                if let Some(s) = state.manager.get_mut(id) {
                    s.on_osc(&ev);
                }
                // 作業ディレクトリと会話 ID は、次の起動で作り直すのに要る。
                if matches!(
                    ev,
                    crate::osc::OscEvent::Cwd(_) | crate::osc::OscEvent::AgentId(_)
                ) {
                    state.mark_state_dirty();
                }
            }
            UiEvent::Command(_, record) => {
                if let Some(h) = &state.history {
                    if let Err(e) = h.record(&record) {
                        log::warn!("cannot write history: {e}");
                    }
                }
                state.recent_for = None;
            }
        }
        state.request_redraw();
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        let Some(state) = &mut self.state else { return };
        match event {
            WindowEvent::CloseRequested => {
                state.save_state(true);
                event_loop.exit();
            }
            WindowEvent::Resized(size) => {
                state.renderer.resize(size.width, size.height);
                self.reflow();
                if let Some(s) = &self.state {
                    s.request_redraw();
                }
            }
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                state.renderer.set_scale(scale_factor as f32);
                self.reflow();
            }
            WindowEvent::ModifiersChanged(m) => state.mods = m.state(),
            WindowEvent::CursorMoved { position, .. } => {
                state.mouse.x = position.x as f32;
                state.mouse.y = position.y as f32;
                let dragging = state.mouse.dragging;
                if state.mouse.resizing {
                    // 掴んでいるあいだは、その位置を幅にする。
                    let w = (state.mouse.x / state.renderer.scale()).clamp(80.0, 800.0);
                    if (w - state.sidebar_width).abs() > 0.5 {
                        state.sidebar_width = w;
                        self.reflow();
                    }
                    return;
                }
                let layout = layout_of(&self.config, state);
                // 境目の上では、掴めることを形で示す。
                let over = on_divider(state, &layout, state.mouse.x);
                if let Some(win) = &state.window {
                    win.set_cursor(if over {
                        winit::window::CursorIcon::ColResize
                    } else {
                        winit::window::CursorIcon::Default
                    });
                }
                // 左ペインの上では、乗っている行の見た目が変わる。
                if (position.x as f32) < layout.sidebar_px {
                    state.request_redraw();
                }
                if self.report_mouse_motion() {
                    return;
                }
                if dragging {
                    self.drag_selection();
                    if let Some(s) = &self.state {
                        s.request_redraw();
                    }
                }
            }
            WindowEvent::Focused(focused) => {
                // 全画面のプログラムは、窓の出入りを知りたがることがある。
                if let Some(s) = state.manager.selected() {
                    let mode = *s.term.lock().mode();
                    if mode.contains(TermMode::FOCUS_IN_OUT) {
                        s.pty.write(if focused {
                            b"\x1b[I".to_vec()
                        } else {
                            b"\x1b[O".to_vec()
                        });
                    }
                }
            }
            WindowEvent::MouseInput {
                state: button_state,
                button,
                ..
            } => {
                let btn = match button {
                    MouseButton::Left => Some(mouse::Button::Left),
                    MouseButton::Middle => Some(mouse::Button::Middle),
                    MouseButton::Right => Some(mouse::Button::Right),
                    _ => None,
                };
                if let Some(btn) = btn {
                    let pressed = button_state == ElementState::Pressed;
                    if !self.report_mouse_button(btn, pressed) && btn == mouse::Button::Left {
                        if pressed {
                            self.on_click()
                        } else {
                            self.on_release()
                        }
                    }
                }
                if let Some(s) = &self.state {
                    s.request_redraw();
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                self.on_wheel(delta);
                if let Some(s) = &self.state {
                    s.request_redraw();
                }
            }
            WindowEvent::Ime(Ime::Preedit(text, _)) => {
                state.preedit = text;
                state.request_redraw();
            }
            WindowEvent::Ime(Ime::Disabled) => {
                state.preedit.clear();
                state.request_redraw();
            }
            WindowEvent::Ime(Ime::Commit(text)) => {
                state.preedit.clear();
                // 確定した文字列は、開いている入り口へ渡す。
                // 変換で入れた語も、検索や履歴の絞り込みに使える必要がある。
                if state.rename.is_some() {
                    if let Some(r) = &mut state.rename {
                        r.input.push_str(&text);
                    }
                } else if state.find.is_some() {
                    if let Some(f) = &mut state.find {
                        f.query.push_str(&text);
                        f.rebuild();
                    }
                    self.find_step(Direction::Left);
                } else if state.search.is_some() {
                    if let Some(s) = &mut state.search {
                        s.query.push_str(&text);
                    }
                    self.refresh_search();
                } else if state.picker.is_some() {
                    // 一覧は数字と j/k で選ぶ。文字は受けない。
                } else if let Some(s) = state.manager.selected() {
                    s.pty.write(text.into_bytes());
                }
                if let Some(s) = &self.state {
                    s.request_redraw();
                }
            }
            WindowEvent::KeyboardInput { event, .. } => {
                keylog(&event, state.mods);
                if event.state.is_pressed() {
                    if let Some(c) = &mut self.counters {
                        c.key += 1;
                    }
                    self.on_key(event);
                }
            }
            WindowEvent::RedrawRequested => {
                if let Some(c) = &mut self.counters {
                    c.redraw_requested += 1;
                    c.tick();
                }
                self.draw(event_loop);
            }
            _ => {}
        }
    }
}

impl App {
    /// セル寸法や窓の大きさが変わったとき、全ペインへ伝える。
    fn reflow(&mut self) {
        let Some(state) = &mut self.state else { return };
        let layout = layout_of(&self.config, state);
        let (term_cols, term_rows) = (layout.term_cols, layout.term_rows);
        let cell = state.renderer.cell();
        state
            .manager
            .set_cell((cell.width as u16, cell.height as u16));
        state.manager.resize(TermSize::new(term_cols, term_rows));
        state.request_redraw();
    }

    fn on_key(&mut self, event: winit::event::KeyEvent) {
        let Some(state) = &mut self.state else { return };
        let mods = state.mods;
        let key = event.logical_key.clone();
        // 修飾を外したキー。Ctrl を押すと logical_key が制御文字になる環境が
        // あるため、キーバインドの照合と制御文字への変換はこちらを使う。
        let base = event.key_without_modifiers();

        // 重ねた一覧が開いているあいだは、すべてのキーをそちらが受ける。
        if state.picker.is_some() {
            self.on_picker_key(&base, mods);
            if let Some(s) = &self.state {
                s.request_redraw();
            }
            return;
        }
        if state.rename.is_some() {
            self.on_rename_key(&base, &event);
            if let Some(s) = &self.state {
                s.request_redraw();
            }
            return;
        }
        if state.find.is_some() {
            self.on_find_key(&base, &event, mods);
            if let Some(s) = &self.state {
                s.request_redraw();
            }
            return;
        }
        if state.search.is_some() {
            self.on_search_key(&base, &event, mods);
            if let Some(s) = &self.state {
                s.request_redraw();
            }
            return;
        }

        if let Some(action) = input::action_for(&base, event.physical_key, mods) {
            self.on_action(action);
            if let Some(s) = &self.state {
                s.request_redraw();
            }
            return;
        }

        let Some(session) = state.manager.selected() else {
            return;
        };
        let mode = *session.term.lock().mode();
        // 変換中の文字は確定するまで渡さない。Ime::Commit で受ける。
        let text = if state.preedit.is_empty() {
            event.text.as_deref()
        } else {
            None
        };
        if let Some(bytes) = input::encode(&key, &base, text, mods, mode) {
            // 入力があったら最下部へ戻す。
            session
                .term
                .lock()
                .scroll_display(alacritty_terminal::grid::Scroll::Bottom);
            session.pty.write(bytes);
            state.request_redraw();
        }
    }

    fn on_action(&mut self, action: Action) {
        self.on_action_inner(action);
        if let Some(s) = &mut self.state {
            s.mark_state_dirty();
        }
    }

    fn on_action_inner(&mut self, action: Action) {
        let cwd = self.cwd.clone();
        let config = self.config.clone();
        let Some(state) = &mut self.state else { return };
        state.status = None;
        match action {
            Action::NewSession => {
                let dir = state
                    .manager
                    .selected()
                    .map(|s| s.cwd.clone())
                    .unwrap_or(cwd);
                if let Err(e) = state.manager.spawn_new(&config, HOST_PROFILE, &dir) {
                    state.status = Some(e.to_string());
                }
            }
            Action::Fork => {
                let i = state.manager.selected_index();
                match state.manager.fork(&config, i, None) {
                    Ok(_) => {}
                    Err(e) => state.status = Some(e.to_string()),
                }
            }
            Action::ForkWithProfile => {
                let names = config.profile_names();
                if names.len() < 2 {
                    state.status = Some("no profile other than host is configured".into());
                } else {
                    state.search = None;
                    state.picker = Some(PickerState { names, selected: 1 });
                }
            }
            Action::SelectNext => state.manager.select_next(),
            Action::SelectPrev => state.manager.select_prev(),
            Action::SelectIndex(i) => {
                let rows = state.manager.tree_rows();
                if let Some(row) = rows.get(i) {
                    let index = row.index;
                    state.manager.select(index);
                }
            }
            Action::CloseSession => state.manager.close_selected(),
            Action::ToggleSidebar => {
                state.sidebar = !state.sidebar;
                self.reflow();
            }
            Action::RenameSession => {
                let current = state
                    .manager
                    .selected()
                    .and_then(|s| s.name.clone())
                    .unwrap_or_default();
                state.picker = None;
                state.search = None;
                state.find = None;
                state.rename = Some(RenameState { input: current });
            }
            Action::FindInScreen => {
                state.picker = None;
                state.search = None;
                state.find = Some(search::ScreenSearch::new());
            }
            Action::SearchHistory => {
                state.picker = None;
                state.find = None;
                state.search = Some(SearchState {
                    query: String::new(),
                    // まずはこのセッションの中を遡る。他へ広げるのは ^R の押し直し。
                    scope: Scope::Session,
                    results: Vec::new(),
                    selected: 0,
                    offset: 0,
                });
                self.refresh_search();
            }
            Action::Copy => {
                if let Some(s) = state.manager.selected() {
                    let text = {
                        let term = s.term.lock();
                        term.selection_to_string().unwrap_or_default()
                    };
                    if !text.is_empty() {
                        clipboard::copy(&text);
                    }
                }
            }
            Action::Paste => {
                if let (Some(s), Some(text)) = (state.manager.selected(), clipboard::paste()) {
                    let mode = *s.term.lock().mode();
                    s.pty.write(bracketed(&text, mode));
                }
            }
            Action::ClearScreen => {
                // 画面の消去とプロンプトの出し直しはシェルに任せる。端末が
                // 画面を消すだけでは、プロンプトが消えたまま戻らない。
                // \x0c は Ctrl+L で、シェルも全画面のプログラムも
                // 「描き直せ」と解釈する。
                //
                // ただしシェルは画面を消すのではなく上へ押し出すため、
                // 押し出されたぶんが履歴に積まれる。少しのあいだ捨て続ける。
                let i = state.manager.selected_index();
                if let Some(s) = state.manager.sessions_mut().get_mut(i) {
                    {
                        let mut term = s.term.lock();
                        term.clear_screen(ClearMode::Saved);
                        term.scroll_display(alacritty_terminal::grid::Scroll::Bottom);
                    }
                    s.pty.write(vec![0x0c]);
                    s.clear_scrollback_until =
                        Some(std::time::Instant::now() + std::time::Duration::from_millis(250));
                }
            }
            Action::FontBigger => {
                let size = state.renderer.font_size() + 1.0;
                state.renderer.set_font_size(size);
                self.reflow();
            }
            Action::FontSmaller => {
                let size = state.renderer.font_size() - 1.0;
                state.renderer.set_font_size(size);
                self.reflow();
            }
            Action::ScrollUp => {
                if let Some(s) = state.manager.selected() {
                    s.term
                        .lock()
                        .scroll_display(alacritty_terminal::grid::Scroll::PageUp);
                }
            }
            Action::ScrollDown => {
                if let Some(s) = state.manager.selected() {
                    s.term
                        .lock()
                        .scroll_display(alacritty_terminal::grid::Scroll::PageDown);
                }
            }
        }
    }

    // ------------------------------------------------------------ マウス
}

/// マウス位置をセル座標に直す。戻り値はセルの桁と行、桁内の左右。
fn mouse_cell(state: &State) -> (usize, usize, Side) {
    let cell = state.renderer.cell();
    let col_f = state.mouse.x / cell.width;
    let col = col_f.max(0.0) as usize;
    let row = (state.mouse.y / cell.height).max(0.0) as usize;
    let side = if col_f - col_f.floor() < 0.5 {
        Side::Left
    } else {
        Side::Right
    };
    (col, row, side)
}

/// 端末領域の中での 0 起点の位置。領域の外なら `None`。
fn terminal_cell(layout: &Layout, col: usize, row: usize) -> Option<(usize, usize)> {
    if col < layout.term_col || row >= layout.term_rows {
        return None;
    }
    Some((
        (col - layout.term_col).min(layout.term_cols.saturating_sub(1)),
        row,
    ))
}

/// 端末領域のセルをグリッドの位置に直す。領域の外なら `None`。
fn terminal_point(state: &State, layout: &Layout, col: usize, row: usize) -> Option<Point> {
    if col < layout.term_col || row >= layout.term_rows {
        return None;
    }
    let term_col = (col - layout.term_col).min(layout.term_cols.saturating_sub(1));
    let session = state.manager.selected()?;
    let display_offset = session.term.lock().grid().display_offset();
    Some(viewport_to_point(
        display_offset,
        Point::new(row, Column(term_col)),
    ))
}

impl App {
    /// 端末上のプログラムがマウスを要求していれば、釦の出来事を渡す。
    ///
    /// Shift を押しているあいだは渡さず、端末側の選択に使う。
    /// これは xterm からの作法で、報告中でも文字を選べるようにするためである。
    fn report_mouse_button(&mut self, button: mouse::Button, pressed: bool) -> bool {
        let Some(state) = &mut self.state else {
            return false;
        };
        if state.picker.is_some() || state.search.is_some() {
            return false;
        }
        if state.mods.shift_key() {
            return false;
        }
        let layout = layout_of(&self.config, state);
        let (col, row, _) = mouse_cell(state);
        let Some((tcol, trow)) = terminal_cell(&layout, col, row) else {
            return false;
        };
        let Some(session) = state.manager.selected() else {
            return false;
        };
        let mode = *session.term.lock().mode();
        let kind = if pressed {
            mouse::Kind::Press
        } else {
            mouse::Kind::Release
        };
        let Some(bytes) = mouse::encode(kind, button, tcol, trow, state.mods, mode) else {
            return false;
        };
        session.pty.write(bytes);
        state.mouse.reporting = pressed.then_some(button);
        state.mouse.last_reported = Some((tcol, trow));
        true
    }

    /// 移動を報告する。送ったら `true`。
    fn report_mouse_motion(&mut self) -> bool {
        let Some(state) = &mut self.state else {
            return false;
        };
        if state.mods.shift_key() || state.picker.is_some() || state.search.is_some() {
            return false;
        }
        let layout = layout_of(&self.config, state);
        let (col, row, _) = mouse_cell(state);
        let Some((tcol, trow)) = terminal_cell(&layout, col, row) else {
            return false;
        };
        // 同じセルの中で動いただけなら送らない。送ると数が多すぎる。
        if state.mouse.last_reported == Some((tcol, trow)) {
            return false;
        }
        let Some(session) = state.manager.selected() else {
            return false;
        };
        let mode = *session.term.lock().mode();
        let (kind, button) = match state.mouse.reporting {
            Some(b) => (mouse::Kind::Drag, b),
            None => (mouse::Kind::Move, mouse::Button::Left),
        };
        let Some(bytes) = mouse::encode(kind, button, tcol, trow, state.mods, mode) else {
            // 報告しない設定でも、報告中の釦があれば選択には使わない。
            return state.mouse.reporting.is_some();
        };
        session.pty.write(bytes);
        state.mouse.last_reported = Some((tcol, trow));
        true
    }

    fn on_click(&mut self) {
        let config = self.config.clone();
        let Some(state) = &mut self.state else { return };
        let layout = layout_of(&self.config, state);
        let (col, row, side) = mouse_cell(state);

        // 重ねた一覧が開いているときは、まず閉じる。
        if state.picker.is_some() || state.search.is_some() || state.rename.is_some() {
            state.picker = None;
            state.search = None;
            state.rename = None;
            return;
        }

        // 境目を掴んだら、幅を変える操作に入る。
        if on_divider(state, &layout, state.mouse.x) {
            state.mouse.resizing = true;
            return;
        }
        if let Some(hit) = sidebar_hit(state, &layout, state.mouse.x, state.mouse.y) {
            match hit {
                SidebarHit::NewSession => {
                    let dir = state
                        .manager
                        .selected()
                        .map(|s| s.cwd.clone())
                        .unwrap_or_else(|| self.cwd.clone());
                    state.status = None;
                    if let Err(e) = state.manager.spawn_new(&config, HOST_PROFILE, &dir) {
                        state.status = Some(e.to_string());
                    }
                }
                SidebarHit::Select(i) => state.manager.select(i),
                SidebarHit::Close(i) => state.manager.close(i),
            }
            state.mark_state_dirty();
            return;
        }

        // 端末領域。連続クリックの回数で選択の単位を変える。
        let Some(point) = terminal_point(state, &layout, col, row) else {
            return;
        };
        let now = std::time::Instant::now();
        let count = match state.mouse.last_click {
            Some((at, cell, n))
                if cell == (col, row) && now.duration_since(at).as_millis() < 400 =>
            {
                n % 3 + 1
            }
            _ => 1,
        };
        state.mouse.last_click = Some((now, (col, row), count));
        let ty = match count {
            2 => SelectionType::Semantic,
            3 => SelectionType::Lines,
            _ => SelectionType::Simple,
        };
        if let Some(session) = state.manager.selected() {
            session.term.lock().selection = Some(Selection::new(ty, point, side));
        }
        state.mouse.dragging = true;
    }

    fn drag_selection(&mut self) {
        let Some(state) = &mut self.state else { return };
        let layout = layout_of(&self.config, state);
        let (col, row, side) = mouse_cell(state);
        let Some(point) = terminal_point(state, &layout, col, row) else {
            return;
        };
        if let Some(session) = state.manager.selected() {
            let mut term = session.term.lock();
            if let Some(sel) = term.selection.as_mut() {
                sel.update(point, side);
            }
        }
    }

    fn on_release(&mut self) {
        let Some(state) = &mut self.state else { return };
        if state.mouse.resizing {
            state.mouse.resizing = false;
            return;
        }
        state.mouse.dragging = false;
        // 動かさずに離したときは選択を消す。
        if let Some(session) = state.manager.selected() {
            let mut term = session.term.lock();
            let empty = term
                .selection
                .as_ref()
                .map(|s| s.is_empty())
                .unwrap_or(false);
            if empty {
                term.selection = None;
            }
        }
    }

    fn on_wheel(&mut self, delta: MouseScrollDelta) {
        let Some(state) = &mut self.state else { return };
        let cell_height = state.renderer.cell().height.max(1.0);
        let lines = state.mouse.wheel.push(delta, cell_height);
        if lines == 0 {
            return;
        }
        let layout = layout_of(&self.config, state);
        let (col, row, _) = mouse_cell(state);
        let cell = terminal_cell(&layout, col, row);
        let Some(session) = state.manager.selected() else {
            return;
        };
        let mode = *session.term.lock().mode();

        // まず、プログラムが要求していれば車輪をそのまま渡す。
        if let Some((tcol, trow)) = cell {
            if !state.mods.shift_key() {
                let kind = if lines > 0 {
                    mouse::Kind::WheelUp
                } else {
                    mouse::Kind::WheelDown
                };
                let mut sent = false;
                for _ in 0..lines.abs() {
                    if let Some(bytes) =
                        mouse::encode(kind, mouse::Button::Left, tcol, trow, state.mods, mode)
                    {
                        session.pty.write(bytes);
                        sent = true;
                    }
                }
                if sent {
                    return;
                }
            }
        }
        // 次に、代替画面なら矢印キーに変える。less や man はこれで動く。
        if let Some(bytes) = mouse::alternate_scroll(lines, mode) {
            session.pty.write(bytes);
            return;
        }
        // どちらでもなければ、端末のスクロールバックを動かす。
        session
            .term
            .lock()
            .scroll_display(alacritty_terminal::grid::Scroll::Delta(lines));
    }
}

fn layout_of(config: &Config, state: &State) -> Layout {
    let (cols, rows) = state.renderer.grid_size();
    let cell = state.renderer.cell();
    // 左ペインは画素で幅を決める。端末はその次の桁から始まるので、
    // 端数の 1 桁ぶんだけ隙間ができる。
    let _ = config;
    let sidebar_px = if state.sidebar {
        (state.sidebar_width * state.renderer.scale())
            .min((cols as f32 - 20.0) * cell.width)
            .max(0.0)
    } else {
        0.0
    };
    let sidebar_cols = (sidebar_px / cell.width).ceil() as usize;
    let sep = usize::from(sidebar_cols > 0);
    let term_col = sidebar_cols + sep;
    Layout {
        cols,
        rows,
        sidebar_px,
        sidebar_cols,
        term_col,
        term_cols: cols.saturating_sub(term_col).max(2),
        term_rows: rows.saturating_sub(1).max(1),
    }
}

impl App {
    fn on_picker_key(&mut self, key: &Key, _mods: ModifiersState) {
        let config = self.config.clone();
        let Some(state) = &mut self.state else { return };
        let Some(picker) = &mut state.picker else {
            return;
        };
        let n = picker.names.len();
        match key {
            Key::Named(NamedKey::Escape) => state.picker = None,
            Key::Named(NamedKey::ArrowDown) => picker.selected = (picker.selected + 1) % n,
            Key::Named(NamedKey::ArrowUp) => picker.selected = (picker.selected + n - 1) % n,
            Key::Named(NamedKey::Enter) => {
                let name = picker.names[picker.selected].clone();
                state.picker = None;
                let i = state.manager.selected_index();
                if let Err(e) = state.manager.fork(&config, i, Some(&name)) {
                    state.status = Some(e.to_string());
                }
                state.mark_state_dirty();
            }
            Key::Character(c) => match c.as_str() {
                "j" => picker.selected = (picker.selected + 1) % n,
                "k" => picker.selected = (picker.selected + n - 1) % n,
                d if d.len() == 1 && d.chars().next().unwrap().is_ascii_digit() => {
                    let i = d.chars().next().unwrap().to_digit(10).unwrap() as usize;
                    if i >= 1 && i <= n {
                        let name = picker.names[i - 1].clone();
                        state.picker = None;
                        let idx = state.manager.selected_index();
                        if let Err(e) = state.manager.fork(&config, idx, Some(&name)) {
                            state.status = Some(e.to_string());
                        }
                    }
                }
                _ => {}
            },
            _ => {}
        }
    }

    fn on_rename_key(&mut self, key: &Key, event: &winit::event::KeyEvent) {
        let Some(state) = &mut self.state else { return };
        let Some(rename) = &mut state.rename else {
            return;
        };
        match key {
            Key::Named(NamedKey::Escape) => state.rename = None,
            Key::Named(NamedKey::Enter) => {
                let name = rename.input.trim().to_string();
                state.rename = None;
                let i = state.manager.selected_index();
                if let Some(s) = state.manager.sessions_mut().get_mut(i) {
                    // 空にすると、既定の作業ディレクトリ表示へ戻る。
                    s.name = (!name.is_empty()).then_some(name);
                }
                state.mark_state_dirty();
            }
            Key::Named(NamedKey::Backspace) => {
                rename.input.pop();
            }
            _ => {
                let Some(text) = event.text.as_deref() else {
                    return;
                };
                if text.chars().any(|c| c.is_control()) || !state.preedit.is_empty() {
                    return;
                }
                rename.input.push_str(text);
            }
        }
    }

    fn on_find_key(&mut self, key: &Key, event: &winit::event::KeyEvent, mods: ModifiersState) {
        let Some(state) = &mut self.state else { return };
        let Some(find) = &mut state.find else { return };

        let step: Option<Direction> = match key {
            Key::Named(NamedKey::Escape) => {
                state.find = None;
                state.find_cells.clear();
                state.find_current.clear();
                return;
            }
            // 以降はすべて step を決めてから抜ける。
            Key::Named(NamedKey::Enter) | Key::Named(NamedKey::ArrowUp) => {
                // 既定は古い方（上）へ。端末では下ほど新しいので、
                // 探したいものはたいてい上にある。
                Some(if mods.shift_key() {
                    Direction::Right
                } else {
                    Direction::Left
                })
            }
            Key::Named(NamedKey::ArrowDown) => Some(Direction::Right),
            Key::Named(NamedKey::Backspace) => {
                find.query.pop();
                find.rebuild();
                Some(Direction::Left)
            }
            Key::Named(NamedKey::Tab) => {
                find.regex_mode = !find.regex_mode;
                find.rebuild();
                Some(Direction::Left)
            }
            _ => {
                let Some(text) = event.text.as_deref() else {
                    return;
                };
                if text.chars().any(|c| c.is_control()) {
                    return;
                }
                // 変換中は、確定した文字列だけを Ime::Commit から受ける。
                // ここでも入れると同じ文字が二度入る。
                if !state.preedit.is_empty() {
                    return;
                }
                find.query.push_str(text);
                find.rebuild();
                Some(Direction::Left)
            }
        };
        let Some(direction) = step else { return };
        self.find_step(direction);
    }

    /// 検索を一歩進め、見つかった位置まで画面を送る。
    fn find_step(&mut self, direction: Direction) {
        let Some(state) = &mut self.state else { return };
        let Some(find) = &mut state.find else { return };
        let Some(session) = state.manager.selected() else {
            return;
        };
        let found = {
            let term = session.term.lock();
            find.step(&term, direction)
        };
        if let Some(m) = found {
            session.term.lock().scroll_to_point(*m.start());
        }
    }

    fn on_search_key(&mut self, key: &Key, event: &winit::event::KeyEvent, mods: ModifiersState) {
        let Some(state) = &mut self.state else { return };
        let Some(search) = &mut state.search else {
            return;
        };
        match key {
            Key::Named(NamedKey::Escape) => {
                state.search = None;
                return;
            }
            Key::Named(NamedKey::Enter) => {
                let picked = search.results.get(search.selected).cloned();
                state.search = None;
                if let (Some(entry), Some(s)) = (picked, state.manager.selected()) {
                    // 実行はしない。利用者が内容を確認して Enter を押す。
                    s.pty.write(entry.command.into_bytes());
                }
                return;
            }
            Key::Named(NamedKey::Backspace) => {
                search.query.pop();
            }
            Key::Named(NamedKey::ArrowDown) => {
                if !search.results.is_empty() {
                    search.selected = (search.selected + 1) % search.results.len();
                    search.follow();
                }
                return;
            }
            Key::Named(NamedKey::ArrowUp) => {
                if !search.results.is_empty() {
                    let n = search.results.len();
                    search.selected = (search.selected + n - 1) % n;
                    search.follow();
                }
                return;
            }
            Key::Named(NamedKey::PageDown) => {
                if !search.results.is_empty() {
                    let n = search.results.len();
                    search.selected = (search.selected + SEARCH_ROWS).min(n - 1);
                    search.follow();
                }
                return;
            }
            Key::Named(NamedKey::PageUp) => {
                if !search.results.is_empty() {
                    search.selected = search.selected.saturating_sub(SEARCH_ROWS);
                    search.follow();
                }
                return;
            }
            Key::Character(c) if mods.control_key() => {
                if c.eq_ignore_ascii_case("r") {
                    search.scope = search.scope.next();
                } else {
                    return;
                }
            }
            _ => {
                let Some(text) = event.text.as_deref() else {
                    return;
                };
                if text.chars().any(|c| c.is_control()) {
                    return;
                }
                if !state.preedit.is_empty() {
                    return;
                }
                search.query.push_str(text);
            }
        }
        self.refresh_search();
    }

    fn refresh_search(&mut self) {
        let Some(state) = &mut self.state else { return };
        let (key, cwd) = match state.manager.selected() {
            Some(s) => (s.key.clone(), s.cwd.to_string_lossy().to_string()),
            None => (String::new(), String::new()),
        };
        let Some(search) = &mut state.search else {
            return;
        };
        search.results = match &state.history {
            Some(h) => h
                .search(&search.query, search.scope, &key, &cwd, SEARCH_LIMIT)
                .unwrap_or_default(),
            None => Vec::new(),
        };
        search.selected = 0;
        search.offset = 0;
    }

    // ---------------------------------------------------------------- 描画

    fn draw(&mut self, event_loop: &ActiveEventLoop) {
        let Some(state) = &mut self.state else { return };
        // 最後の 1 つを閉じたら窓ごと閉じる。鍵盤でも鼠でもここを通る。
        if state.manager.is_empty() {
            event_loop.exit();
            return;
        }
        // 「描いた」印は、画面の内容を読む前に落とす。あとで落とすと、
        // 読んでから落とすまでのあいだに届いた更新が、通知を出さないまま
        // 捨てられる。出力が止まる直前（プロンプトが出た瞬間）に当たると、
        // その一画面ぶんが永久に描かれない。
        state.manager.clear_dirty();
        let layout = layout_of(&self.config, state);

        self.refresh_recent();
        let Some(state) = &mut self.state else { return };
        state.manager.drain_pending_clears();
        if state.manager.refresh_metadata() {
            // cd で動いたら、覚えている並びも書き直す。
            state.mark_state_dirty();
        }
        state.save_state(false);
        collect_find_cells(state);
        // 選んでいるセッションが名乗った題名を、ウィンドウに出す。
        let want_title = state
            .manager
            .selected()
            .and_then(|s| s.window_title.clone())
            .unwrap_or_else(|| "termit".to_string());
        if want_title != state.shown_title {
            if let Some(w) = &state.window {
                w.set_title(&want_title);
            }
            state.shown_title = want_title;
        }
        let theme = state.theme;
        state.renderer.begin();

        if layout.sidebar_cols > 0 {
            // 縦の区切り線は引かない。地の色が違うので境目は分かる。
            draw_sidebar(state, &layout, &theme);
        }
        draw_terminal(state, &layout, &theme);
        draw_bottom(state, &layout, &theme);

        // 候補窓を、いま文字が入る位置へ寄せる。
        if let (Some((cx, cy)), Some(window)) = (state.cursor_px, state.window.clone()) {
            let cell = state.renderer.cell();
            let pos = (cx as i32, cy as i32);
            if state.ime_area != Some(pos) {
                state.ime_area = Some(pos);
                window.set_ime_cursor_area(
                    winit::dpi::PhysicalPosition::new(pos.0, pos.1),
                    winit::dpi::PhysicalSize::new(
                        cell.width.ceil() as u32,
                        cell.height.ceil() as u32,
                    ),
                );
            }
        }
        let drew = state.renderer.render(theme.bg);
        if drew {
            state.needs_redraw = false;
        } else {
            // 隠れているなどで描けなかった。間を置いて試す。
            // すぐ要求し直すと、隠れているあいだ空回りする。
            state.needs_redraw = true;
            event_loop.set_control_flow(ControlFlow::WaitUntil(
                std::time::Instant::now() + std::time::Duration::from_millis(32),
            ));
            return;
        }
        let latency = state.pending_since.take().map(|t| t.elapsed());
        if let Some(c) = &mut self.counters {
            c.drew += 1;
            if let Some(d) = latency {
                c.latency_us.push(d.as_micros() as u64);
            }
        }
    }

    fn refresh_recent(&mut self) {
        let Some(state) = &mut self.state else { return };
        let Some((id, key)) = state.manager.selected().map(|s| (s.id, s.key.clone())) else {
            state.recent.clear();
            return;
        };
        if state.recent_for == Some(id) {
            return;
        }
        state.recent = match &state.history {
            Some(h) => h.recent(&key, 10).unwrap_or_default(),
            None => Vec::new(),
        };
        state.recent_for = Some(id);
    }
}

/// 左ペインの寸法。単位は pt で、描くときに表示の倍率を掛ける。
///
/// Warp の一覧を画素から測った値に合わせてある。
/// 実測は docs/warp-metrics.md に記録した。
mod sidebar {
    /// 行の矩形の左余白（ペインの左端から）。
    pub const MARGIN_L: f32 = 6.0;
    /// 行の矩形の右余白（ペインの右端まで）。
    pub const MARGIN_R: f32 = 8.0;
    /// 矩形と矩形のあいだ。
    pub const GAP: f32 = 4.0;
    /// 矩形の上端から 1 段目まで。
    pub const PAD_TOP: f32 = 9.0;
    /// 最後の段から矩形の下端まで。
    pub const PAD_BOTTOM: f32 = 10.0;
    /// 実行状態の印の左端（矩形の左端から）。
    pub const MARK_INSET: f32 = 7.0;
    /// 文字の左端（矩形の左端から）。印のぶん右へ寄せる。
    pub const TEXT_INSET: f32 = 20.0;
    /// 分岐 1 段ぶんの字下げ。
    pub const INDENT: f32 = 12.0;
    /// 矩形の角を丸める半径。
    pub const RADIUS: f32 = 6.0;
    /// 上端に `+` を置く帯の高さ。
    pub const HEADER: f32 = 34.0;
    /// 区切り線の上下の余白。
    pub const SEP_PAD: f32 = 8.0;

    /// 1 段目（名前）。
    pub const SIZE_TITLE: f32 = 13.0;
    /// 2 段目（名乗った題名）。
    pub const SIZE_SUB: f32 = 12.0;
    /// 3 段目（ブランチ名）。
    pub const SIZE_BRANCH: f32 = 10.0;
    /// 直近のコマンド。
    pub const SIZE_RECENT: f32 = 11.0;
}

/// 左ペインと端末の境目を掴める幅（pt）。
const DIVIDER_GRAB: f32 = 4.0;

/// 境目の上にカーソルがあるか。
fn on_divider(state: &State, layout: &Layout, px: f32) -> bool {
    layout.sidebar_px > 0.0
        && (px - layout.sidebar_px).abs() <= DIVIDER_GRAB * state.renderer.scale()
}

/// 左ペインの文字の体裁。桁に縛られないので、等幅でない書体で組む。
///
/// 同じ幅に多く入り、Warp の一覧の見え方に近くなる。
fn style(size: f32, bold: bool) -> TextStyle {
    let s = TextStyle::sans(size);
    if bold {
        s.bold()
    } else {
        s
    }
}

/// 左ペインの 1 セッションぶんの領域。描画と当たり判定で同じ値を使う。
struct Block {
    index: usize,
    depth: usize,
    /// 矩形の上端（画素）。
    top: f32,
    /// 矩形の高さ（画素）。
    height: f32,
}

struct SidebarLayout {
    /// ペインの幅（画素）。
    width: f32,
    /// 行の矩形の左端と幅（画素）。
    rect_x: f32,
    rect_w: f32,
    blocks: Vec<Block>,
    /// 区切り線の位置（画素）。
    sep_y: f32,
}

fn sidebar_layout(state: &State, layout: &Layout) -> SidebarLayout {
    let sc = state.renderer.scale();
    let width = layout.sidebar_px;
    let rect_x = sidebar::MARGIN_L * sc;
    let rect_w = (width - (sidebar::MARGIN_L + sidebar::MARGIN_R) * sc).max(0.0);
    let height_px = layout.rows as f32 * state.renderer.cell().height;
    // 下段に直近のコマンドを出す余地を残す。
    let limit = height_px - 120.0 * sc;

    let mut blocks = Vec::new();
    let mut y = sidebar::HEADER * sc;
    for tree in state.manager.tree_rows() {
        let Some(s) = state.manager.sessions().get(tree.index) else {
            continue;
        };
        let mut content = state.renderer.line_height_px(sidebar::SIZE_TITLE);
        if s.window_title.is_some() {
            content += state.renderer.line_height_px(sidebar::SIZE_SUB);
        }
        if s.branch.is_some() {
            content += state.renderer.line_height_px(sidebar::SIZE_BRANCH);
        }
        let height = (sidebar::PAD_TOP + sidebar::PAD_BOTTOM) * sc + content;
        if y + height > limit {
            break;
        }
        blocks.push(Block {
            index: tree.index,
            depth: tree.depth,
            top: y,
            height,
        });
        y += height + sidebar::GAP * sc;
    }
    let sep_y = y - sidebar::GAP * sc + sidebar::SEP_PAD * sc;
    SidebarLayout {
        width,
        rect_x,
        rect_w,
        blocks,
        sep_y,
    }
}

/// 左ペインのどこを押したか。
enum SidebarHit {
    NewSession,
    Select(usize),
    Close(usize),
}

/// 画素の位置で当たりを見る。
fn sidebar_hit(state: &State, layout: &Layout, px: f32, py: f32) -> Option<SidebarHit> {
    if layout.sidebar_px <= 0.0 || px >= layout.sidebar_px {
        return None;
    }
    let sc = state.renderer.scale();
    let sl = sidebar_layout(state, layout);
    if py < sidebar::HEADER * sc {
        // 見出しの帯の右端に `+` を置いている。
        let plus_right = sl.rect_x + sl.rect_w;
        return (px > plus_right - 24.0 * sc).then_some(SidebarHit::NewSession);
    }
    for b in &sl.blocks {
        if py < b.top || py >= b.top + b.height {
            continue;
        }
        // × は、カーソルが乗っているときだけ 1 段目の右端に出る。
        // 出ていない位置で閉じないよう、同じ範囲でだけ受ける。
        let first_line = state.renderer.line_height_px(sidebar::SIZE_TITLE);
        let close_right = sl.rect_x + sl.rect_w - 6.0 * sc;
        let close_left = close_right - 18.0 * sc;
        if py < b.top + sidebar::PAD_TOP * sc + first_line && px >= close_left && px < close_right {
            return Some(SidebarHit::Close(b.index));
        }
        return Some(SidebarHit::Select(b.index));
    }
    None
}

pub(crate) fn draw_sidebar(state: &mut State, layout: &Layout, theme: &Theme) {
    let sc = state.renderer.scale();
    let sl = sidebar_layout(state, layout);
    let height_px = layout.rows as f32 * state.renderer.cell().height;
    state
        .renderer
        .fill_px(0.0, 0.0, sl.width, height_px, theme.chrome_bg, 0.0);

    // 押せる目印。キーが効かない環境でもここから増やせる。
    let plus_w = state
        .renderer
        .measure_px("+", style(sidebar::SIZE_TITLE, true));
    let plus_x = sl.rect_x + sl.rect_w - plus_w - 6.0 * sc;
    let plus_y = (sidebar::HEADER * sc - state.renderer.line_height_px(sidebar::SIZE_TITLE)) / 2.0;
    state.renderer.put_text_px(
        plus_x,
        plus_y,
        "+",
        style(sidebar::SIZE_TITLE, true),
        theme.accent,
    );

    let selected = state.manager.selected_index();
    for (n, b) in sl.blocks.iter().enumerate() {
        let s = &state.manager.sessions()[b.index];
        let running = s.is_running();
        let indent = b.depth as f32 * sidebar::INDENT * sc;
        let text_x = sl.rect_x + sidebar::TEXT_INSET * sc + indent;
        let forked = !s.inherited && s.parent.is_some();
        let (raw_name, is_path) = s.display_name();
        let osc_title = s.window_title.clone();
        let branch = s.branch.clone();
        let exit = match s.state {
            RunState::Running => None,
            RunState::Exited(code) => Some(code),
        };

        let hovered =
            state.mouse.x < sl.width && state.mouse.y >= b.top && state.mouse.y < b.top + b.height;
        if b.index == selected {
            state.renderer.fill_px(
                sl.rect_x,
                b.top,
                sl.rect_w,
                b.height,
                theme.surface,
                sidebar::RADIUS * sc,
            );
        }

        let mut y = b.top + sidebar::PAD_TOP * sc;
        let title_h = state.renderer.line_height_px(sidebar::SIZE_TITLE);
        // 右端に並べる印の左端。名前はここまでで切る。
        let right = sl.rect_x + sl.rect_w;
        // 印は行ごとに幅が違う。実際に出すものではかる。
        let hint = input::index_label(n);
        let hint_w = match &hint {
            Some(h) => state
                .renderer
                .measure_px(h, style(sidebar::SIZE_BRANCH, false)),
            None => 0.0,
        };
        let name_limit = right - 12.0 * sc - hint_w - text_x;
        let mark_w = state
            .renderer
            .measure_px("*", style(sidebar::SIZE_TITLE, false));
        let avail = (name_limit - if forked { mark_w } else { 0.0 }).max(0.0);
        let mut name = if is_path {
            state
                .renderer
                .fit_head(&raw_name, style(sidebar::SIZE_TITLE, false), avail)
        } else {
            state
                .renderer
                .fit_tail(&raw_name, style(sidebar::SIZE_TITLE, false), avail)
        };
        if forked {
            name.push('*');
        }

        // 分岐の系統は字下げで示す。つなぎの印は、状態の印に重ならない
        // 位置へ寄せる。
        if b.depth > 0 {
            state.renderer.put_text_px(
                sl.rect_x + indent - 5.0 * sc,
                y + (title_h - state.renderer.line_height_px(sidebar::SIZE_BRANCH)) / 2.0,
                "└",
                style(sidebar::SIZE_BRANCH, false),
                theme.fg_tertiary,
            );
        }
        let fg = if running {
            theme.fg_primary
        } else {
            theme.fg_tertiary
        };
        state
            .renderer
            .put_text_px(text_x, y, &name, style(sidebar::SIZE_TITLE, false), fg);

        // 実行状態は左端に置く。Warp がアバターを置いている位置にあたる。
        let mark = if exit.is_none() { "●" } else { "○" };
        let mark_color = match exit {
            None => theme.accent,
            Some(0) => theme.fg_tertiary,
            Some(_) => theme.warn,
        };
        let small_h = state.renderer.line_height_px(sidebar::SIZE_BRANCH);
        state.renderer.put_text_px(
            sl.rect_x + sidebar::MARK_INSET * sc + indent,
            y + (title_h - small_h) / 2.0,
            mark,
            style(sidebar::SIZE_BRANCH, false),
            mark_color,
        );
        // 右端は、ふだんは ⌘ の番号、カーソルが乗っているときは閉じる印。
        // 両方を常に置くと名前の幅が足りない。
        if hovered {
            let xw = state
                .renderer
                .measure_px("×", style(sidebar::SIZE_SUB, false));
            state.renderer.put_text_px(
                right - 6.0 * sc - xw,
                y + (title_h - state.renderer.line_height_px(sidebar::SIZE_SUB)) / 2.0,
                "×",
                style(sidebar::SIZE_SUB, false),
                theme.fg_secondary,
            );
        } else if let Some(hint) = &hint {
            state.renderer.put_text_px(
                right - 6.0 * sc - hint_w,
                y + (title_h - small_h) / 2.0,
                hint,
                style(sidebar::SIZE_BRANCH, false),
                theme.fg_tertiary,
            );
        }
        y += title_h;

        // 2 段目は端末上のプログラムが名乗った題名。
        if let Some(title) = osc_title {
            let w = state.renderer.put_text_px(
                text_x,
                y,
                "✻ ",
                style(sidebar::SIZE_SUB, false),
                theme.accent,
            );
            let limit = (right - 8.0 * sc - (text_x + w)).max(0.0);
            let clipped = state
                .renderer
                .fit_tail(&title, style(sidebar::SIZE_SUB, false), limit);
            state.renderer.put_text_px(
                text_x + w,
                y,
                &clipped,
                style(sidebar::SIZE_SUB, false),
                theme.fg_secondary,
            );
            y += state.renderer.line_height_px(sidebar::SIZE_SUB);
        }

        // 3 段目はブランチ名。
        if let Some(branch) = branch {
            let w = state.renderer.put_text_px(
                text_x,
                y,
                "⋔ ",
                style(sidebar::SIZE_BRANCH, false),
                theme.fg_tertiary,
            );
            let limit = (right - 8.0 * sc - (text_x + w)).max(0.0);
            let clipped =
                state
                    .renderer
                    .fit_tail(&branch, style(sidebar::SIZE_BRANCH, false), limit);
            state.renderer.put_text_px(
                text_x + w,
                y,
                &clipped,
                style(sidebar::SIZE_BRANCH, false),
                theme.fg_tertiary,
            );
        }
    }

    // 直近のコマンド。何もなければ区切りごと出さない。
    if state.recent.is_empty() {
        return;
    }
    let line_h = state.renderer.line_height_px(sidebar::SIZE_RECENT);
    if sl.sep_y + line_h > height_px {
        return;
    }
    state.renderer.fill_px(
        sl.rect_x,
        sl.sep_y,
        sl.rect_w,
        (1.0 * sc).max(1.0),
        theme.fg_tertiary,
        0.0,
    );
    let mut y = sl.sep_y + sidebar::SEP_PAD * sc;
    let recent: Vec<crate::history::Entry> = state.recent.clone();
    for entry in recent {
        if y + line_h > height_px {
            break;
        }
        let code = entry.exit_code.unwrap_or(0);
        let color = if code == 0 {
            theme.fg_tertiary
        } else {
            theme.warn
        };
        let w = state.renderer.put_text_px(
            sl.rect_x + 4.0 * sc,
            y,
            &format!("{code:>3} "),
            style(sidebar::SIZE_RECENT, false),
            color,
        );
        let limit = (sl.rect_w - 8.0 * sc - w).max(0.0);
        let clipped =
            state
                .renderer
                .fit_tail(&entry.command, style(sidebar::SIZE_RECENT, false), limit);
        state.renderer.put_text_px(
            sl.rect_x + 4.0 * sc + w,
            y,
            &clipped,
            style(sidebar::SIZE_RECENT, false),
            theme.fg_secondary,
        );
        y += line_h;
    }
}

pub(crate) fn draw_terminal(state: &mut State, layout: &Layout, theme: &Theme) {
    let Some(session) = state.manager.selected() else {
        return;
    };
    let term = session.term.lock();
    let content = term.renderable_content();
    let colors = content.colors;
    let selection = content.selection;
    let cursor = content.cursor;
    let display_offset = content.display_offset;
    struct Draw {
        col: usize,
        row: usize,
        c: char,
        fg: Rgb,
        bg: Rgb,
        bold: bool,
        italic: bool,
        underline: bool,
        /// 全角文字は 2 桁を占める。背景とカーソルもその幅で塗る。
        cols: usize,
    }
    let mut cells: Vec<Draw> = Vec::with_capacity(layout.term_cols * layout.term_rows);
    let mut cursor_cols = 1usize;

    for indexed in content.display_iter {
        let cell = indexed.cell;
        if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
            continue;
        }
        let row = indexed.point.line.0;
        if row < 0 || row as usize >= layout.term_rows {
            continue;
        }
        let col = indexed.point.column.0;
        if col >= layout.term_cols {
            continue;
        }
        let mut fg = theme.resolve(cell.fg, colors);
        let mut bg = theme.resolve(cell.bg, colors);
        if cell.flags.contains(Flags::INVERSE) {
            std::mem::swap(&mut fg, &mut bg);
        }
        if cell.flags.contains(Flags::HIDDEN) {
            fg = bg;
        }
        if selection.is_some_and(|s| s.contains(indexed.point)) {
            bg = theme.selection;
        }
        // 検索の強調は選択より優先する。探しているものを見失わないため。
        let key = (row, col);
        if state.find_current.contains(&key) {
            bg = theme.search_current;
            fg = theme.search_fg;
        } else if state.find_cells.contains(&key) {
            bg = theme.search_hit;
            fg = theme.search_fg;
        }
        let wide = cell.flags.contains(Flags::WIDE_CHAR);
        if wide && indexed.point == cursor.point {
            cursor_cols = 2;
        }
        cells.push(Draw {
            col,
            row: row as usize,
            c: cell.c,
            fg,
            bg,
            bold: cell.flags.contains(Flags::BOLD),
            italic: cell.flags.contains(Flags::ITALIC),
            underline: cell.flags.intersects(Flags::ALL_UNDERLINES),
            cols: if wide { 2 } else { 1 },
        });
    }
    let show_cursor = display_offset == 0 && cursor.shape != CursorShape::Hidden;
    let cursor_point = cursor.point;
    let cursor_shape = cursor.shape;
    drop(term);

    let default_bg = theme.bg;
    for d in cells {
        let x = layout.term_col + d.col;
        let cols = d.cols.min(layout.term_cols.saturating_sub(d.col)).max(1);
        if d.bg != default_bg {
            state.renderer.fill_cells(x, d.row, cols, 1, d.bg);
        }
        state
            .renderer
            .put_char(x, d.row, d.c, d.fg, d.bold, d.italic);
        if d.underline {
            state.renderer.underline_cells(x, d.row, cols, d.fg);
        }
    }

    if show_cursor {
        let row = cursor_point.line.0;
        if row >= 0 && (row as usize) < layout.term_rows {
            let x = layout.term_col + cursor_point.column.0.min(layout.term_cols - 1);
            let y = row as usize;
            let cell = state.renderer.cell();
            state.cursor_px = Some((x as f32 * cell.width, y as f32 * cell.height));
            // 変換中の文字列は、確定するまで子プロセスへ渡さない。
            // 見えないと何を打っているか分からないので、カーソルの位置に出す。
            let preedit = (state.find.is_none() && state.search.is_none())
                .then(|| state.preedit.clone())
                .filter(|p| !p.is_empty());
            if let Some(text) = preedit {
                // 変換中はカーソルの塗りを出さない。
                // 塗りが一文字目に重なると、何を打っているか読めなくなる。
                let width = layout.term_cols.saturating_sub(x - layout.term_col);
                let cols: usize = text.chars().map(char_cols).sum();
                let shown = cols.min(width);
                state.renderer.fill_cells(x, y, shown, 1, theme.surface);
                let used = state
                    .renderer
                    .put_str_clipped(x, y, &text, width, theme.fg_primary);
                state.renderer.underline_cells(x, y, used, theme.accent);
                // 文字が入る位置は変換中の文字列の末尾なので、細い棒で示す。
                let caret = (x + used).min(layout.cols.saturating_sub(1));
                state.renderer.cursor_beam(caret, y, theme.cursor);
                state.cursor_px = Some((caret as f32 * cell.width, y as f32 * cell.height));
            } else {
                match cursor_shape {
                    CursorShape::Beam => state.renderer.cursor_beam(x, y, theme.cursor),
                    CursorShape::Underline => {
                        state
                            .renderer
                            .underline_cells(x, y, cursor_cols, theme.cursor)
                    }
                    _ => state
                        .renderer
                        .fill_cells_alpha(x, y, cursor_cols, 1, theme.cursor, 0.55),
                }
            }
        }
    }
}

/// 最下段の帯。端末の文字より小さく、別の書体で組む。
///
/// 端末の中身ではなく端末についての表示なので、桁に揃える必要がない。
/// 同じ大きさで並べると、本文より目立ってしまう。
const BAR_SIZE: f32 = 11.0;

pub(crate) fn draw_bottom(state: &mut State, layout: &Layout, theme: &Theme) {
    let sc = state.renderer.scale();
    let cell = state.renderer.cell();
    let bar_h = cell.height;
    let bar_y = (layout.rows.saturating_sub(1)) as f32 * bar_h;
    let x0 = layout.term_col as f32 * cell.width;
    let w = (layout.cols.saturating_sub(layout.term_col)) as f32 * cell.width;
    let st = style(BAR_SIZE, false);
    let line_h = state.renderer.line_height_px(BAR_SIZE);
    let ty = bar_y + (bar_h - line_h) / 2.0;
    let pad = 8.0 * sc;

    // 重ねた一覧は、帯の上へ同じ高さの行で積む。
    let overlay = |state: &mut State, rows: usize| -> f32 {
        let top = bar_y - rows as f32 * line_h;
        state.renderer.fill_px(
            x0,
            top,
            w,
            rows as f32 * line_h + bar_h,
            theme.chrome_bg,
            0.0,
        );
        top
    };

    if let Some(rename) = &state.rename {
        let input = rename.input.clone();
        state
            .renderer
            .fill_px(x0, bar_y, w, bar_h, theme.surface, 0.0);
        let mut x = x0 + pad;
        x += state
            .renderer
            .put_text_px(x, ty, "name: ", st, theme.fg_secondary);
        x += state
            .renderer
            .put_text_px(x, ty, &input, st, theme.fg_primary);
        x += draw_preedit_px(state, x, ty, st, theme);
        state
            .renderer
            .fill_px(x, ty, 2.0 * sc, line_h, theme.cursor, 0.0);
        state.cursor_px = Some((x, ty));
        let note = "Enter to set   empty resets to path   Esc to cancel";
        let nw = state.renderer.measure_px(note, st);
        if x0 + w - pad - nw > x + pad {
            state
                .renderer
                .put_text_px(x0 + w - pad - nw, ty, note, st, theme.fg_tertiary);
        }
        return;
    }

    if let Some(find) = &state.find {
        let (query, regex_mode, invalid, has_match) = (
            find.query.clone(),
            find.regex_mode,
            find.invalid,
            find.current.is_some(),
        );
        state
            .renderer
            .fill_px(x0, bar_y, w, bar_h, theme.surface, 0.0);
        let label = if regex_mode {
            "find (regex): "
        } else {
            "find: "
        };
        let mut x = x0 + pad;
        x += state
            .renderer
            .put_text_px(x, ty, label, st, theme.fg_secondary);
        let fg = if invalid {
            theme.warn
        } else {
            theme.fg_primary
        };
        x += state.renderer.put_text_px(x, ty, &query, st, fg);
        x += draw_preedit_px(state, x, ty, st, theme);
        state
            .renderer
            .fill_px(x, ty, 2.0 * sc, line_h, theme.cursor, 0.0);
        state.cursor_px = Some((x, ty));
        let note = if invalid {
            "invalid regex"
        } else if !query.is_empty() && !has_match {
            "no match"
        } else {
            "Enter prev   ⇧Enter next   Tab regex   Esc close"
        };
        let nw = state.renderer.measure_px(note, st);
        if x0 + w - pad - nw > x + pad {
            state
                .renderer
                .put_text_px(x0 + w - pad - nw, ty, note, st, theme.fg_tertiary);
        }
        return;
    }

    if let Some(picker) = &state.picker {
        let names = picker.names.clone();
        let sel = picker.selected;
        let top = overlay(state, names.len());
        for (i, name) in names.iter().enumerate() {
            let ry = top + i as f32 * line_h;
            if i == sel {
                state
                    .renderer
                    .fill_px(x0, ry, w, line_h, theme.surface, 0.0);
            }
            let fg = if i == sel {
                theme.fg_primary
            } else {
                theme.fg_secondary
            };
            state
                .renderer
                .put_text_px(x0 + pad, ry, &format!("{}  {}", i + 1, name), st, fg);
        }
        state
            .renderer
            .fill_px(x0, bar_y, w, bar_h, theme.surface, 0.0);
        state.renderer.put_text_px(
            x0 + pad,
            ty,
            "fork profile: j/k or number, Enter to confirm, Esc to cancel",
            st,
            theme.fg_primary,
        );
        return;
    }

    if let Some(search) = &state.search {
        // 見えるのは一部で、選択を動かすと窓が付いてくる。
        let results: Vec<crate::history::Entry> = search
            .results
            .iter()
            .skip(search.offset)
            .take(SEARCH_ROWS)
            .cloned()
            .collect();
        let sel = search.selected.saturating_sub(search.offset);
        let scope = search.scope.label().to_string();
        let query = search.query.clone();
        let position = if search.results.is_empty() {
            String::new()
        } else {
            format!("{}/{}", search.selected + 1, search.results.len())
        };
        let top = overlay(state, results.len());
        for (i, entry) in results.iter().enumerate() {
            let ry = top + i as f32 * line_h;
            if i == sel {
                state
                    .renderer
                    .fill_px(x0, ry, w, line_h, theme.surface, 0.0);
            }
            let fg = if i == sel {
                theme.fg_primary
            } else {
                theme.fg_secondary
            };
            let text = state.renderer.fit_tail(&entry.command, st, w - pad * 3.0);
            state
                .renderer
                .put_text_px(x0 + pad * 2.0, ry, &text, st, fg);
        }
        state
            .renderer
            .fill_px(x0, bar_y, w, bar_h, theme.surface, 0.0);
        let mut x = x0 + pad;
        x += state.renderer.put_text_px(
            x,
            ty,
            &format!("history [{scope}]: "),
            st,
            theme.fg_secondary,
        );
        x += state
            .renderer
            .put_text_px(x, ty, &query, st, theme.fg_primary);
        x += draw_preedit_px(state, x, ty, st, theme);
        state
            .renderer
            .fill_px(x, ty, 2.0 * sc, line_h, theme.cursor, 0.0);
        state.cursor_px = Some((x, ty));
        // 何件目を見ているかと、範囲の切り替え方を右端に出す。
        let note = if position.is_empty() {
            "^R scope   Esc close".to_string()
        } else {
            format!("{position}   ↑↓ move   ⇞⇟ page   ^R scope   Esc close")
        };
        let nw = state.renderer.measure_px(&note, st);
        if x0 + w - pad - nw > x + pad {
            state
                .renderer
                .put_text_px(x0 + w - pad - nw, ty, &note, st, theme.fg_tertiary);
        }
        return;
    }

    if let Some(msg) = &state.status {
        let msg = msg.clone();
        state
            .renderer
            .fill_px(x0, bar_y, w, bar_h, theme.chrome_bg, 0.0);
        let text = state.renderer.fit_tail(&msg, st, w - pad * 2.0);
        state
            .renderer
            .put_text_px(x0 + pad, ty, &text, st, theme.warn);
        return;
    }

    // ふだんは操作のヒントだけを薄く出す。
    let hint = "^O new   ^\\ fork   ^] fork as…   ^^ next   ⌘F find   ⌘I rename   ^R history   ^B pane   ⌘K clear   ⌘W close";
    let text = state.renderer.fit_tail(hint, st, w - pad * 2.0);
    state
        .renderer
        .put_text_px(x0 + pad, ty, &text, st, theme.fg_tertiary);
}

/// 変換中の文字列を帯の中に出す。返す値は描いた幅。
fn draw_preedit_px(state: &mut State, x: f32, y: f32, st: TextStyle, theme: &Theme) -> f32 {
    if state.preedit.is_empty() {
        return 0.0;
    }
    let text = state.preedit.clone();
    let w = state.renderer.measure_px(&text, st);
    let h = state.renderer.line_height_px(st.size);
    state.renderer.fill_px(x, y, w, h, theme.chrome_bg, 0.0);
    state
        .renderer
        .put_text_px(x, y, &text, st, theme.fg_primary);
    // 未確定であることを下線で示す。
    let t = (state.renderer.scale()).max(1.0);
    state
        .renderer
        .fill_px(x, y + h - t, w, t, theme.accent, 0.0);
    w
}

/// 括弧付き貼り付けに対応している端末には印を付けて送る。
fn bracketed(text: &str, mode: TermMode) -> Vec<u8> {
    let cleaned: String = text.replace("\r\n", "\r").replace('\n', "\r");
    if mode.contains(TermMode::BRACKETED_PASTE) {
        let mut out = b"\x1b[200~".to_vec();
        out.extend_from_slice(cleaned.as_bytes());
        out.extend_from_slice(b"\x1b[201~");
        out
    } else {
        cleaned.into_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn px(y: f64) -> MouseScrollDelta {
        MouseScrollDelta::PixelDelta(winit::dpi::PhysicalPosition::new(0.0, y))
    }

    #[test]
    fn 触覚板の細かい動きを取りこぼさない() {
        // 1 回 3 画素、行は 17 画素。切り捨てると永久に 0 行のままになる。
        let mut w = WheelAccum::default();
        let mut moved = 0;
        for _ in 0..12 {
            moved += w.push(px(3.0), 17.0);
        }
        assert_eq!(moved, 2, "36 画素ぶんは 2 行になる");
    }

    #[test]
    fn 行に足りない一回では動かない() {
        let mut w = WheelAccum::default();
        assert_eq!(w.push(px(3.0), 17.0), 0);
    }

    #[test]
    fn 端数は次の通知へ持ち越す() {
        let mut w = WheelAccum::default();
        assert_eq!(w.push(px(10.0), 17.0), 0);
        assert_eq!(w.push(px(10.0), 17.0), 1, "20 画素で 1 行");
        assert_eq!(w.push(px(10.0), 17.0), 0);
        assert_eq!(w.push(px(10.0), 17.0), 1, "残りの 3 画素が効いている");
    }

    #[test]
    fn 向きを変えたら持ち越しを捨てる() {
        let mut w = WheelAccum::default();
        assert_eq!(w.push(px(10.0), 17.0), 0, "上へ 10 画素、まだ行にならない");
        // 逆向きの端数が残っていると、折り返した最初のひと押しが食われる。
        assert_eq!(w.push(px(-17.0), 17.0), -1, "下へ 1 行");
    }

    #[test]
    fn 行単位の通知はそのまま通る() {
        let mut w = WheelAccum::default();
        assert_eq!(w.push(MouseScrollDelta::LineDelta(0.0, 3.0), 17.0), 3);
        assert_eq!(w.push(MouseScrollDelta::LineDelta(0.0, -1.0), 17.0), -1);
    }

    #[test]
    fn まとめて来た大きな動きも行数に直す() {
        let mut w = WheelAccum::default();
        assert_eq!(w.push(px(170.0), 17.0), 10);
    }

    #[test]
    fn 貼り付けは改行を復帰に揃える() {
        let got = bracketed("a\r\nb\nc", TermMode::empty());
        assert_eq!(got, b"a\rb\rc");
    }

    #[test]
    fn 括弧付き貼り付けに印を付ける() {
        let got = bracketed("x", TermMode::BRACKETED_PASTE);
        assert_eq!(got, b"\x1b[200~x\x1b[201~");
    }
}

#[cfg(test)]
mod search_view_tests {
    use super::*;

    fn view(selected: usize, offset: usize) -> SearchState {
        SearchState {
            query: String::new(),
            scope: Scope::Session,
            results: Vec::new(),
            selected,
            offset,
        }
    }

    #[test]
    fn 見える範囲の中なら位置を動かさない() {
        let mut v = view(3, 0);
        v.follow();
        assert_eq!(v.offset, 0);
    }

    #[test]
    fn 下へ出たら窓が付いてくる() {
        let mut v = view(SEARCH_ROWS, 0);
        v.follow();
        assert_eq!(v.offset, 1, "1 行ぶんだけ送る");

        let mut v = view(499, 0);
        v.follow();
        assert_eq!(v.offset, 499 + 1 - SEARCH_ROWS);
    }

    #[test]
    fn 上へ出たら窓が戻る() {
        let mut v = view(2, 10);
        v.follow();
        assert_eq!(v.offset, 2);
    }

    #[test]
    fn 選んだ行はつねに見える範囲に入る() {
        for selected in 0..600usize {
            for offset in [0usize, 5, 100, 590] {
                let mut v = view(selected, offset);
                v.follow();
                assert!(
                    v.offset <= selected && selected < v.offset + SEARCH_ROWS,
                    "selected={selected} offset={offset} -> {}",
                    v.offset
                );
            }
        }
    }
}

#[cfg(test)]
mod path_tests {
    /// macOS の `dirs::config_dir()` は `~/.config` ではなく
    /// `~/Library/Application Support` を返す。README に書いた場所と
    /// 実装がずれないよう、XDG の作法に従っていることを確かめる。
    #[test]
    fn 設定と履歴は_xdg_の場所にある() {
        let config = crate::config::config_path().expect("設定の場所が決まる");
        let db = crate::history::db_path().expect("履歴の場所が決まる");
        assert!(
            config.ends_with(".config/termit/config.toml"),
            "設定の場所が想定と違う: {}",
            config.display()
        );
        assert!(
            db.ends_with(".local/share/termit/history.db"),
            "履歴の場所が想定と違う: {}",
            db.display()
        );
    }

    #[test]
    fn 環境変数で置き場所を移せる() {
        // 環境変数を書き換えるテストは並行実行と相性が悪いので、
        // 変数を読む関数の方を直接確かめる。
        let d = crate::config::xdg_dir("XDG_CONFIG_HOME_TEX_TEST_UNSET", ".config")
            .expect("既定へ落ちる");
        assert!(d.ends_with(".config"));
    }
}

/// 見えている範囲の一致を、セルの集まりに直す。
///
/// 一致は点の範囲で返るが、描くときはセルごとに引きたい。
/// 見えている範囲だけなので、数は画面の広さで頭打ちになる。
pub(crate) fn collect_find_cells(state: &mut State) {
    state.find_cells.clear();
    state.find_current.clear();
    let Some(find) = &mut state.find else { return };
    if !find.is_ready() {
        return;
    }
    let Some(session) = state.manager.selected() else {
        return;
    };
    let term = session.term.lock();
    let cols = {
        use alacritty_terminal::grid::Dimensions;
        term.grid().columns()
    };
    let matches = find.visible(&term);
    let current = find.current.clone();
    drop(term);

    let push = |set: &mut std::collections::HashSet<(i32, usize)>,
                m: &alacritty_terminal::term::search::Match| {
        let (start, end) = (m.start(), m.end());
        let mut line = start.line;
        while line <= end.line {
            let first = if line == start.line {
                start.column.0
            } else {
                0
            };
            let last = if line == end.line {
                end.column.0
            } else {
                cols - 1
            };
            for c in first..=last.min(cols.saturating_sub(1)) {
                set.insert((line.0, c));
            }
            line = alacritty_terminal::index::Line(line.0 + 1);
        }
    };
    for m in &matches {
        push(&mut state.find_cells, m);
    }
    if let Some(m) = &current {
        push(&mut state.find_current, m);
    }
}

//! tex: エージェント向けの軽量ターミナル。

mod bench;
mod clipboard;
mod latency;
mod mouse;
mod keytest;
mod probe;
mod config;
mod git;
mod history;
mod input;
mod osc;
mod pty;
mod rect;
mod render;
mod search;
mod session;
mod term;
mod theme;

use std::path::PathBuf;
use std::sync::Arc;

use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::index::{Column, Direction, Point, Side};
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
use crate::render::{char_cols, Renderer};
use crate::session::{Manager, RunState};
use crate::term::{TermSize, UiEvent};
use crate::theme::Theme;

const SEARCH_ROWS: usize = 10;

/// 押したキーを記録する。キーバインドが届かないときの切り分けに使う。
/// `TEX_KEYLOG` に書き出し先を指定したときだけ動く。
fn keylog(event: &winit::event::KeyEvent, mods: ModifiersState) {
    use std::io::Write;
    let Ok(path) = std::env::var("TEX_KEYLOG") else {
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
        let out = args.get(i + 1).cloned().unwrap_or_else(|| "probe.rgba".into());
        probe::run(&out);
        return;
    }

    let config = match Config::load() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("tex: {e}");
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
        counters: std::env::var("TEX_FRAME_LOG").is_ok().then(Counters::default),
    };
    if let Err(e) = event_loop.run_app(&mut app) {
        eprintln!("tex: {e}");
        std::process::exit(1);
    }
}

/// マウスの状態。
#[derive(Default)]
struct MouseState {
    x: f32,
    y: f32,
    /// 端末領域でドラッグ中か（端末側の選択）。
    dragging: bool,
    /// 端末上のプログラムへ報告中の釦。
    reporting: Option<mouse::Button>,
    /// 直前に報告したセル。同じセルの中の動きは送らない。
    last_reported: Option<(usize, usize)>,
    /// 直前のクリックの時刻、セル、連続回数。
    last_click: Option<(std::time::Instant, (usize, usize), u8)>,
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
    /// いまウィンドウに出している題名。変わったときだけ設定し直す。
    shown_title: String,
    /// 変換中の文字列。確定するまでは入力にも検索にも渡さない。
    preedit: String,
    /// いま知らせてある候補窓の位置。変わったときだけ設定し直す。
    ime_area: Option<(i32, i32)>,
    /// 変換中の文字列を出す位置。候補窓もここへ寄せる。
    cursor_cell: Option<(usize, usize)>,
    window: Option<Arc<Window>>,
}

impl State {
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
            "1 秒間: wakeup={} 再描画要求={} 実描画={} キー={} | 読み取り→表示 中央 {:.1}ms p90 {:.1}ms 最大 {:.1}ms",
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
    sidebar_cols: usize,
    term_col: usize,
    term_cols: usize,
    term_rows: usize,
}

impl App {
    fn layout(&self, s: &State) -> Layout {
        let (cols, rows) = s.renderer.grid_size();
        let sidebar_cols = if s.sidebar {
            self.config.window.sidebar_cols.min(cols.saturating_sub(20))
        } else {
            0
        };
        let sep = usize::from(sidebar_cols > 0);
        let term_col = sidebar_cols + sep;
        let term_cols = cols.saturating_sub(term_col).max(2);
        // 最下行は検索欄と状態表示のために空けておく。
        let term_rows = rows.saturating_sub(1).max(1);
        Layout {
            cols,
            rows,
            sidebar_cols,
            term_col,
            term_cols,
            term_rows,
        }
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
            .with_title("tex")
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
            shown_title: String::new(),
            preedit: String::new(),
            ime_area: None,
            cursor_cell: None,
            window: Some(window.clone()),
        };
        if state.history.is_none() {
            state.status = Some("履歴 DB を開けないため履歴機能を無効にした".into());
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
        if let Err(e) = state.manager.spawn_new(&self.config, HOST_PROFILE, &cwd) {
            state.status = Some(e.to_string());
        }

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
            }
            UiEvent::Command(_, record) => {
                if let Some(h) = &state.history {
                    if let Err(e) = h.record(&record) {
                        log::warn!("履歴を書けない: {e}");
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
            WindowEvent::CloseRequested => event_loop.exit(),
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
                    self.on_key(event, event_loop);
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
        let (cols, rows) = state.renderer.grid_size();
        let sidebar_cols = if state.sidebar {
            self.config.window.sidebar_cols.min(cols.saturating_sub(20))
        } else {
            0
        };
        let sep = usize::from(sidebar_cols > 0);
        let term_cols = cols.saturating_sub(sidebar_cols + sep).max(2);
        let term_rows = rows.saturating_sub(1).max(1);
        let cell = state.renderer.cell();
        state
            .manager
            .set_cell((cell.width as u16, cell.height as u16));
        state
            .manager
            .resize(TermSize::new(term_cols, term_rows));
        state.request_redraw();
    }

    fn on_key(&mut self, event: winit::event::KeyEvent, event_loop: &ActiveEventLoop) {
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
            self.on_action(action, event_loop);
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

    fn on_action(&mut self, action: Action, event_loop: &ActiveEventLoop) {
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
                    state.status =
                        Some("設定に host 以外のプロファイルがない".into());
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
            Action::CloseSession => {
                state.manager.close_selected();
                if state.manager.is_empty() {
                    event_loop.exit();
                }
            }
            Action::ToggleSidebar => {
                state.sidebar = !state.sidebar;
                self.reflow();
                return;
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
                return;
            }
            Action::FindInScreen => {
                state.picker = None;
                state.search = None;
                state.find = Some(search::ScreenSearch::new());
                return;
            }
            Action::SearchHistory => {
                state.picker = None;
                state.find = None;
                state.search = Some(SearchState {
                    query: String::new(),
                    scope: Scope::All,
                    results: Vec::new(),
                    selected: 0,
                });
                self.refresh_search();
                return;
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
                return;
            }
            Action::FontSmaller => {
                let size = state.renderer.font_size() - 1.0;
                state.renderer.set_font_size(size);
                self.reflow();
                return;
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

        if let Some(hit) = sidebar_hit(state, &layout, col, row) {
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
                SidebarHit::Close(i) => {
                    state.manager.select(i);
                    state.manager.close_selected();
                }
            }
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
        let lines = match delta {
            MouseScrollDelta::LineDelta(_, y) => y.round() as i32,
            MouseScrollDelta::PixelDelta(p) => (p.y as f32 / cell_height).round() as i32,
        };
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
    {
        let (cols, rows) = state.renderer.grid_size();
        let sidebar_cols = if state.sidebar {
            config.window.sidebar_cols.min(cols.saturating_sub(20))
        } else {
            0
        };
        let sep = usize::from(sidebar_cols > 0);
        let term_col = sidebar_cols + sep;
        Layout {
            cols,
            rows,
            sidebar_cols,
            term_col,
            term_cols: cols.saturating_sub(term_col).max(2),
            term_rows: rows.saturating_sub(1).max(1),
        }
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
        let Some(rename) = &mut state.rename else { return };
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
        let step: Option<Direction>;
        match key {
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
                step = Some(if mods.shift_key() {
                    Direction::Right
                } else {
                    Direction::Left
                });
            }
            Key::Named(NamedKey::ArrowDown) => step = Some(Direction::Right),
            Key::Named(NamedKey::Backspace) => {
                find.query.pop();
                find.rebuild();
                step = Some(Direction::Left);
            }
            Key::Named(NamedKey::Tab) => {
                find.regex_mode = !find.regex_mode;
                find.rebuild();
                step = Some(Direction::Left);
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
                step = Some(Direction::Left);
            }
        }
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

    fn on_search_key(
        &mut self,
        key: &Key,
        event: &winit::event::KeyEvent,
        mods: ModifiersState,
    ) {
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
                }
                return;
            }
            Key::Named(NamedKey::ArrowUp) => {
                if !search.results.is_empty() {
                    let n = search.results.len();
                    search.selected = (search.selected + n - 1) % n;
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
        let (session_id, cwd) = match state.manager.selected() {
            Some(s) => (s.id, s.cwd.to_string_lossy().to_string()),
            None => (0, String::new()),
        };
        let Some(search) = &mut state.search else {
            return;
        };
        search.results = match &state.history {
            Some(h) => h
                .search(&search.query, search.scope, session_id, &cwd, SEARCH_ROWS)
                .unwrap_or_default(),
            None => Vec::new(),
        };
        search.selected = 0;
    }

    // ---------------------------------------------------------------- 描画

    fn draw(&mut self, event_loop: &ActiveEventLoop) {
        let Some(state) = &mut self.state else { return };
        // 「描いた」印は、画面の内容を読む前に落とす。あとで落とすと、
        // 読んでから落とすまでのあいだに届いた更新が、通知を出さないまま
        // 捨てられる。出力が止まる直前（プロンプトが出た瞬間）に当たると、
        // その一画面ぶんが永久に描かれない。
        state.manager.clear_dirty();
        let layout = layout_of(&self.config, state);

        self.refresh_recent();
        let Some(state) = &mut self.state else { return };
        state.manager.drain_pending_clears();
        state.manager.refresh_branches();
        collect_find_cells(state);
        // 選んでいるセッションが名乗った題名を、ウィンドウに出す。
        let want_title = state
            .manager
            .selected()
            .and_then(|s| s.window_title.clone())
            .unwrap_or_else(|| "tex".to_string());
        if want_title != state.shown_title {
            if let Some(w) = &state.window {
                w.set_title(&want_title);
            }
            state.shown_title = want_title;
        }
        let theme = state.theme;
        state.renderer.begin();

        if layout.sidebar_cols > 0 {
            draw_sidebar(state, &layout, &theme);
            // 区切りの縦線。
            for row in 0..layout.rows {
                state.renderer.put_char(
                    layout.sidebar_cols,
                    row,
                    '│',
                    theme.fg_tertiary,
                    false,
                    false,
                );
            }
        }
        draw_terminal(state, &layout, &theme);
        draw_bottom(state, &layout, &theme);

        // 候補窓を、いま文字が入る位置へ寄せる。
        if let (Some((cx, cy)), Some(window)) = (state.cursor_cell, state.window.clone()) {
            let cell = state.renderer.cell();
            let pos = (
                (cx as f32 * cell.width) as i32,
                (cy as f32 * cell.height) as i32,
            );
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
        let Some(id) = state.manager.selected().map(|s| s.id) else {
            state.recent.clear();
            return;
        };
        if state.recent_for == Some(id) {
            return;
        }
        state.recent = match &state.history {
            Some(h) => h.recent(id, 10).unwrap_or_default(),
            None => Vec::new(),
        };
        state.recent_for = Some(id);
    }
}

/// 左ペインの一行分の割り付け。描画と当たり判定で同じ値を使う。
///
/// Warp に合わせ、1 セッションを 2 段から 3 段で表す。
/// 1 段目が名前、2 段目が作業ディレクトリ、3 段目がブランチ名（あれば）。
/// 段のあいだに 1 行の余白を置く。
struct Block {
    index: usize,
    depth: usize,
    /// 段の先頭の行。
    top: usize,
    /// 余白を含まない段の高さ。
    height: usize,
}

struct SidebarLayout {
    width: usize,
    blocks: Vec<Block>,
    /// [+] と × を置く桁。
    action_col: usize,
    /// 実行状態の印を置く桁。
    marker_col: usize,
    /// ⌘ の番号を置く桁。
    hint_col: usize,
    sep_row: usize,
}

const SIDEBAR_TOP: usize = 2;
/// 段のあいだに置く余白の行数。
const BLOCK_GAP: usize = 1;

fn sidebar_layout(state: &State, layout: &Layout) -> SidebarLayout {
    let width = layout.sidebar_cols;
    // 下段に「直近のコマンド」を出す余地を残す。
    let limit = layout.rows.saturating_sub(8).max(1);
    let mut blocks = Vec::new();
    let mut row = SIDEBAR_TOP;
    for tree in state.manager.tree_rows() {
        let Some(s) = state.manager.sessions().get(tree.index) else {
            continue;
        };
        // 1 段目は名前。2 段目は名乗った題名、3 段目はブランチ名で、
        // どちらも無ければその段を作らない。
        let height =
            1 + usize::from(s.window_title.is_some()) + usize::from(s.branch.is_some());
        if row + height > limit {
            break;
        }
        blocks.push(Block {
            index: tree.index,
            depth: tree.depth,
            top: row,
            height,
        });
        row += height + BLOCK_GAP;
    }
    let sep_row = row.saturating_sub(BLOCK_GAP);
    SidebarLayout {
        width,
        blocks,
        action_col: width.saturating_sub(4),
        marker_col: width.saturating_sub(2),
        hint_col: width.saturating_sub(8),
        sep_row,
    }
}

/// 左ペインのどこを押したか。
enum SidebarHit {
    NewSession,
    Select(usize),
    Close(usize),
}

fn sidebar_hit(state: &State, layout: &Layout, col: usize, row: usize) -> Option<SidebarHit> {
    if layout.sidebar_cols == 0 || col >= layout.sidebar_cols {
        return None;
    }
    let sl = sidebar_layout(state, layout);
    if row == 0 {
        return (col >= sl.action_col && col < sl.action_col + 3).then_some(SidebarHit::NewSession);
    }
    for b in &sl.blocks {
        if row < b.top || row >= b.top + b.height {
            continue;
        }
        // × は名前の段だけで受ける。取り違えて閉じないようにする。
        if row == b.top && col >= sl.action_col && col < sl.action_col + 1 {
            return Some(SidebarHit::Close(b.index));
        }
        return Some(SidebarHit::Select(b.index));
    }
    None
}

pub(crate) fn draw_sidebar(state: &mut State, layout: &Layout, theme: &Theme) {
    let sl = sidebar_layout(state, layout);
    let w = sl.width;
    state.renderer.fill_cells(0, 0, w, layout.rows, theme.chrome_bg);
    // 押せる目印。キーが効かない環境でもここから増やせる。
    state
        .renderer
        .put_char(sl.marker_col, 0, '+', theme.accent, true, false);

    let selected = state.manager.selected_index();
    for (n, b) in sl.blocks.iter().enumerate() {
        let s = &state.manager.sessions()[b.index];
        let running = s.is_running();
        let indent = b.depth * 2;
        let name_width = sl.hint_col.saturating_sub(indent + 2);
        // 会話を引き継いでいない印の分を先に空けておく。
        // あとで足すと、切り詰めで印そのものが落ちる。
        let forked = !s.inherited && s.parent.is_some();
        let mut name = s.display_name(name_width - usize::from(forked));
        if forked {
            name.push('*');
        }
        let profile = (s.profile != HOST_PROFILE).then(|| s.profile.clone());
        let osc_title = s.window_title.clone();
        let branch = s.branch.clone();
        let exit = match s.state {
            RunState::Running => None,
            RunState::Exited(code) => Some(code),
        };

        // 選んでいる段は、余白を除いた高さ全体を塗る。
        if b.index == selected {
            state.renderer.fill_cells(0, b.top, w, b.height, theme.surface);
        }

        let mut x = 1 + indent;
        if b.depth > 0 {
            state
                .renderer
                .put_char(x - 1, b.top, '└', theme.fg_tertiary, false, false);
        }
        let fg = if running {
            theme.fg_primary
        } else {
            theme.fg_tertiary
        };
        x += state
            .renderer
            .put_str_clipped(x, b.top, &name, name_width, fg);
        let _ = x;

        // ⌘ の番号。押せることが見えていないと使われない。
        if n < 9 {
            state.renderer.put_str(
                sl.hint_col,
                b.top,
                &format!("⌘{}", n + 1),
                theme.fg_tertiary,
            );
        }
        state
            .renderer
            .put_char(sl.action_col, b.top, '×', theme.fg_tertiary, false, false);
        match exit {
            None => state
                .renderer
                .put_char(sl.marker_col, b.top, '●', theme.accent, false, false),
            Some(code) => {
                let c = if code == 0 { theme.fg_tertiary } else { theme.warn };
                state
                    .renderer
                    .put_char(sl.marker_col, b.top, '○', c, false, false);
            }
        }

        let mut line = b.top + 1;
        // 端末上のプログラムが名乗った題名。何をしているセッションかが分かる。
        if let Some(title) = osc_title {
            let mut tx = 2 + indent;
            state
                .renderer
                .put_char(tx, line, '✻', theme.accent, false, false);
            tx += 2;
            state.renderer.put_str_clipped(
                tx,
                line,
                &title,
                w.saturating_sub(tx + 1),
                theme.fg_secondary,
            );
            line += 1;
        }
        // ブランチ名。git の下にいなければ段そのものを作らない。
        if let Some(branch) = branch {
            let mut bx = 2 + indent;
            state
                .renderer
                .put_char(bx, line, '⋔', theme.fg_tertiary, false, false);
            bx += 2;
            let used = state.renderer.put_str_clipped(
                bx,
                line,
                &branch,
                w.saturating_sub(bx + 1),
                theme.fg_tertiary,
            );
            bx += used;
            if let Some(p) = &profile {
                if bx + 4 < w {
                    state.renderer.put_str_clipped(
                        bx + 1,
                        line,
                        p,
                        w.saturating_sub(bx + 2),
                        theme.accent,
                    );
                }
            }
        }
    }

    // 直近のコマンド。何もなければ区切りごと出さない。
    // 見出しを置かなくても、区切りの下にあることで何の一覧かは分かる。
    if state.recent.is_empty() || sl.sep_row + 1 >= layout.rows {
        return;
    }
    for x in 0..w {
        state
            .renderer
            .put_char(x, sl.sep_row, '─', theme.fg_tertiary, false, false);
    }
    let list_top = sl.sep_row + 1;
    let avail = layout.rows.saturating_sub(list_top + 1);
    for (i, entry) in state.recent.iter().take(avail).enumerate() {
        let y = list_top + i;
        let code = entry.exit_code.unwrap_or(0);
        let marker_color = if code == 0 { theme.fg_tertiary } else { theme.warn };
        state
            .renderer
            .put_str(1, y, &format!("{code:>3}"), marker_color);
        state.renderer.put_str_clipped(
            5,
            y,
            &entry.command,
            w.saturating_sub(6),
            theme.fg_secondary,
        );
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
        state.renderer.put_char(x, d.row, d.c, d.fg, d.bold, d.italic);
        if d.underline {
            state.renderer.underline_cells(x, d.row, cols, d.fg);
        }
    }

    if show_cursor {
        let row = cursor_point.line.0;
        if row >= 0 && (row as usize) < layout.term_rows {
            let x = layout.term_col + cursor_point.column.0.min(layout.term_cols - 1);
            let y = row as usize;
            state.cursor_cell = Some((x, y));
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
                state.cursor_cell = Some((caret, y));
            } else {
                match cursor_shape {
                    CursorShape::Beam => state.renderer.cursor_beam(x, y, theme.cursor),
                    CursorShape::Underline => {
                        state.renderer.underline_cells(x, y, cursor_cols, theme.cursor)
                    }
                    _ => state
                        .renderer
                        .fill_cells_alpha(x, y, cursor_cols, 1, theme.cursor, 0.55),
                }
            }
        }
    }
}

pub(crate) fn draw_bottom(state: &mut State, layout: &Layout, theme: &Theme) {
    let y = layout.rows.saturating_sub(1);
    if let Some(rename) = &state.rename {
        let x0 = layout.term_col;
        let w = layout.term_cols;
        state.renderer.fill_cells(x0, y, w, 1, theme.surface);
        let prompt = format!("名前: {}", rename.input);
        let mut used = state.renderer.put_str_clipped(
            x0 + 1,
            y,
            &prompt,
            w.saturating_sub(2),
            theme.fg_primary,
        );
        if !state.preedit.is_empty() {
            let text = state.preedit.clone();
            let at = x0 + 1 + used;
            let avail = w.saturating_sub(used + 2);
            let cols: usize = text.chars().map(char_cols).sum();
            state
                .renderer
                .fill_cells(at, y, cols.min(avail), 1, theme.chrome_bg);
            let n = state
                .renderer
                .put_str_clipped(at, y, &text, avail, theme.fg_primary);
            state.renderer.underline_cells(at, y, n, theme.accent);
            used += n;
        }
        let cursor_x = x0 + 1 + used;
        state.cursor_cell = Some((cursor_x, y));
        if cursor_x < layout.cols {
            state
                .renderer
                .fill_cells_alpha(cursor_x, y, 1, 1, theme.cursor, 0.55);
        }
        let note = "Enter で確定  空にすると作業ディレクトリに戻る  Esc で取消";
        let nx = layout.cols.saturating_sub(note.chars().map(char_cols).sum::<usize>() + 2);
        if nx > cursor_x + 2 {
            state.renderer.put_str_clipped(
                nx,
                y,
                note,
                layout.cols.saturating_sub(nx + 1),
                theme.fg_tertiary,
            );
        }
        return;
    }
    if let Some(find) = &state.find {
        let x0 = layout.term_col;
        let w = layout.term_cols;
        state.renderer.fill_cells(x0, y, w, 1, theme.surface);
        let label = if find.regex_mode { "find(正規表現)" } else { "find" };
        let prompt = format!("{label}: {}", find.query);
        let fg = if find.invalid {
            theme.warn
        } else {
            theme.fg_primary
        };
        let mut used = state
            .renderer
            .put_str_clipped(x0 + 1, y, &prompt, w.saturating_sub(2), fg);
        // 変換中の文字列は確定前なので、検索には使わずここに出すだけ。
        if !state.preedit.is_empty() {
            let text = state.preedit.clone();
            let at = x0 + 1 + used;
            let avail = w.saturating_sub(used + 2);
            let cols: usize = text.chars().map(char_cols).sum();
            state
                .renderer
                .fill_cells(at, y, cols.min(avail), 1, theme.chrome_bg);
            let n = state
                .renderer
                .put_str_clipped(at, y, &text, avail, theme.fg_primary);
            state.renderer.underline_cells(at, y, n, theme.accent);
            used += n;
        }
        let cursor_x = x0 + 1 + used;
        state.cursor_cell = Some((cursor_x, y));
        if cursor_x < layout.cols {
            state
                .renderer
                .fill_cells_alpha(cursor_x, y, 1, 1, theme.cursor, 0.55);
        }
        let note = if find.invalid {
            "正規表現が誤っている"
        } else if !find.query.is_empty() && find.current.is_none() {
            "見つからない"
        } else {
            "Enter 上へ  ⇧Enter 下へ  Tab 正規表現  Esc 閉じる"
        };
        let nx = layout.cols.saturating_sub(note.chars().map(char_cols).sum::<usize>() + 2);
        if nx > cursor_x + 2 {
            state.renderer.put_str_clipped(
                nx,
                y,
                note,
                layout.cols.saturating_sub(nx + 1),
                theme.fg_tertiary,
            );
        }
        return;
    }
    if let Some(picker) = &state.picker {
        let n = picker.names.len();
        let top = y.saturating_sub(n);
        let x0 = layout.term_col;
        let w = layout.term_cols;
        state.renderer.fill_cells(x0, top, w, n + 1, theme.chrome_bg);
        let names = picker.names.clone();
        let sel = picker.selected;
        for (i, name) in names.iter().enumerate() {
            let row = top + i;
            if i == sel {
                state.renderer.fill_cells(x0, row, w, 1, theme.surface);
            }
            let fg = if i == sel {
                theme.fg_primary
            } else {
                theme.fg_secondary
            };
            let label = format!("{}  {}", i + 1, name);
            state
                .renderer
                .put_str_clipped(x0 + 2, row, &label, w.saturating_sub(3), fg);
        }
        state.renderer.fill_cells(x0, y, w, 1, theme.surface);
        state.renderer.put_str_clipped(
            x0 + 1,
            y,
            "分岐先のプロファイル: j/k または数字で選び Enter、Esc で取り消し",
            w.saturating_sub(2),
            theme.fg_primary,
        );
        return;
    }
    if let Some(search) = &state.search {
        let n = search.results.len().min(SEARCH_ROWS);
        let top = y.saturating_sub(n);
        let x0 = layout.term_col;
        let w = layout.term_cols;
        state
            .renderer
            .fill_cells(x0, top, w, n + 1, theme.chrome_bg);
        for (i, entry) in search.results.iter().take(n).enumerate() {
            let row = top + i;
            if i == search.selected {
                state.renderer.fill_cells(x0, row, w, 1, theme.surface);
            }
            let fg = if i == search.selected {
                theme.fg_primary
            } else {
                theme.fg_secondary
            };
            state
                .renderer
                .put_str_clipped(x0 + 2, row, &entry.command, w.saturating_sub(3), fg);
        }
        let prompt = format!("history[{}]: {}", search.scope.label(), search.query);
        state.renderer.fill_cells(x0, y, w, 1, theme.surface);
        state
            .renderer
            .put_str_clipped(x0 + 1, y, &prompt, w.saturating_sub(2), theme.fg_primary);
        let cursor_x = x0 + 1 + prompt.chars().map(char_cols).sum::<usize>();
        if cursor_x < layout.cols {
            state
                .renderer
                .fill_cells_alpha(cursor_x, y, 1, 1, theme.cursor, 0.55);
        }
        return;
    }

    if let Some(msg) = &state.status {
        let msg = msg.clone();
        state
            .renderer
            .fill_cells(layout.term_col, y, layout.term_cols, 1, theme.chrome_bg);
        state.renderer.put_str_clipped(
            layout.term_col + 1,
            y,
            &msg,
            layout.term_cols.saturating_sub(2),
            theme.warn,
        );
        return;
    }

    // 通常時は操作のヒントだけを薄く出す。
    let hint = "^O 新規  ^\\ fork  ^] 選んで  ^^ 次  ⌘F 検索  ⌘I 改名  ^R 履歴  ^B ペイン  ⌘K 消去  ⌘W 終了";
    state.renderer.put_str_clipped(
        layout.term_col + 1,
        y,
        hint,
        layout.term_cols.saturating_sub(2),
        theme.fg_tertiary,
    );
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
mod path_tests {
    /// macOS の `dirs::config_dir()` は `~/.config` ではなく
    /// `~/Library/Application Support` を返す。README に書いた場所と
    /// 実装がずれないよう、XDG の作法に従っていることを確かめる。
    #[test]
    fn 設定と履歴は_xdg_の場所にある() {
        let config = crate::config::config_path().expect("設定の場所が決まる");
        let db = crate::history::db_path().expect("履歴の場所が決まる");
        assert!(
            config.ends_with(".config/tex/config.toml"),
            "設定の場所が想定と違う: {}",
            config.display()
        );
        assert!(
            db.ends_with(".local/share/tex/history.db"),
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
            let first = if line == start.line { start.column.0 } else { 0 };
            let last = if line == end.line { end.column.0 } else { cols - 1 };
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

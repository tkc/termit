//! tex: エージェント向けの軽量ターミナル。

mod clipboard;
mod keytest;
mod probe;
mod config;
mod history;
mod input;
mod osc;
mod pty;
mod rect;
mod render;
mod session;
mod term;
mod theme;

use std::path::PathBuf;
use std::sync::Arc;

use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::index::{Column, Point, Side};
use alacritty_terminal::term::{viewport_to_point, TermMode};
use alacritty_terminal::vte::ansi::{CursorShape, Rgb};
use winit::application::ApplicationHandler;
use winit::event::{ElementState, Ime, MouseButton, MouseScrollDelta, WindowEvent};
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
    /// 端末領域でドラッグ中か。
    dragging: bool,
    /// 直前のクリックの時刻、セル、連続回数。
    last_click: Option<(std::time::Instant, (usize, usize), u8)>,
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
    mouse: MouseState,
    mods: ModifiersState,
    status: Option<String>,
    recent: Vec<Entry>,
    recent_for: Option<u32>,
    window: Option<Arc<Window>>,
}

impl State {
    fn request_redraw(&self) {
        if let Some(w) = &self.window {
            w.request_redraw();
        }
    }
}

struct App {
    config: Config,
    cwd: PathBuf,
    proxy: EventLoopProxy<UiEvent>,
    state: Option<State>,
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
        ));

        let mut state = State {
            renderer,
            manager: Manager::new(TermSize::new(80, 24), (8, 16), self.proxy.clone().into()),
            history: History::open_default(),
            theme: Theme::default(),
            sidebar: true,
            search: None,
            picker: None,
            mouse: MouseState::default(),
            mods: ModifiersState::empty(),
            status: None,
            recent: Vec::new(),
            recent_for: None,
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
        let Some(state) = &mut self.state else { return };
        match event {
            UiEvent::Wakeup(_) => {}
            UiEvent::Title(_, _) => {}
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
                if state.mouse.dragging {
                    self.drag_selection();
                    if let Some(s) = &self.state {
                        s.request_redraw();
                    }
                }
            }
            WindowEvent::MouseInput {
                state: button_state,
                button: MouseButton::Left,
                ..
            } => {
                match button_state {
                    ElementState::Pressed => self.on_click(),
                    ElementState::Released => self.on_release(),
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
            WindowEvent::Ime(Ime::Commit(text)) => {
                if state.search.is_some() {
                    if let Some(s) = &mut state.search {
                        s.query.push_str(&text);
                    }
                    self.refresh_search();
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
                    self.on_key(event, event_loop);
                }
            }
            WindowEvent::RedrawRequested => self.draw(),
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
        if let Some(bytes) = input::encode(&key, &base, event.text.as_deref(), mods, mode) {
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
            Action::SearchHistory => {
                state.picker = None;
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
    fn on_click(&mut self) {
        let config = self.config.clone();
        let Some(state) = &mut self.state else { return };
        let layout = layout_of(&self.config, state);
        let (col, row, side) = mouse_cell(state);

        // 重ねた一覧が開いているときは、まず閉じる。
        if state.picker.is_some() || state.search.is_some() {
            state.picker = None;
            state.search = None;
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
        if let Some(session) = state.manager.selected() {
            session
                .term
                .lock()
                .scroll_display(alacritty_terminal::grid::Scroll::Delta(lines));
        }
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

    fn draw(&mut self) {
        let Some(state) = &mut self.state else { return };
        let layout = layout_of(&self.config, state);

        self.refresh_recent();
        let Some(state) = &mut self.state else { return };
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

        state.renderer.render(theme.bg);
        state.manager.clear_dirty();
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

/// 左ペインの中の位置。描画と当たり判定で同じ値を使う。
struct SidebarLayout {
    width: usize,
    tree_top: usize,
    shown: usize,
    /// [+] と × を置く桁。
    action_col: usize,
    /// 実行状態の印を置く桁。
    marker_col: usize,
    sep_row: usize,
    list_top: usize,
}

fn sidebar_layout(state: &State, layout: &Layout) -> SidebarLayout {
    let width = layout.sidebar_cols;
    let tree_top = 1;
    let max_tree = layout.rows.saturating_sub(6).max(1);
    let shown = state.manager.tree_rows().len().min(max_tree);
    let sep_row = tree_top + shown;
    SidebarLayout {
        width,
        tree_top,
        shown,
        action_col: width.saturating_sub(4),
        marker_col: width.saturating_sub(2),
        sep_row,
        list_top: sep_row + 2,
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
    if row >= sl.tree_top && row < sl.tree_top + sl.shown {
        let i = row - sl.tree_top;
        let index = state.manager.tree_rows().get(i)?.index;
        if col >= sl.action_col && col < sl.action_col + 1 {
            return Some(SidebarHit::Close(index));
        }
        return Some(SidebarHit::Select(index));
    }
    None
}

pub(crate) fn draw_sidebar(state: &mut State, layout: &Layout, theme: &Theme) {
    let sl = sidebar_layout(state, layout);
    let w = sl.width;
    state.renderer.fill_cells(0, 0, w, layout.rows, theme.chrome_bg);
    state.renderer.put_str(1, 0, "SESSIONS", theme.fg_secondary);
    // 押せる目印。キーが効かない環境でもここから増やせる。
    state.renderer.put_str(sl.action_col, 0, "[+]", theme.accent);

    let rows = state.manager.tree_rows();
    let selected = state.manager.selected_index();

    for (i, row) in rows.iter().take(sl.shown).enumerate() {
        let y = sl.tree_top + i;
        let s = &state.manager.sessions()[row.index];
        let is_selected = row.index == selected;
        if is_selected {
            state.renderer.fill_cells(0, y, w, 1, theme.surface);
        }
        let indent = row.depth * 2;
        let mut x = 1 + indent;
        if row.depth > 0 {
            state
                .renderer
                .put_char(x - 1, y, '└', theme.fg_tertiary, false, false);
        }
        let fg = if s.is_running() {
            theme.fg_primary
        } else {
            theme.fg_tertiary
        };
        let mut label = s.title.clone();
        if !s.inherited && s.parent.is_some() {
            label.push('*');
        }
        if s.profile != HOST_PROFILE {
            label.push_str(" box");
        }
        let name_width = w.saturating_sub(indent + 6);
        x += state
            .renderer
            .put_str_clipped(x, y, &label, name_width, fg);
        let _ = x;
        state
            .renderer
            .put_char(sl.action_col, y, '×', theme.fg_tertiary, false, false);
        match s.state {
            RunState::Running => {
                state
                    .renderer
                    .put_char(sl.marker_col, y, '●', theme.accent, false, false);
            }
            RunState::Exited(code) => {
                let c = if code == 0 { theme.fg_tertiary } else { theme.warn };
                state
                    .renderer
                    .put_char(sl.marker_col, y, '○', c, false, false);
            }
        }
    }

    if sl.sep_row + 2 >= layout.rows {
        return;
    }
    for x in 0..w {
        state
            .renderer
            .put_char(x, sl.sep_row, '─', theme.fg_tertiary, false, false);
    }
    let title = state
        .manager
        .selected()
        .map(|s| s.title.clone())
        .unwrap_or_default();
    state
        .renderer
        .put_str(1, sl.sep_row + 1, "RECENT", theme.fg_secondary);
    state.renderer.put_str_clipped(
        8,
        sl.sep_row + 1,
        &title,
        w.saturating_sub(9),
        theme.fg_tertiary,
    );

    let avail = layout.rows.saturating_sub(sl.list_top + 1);
    if state.recent.is_empty() && state.history.is_some() && avail > 0 {
        state.renderer.put_str_clipped(
            1,
            sl.list_top,
            "シェル統合が未設定",
            w.saturating_sub(2),
            theme.fg_tertiary,
        );
        return;
    }
    for (i, entry) in state.recent.iter().take(avail).enumerate() {
        let y = sl.list_top + i;
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

pub(crate) fn draw_bottom(state: &mut State, layout: &Layout, theme: &Theme) {
    let y = layout.rows.saturating_sub(1);
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
    let hint = "^O 新規  ^\\ fork  ^] 選んで fork  ^^ 次のセッション  ^R 履歴  ^B ペイン  ⌘W 終了  ⌘C コピー";
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

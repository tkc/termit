//! tex: エージェント向けの軽量ターミナル。

mod clipboard;
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

use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::TermMode;
use alacritty_terminal::vte::ansi::{CursorShape, Rgb};
use winit::application::ApplicationHandler;
use winit::event::{Ime, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::window::{Window, WindowId};

use crate::config::{Config, HOST_PROFILE};
use crate::history::{Entry, History, Scope};
use crate::input::Action;
use crate::render::{char_cols, Renderer};
use crate::session::{Manager, RunState};
use crate::term::{TermSize, UiEvent};
use crate::theme::Theme;

const SEARCH_ROWS: usize = 10;

fn main() {
    env_logger::init();

    let args: Vec<String> = std::env::args().collect();
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

struct SearchState {
    query: String,
    scope: Scope,
    results: Vec<Entry>,
    selected: usize,
}

pub(crate) struct State {
    renderer: Renderer,
    manager: Manager,
    history: Option<History>,
    theme: Theme,
    sidebar: bool,
    search: Option<SearchState>,
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

        // 検索中はすべてのキーを検索欄が受ける。
        if state.search.is_some() {
            self.on_search_key(&key, &event, mods);
            if let Some(s) = &self.state {
                s.request_redraw();
            }
            return;
        }

        if let Some(action) = input::action_for(&key, mods) {
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
        if let Some(bytes) = input::encode(&key, event.text.as_deref(), mods, mode) {
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
                // host 以外の最初のプロファイルへ分岐する。
                let names = config.profile_names();
                let target = names.iter().find(|n| n.as_str() != HOST_PROFILE).cloned();
                match target {
                    Some(p) => {
                        let i = state.manager.selected_index();
                        if let Err(e) = state.manager.fork(&config, i, Some(&p)) {
                            state.status = Some(e.to_string());
                        }
                    }
                    None => {
                        state.status =
                            Some("設定に host 以外のプロファイルがない".into())
                    }
                }
            }
            Action::SelectNext => state.manager.select_next(),
            Action::SelectPrev => state.manager.select_prev(),
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
        let layout = {
            let (cols, rows) = state.renderer.grid_size();
            let sidebar_cols = if state.sidebar {
                self.config.window.sidebar_cols.min(cols.saturating_sub(20))
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
        };

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
                    theme.sidebar_dim,
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

pub(crate) fn draw_sidebar(state: &mut State, layout: &Layout, theme: &Theme) {
    let w = layout.sidebar_cols;
    state.renderer.fill_cells(0, 0, w, layout.rows, theme.sidebar_bg);
    state.renderer.put_str(1, 0, "SESSIONS", theme.sidebar_dim);

    let rows = state.manager.tree_rows();
    let selected = state.manager.selected_index();
    let tree_top = 1usize;
    let max_tree = layout.rows.saturating_sub(6).max(1);
    let shown = rows.len().min(max_tree);

    for (i, row) in rows.iter().take(shown).enumerate() {
        let y = tree_top + i;
        let s = &state.manager.sessions()[row.index];
        if row.index == selected {
            state.renderer.fill_cells(0, y, w, 1, theme.sidebar_sel);
        }
        let indent = row.depth * 2;
        let mut x = 1 + indent;
        if row.depth > 0 {
            state
                .renderer
                .put_char(x - 1, y, '└', theme.sidebar_dim, false, false);
        }
        let fg = if s.is_running() {
            theme.sidebar_fg
        } else {
            theme.sidebar_dim
        };
        // 実行状態の印は右端に置くので、名前の幅から差し引く。
        let name_width = w.saturating_sub(indent + 4);
        let mut label = s.title.clone();
        if !s.inherited && s.parent.is_some() {
            label.push('*');
        }
        x += state.renderer.put_str_clipped(x, y, &label, name_width, fg);
        let _ = x;
        match s.state {
            RunState::Running => {
                state
                    .renderer
                    .put_char(w - 2, y, '●', theme.accent, false, false);
            }
            RunState::Exited(code) => {
                let c = if code == 0 { theme.sidebar_dim } else { theme.warn };
                state.renderer.put_char(w - 2, y, '○', c, false, false);
            }
        }
        if s.profile != HOST_PROFILE {
            let py = y;
            let px = w.saturating_sub(6);
            state
                .renderer
                .put_str_clipped(px.max(1), py, "box", 3, theme.accent);
        }
    }

    // 区切りと直近のコマンド。
    let sep_row = tree_top + shown;
    if sep_row + 2 >= layout.rows {
        return;
    }
    for x in 0..w {
        state
            .renderer
            .put_char(x, sep_row, '─', theme.sidebar_dim, false, false);
    }
    let title = state
        .manager
        .selected()
        .map(|s| s.title.clone())
        .unwrap_or_default();
    state.renderer.put_str(1, sep_row + 1, "RECENT", theme.sidebar_dim);
    state
        .renderer
        .put_str_clipped(8, sep_row + 1, &title, w.saturating_sub(9), theme.sidebar_dim);

    let list_top = sep_row + 2;
    let avail = layout.rows.saturating_sub(list_top + 1);
    if state.recent.is_empty() && state.history.is_some() && avail > 0 {
        state.renderer.put_str_clipped(
            1,
            list_top,
            "シェル統合が未設定",
            w.saturating_sub(2),
            theme.sidebar_dim,
        );
        return;
    }
    for (i, entry) in state.recent.iter().take(avail).enumerate() {
        let y = list_top + i;
        let code = entry.exit_code.unwrap_or(0);
        let marker_color = if code == 0 { theme.sidebar_dim } else { theme.warn };
        state
            .renderer
            .put_str(1, y, &format!("{code:>3}"), marker_color);
        state.renderer.put_str_clipped(
            5,
            y,
            &entry.command,
            w.saturating_sub(6),
            theme.sidebar_fg,
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
    if let Some(search) = &state.search {
        let n = search.results.len().min(SEARCH_ROWS);
        let top = y.saturating_sub(n);
        let x0 = layout.term_col;
        let w = layout.term_cols;
        state
            .renderer
            .fill_cells(x0, top, w, n + 1, theme.sidebar_bg);
        for (i, entry) in search.results.iter().take(n).enumerate() {
            let row = top + i;
            if i == search.selected {
                state.renderer.fill_cells(x0, row, w, 1, theme.sidebar_sel);
            }
            let fg = if i == search.selected {
                theme.fg
            } else {
                theme.sidebar_fg
            };
            state
                .renderer
                .put_str_clipped(x0 + 2, row, &entry.command, w.saturating_sub(3), fg);
        }
        let prompt = format!("history[{}]: {}", search.scope.label(), search.query);
        state.renderer.fill_cells(x0, y, w, 1, theme.sidebar_sel);
        state
            .renderer
            .put_str_clipped(x0 + 1, y, &prompt, w.saturating_sub(2), theme.fg);
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
            .fill_cells(layout.term_col, y, layout.term_cols, 1, theme.sidebar_bg);
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
    let hint = "^⇧N 新規  ^⇧F fork  ^⇧S sandbox fork  ^⇧J/K 選択  ^R 履歴  ^B ペイン";
    state.renderer.put_str_clipped(
        layout.term_col + 1,
        y,
        hint,
        layout.term_cols.saturating_sub(2),
        theme.sidebar_dim,
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

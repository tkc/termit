//! 押したキーが何として届いているかを、その場で画面に出す。
//!
//! キーバインドが効かないとき、ログを読む往復をせずに切り分けるために使う。

use std::sync::Arc;

use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::ModifiersState;
use winit::platform::modifier_supplement::KeyEventExtModifierSupplement;
use winit::window::{Window, WindowId};

use crate::input;
use crate::render::Renderer;
use crate::theme::Theme;

const KEEP: usize = 24;

struct Row {
    text: String,
    matched: bool,
}

struct App {
    renderer: Option<Renderer>,
    window: Option<Arc<Window>>,
    mods: ModifiersState,
    rows: Vec<Row>,
    font: String,
    font_size: f32,
}

pub fn run(font: &str, font_size: f32) {
    let event_loop = EventLoop::new().expect("イベントループを作れない");
    event_loop.set_control_flow(ControlFlow::Wait);
    let mut app = App {
        renderer: None,
        window: None,
        mods: ModifiersState::empty(),
        rows: Vec::new(),
        font: font.to_string(),
        font_size,
    };
    let _ = event_loop.run_app(&mut app);
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attrs = Window::default_attributes()
            .with_title("termit — keytest")
            .with_inner_size(winit::dpi::LogicalSize::new(900.0, 560.0));
        let window = Arc::new(
            event_loop
                .create_window(attrs)
                .expect("ウィンドウを作れない"),
        );
        window.set_ime_allowed(true);
        let renderer = pollster::block_on(Renderer::new(
            window.clone(),
            event_loop,
            &self.font,
            self.font_size,
            true,
        ));
        self.renderer = Some(renderer);
        self.window = Some(window.clone());
        window.request_redraw();
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(size) => {
                if let Some(r) = &mut self.renderer {
                    r.resize(size.width, size.height);
                }
            }
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                if let Some(r) = &mut self.renderer {
                    r.set_scale(scale_factor as f32);
                }
            }
            WindowEvent::ModifiersChanged(m) => self.mods = m.state(),
            WindowEvent::KeyboardInput { event, .. } => {
                if event.state.is_pressed() {
                    self.push(&event);
                }
            }
            WindowEvent::RedrawRequested => self.draw(),
            _ => {}
        }
        if let Some(w) = &self.window {
            w.request_redraw();
        }
    }
}

impl App {
    fn push(&mut self, event: &winit::event::KeyEvent) {
        let base = event.key_without_modifiers();
        let action = input::action_for(&base, event.physical_key, self.mods);
        let mut held = Vec::new();
        if self.mods.control_key() {
            held.push("Ctrl");
        }
        if self.mods.shift_key() {
            held.push("Shift");
        }
        if self.mods.alt_key() {
            held.push("Alt");
        }
        if self.mods.super_key() {
            held.push("Cmd");
        }
        let held = if held.is_empty() {
            "none".to_string()
        } else {
            held.join("+")
        };
        let text = format!(
            "mods={held:<20} base={:<14} logical={:<14} physical={:<16} action={}",
            short_key(&base),
            short_key(&event.logical_key),
            format!("{:?}", event.physical_key)
                .replace("Code(", "")
                .replace(')', ""),
            match &action {
                Some(a) => format!("{a:?}"),
                None => "—".to_string(),
            }
        );
        self.rows.push(Row {
            text,
            matched: action.is_some(),
        });
        if self.rows.len() > KEEP {
            self.rows.remove(0);
        }
    }

    fn draw(&mut self) {
        let Some(renderer) = &mut self.renderer else {
            return;
        };
        let theme = Theme::default();
        let (cols, _rows) = renderer.grid_size();
        renderer.begin();
        renderer.put_str(
            1,
            0,
            "termit keytest — shows what each key press arrives as",
            theme.fg_primary,
        );
        renderer.put_str(
            1,
            1,
            "if the action column stays —, that combination never reaches the terminal",
            theme.fg_secondary,
        );
        let mut y = 3;
        renderer.put_str(
            1,
            y,
            "Ctrl combinations this terminal takes:",
            theme.fg_secondary,
        );
        y += 1;
        for (keys, what) in input::STOLEN_CTRL_KEYS {
            renderer.put_str(3, y, keys, theme.accent);
            renderer.put_str(8, y, what, theme.fg_secondary);
            y += 1;
        }
        y += 1;
        renderer.put_str(
            1,
            y,
            "Cmd side: ⌘N new  ⌘D fork  ⌘E fork as…  ⌘K clear  ⌘W close  ⌘[ ⌘] select  ⌘C ⌘V",
            theme.fg_secondary,
        );
        y += 2;

        if self.rows.is_empty() {
            renderer.put_str(1, y, "press a key", theme.fg_tertiary);
        }
        let rows: Vec<(String, bool)> = self
            .rows
            .iter()
            .map(|r| (r.text.clone(), r.matched))
            .collect();
        for (text, matched) in rows {
            let color = if matched {
                theme.accent
            } else {
                theme.fg_secondary
            };
            renderer.put_str_clipped(1, y, &text, cols.saturating_sub(2), color);
            y += 1;
        }
        renderer.render(theme.bg);
    }
}

fn short_key(key: &winit::keyboard::Key) -> String {
    use winit::keyboard::Key;
    match key {
        Key::Character(c) => format!("{c:?}"),
        Key::Named(n) => format!("{n:?}"),
        Key::Dead(d) => format!("Dead({d:?})"),
        other => format!("{other:?}"),
    }
}

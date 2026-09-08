//! キー入力を PTY へ送るバイト列へ変換する。
//!
//! 端末が横取りするのは第 11 節の一覧だけで、それ以外はすべて子プロセスへ渡す。

use alacritty_terminal::term::TermMode;
use winit::keyboard::{Key, ModifiersState, NamedKey};

/// 端末自身が処理する操作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    NewSession,
    Fork,
    ForkWithProfile,
    SelectNext,
    SelectPrev,
    CloseSession,
    SearchHistory,
    ToggleSidebar,
    Copy,
    Paste,
    FontBigger,
    FontSmaller,
    ScrollUp,
    ScrollDown,
}

/// 横取りする組み合わせかを判定する。
pub fn action_for(key: &Key, mods: ModifiersState) -> Option<Action> {
    let ctrl = mods.control_key();
    let shift = mods.shift_key();
    match key {
        Key::Character(c) => {
            let c = c.to_lowercase();
            match (ctrl, shift, c.as_str()) {
                (true, true, "n") => Some(Action::NewSession),
                (true, true, "f") => Some(Action::Fork),
                (true, true, "s") => Some(Action::ForkWithProfile),
                (true, true, "j") => Some(Action::SelectNext),
                (true, true, "k") => Some(Action::SelectPrev),
                (true, true, "w") => Some(Action::CloseSession),
                (true, true, "c") => Some(Action::Copy),
                (true, true, "v") => Some(Action::Paste),
                (true, true, "=") | (true, true, "+") => Some(Action::FontBigger),
                (true, true, "-") | (true, true, "_") => Some(Action::FontSmaller),
                (true, false, "r") => Some(Action::SearchHistory),
                (true, false, "b") => Some(Action::ToggleSidebar),
                _ => None,
            }
        }
        Key::Named(NamedKey::PageUp) if shift => Some(Action::ScrollUp),
        Key::Named(NamedKey::PageDown) if shift => Some(Action::ScrollDown),
        _ => None,
    }
}

/// キーを PTY へ送るバイト列へ変換する。送るものがなければ `None`。
pub fn encode(
    key: &Key,
    text: Option<&str>,
    mods: ModifiersState,
    mode: TermMode,
) -> Option<Vec<u8>> {
    let alt = mods.alt_key();
    let ctrl = mods.control_key();
    let shift = mods.shift_key();
    let app_cursor = mode.contains(TermMode::APP_CURSOR);

    let base: Vec<u8> = match key {
        Key::Named(named) => match named {
            NamedKey::Enter => vec![b'\r'],
            NamedKey::Backspace => vec![0x7f],
            NamedKey::Tab => {
                if shift {
                    b"\x1b[Z".to_vec()
                } else {
                    vec![b'\t']
                }
            }
            NamedKey::Escape => vec![0x1b],
            NamedKey::Delete => b"\x1b[3~".to_vec(),
            NamedKey::Insert => b"\x1b[2~".to_vec(),
            NamedKey::ArrowUp => cursor_seq(b'A', app_cursor),
            NamedKey::ArrowDown => cursor_seq(b'B', app_cursor),
            NamedKey::ArrowRight => cursor_seq(b'C', app_cursor),
            NamedKey::ArrowLeft => cursor_seq(b'D', app_cursor),
            NamedKey::Home => cursor_seq(b'H', app_cursor),
            NamedKey::End => cursor_seq(b'F', app_cursor),
            NamedKey::PageUp => b"\x1b[5~".to_vec(),
            NamedKey::PageDown => b"\x1b[6~".to_vec(),
            NamedKey::Space => vec![b' '],
            NamedKey::F1 => b"\x1bOP".to_vec(),
            NamedKey::F2 => b"\x1bOQ".to_vec(),
            NamedKey::F3 => b"\x1bOR".to_vec(),
            NamedKey::F4 => b"\x1bOS".to_vec(),
            NamedKey::F5 => b"\x1b[15~".to_vec(),
            NamedKey::F6 => b"\x1b[17~".to_vec(),
            NamedKey::F7 => b"\x1b[18~".to_vec(),
            NamedKey::F8 => b"\x1b[19~".to_vec(),
            NamedKey::F9 => b"\x1b[20~".to_vec(),
            NamedKey::F10 => b"\x1b[21~".to_vec(),
            NamedKey::F11 => b"\x1b[23~".to_vec(),
            NamedKey::F12 => b"\x1b[24~".to_vec(),
            _ => return None,
        },
        Key::Character(c) => {
            if ctrl {
                let byte = control_byte(c)?;
                vec![byte]
            } else {
                text.or(Some(c.as_str()))?.as_bytes().to_vec()
            }
        }
        _ => text?.as_bytes().to_vec(),
    };

    if base.is_empty() {
        return None;
    }
    // Alt を押した入力は ESC を前置する。
    if alt {
        let mut out = Vec::with_capacity(base.len() + 1);
        out.push(0x1b);
        out.extend_from_slice(&base);
        return Some(out);
    }
    Some(base)
}

fn cursor_seq(final_byte: u8, app_cursor: bool) -> Vec<u8> {
    if app_cursor {
        vec![0x1b, b'O', final_byte]
    } else {
        vec![0x1b, b'[', final_byte]
    }
}

/// Ctrl と組み合わせた文字を制御文字へ落とす。
fn control_byte(c: &str) -> Option<u8> {
    let ch = c.chars().next()?;
    match ch {
        ' ' | '@' => Some(0x00),
        'a'..='z' => Some(ch as u8 - b'a' + 1),
        'A'..='Z' => Some(ch as u8 - b'A' + 1),
        '[' => Some(0x1b),
        '\\' => Some(0x1c),
        ']' => Some(0x1d),
        '^' => Some(0x1e),
        '_' | '/' => Some(0x1f),
        '?' => Some(0x7f),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use winit::keyboard::SmolStr;

    fn ch(s: &str) -> Key {
        Key::Character(SmolStr::new(s))
    }
    fn named(k: NamedKey) -> Key {
        Key::Named(k)
    }
    const NONE: ModifiersState = ModifiersState::empty();

    #[test]
    fn 印字可能な文字をそのまま送る() {
        assert_eq!(
            encode(&ch("a"), Some("a"), NONE, TermMode::empty()).unwrap(),
            b"a"
        );
    }

    #[test]
    fn 日本語の確定文字列を送る() {
        assert_eq!(
            encode(&ch("あ"), Some("あ"), NONE, TermMode::empty()).unwrap(),
            "あ".as_bytes()
        );
    }

    #[test]
    fn ctrl_と英字を制御文字へ落とす() {
        let m = ModifiersState::CONTROL;
        assert_eq!(encode(&ch("c"), None, m, TermMode::empty()).unwrap(), vec![0x03]);
        assert_eq!(encode(&ch("d"), None, m, TermMode::empty()).unwrap(), vec![0x04]);
        assert_eq!(encode(&ch("a"), None, m, TermMode::empty()).unwrap(), vec![0x01]);
    }

    #[test]
    fn ctrl_と記号を制御文字へ落とす() {
        let m = ModifiersState::CONTROL;
        assert_eq!(encode(&ch("["), None, m, TermMode::empty()).unwrap(), vec![0x1b]);
        assert_eq!(encode(&ch(" "), None, m, TermMode::empty()).unwrap(), vec![0x00]);
    }

    #[test]
    fn alt_は_esc_を前置する() {
        let m = ModifiersState::ALT;
        assert_eq!(
            encode(&ch("b"), Some("b"), m, TermMode::empty()).unwrap(),
            vec![0x1b, b'b']
        );
    }

    #[test]
    fn backspace_は_0x7f_を送る() {
        assert_eq!(
            encode(&named(NamedKey::Backspace), None, NONE, TermMode::empty()).unwrap(),
            vec![0x7f]
        );
    }

    #[test]
    fn delete_は_csi_3_チルダを送る() {
        assert_eq!(
            encode(&named(NamedKey::Delete), None, NONE, TermMode::empty()).unwrap(),
            b"\x1b[3~"
        );
    }

    #[test]
    fn 矢印はカーソルモードで形が変わる() {
        assert_eq!(
            encode(&named(NamedKey::ArrowUp), None, NONE, TermMode::empty()).unwrap(),
            b"\x1b[A"
        );
        assert_eq!(
            encode(&named(NamedKey::ArrowUp), None, NONE, TermMode::APP_CURSOR).unwrap(),
            b"\x1bOA"
        );
    }

    #[test]
    fn shift_tab_は逆タブを送る() {
        assert_eq!(
            encode(&named(NamedKey::Tab), None, ModifiersState::SHIFT, TermMode::empty()).unwrap(),
            b"\x1b[Z"
        );
    }

    #[test]
    fn 端末が横取りする組み合わせを判定する() {
        let cs = ModifiersState::CONTROL | ModifiersState::SHIFT;
        assert_eq!(action_for(&ch("f"), cs), Some(Action::Fork));
        assert_eq!(action_for(&ch("F"), cs), Some(Action::Fork));
        assert_eq!(action_for(&ch("n"), cs), Some(Action::NewSession));
        assert_eq!(
            action_for(&ch("r"), ModifiersState::CONTROL),
            Some(Action::SearchHistory)
        );
        assert_eq!(
            action_for(&ch("b"), ModifiersState::CONTROL),
            Some(Action::ToggleSidebar)
        );
    }

    #[test]
    fn 横取りしないキーは_none_になる() {
        // Ctrl+C は子プロセスへ渡す。
        assert_eq!(action_for(&ch("c"), ModifiersState::CONTROL), None);
        assert_eq!(action_for(&ch("a"), NONE), None);
        assert_eq!(action_for(&named(NamedKey::Enter), NONE), None);
    }
}

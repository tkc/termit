//! キー入力を PTY へ送るバイト列へ変換する。
//!
//! 端末が横取りするのは第 11 節の一覧だけで、それ以外はすべて子プロセスへ渡す。

use alacritty_terminal::term::TermMode;
use winit::keyboard::{Key, KeyCode, ModifiersState, NamedKey, PhysicalKey};

/// 端末自身が処理する操作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    NewSession,
    Fork,
    ForkWithProfile,
    SelectNext,
    SelectPrev,
    /// 左ペインの n 番目へ直接切り替える。
    SelectIndex(usize),
    CloseSession,
    SearchHistory,
    ToggleSidebar,
    Copy,
    Paste,
    /// 画面とスクロールバックを消し、プロンプトを出し直す。
    ClearScreen,
    /// 画面とスクロールバックの中を探す。
    FindInScreen,
    /// セッションに名前を付ける。
    RenameSession,
    FontBigger,
    FontSmaller,
    ScrollUp,
    ScrollDown,
}

/// 横取りする組み合わせかを判定する。
///
/// 渡すのは修飾を外したキーである。macOS では Ctrl を押した時点で
/// `logical_key` が制御文字になることがあり、文字で照合できない。
///
/// 割り当ては二系統ある。
/// Ctrl 側はこの端末の操作で、シェルとエージェントから 6 個だけ奪う。
/// Cmd 側は macOS の作法に合わせたもので、シェルもエージェントも
/// Cmd を使わないため何も奪わない。
pub fn action_for(key: &Key, physical: PhysicalKey, mods: ModifiersState) -> Option<Action> {
    if let Some(a) = action_from_char(key, mods) {
        return Some(a);
    }
    // 配列によっては文字が取れない。物理キーでも照合する。
    action_from_physical(physical, mods)
}

/// 10 番目から先のセッションに割り当てる文字と、その物理キー。
///
/// ⌘ と組み合わせる文字は、すでに大半がこの端末の命令で埋まっている。
/// 空いているものだけを並べるため、A B C とは続かない。
/// H M Q は macOS 自身が隠す・仕舞う・終わるに使うので取らない。
const INDEX_KEYS: &[(char, KeyCode)] = &[
    ('a', KeyCode::KeyA),
    ('g', KeyCode::KeyG),
    ('j', KeyCode::KeyJ),
    ('l', KeyCode::KeyL),
    ('o', KeyCode::KeyO),
    ('p', KeyCode::KeyP),
    ('s', KeyCode::KeyS),
    ('t', KeyCode::KeyT),
    ('u', KeyCode::KeyU),
    ('x', KeyCode::KeyX),
    ('y', KeyCode::KeyY),
    ('z', KeyCode::KeyZ),
];

/// 番号と文字で直接切り替えられるセッションの数。これを超えた行に印は付かない。
#[cfg(test)]
const INDEXABLE: usize = 9 + INDEX_KEYS.len();

/// 左ペインの右端に出す ⌘ の印。割り当てがなければ `None`。
///
/// 描く側と受ける側で表がずれないよう、どちらもここを通す。
pub fn index_label(n: usize) -> Option<String> {
    if n < 9 {
        return Some(format!("⌘{}", n + 1));
    }
    let (c, _) = INDEX_KEYS.get(n - 9)?;
    Some(format!("⌘{}", c.to_ascii_uppercase()))
}

/// この端末が奪う Ctrl の組み合わせ。
pub const STOLEN_CTRL_KEYS: &[(&str, &str)] = &[
    ("^O", "new session"),
    ("^\\", "fork"),
    ("^]", "fork with a chosen profile"),
    ("^^", "select next session"),
    ("^B", "toggle the left pane"),
    ("^R", "search command history"),
];

fn action_from_char(key: &Key, mods: ModifiersState) -> Option<Action> {
    let Key::Character(c) = key else {
        return match key {
            Key::Named(NamedKey::PageUp) if mods.shift_key() => Some(Action::ScrollUp),
            Key::Named(NamedKey::PageDown) if mods.shift_key() => Some(Action::ScrollDown),
            _ => None,
        };
    };
    let c = c.to_lowercase();
    // Ctrl 側。Shift との同時押しが端末まで届かない環境があるため、
    // Shift を要求する組み合わせは作らない。
    if mods.control_key() && !mods.super_key() && !mods.alt_key() {
        return match c.as_str() {
            "o" => Some(Action::NewSession),
            "\\" => Some(Action::Fork),
            "]" => Some(Action::ForkWithProfile),
            "^" => Some(Action::SelectNext),
            "b" => Some(Action::ToggleSidebar),
            "r" => Some(Action::SearchHistory),
            _ => None,
        };
    }
    // Cmd 側。Shift を併用する組み合わせは ⌘⇧R だけに限る。
    if mods.super_key() && !mods.control_key() && !mods.alt_key() {
        if mods.shift_key() {
            return (c.as_str() == "r").then_some(Action::RenameSession);
        }
        return match c.as_str() {
            "n" => Some(Action::NewSession),
            "d" => Some(Action::Fork),
            "e" => Some(Action::ForkWithProfile),
            "w" => Some(Action::CloseSession),
            "r" => Some(Action::SearchHistory),
            "b" => Some(Action::ToggleSidebar),
            "c" => Some(Action::Copy),
            "v" => Some(Action::Paste),
            "k" => Some(Action::ClearScreen),
            "f" => Some(Action::FindInScreen),
            // Shift を併用する組み合わせが届かない環境があるため、
            // Shift の要らない ⌘I も受ける。
            "i" => Some(Action::RenameSession),
            "[" => Some(Action::SelectPrev),
            "]" => Some(Action::SelectNext),
            "=" | "+" => Some(Action::FontBigger),
            "-" | "_" => Some(Action::FontSmaller),
            // 命令の割り当てを先に見ているので、ここへ来るのは余った文字だけ。
            d if d.len() == 1 => {
                let c = d.chars().next()?;
                if let Some(n) = c.to_digit(10) {
                    return (n >= 1).then(|| Action::SelectIndex(n as usize - 1));
                }
                INDEX_KEYS
                    .iter()
                    .position(|&(l, _)| l == c)
                    .map(|i| Action::SelectIndex(9 + i))
            }
            _ => None,
        };
    }
    None
}

fn action_from_physical(physical: PhysicalKey, mods: ModifiersState) -> Option<Action> {
    let PhysicalKey::Code(code) = physical else {
        return None;
    };
    if mods.control_key() && !mods.super_key() && !mods.alt_key() {
        return match code {
            KeyCode::KeyO => Some(Action::NewSession),
            KeyCode::Backslash => Some(Action::Fork),
            KeyCode::BracketRight => Some(Action::ForkWithProfile),
            KeyCode::KeyB => Some(Action::ToggleSidebar),
            KeyCode::KeyR => Some(Action::SearchHistory),
            _ => None,
        };
    }
    if mods.super_key() && !mods.control_key() && !mods.alt_key() {
        let command = match code {
            KeyCode::KeyN => Some(Action::NewSession),
            KeyCode::KeyD => Some(Action::Fork),
            KeyCode::KeyE => Some(Action::ForkWithProfile),
            KeyCode::KeyW => Some(Action::CloseSession),
            KeyCode::KeyR => Some(Action::SearchHistory),
            KeyCode::KeyB => Some(Action::ToggleSidebar),
            KeyCode::KeyC => Some(Action::Copy),
            KeyCode::KeyV => Some(Action::Paste),
            KeyCode::KeyK => Some(Action::ClearScreen),
            KeyCode::KeyF => Some(Action::FindInScreen),
            KeyCode::KeyI => Some(Action::RenameSession),
            KeyCode::BracketLeft => Some(Action::SelectPrev),
            KeyCode::BracketRight => Some(Action::SelectNext),
            KeyCode::Equal => Some(Action::FontBigger),
            KeyCode::Minus => Some(Action::FontSmaller),
            _ => None,
        };
        if command.is_some() {
            return command;
        }
        return INDEX_KEYS
            .iter()
            .position(|&(_, k)| k == code)
            .map(|i| Action::SelectIndex(9 + i));
    }
    None
}

/// キーを PTY へ送るバイト列へ変換する。送るものがなければ `None`。
///
/// `base` は修飾を外したキーである。Ctrl と組み合わせたとき、
/// `logical_key` が制御文字そのものを返す環境があるため、
/// 制御文字への変換はこちらを使う。
pub fn encode(
    key: &Key,
    base: &Key,
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
                let source = match base {
                    Key::Character(b) => b.as_str(),
                    _ => c.as_str(),
                };
                vec![control_byte(source)?]
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
            encode(&ch("a"), &ch("a"), Some("a"), NONE, TermMode::empty()).unwrap(),
            b"a"
        );
    }

    #[test]
    fn 日本語の確定文字列を送る() {
        assert_eq!(
            encode(&ch("あ"), &ch("あ"), Some("あ"), NONE, TermMode::empty()).unwrap(),
            "あ".as_bytes()
        );
    }

    #[test]
    fn ctrl_と英字を制御文字へ落とす() {
        let m = ModifiersState::CONTROL;
        assert_eq!(
            encode(&ch("\u{3}"), &ch("c"), None, m, TermMode::empty()).unwrap(),
            vec![0x03]
        );
        assert_eq!(
            encode(&ch("\u{4}"), &ch("d"), None, m, TermMode::empty()).unwrap(),
            vec![0x04]
        );
        assert_eq!(
            encode(&ch("\u{1}"), &ch("a"), None, m, TermMode::empty()).unwrap(),
            vec![0x01]
        );
    }

    #[test]
    fn ctrl_と記号を制御文字へ落とす() {
        let m = ModifiersState::CONTROL;
        assert_eq!(
            encode(&ch("\u{1b}"), &ch("["), None, m, TermMode::empty()).unwrap(),
            vec![0x1b]
        );
        assert_eq!(
            encode(&ch("\0"), &ch(" "), None, m, TermMode::empty()).unwrap(),
            vec![0x00]
        );
    }

    #[test]
    fn alt_は_esc_を前置する() {
        let m = ModifiersState::ALT;
        assert_eq!(
            encode(&ch("b"), &ch("b"), Some("b"), m, TermMode::empty()).unwrap(),
            vec![0x1b, b'b']
        );
    }

    #[test]
    fn backspace_は_0x7f_を送る() {
        assert_eq!(
            encode(
                &named(NamedKey::Backspace),
                &named(NamedKey::Backspace),
                None,
                NONE,
                TermMode::empty()
            )
            .unwrap(),
            vec![0x7f]
        );
    }

    #[test]
    fn delete_は_csi_3_チルダを送る() {
        assert_eq!(
            encode(
                &named(NamedKey::Delete),
                &named(NamedKey::Delete),
                None,
                NONE,
                TermMode::empty()
            )
            .unwrap(),
            b"\x1b[3~"
        );
    }

    #[test]
    fn 矢印はカーソルモードで形が変わる() {
        assert_eq!(
            encode(
                &named(NamedKey::ArrowUp),
                &named(NamedKey::ArrowUp),
                None,
                NONE,
                TermMode::empty()
            )
            .unwrap(),
            b"\x1b[A"
        );
        assert_eq!(
            encode(
                &named(NamedKey::ArrowUp),
                &named(NamedKey::ArrowUp),
                None,
                NONE,
                TermMode::APP_CURSOR
            )
            .unwrap(),
            b"\x1bOA"
        );
    }

    #[test]
    fn shift_tab_は逆タブを送る() {
        assert_eq!(
            encode(
                &named(NamedKey::Tab),
                &named(NamedKey::Tab),
                None,
                ModifiersState::SHIFT,
                TermMode::empty()
            )
            .unwrap(),
            b"\x1b[Z"
        );
    }

    #[test]
    fn ctrl_で論理キーが制御文字になっても照合できる() {
        // macOS では Ctrl+C の logical_key が "\u{3}" になることがある。
        // 修飾を外したキーで照合するため、動作の判定は影響を受けない。
        assert_eq!(
            action_for(
                &ch("o"),
                PhysicalKey::Code(KeyCode::F35),
                ModifiersState::CONTROL
            ),
            Some(Action::NewSession)
        );
        assert_eq!(
            encode(
                &ch("\u{3}"),
                &ch("c"),
                None,
                ModifiersState::CONTROL,
                TermMode::empty()
            )
            .unwrap(),
            vec![0x03]
        );
    }

    #[test]
    fn 端末が横取りする組み合わせを判定する() {
        let ctrl = ModifiersState::CONTROL;
        let phys = PhysicalKey::Code(KeyCode::F35);
        assert_eq!(action_for(&ch("o"), phys, ctrl), Some(Action::NewSession));
        assert_eq!(action_for(&ch("\\"), phys, ctrl), Some(Action::Fork));
        assert_eq!(
            action_for(&ch("]"), phys, ctrl),
            Some(Action::ForkWithProfile)
        );
        assert_eq!(action_for(&ch("^"), phys, ctrl), Some(Action::SelectNext));
        assert_eq!(
            action_for(&ch("b"), phys, ctrl),
            Some(Action::ToggleSidebar)
        );
        assert_eq!(
            action_for(&ch("r"), phys, ctrl),
            Some(Action::SearchHistory)
        );
        // Cmd 側は macOS の作法に合わせる。何も奪わない。
        let cmd = ModifiersState::SUPER;
        assert_eq!(action_for(&ch("n"), phys, cmd), Some(Action::NewSession));
        assert_eq!(action_for(&ch("c"), phys, cmd), Some(Action::Copy));
        assert_eq!(action_for(&ch("k"), phys, cmd), Some(Action::ClearScreen));
        assert_eq!(action_for(&ch("f"), phys, cmd), Some(Action::FindInScreen));
        assert_eq!(action_for(&ch("["), phys, cmd), Some(Action::SelectPrev));
    }

    #[test]
    fn 物理キーでも照合できる() {
        // 文字が取れない配列でも、物理キーの位置で組み合わせが届く。
        let dead = Key::Dead(None);
        assert_eq!(
            action_for(
                &dead,
                PhysicalKey::Code(KeyCode::KeyO),
                ModifiersState::CONTROL
            ),
            Some(Action::NewSession)
        );
        assert_eq!(
            action_for(
                &dead,
                PhysicalKey::Code(KeyCode::Backslash),
                ModifiersState::CONTROL
            ),
            Some(Action::Fork)
        );
        assert_eq!(
            action_for(
                &dead,
                PhysicalKey::Code(KeyCode::KeyD),
                ModifiersState::SUPER
            ),
            Some(Action::Fork)
        );
    }

    #[test]
    fn ctrl_の割り当ては六個だけである() {
        let ctrl = ModifiersState::CONTROL;
        let phys = PhysicalKey::Code(KeyCode::F35);
        let mut found = Vec::new();
        for c in [
            "a", "b", "c", "d", "e", "f", "g", "h", "i", "j", "k", "l", "m", "n", "o", "p", "q",
            "r", "s", "t", "u", "v", "w", "x", "y", "z", "\\", "]", "[", "^", "-", "=", ";", ",",
            ".", "/",
        ] {
            if action_for(&ch(c), phys, ctrl).is_some() {
                found.push(c);
            }
        }
        found.sort_unstable();
        assert_eq!(found, vec!["\\", "]", "^", "b", "o", "r"]);
    }

    #[test]
    fn shift_を要求する組み合わせは作らない() {
        // Ctrl と Shift の同時押しが端末へ届かない環境があるため、
        // Shift を足すと一致しなくなる組み合わせを残さない。
        let cs = ModifiersState::CONTROL | ModifiersState::SHIFT;
        let phys = PhysicalKey::Code(KeyCode::F35);
        for c in ["o", "b", "r", "]", "^", "\\"] {
            assert_eq!(
                action_for(&ch(c), phys, ModifiersState::CONTROL),
                action_for(&ch(c), phys, cs),
                "{c} は Shift の有無で結果が変わらない"
            );
        }
    }

    #[test]
    fn 画面消去は_cmd_側だけに置く() {
        // Ctrl+K はシェルが「行末まで削除」に使う。奪わない。
        let phys = PhysicalKey::Code(KeyCode::F35);
        assert_eq!(action_for(&ch("k"), phys, ModifiersState::CONTROL), None);
        assert_eq!(
            action_for(&ch("k"), phys, ModifiersState::SUPER),
            Some(Action::ClearScreen)
        );
    }

    #[test]
    fn cmd_で番号のセッションへ切り替える() {
        let phys = PhysicalKey::Code(KeyCode::F35);
        assert_eq!(
            action_for(&ch("1"), phys, ModifiersState::SUPER),
            Some(Action::SelectIndex(0))
        );
        assert_eq!(
            action_for(&ch("9"), phys, ModifiersState::SUPER),
            Some(Action::SelectIndex(8))
        );
        assert_eq!(action_for(&ch("0"), phys, ModifiersState::SUPER), None);
    }

    #[test]
    fn 十番目から先は文字で切り替える() {
        let phys = PhysicalKey::Code(KeyCode::F35);
        assert_eq!(index_label(9).as_deref(), Some("⌘A"));
        assert_eq!(
            action_for(&ch("a"), phys, ModifiersState::SUPER),
            Some(Action::SelectIndex(9))
        );
        // 大文字で届いても同じところへ行く。
        assert_eq!(
            action_for(&ch("A"), phys, ModifiersState::SUPER),
            Some(Action::SelectIndex(9))
        );
    }

    #[test]
    fn 出す印と受けるキーが一致する() {
        // 左ペインに出した印を押せば、その行が選ばれる。
        let phys = PhysicalKey::Code(KeyCode::F35);
        for n in 0..INDEXABLE {
            let label = index_label(n).expect("{n} 番目には印がある");
            let key: String = label.chars().skip(1).collect();
            assert_eq!(
                action_for(&ch(&key), phys, ModifiersState::SUPER),
                Some(Action::SelectIndex(n)),
                "{label} は {n} 番目を選ぶ"
            );
        }
        assert_eq!(index_label(INDEXABLE), None);
    }

    #[test]
    fn 切り替えの文字は命令とぶつからない() {
        // 命令の割り当てが先にあるため、ぶつかると行へ行けなくなる。
        let phys = PhysicalKey::Code(KeyCode::F35);
        for (c, code) in INDEX_KEYS {
            let by_char = action_for(&ch(&c.to_string()), phys, ModifiersState::SUPER);
            assert!(
                matches!(by_char, Some(Action::SelectIndex(_))),
                "⌘{c} が命令に取られている: {by_char:?}"
            );
            let by_code = action_for(
                &Key::Dead(None),
                PhysicalKey::Code(*code),
                ModifiersState::SUPER,
            );
            assert_eq!(by_char, by_code, "⌘{c} は文字でも物理キーでも同じ");
        }
    }

    #[test]
    fn 命令の割り当てが切り替えに奪われない() {
        let phys = PhysicalKey::Code(KeyCode::F35);
        for (c, want) in [
            ("b", Action::ToggleSidebar),
            ("c", Action::Copy),
            ("d", Action::Fork),
            ("e", Action::ForkWithProfile),
            ("f", Action::FindInScreen),
            ("i", Action::RenameSession),
            ("k", Action::ClearScreen),
            ("n", Action::NewSession),
            ("r", Action::SearchHistory),
            ("v", Action::Paste),
            ("w", Action::CloseSession),
        ] {
            assert_eq!(
                action_for(&ch(c), phys, ModifiersState::SUPER),
                Some(want),
                "⌘{c} は命令のまま"
            );
        }
    }

    #[test]
    fn macos_が使う組み合わせは取らない() {
        // ⌘H は隠す、⌘M は仕舞う、⌘Q は終わる。横取りすると戸惑う。
        let phys = PhysicalKey::Code(KeyCode::F35);
        for c in ["h", "m", "q"] {
            assert_eq!(action_for(&ch(c), phys, ModifiersState::SUPER), None);
        }
    }

    #[test]
    fn 横取りしないキーは_none_になる() {
        // Ctrl+C も Ctrl+D も子プロセスへ渡す。
        let phys = PhysicalKey::Code(KeyCode::F35);
        for c in ["a", "c", "d", "e", "k", "l", "n", "p", "u", "w"] {
            assert_eq!(
                action_for(&ch(c), phys, ModifiersState::CONTROL),
                None,
                "Ctrl+{c} は子プロセスへ渡す"
            );
        }
        assert_eq!(action_for(&ch("a"), phys, NONE), None);
        assert_eq!(action_for(&named(NamedKey::Enter), phys, NONE), None);
    }
}

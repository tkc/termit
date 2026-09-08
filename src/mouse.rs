//! マウスの出来事を、端末上のプログラムへ送る形に変換する。
//!
//! vim や htop、エージェントの全画面 UI は、マウスの報告を要求してくる。
//! 実測すると Claude Code は起動時に `?1000` `?1002` `?1003` `?1006` を設定する。
//! 要求されているあいだは、端末が選択に使うのではなく、そのまま渡す。

use alacritty_terminal::term::TermMode;
use winit::keyboard::ModifiersState;

/// 押された釦。報告の番号に対応する。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Button {
    Left = 0,
    Middle = 1,
    Right = 2,
}

/// 何が起きたか。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Press,
    Release,
    /// 釦を押していない移動。
    Move,
    /// 釦を押したままの移動。
    Drag,
    WheelUp,
    WheelDown,
}

/// 端末上のプログラムがマウスを要求しているか。
pub fn wants_mouse(mode: TermMode) -> bool {
    mode.intersects(TermMode::MOUSE_MODE)
}

/// 出来事を報告の列に変換する。送るものがなければ `None`。
///
/// `col` と `row` は端末の中での 0 起点の位置である。
pub fn encode(
    kind: Kind,
    button: Button,
    col: usize,
    row: usize,
    mods: ModifiersState,
    mode: TermMode,
) -> Option<Vec<u8>> {
    if !wants_mouse(mode) {
        return None;
    }
    // 移動の報告は、要求された種類のときだけ送る。
    match kind {
        Kind::Move if !mode.contains(TermMode::MOUSE_MOTION) => return None,
        Kind::Drag if !mode.intersects(TermMode::MOUSE_DRAG | TermMode::MOUSE_MOTION) => {
            return None
        }
        _ => {}
    }

    let mut code = match kind {
        Kind::WheelUp => 64,
        Kind::WheelDown => 65,
        _ => button as u8,
    };
    if matches!(kind, Kind::Move | Kind::Drag) {
        code += 32;
    }
    if mods.shift_key() {
        code += 4;
    }
    if mods.alt_key() {
        code += 8;
    }
    if mods.control_key() {
        code += 16;
    }

    if mode.contains(TermMode::SGR_MOUSE) {
        let end = if kind == Kind::Release { 'm' } else { 'M' };
        return Some(format!("\x1b[<{};{};{}{}", code, col + 1, row + 1, end).into_bytes());
    }

    // 旧来の形。位置は 1 バイトで表すので 223 桁までしか送れない。
    if col + 1 > 223 || row + 1 > 223 {
        return None;
    }
    let code = if kind == Kind::Release { 3 } else { code };
    Some(vec![
        0x1b,
        b'[',
        b'M',
        32 + code,
        32 + (col + 1) as u8,
        32 + (row + 1) as u8,
    ])
}

/// 代替画面での車輪の扱い。
///
/// マウスを要求していない全画面プログラム（`less` や `man` など）は、
/// 車輪を回したときに矢印キーが来ることを期待している。
/// これがないと、代替画面ではまったくスクロールできない。
pub fn alternate_scroll(lines: i32, mode: TermMode) -> Option<Vec<u8>> {
    if wants_mouse(mode)
        || !mode.contains(TermMode::ALT_SCREEN)
        || !mode.contains(TermMode::ALTERNATE_SCROLL)
        || lines == 0
    {
        return None;
    }
    let up = lines > 0;
    let seq: &[u8] = match (up, mode.contains(TermMode::APP_CURSOR)) {
        (true, false) => b"\x1b[A",
        (false, false) => b"\x1b[B",
        (true, true) => b"\x1bOA",
        (false, true) => b"\x1bOB",
    };
    let mut out = Vec::with_capacity(seq.len() * lines.unsigned_abs() as usize);
    for _ in 0..lines.abs() {
        out.extend_from_slice(seq);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NONE: ModifiersState = ModifiersState::empty();
    fn sgr() -> TermMode {
        TermMode::MOUSE_REPORT_CLICK | TermMode::SGR_MOUSE
    }

    #[test]
    fn 要求されていなければ何も送らない() {
        assert_eq!(
            encode(Kind::Press, Button::Left, 0, 0, NONE, TermMode::empty()),
            None
        );
    }

    #[test]
    fn sgr_の形で押下と解放を送る() {
        assert_eq!(
            encode(Kind::Press, Button::Left, 4, 9, NONE, sgr()).unwrap(),
            b"\x1b[<0;5;10M"
        );
        assert_eq!(
            encode(Kind::Release, Button::Left, 4, 9, NONE, sgr()).unwrap(),
            b"\x1b[<0;5;10m"
        );
    }

    #[test]
    fn 釦の種類を番号で分ける() {
        assert_eq!(
            encode(Kind::Press, Button::Middle, 0, 0, NONE, sgr()).unwrap(),
            b"\x1b[<1;1;1M"
        );
        assert_eq!(
            encode(Kind::Press, Button::Right, 0, 0, NONE, sgr()).unwrap(),
            b"\x1b[<2;1;1M"
        );
    }

    #[test]
    fn 修飾キーを番号に足す() {
        let m = ModifiersState::CONTROL | ModifiersState::SHIFT;
        // 左釦 0 + Shift 4 + Ctrl 16 = 20
        assert_eq!(
            encode(Kind::Press, Button::Left, 0, 0, m, sgr()).unwrap(),
            b"\x1b[<20;1;1M"
        );
    }

    #[test]
    fn 車輪は六十四番から始まる() {
        assert_eq!(
            encode(Kind::WheelUp, Button::Left, 2, 3, NONE, sgr()).unwrap(),
            b"\x1b[<64;3;4M"
        );
        assert_eq!(
            encode(Kind::WheelDown, Button::Left, 2, 3, NONE, sgr()).unwrap(),
            b"\x1b[<65;3;4M"
        );
    }

    #[test]
    fn 移動は要求された種類のときだけ送る() {
        // クリックだけを要求している場合、移動は送らない。
        assert_eq!(encode(Kind::Move, Button::Left, 0, 0, NONE, sgr()), None);
        assert_eq!(encode(Kind::Drag, Button::Left, 0, 0, NONE, sgr()), None);

        // ドラッグを要求していれば、押したままの移動だけ送る。
        let drag = sgr() | TermMode::MOUSE_DRAG;
        assert_eq!(encode(Kind::Move, Button::Left, 0, 0, NONE, drag), None);
        assert_eq!(
            encode(Kind::Drag, Button::Left, 0, 0, NONE, drag).unwrap(),
            b"\x1b[<32;1;1M"
        );

        // 全移動を要求していれば、押していない移動も送る。
        let motion = sgr() | TermMode::MOUSE_MOTION;
        assert_eq!(
            encode(Kind::Move, Button::Left, 0, 0, NONE, motion).unwrap(),
            b"\x1b[<32;1;1M"
        );
    }

    #[test]
    fn sgr_でなければ旧来の形で送る() {
        let mode = TermMode::MOUSE_REPORT_CLICK;
        assert_eq!(
            encode(Kind::Press, Button::Left, 0, 0, NONE, mode).unwrap(),
            vec![0x1b, b'[', b'M', 32, 33, 33]
        );
        // 旧来の形では解放の種類を区別せず、3 番で送る。
        assert_eq!(
            encode(Kind::Release, Button::Left, 0, 0, NONE, mode).unwrap(),
            vec![0x1b, b'[', b'M', 35, 33, 33]
        );
    }

    #[test]
    fn 旧来の形は二百二十三桁までしか送れない() {
        let mode = TermMode::MOUSE_REPORT_CLICK;
        assert_eq!(encode(Kind::Press, Button::Left, 300, 0, NONE, mode), None);
        // SGR なら桁数の制限はない。
        assert!(encode(Kind::Press, Button::Left, 300, 0, NONE, sgr()).is_some());
    }

    #[test]
    fn 代替画面の車輪を矢印に変える() {
        let alt = TermMode::ALT_SCREEN | TermMode::ALTERNATE_SCROLL;
        assert_eq!(alternate_scroll(2, alt).unwrap(), b"\x1b[A\x1b[A");
        assert_eq!(alternate_scroll(-1, alt).unwrap(), b"\x1b[B");
        assert_eq!(
            alternate_scroll(1, alt | TermMode::APP_CURSOR).unwrap(),
            b"\x1bOA"
        );
    }

    #[test]
    fn 代替画面でなければ矢印にしない() {
        assert_eq!(alternate_scroll(1, TermMode::ALTERNATE_SCROLL), None);
    }

    #[test]
    fn マウスを要求していれば矢印にしない() {
        let alt = TermMode::ALT_SCREEN | TermMode::ALTERNATE_SCROLL | TermMode::MOUSE_REPORT_CLICK;
        assert_eq!(alternate_scroll(1, alt), None);
    }
}

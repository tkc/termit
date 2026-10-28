//! 行の通し番号。
//!
//! 行から時刻を引くための土台である。`Grid::history_size()` はスクロールバックの
//! 上限で頭打ちになるので、1 時間前の行と今の行が同じ番号になってしまう。そこで
//! `Term` へ渡す前に `Handler` を包み、履歴へ押し出した行を自分で数える。
//!
//! 画面上の行位置 `line` は、新しい行が来るたびに 1 ずつ小さくなる。押し出した
//! 行数 `pushed` を足せば、その行に一度しか付かない番号になる（`serial`）。
//! 逆に、覚えておいた番号からいまの行位置も出せる（`line_of`）。
//!
//! 番号を引く側（`serial`・`line_of`・`Ticks::time_of`）の読み手は左ペインの検索で、
//! まだ入っていない。数える側だけを先に置いている。

use std::collections::VecDeque;
use std::time::Instant;

use alacritty_terminal::grid::{Dimensions, GridCell};
use alacritty_terminal::index::Line;
use alacritty_terminal::term::TermMode;
use alacritty_terminal::vte::ansi::cursor_icon::CursorIcon;
use alacritty_terminal::vte::ansi::{
    Attr, CharsetIndex, ClearMode, CursorShape, CursorStyle, Handler, Hyperlink, KeyboardModes,
    KeyboardModesApplyBehavior, LineClearMode, Mode, ModifyOtherKeys, PrivateMode, Rgb,
    ScpCharPath, ScpUpdateMode, StandardCharset, TabulationClearMode,
};
use alacritty_terminal::Term;
use unicode_width::UnicodeWidthChar;

use crate::term::EventProxy;

/// 行位置 `line` の通し番号。画面の最上段がちょうど `pushed` になる。
#[allow(dead_code)]
pub fn serial(pushed: u64, line: i32) -> i64 {
    pushed as i64 + i64::from(line)
}

/// 通し番号 `serial` がいまどの行位置にあるか。履歴から押し出されていれば `None`。
#[allow(dead_code)]
pub fn line_of(serial: i64, pushed: u64, history_size: usize) -> Option<i32> {
    let line = serial - pushed as i64;
    if line < -(history_size as i64) {
        return None;
    }
    i32::try_from(line).ok()
}

/// 覚えておく目盛りの数。1 読みにつき 1 つ積むので、約 30 秒から数分ぶんになる。
pub const TICKS: usize = 4096;

/// 通し番号と時刻の対応。古いものから捨てる環である。
#[derive(Debug, Default)]
pub struct Ticks {
    /// `(そのときまでに押し出した行数, 時刻)`。行数の昇順に並ぶ。
    marks: VecDeque<(u64, Instant)>,
}

impl Ticks {
    pub fn new() -> Self {
        Self::default()
    }

    /// いまの行数と時刻を積む。行数が増えていなければ何もしない。
    pub fn push(&mut self, pushed: u64, at: Instant) {
        if self.marks.back().is_some_and(|&(last, _)| last >= pushed) {
            return;
        }
        if self.marks.len() == TICKS {
            self.marks.pop_front();
        }
        self.marks.push_back((pushed, at));
    }

    /// 通し番号 `serial` の行が現れた時刻。
    ///
    /// 目盛りを捨てたぶんの時刻は分からない。残っている最古の時刻を返す。
    /// その行がそれより古いことは確かである。
    #[allow(dead_code)]
    pub fn time_of(&self, serial: i64) -> Option<Instant> {
        let after = self
            .marks
            .partition_point(|&(pushed, _)| (pushed as i64) <= serial);
        self.marks.get(after.saturating_sub(1)).map(|&(_, at)| at)
    }
}

/// 履歴へ押し出した行を数えながら `Term` へ中継する。
///
/// `Processor::advance` は `Handler` を実装するものを何でも受け取るので、
/// `Term` の代わりにこれを渡せば、通った escape sequence を数えられる。
///
/// 数えるのは、`Term` の中で履歴へ行が積まれる道すべてである。`alacritty_terminal`
/// で履歴が伸びるのは `Grid::scroll_up` を画面の先頭から呼んだときだけで、そこへ
/// 届くのは改行・NEL・SU・行削除・画面消去（ED 2）と、行末の折り返しである。
/// 折り返しと NEL は `Term` が自分の `linefeed` を呼ぶので、こちらの `linefeed`
/// は通らない。書き込みの側から数えないと取りこぼす。
///
/// 上限に達していないあいだは履歴行数の差がそのまま答えなので、どの道を通っても
/// 正確である。上限に達してからは見積もりになり、DECSTBM で画面の一部だけを
/// スクロール範囲にしている本画面では、改行を数え違えることがある。
/// また、`Term::resize` の折り返し直しは行の位置を動かすので、
/// 大きさを変えると古い通し番号は少しずれる。
pub struct Counting<'a> {
    inner: &'a mut Term<EventProxy>,
    pushed: u64,
    /// スクロールバックの上限（行）。履歴行数の差が頭打ちになったことを知るのに使う。
    cap: usize,
}

impl<'a> Counting<'a> {
    pub fn new(inner: &'a mut Term<EventProxy>, pushed: u64, cap: usize) -> Self {
        Self { inner, pushed, cap }
    }

    /// これまでに履歴へ押し出した行数。
    pub fn pushed(&self) -> u64 {
        self.pushed
    }

    /// 押し出しの起きうる呼び出しを、数えながら行う。
    ///
    /// 上限に届くまでは履歴行数の差が正確なので、差をそのまま使う。上限に達すると
    /// 差が出なくなるので、そのときだけ `guess` 行押し出したものとして数える。
    /// 上限をまたぐ 1 回では差が足りないことがあるので、多いほうを採る。
    fn counted(&mut self, guess: usize, call: impl FnOnce(&mut Term<EventProxy>)) {
        // 代替画面に履歴は無い。全画面の UI が流す行は数えない。
        if self.alt() {
            call(self.inner);
            return;
        }
        let before = self.history();
        call(self.inner);
        let after = self.history();
        let diff = after.saturating_sub(before);
        let counted = if after >= self.cap {
            diff.max(guess)
        } else {
            diff
        };
        self.pushed += counted as u64;
    }

    fn history(&self) -> usize {
        self.inner.grid().history_size()
    }

    fn alt(&self) -> bool {
        self.inner.mode().contains(TermMode::ALT_SCREEN)
    }

    /// 画面の最下行にいるか。そこで改行すると 1 行が押し出される。
    fn at_bottom(&self) -> bool {
        let grid = self.inner.grid();
        grid.cursor.point.line.0 as usize + 1 == grid.screen_lines()
    }

    /// いま折り返すと 1 行押し出される位置にいるか。
    fn folds(&self) -> bool {
        self.inner.mode().contains(TermMode::LINE_WRAP) && self.at_bottom()
    }

    /// 溜めてある折り返しが、次の書き込みで 1 行押し出すか。
    fn wrapping(&self) -> bool {
        self.inner.grid().cursor.input_needs_wrap && self.folds()
    }

    /// 折り返しの起きうる 1 文字を、数えながら書き込む。
    ///
    /// 行末でしか通らないので、`input` の本体から外へ出しておく。
    /// 入れたままにすると、字を流す速さが 1 割ほど落ちる（`--throughput` で確かめた）。
    #[cold]
    #[inline(never)]
    fn wrapped_input(&mut self, c: char) {
        let guess = usize::from(self.folds());
        self.counted(guess, |term| term.input(c));
    }

    /// 画面に何か書いてある行数。ED 2 が履歴へ押し上げる行数になる。
    fn filled_lines(&self) -> usize {
        let grid = self.inner.grid();
        (0..grid.screen_lines() as i32)
            .rev()
            .find(|&line| grid[Line(line)][..].iter().any(|cell| !cell.is_empty()))
            .map_or(0, |line| line as usize + 1)
    }
}

/// `Handler` の残りをそのまま `Term` へ渡す。
///
/// 1 本でも書き漏らすと、その escape sequence は黙って捨てられる。
macro_rules! forward {
    ($( fn $name:ident ( $( $arg:ident : $ty:ty ),* ); )*) => {
        $(
            #[inline]
            fn $name(&mut self $(, $arg: $ty)*) {
                self.inner.$name($($arg),*)
            }
        )*
    };
}

impl Handler for Counting<'_> {
    fn input(&mut self, c: char) {
        // 折り返すのは、行末に折り返しを溜めているときと、行末の 1 桁に入らない
        // 全角文字が来たときだけである。1 文字ごとに通る道なので、それ以外は
        // 何も読まずに通す。ASCII は必ず 1 桁なので、桁数も幅も見ない。
        let grid = self.inner.grid();
        let may_wrap = grid.cursor.input_needs_wrap
            || (!c.is_ascii()
                && grid.cursor.point.column.0 + 1 >= grid.columns()
                && c.width().is_some_and(|w| w >= 2));
        if may_wrap {
            self.wrapped_input(c);
            return;
        }
        self.inner.input(c);
    }

    fn put_tab(&mut self, count: u16) {
        // 行末のタブは折り返しになる。
        let guess = usize::from(self.wrapping());
        self.counted(guess, |term| term.put_tab(count));
    }

    fn linefeed(&mut self) {
        let guess = usize::from(self.at_bottom());
        self.counted(guess, |term| term.linefeed());
    }

    fn newline(&mut self) {
        // NEL は改行と同じ道を通る。
        let guess = usize::from(self.at_bottom());
        self.counted(guess, |term| term.newline());
    }

    fn scroll_up(&mut self, lines: usize) {
        let guess = lines.min(self.inner.grid().screen_lines());
        self.counted(guess, |term| term.scroll_up(lines));
    }

    fn delete_lines(&mut self, lines: usize) {
        // 画面の先頭で消すと、下の行が上へ送られて履歴へ出る。
        let grid = self.inner.grid();
        let guess = if grid.cursor.point.line.0 == 0 {
            lines.min(grid.screen_lines())
        } else {
            0
        };
        self.counted(guess, |term| term.delete_lines(lines));
    }

    fn clear_screen(&mut self, mode: ClearMode) {
        // ED 2 は本画面では消さずに、書いてあるぶんを履歴へ押し上げる。
        let guess = if matches!(mode, ClearMode::All) && !self.alt() {
            self.filled_lines()
        } else {
            0
        };
        self.counted(guess, |term| term.clear_screen(mode));
    }

    forward! {
        fn set_title(title: Option<String>);
        fn set_cursor_style(style: Option<CursorStyle>);
        fn set_cursor_shape(shape: CursorShape);
        fn goto(line: i32, col: usize);
        fn goto_line(line: i32);
        fn goto_col(col: usize);
        fn insert_blank(count: usize);
        fn move_up(lines: usize);
        fn move_down(lines: usize);
        fn identify_terminal(intermediate: Option<char>);
        fn device_status(arg: usize);
        fn move_forward(col: usize);
        fn move_backward(col: usize);
        fn move_down_and_cr(row: usize);
        fn move_up_and_cr(row: usize);
        fn backspace();
        fn carriage_return();
        fn bell();
        fn substitute();
        fn set_horizontal_tabstop();
        fn scroll_down(lines: usize);
        fn insert_blank_lines(lines: usize);
        fn erase_chars(count: usize);
        fn delete_chars(count: usize);
        fn move_backward_tabs(count: u16);
        fn move_forward_tabs(count: u16);
        fn save_cursor_position();
        fn restore_cursor_position();
        fn clear_line(mode: LineClearMode);
        fn clear_tabs(mode: TabulationClearMode);
        fn set_tabs(interval: u16);
        fn reset_state();
        fn reverse_index();
        fn terminal_attribute(attr: Attr);
        fn set_mode(mode: Mode);
        fn unset_mode(mode: Mode);
        fn report_mode(mode: Mode);
        fn set_private_mode(mode: PrivateMode);
        fn unset_private_mode(mode: PrivateMode);
        fn report_private_mode(mode: PrivateMode);
        fn set_scrolling_region(top: usize, bottom: Option<usize>);
        fn set_keypad_application_mode();
        fn unset_keypad_application_mode();
        fn set_active_charset(index: CharsetIndex);
        fn configure_charset(index: CharsetIndex, charset: StandardCharset);
        fn set_color(index: usize, color: Rgb);
        fn dynamic_color_sequence(prefix: String, index: usize, terminator: &str);
        fn reset_color(index: usize);
        fn clipboard_store(kind: u8, bytes: &[u8]);
        fn clipboard_load(kind: u8, terminator: &str);
        fn decaln();
        fn push_title();
        fn pop_title();
        fn text_area_size_pixels();
        fn text_area_size_chars();
        fn set_hyperlink(link: Option<Hyperlink>);
        fn set_mouse_cursor_icon(icon: CursorIcon);
        fn report_keyboard_mode();
        fn push_keyboard_mode(mode: KeyboardModes);
        fn pop_keyboard_modes(to_pop: u16);
        fn set_keyboard_mode(mode: KeyboardModes, behavior: KeyboardModesApplyBehavior);
        fn set_modify_other_keys(mode: ModifyOtherKeys);
        fn report_modify_other_keys();
        fn set_scp(char_path: ScpCharPath, update_mode: ScpUpdateMode);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::term::{TermSize, UiSender};
    use alacritty_terminal::event::WindowSize;
    use alacritty_terminal::sync::FairMutex;
    use alacritty_terminal::vte::ansi::{Processor, StdSyncHandler};
    use std::sync::mpsc::channel;
    use std::sync::Arc;
    use std::time::Duration;

    fn make_term(cols: usize, lines: usize, scrollback: usize) -> Term<EventProxy> {
        let (pty_tx, _pty_rx) = channel();
        let (ui_tx, _ui_rx) = channel();
        let size = TermSize::new(cols, lines);
        let ws = Arc::new(FairMutex::new(WindowSize {
            num_lines: lines as u16,
            num_cols: cols as u16,
            cell_width: 8,
            cell_height: 16,
        }));
        let proxy = EventProxy::new(1, pty_tx, UiSender::Channel(ui_tx), ws);
        crate::term::new_term(size, scrollback, proxy)
    }

    /// 台本を端末へ流し、数えた行数と、端末が実際に抱えた履歴行数を返す。
    ///
    /// 上限に届くまでは履歴行数が答え合わせになる。
    fn feed(cols: usize, lines: usize, scrollback: usize, script: &[u8]) -> (u64, usize) {
        let mut term = make_term(cols, lines, scrollback);
        let mut parser: Processor<StdSyncHandler> = Processor::new();
        let mut counting = Counting::new(&mut term, 0, scrollback);
        parser.advance(&mut counting, script);
        let pushed = counting.pushed();
        (pushed, term.grid().history_size())
    }

    /// `n` 行の台本。末尾に改行は付けないので、改行は `n - 1` 個になる。
    fn plain_lines(n: usize) -> Vec<u8> {
        (1..=n)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\r\n")
            .into_bytes()
    }

    #[test]
    fn 本画面の改行で通し番号が増える() {
        // 24 行の画面に 30 行流す。改行は 29 個で、うち 23 個は画面の中を
        // 下りるだけなので、履歴へ出るのは 6 行。
        let (pushed, history) = feed(20, 24, 100, &plain_lines(30));
        assert_eq!(pushed, 6);
        assert_eq!(history, 6, "上限に届く前は端末の履歴行数と一致する");
    }

    #[test]
    fn 代替画面にいるあいだは数えない() {
        let mut script = b"\x1b[?1049h".to_vec();
        script.extend(plain_lines(30));
        script.extend(b"\x1b[?1049l");
        let (pushed, history) = feed(20, 24, 100, &script);
        assert_eq!(pushed, 0, "代替画面に履歴は無い");
        assert_eq!(history, 0);
    }

    #[test]
    fn 上限に達しても増え続ける() {
        // 4 行の画面に 40 行流す。履歴は 4 行で頭打ちになるが、
        // 押し出した行は 36 行ある。
        let (pushed, history) = feed(20, 4, 4, &plain_lines(40));
        assert_eq!(pushed, 36);
        assert_eq!(history, 4, "履歴行数は上限で止まる");
    }

    #[test]
    fn 折り返した行も数える() {
        // 10 桁の画面を最下行まで埋めてから、35 文字を流す。
        // 折り返しも改行と同じように 1 行ずつ押し出す。
        let mut script = plain_lines(4);
        script.extend(b"\r\n");
        script.extend([b'x'; 35]);
        let (pushed, history) = feed(10, 4, 100, &script);
        assert_eq!(pushed, 4, "改行 4 個のうち 1 個と、折り返し 3 回");
        assert_eq!(history, 4);
    }

    #[test]
    fn 画面消去で押し出された行も数える() {
        // 4 行を埋めてから ED 2。書いてある 4 行が履歴へ出る。
        let mut script = plain_lines(4);
        script.extend(b"\x1b[2J");
        let (pushed, history) = feed(20, 4, 100, &script);
        assert_eq!(pushed, 4);
        assert_eq!(history, 4);
    }

    #[test]
    fn 行削除で押し出された行も数える() {
        // 画面の先頭で 2 行消すと、画面の中身が 2 行ぶん上へ送られる。
        let mut script = plain_lines(4);
        script.extend(b"\x1b[H\x1b[2M");
        let (pushed, history) = feed(20, 4, 100, &script);
        assert_eq!(pushed, 2);
        assert_eq!(history, 2);
    }

    #[test]
    fn 上限に達してからも数え方は変わらない() {
        // 同じ台本を、上限の広い端末と狭い端末へ流す。広いほうは履歴行数が
        // 答え合わせになり、狭いほうは見積もりで数える。両者は一致する。
        let mut script = plain_lines(10);
        script.extend(b"\r\n");
        script.extend([b'y'; 73]); // 折り返し
        script.extend(b"\r\n\x1b[3S"); // 改行とスクロールアップ
        script.extend(b"\x1bE\x1bE"); // NEL
        script.extend(plain_lines(5));
        script.extend(b"\x1b[2J"); // 画面消去
        script.extend(plain_lines(5));

        let (wide, history) = feed(10, 4, 10_000, &script);
        assert_eq!(wide, history as u64, "上限に届かなければ履歴行数と一致する");
        let (narrow, capped) = feed(10, 4, 3, &script);
        assert_eq!(capped, 3, "狭いほうは上限で止まっている");
        assert_eq!(narrow, wide, "上限に達しても同じ数になる");
    }

    #[test]
    fn 通し番号から行位置を出す() {
        assert_eq!(serial(100, 0), 100);
        assert_eq!(serial(100, -3), 97);
        // 20 行流れたあと、通し番号 97 は 23 行上にある。
        assert_eq!(line_of(97, 120, 10000), Some(-23));
        // 履歴から押し出されていれば無い。
        assert_eq!(line_of(97, 120, 10), None);
    }

    #[test]
    fn 目盛りから時刻を引く() {
        let t0 = Instant::now();
        let t1 = t0 + Duration::from_secs(1);
        let t2 = t0 + Duration::from_secs(2);
        let mut ticks = Ticks::new();
        ticks.push(10, t0);
        ticks.push(20, t1);
        ticks.push(30, t2);
        assert_eq!(ticks.time_of(25), Some(t1));
        assert_eq!(ticks.time_of(30), Some(t2));
        assert_eq!(
            ticks.time_of(5),
            Some(t0),
            "一番古い目盛りより前なら、残っている最古の時刻を返す"
        );
        assert_eq!(Ticks::new().time_of(1), None, "目盛りが無ければ時刻も無い");
    }

    #[test]
    fn 同じ行数の目盛りは積まない() {
        let t0 = Instant::now();
        let mut ticks = Ticks::new();
        ticks.push(10, t0);
        ticks.push(10, t0 + Duration::from_secs(1));
        assert_eq!(ticks.time_of(10), Some(t0), "先に見た時刻を残す");
    }

    #[test]
    fn 目盛りは古いものから捨てる() {
        let t0 = Instant::now();
        let mut ticks = Ticks::new();
        for i in 0..(TICKS as u64 + 100) {
            ticks.push(i, t0 + Duration::from_millis(i));
        }
        assert_eq!(
            ticks.time_of(0),
            Some(t0 + Duration::from_millis(100)),
            "溢れた 100 件は捨て、残っている最古の時刻を返す"
        );
    }
}

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
//! 番号を引く側（`serial`・`line_of`・`Ticks::time_of`）の読み手は、左ペインの
//! 全セッション検索（`scan.rs`）である。

use std::collections::VecDeque;
use std::time::{Duration, Instant};

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

use crate::term::EventProxy;

/// 行位置 `line` の通し番号。画面の最上段がちょうど `pushed` になる。
pub fn serial(pushed: u64, line: i32) -> i64 {
    pushed as i64 + i64::from(line)
}

/// 通し番号 `serial` がいまどの行位置にあるか。履歴から押し出されていれば `None`。
pub fn line_of(serial: i64, pushed: u64, history_size: usize) -> Option<i32> {
    let line = serial - pushed as i64;
    if line < -(history_size as i64) {
        return None;
    }
    i32::try_from(line).ok()
}

/// 覚えておく目盛りの数。
///
/// 積むのは行を押し出した読みだけで、それも 1 秒に 1 つまでなので、
/// 68 分以上ぶんになる。プロンプトの描き直しのような押し出さない読みは
/// 枠を使わないので、静かな端末ではもっと長く覚えている。
///
/// 1 件は `(u64, Instant)` の 24 バイトで、セッションあたり約 96KB になる
/// （`size_of` で確かめた）。
pub const TICKS: usize = 4096;

/// 目盛りの間隔。仕様は 1 秒の分解能で足りるとしている。
///
/// 1 読みにつき 1 つ積むと、忙しい出力では数分で環を使い切ってしまう。
const TICK_GAP: Duration = Duration::from_secs(1);

/// 通し番号と時刻の対応。古いものから捨てる環である。
#[derive(Debug)]
pub struct Ticks {
    /// `(そのときまでに押し出した行数, 時刻)`。行数の昇順に並ぶ。
    marks: VecDeque<(u64, Instant)>,
}

impl Ticks {
    /// 目盛りを 1 つ入れた環を作る。
    ///
    /// 種を入れるのは、一度も行を押し出していない本（開いたばかり・静かな本・
    /// 流れる前に代替画面へ入った本）の行にも時刻を付けるためである。空の環では
    /// `time_of` が `None` を返し、その行が全セッション検索で時刻の無い件になって
    /// しまう。種の時刻は本を開いた時刻なので、静かな古い行を本当に新しい行より
    /// 上に出すことはない。
    ///
    /// 代わりに、最初に**押し出した**読みの目盛りが `push` の「同じ行数は積まない」で
    /// 落ちる。本を開いた数ミリ秒後に来るのはプロンプトの読みで、あれは 1 行も
    /// 押し出さないので目盛りを取らない。最初に押し出すのは画面より長い出力を出した
    /// 読みで、それは何分も後かもしれない。その読みで出た行は、本当の時刻ではなく
    /// 本を開いた時刻になる。それ以降の読みは正しく日付が付く。
    /// 古く見えるぶんには倒れるので、静かな古い行が本当に新しい行より上へ出ることはない。
    pub fn new() -> Self {
        Self {
            marks: VecDeque::from([(0, Instant::now())]),
        }
    }

    /// 目盛りを 1 つも持たない環。
    ///
    /// 環そのものの決まり（積む・捨てる・引く）を確かめる試験のためにある。
    /// `new` の種が入っていると、試験が置いた目盛りが「同じ行数は積まない」や
    /// 「1 秒おきに積む」で落ちてしまう。種の入らない道はここだけにしてある。
    #[cfg(test)]
    pub(crate) fn empty() -> Self {
        Self {
            marks: VecDeque::new(),
        }
    }

    /// 1 回の読みを目盛りに残す。`before` は読む前、`after` は読んだ後の行数。
    ///
    /// 目盛り `(p, t)` は「番号 p から次の目盛りまでの行が t に出た」という意味なので、
    /// 1 行も押し出していない読みには残すものが無い。それどころか、同じ行数の目盛りは
    /// 積まないので、次に本当に押し出した読みの枠を取ってしまう。プロンプトの
    /// 描き直しや打った字の echo がまさにそれで、`ls` の出力がその前の出力の時刻に
    /// なってしまう。
    pub fn mark_read(&mut self, before: u64, after: u64, at: Instant) {
        if after == before {
            return;
        }
        self.push(before, at);
    }

    /// いまの行数と時刻を積む。
    ///
    /// 行数が増えていないか、前の目盛りから 1 秒経っていなければ何もしない。
    /// 読み手は押し出した読みでしか呼ばないので、行数は必ず増えている。
    /// それでも見るのは、並びが昇順であることが `time_of` の前提だからである。
    fn push(&mut self, pushed: u64, at: Instant) {
        if let Some(&(last, when)) = self.marks.back() {
            if pushed <= last || at.saturating_duration_since(when) < TICK_GAP {
                return;
            }
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
    pub fn time_of(&self, serial: i64) -> Option<Instant> {
        let after = self
            .marks
            .partition_point(|&(pushed, _)| (pushed as i64) <= serial);
        self.marks.get(after.saturating_sub(1)).map(|&(_, at)| at)
    }

    /// いちばん新しい目盛りの時刻。
    ///
    /// 代替画面の行は通し番号が引けない（`pushed` が進まないので、付けた番号は
    /// 本画面の無関係な行を指す）。その行には、この本が最後に行を押し出した
    /// 時刻を代わりに付ける。
    pub fn newest(&self) -> Option<Instant> {
        self.marks.back().map(|&(_, at)| at)
    }
}

impl Default for Ticks {
    fn default() -> Self {
        Self::new()
    }
}

/// 履歴へ押し出した行を数えながら `Term` へ中継する。
///
/// `Processor::advance` は `Handler` を実装するものを何でも受け取るので、
/// `Term` の代わりにこれを渡せば、通った escape sequence を数えられる。
///
/// 数え方は 2 つあり、始めた時点で履歴が上限に達していたかで選ぶ。
///
/// 達していなければ、履歴行数の差がそのまま答えである。押し出した行が 1 行ずつ
/// 履歴を伸ばすので、どの道を通っても漏れない。履歴へ入らない送り（DECSTBM で
/// 範囲を絞った中のスクロール）を数えてしまうこともない。
/// 達していれば差が出ないので、行ごとの道で数えたほうを答えにする。行ごとに
/// 数えるのは、改行・NEL・SU・行削除・画面消去（ED 2）・行末のタブである。
///
/// 上限に達してからは、数え漏らしと数え過ぎが残る。行ごとの道は画面の形しか
/// 見ていないからである。
///
/// - 行末の折り返しは `Term` が自分の `linefeed` を呼ぶので、こちらの `linefeed`
///   を通らない。上限に届くまでは差に出るので数えられるが、上限に達してからは
///   数えられない。1 文字ごとに見れば数えられるが、それは `input` を 1 割ほど
///   遅くするので採らない（仕様 §4 の非目標）。
/// - DECSTBM で送り範囲を絞った本画面では、履歴へ入らない送りを数えてしまう。
///   `at_bottom` は画面の最下行を見るが `Term::linefeed` が見るのは
///   `scroll_region.end` で、履歴が伸びるのは `region.start == 0` のときだけである。
///   `scroll_up` は画面の高さで、`delete_lines` はカーソルが 0 行目かどうかで、
///   `folds` は範囲を見ずに数える。範囲は `Term` の外から読めないので追えない。
/// - `Term::resize` の折り返し直しは行の位置を動かすので、大きさを変えると
///   古い通し番号は少しずれる。
pub struct Counting<'a> {
    inner: &'a mut Term<EventProxy>,
    pushed: u64,
    /// 始めた時点の履歴行数。代替画面にいたなら `None`。
    before: Option<usize>,
    /// スクロールバックの上限（行）。履歴行数の差が出ない側にいるかの判定に使う。
    cap: usize,
    /// 行ごとの道で数えた行数。
    hooked: u64,
}

impl<'a> Counting<'a> {
    pub fn new(inner: &'a mut Term<EventProxy>, pushed: u64, cap: usize) -> Self {
        // 代替画面の履歴行数は本画面のものではない。差の起点にはしない。
        let before =
            (!inner.mode().contains(TermMode::ALT_SCREEN)).then(|| inner.grid().history_size());
        Self {
            inner,
            pushed,
            before,
            cap,
            hooked: 0,
        }
    }

    /// 数え終えて、押し出した行の合計を返す。
    pub fn finish(self) -> u64 {
        let counted = match self.before {
            // すでに上限に達していれば、履歴は伸びない。行ごとに数えたほうを採る。
            Some(before) if before >= self.cap => self.hooked,
            Some(before) if !self.alt() => {
                let after = self.history();
                let delta = after.saturating_sub(before) as u64;
                if after >= self.cap {
                    // この 1 回で上限に届いた。届いたあとの押し出しは差に出ない。
                    // 1 回の読みは 64KB まで入るので、改行だけなら何千行にもなる。
                    delta.max(self.hooked)
                } else {
                    // 伸びる側にいるあいだは、差が正確である。
                    delta
                }
            }
            // 途中で画面が入れ替わっていれば、差は本画面のものではない。
            _ => self.hooked,
        };
        self.pushed + counted
    }

    /// 押し出しの起きうる呼び出しを、数えながら行う。
    ///
    /// `lines` は押し出される行数。カーソルが動く前に数えるので、呼ぶ側で出す。
    fn counted(&mut self, lines: usize, call: impl FnOnce(&mut Term<EventProxy>)) {
        // 代替画面に履歴は無い。全画面の UI が流す行は数えない。
        if !self.alt() {
            self.hooked += lines as u64;
        }
        call(self.inner);
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
    fn put_tab(&mut self, count: u16) {
        // 行末のタブは折り返しになる。
        let lines = usize::from(self.wrapping());
        self.counted(lines, |term| term.put_tab(count));
    }

    fn linefeed(&mut self) {
        let lines = usize::from(self.at_bottom());
        self.counted(lines, |term| term.linefeed());
    }

    fn newline(&mut self) {
        // NEL は改行と同じ道を通る。
        let lines = usize::from(self.at_bottom());
        self.counted(lines, |term| term.newline());
    }

    fn scroll_up(&mut self, lines: usize) {
        let pushed = lines.min(self.inner.grid().screen_lines());
        self.counted(pushed, |term| term.scroll_up(lines));
    }

    fn delete_lines(&mut self, lines: usize) {
        // 画面の先頭で消すと、下の行が上へ送られて履歴へ出る。
        let grid = self.inner.grid();
        let pushed = if grid.cursor.point.line.0 == 0 {
            lines.min(grid.screen_lines())
        } else {
            0
        };
        self.counted(pushed, |term| term.delete_lines(lines));
    }

    fn clear_screen(&mut self, mode: ClearMode) {
        // ED 2 は本画面では消さずに、書いてあるぶんを履歴へ押し上げる。
        let lines = if matches!(mode, ClearMode::All) && !self.alt() {
            self.filled_lines()
        } else {
            0
        };
        self.counted(lines, |term| term.clear_screen(mode));
    }

    forward! {
        fn set_title(title: Option<String>);
        fn set_cursor_style(style: Option<CursorStyle>);
        fn set_cursor_shape(shape: CursorShape);
        fn input(c: char);
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

    /// 台本を 1 回の読みとして流し、数えた行数と、端末が抱えた履歴行数を返す。
    ///
    /// 読み手は 64KB の器へ読み、OSC の終端でだけ区切る。改行の多い出力なら
    /// 1 回の読みに何千行も入るので、1 回で流すのが本物に近い。
    /// 上限に届くまでは履歴行数が答え合わせになる。
    fn feed(cols: usize, lines: usize, scrollback: usize, script: &[u8]) -> (u64, usize) {
        feed_reads(cols, lines, scrollback, &[script])
    }

    /// 台本を行ごとの読みに分けて流す。
    fn feed_by_line(cols: usize, lines: usize, scrollback: usize, script: &[u8]) -> (u64, usize) {
        let reads: Vec<&[u8]> = script.split_inclusive(|&b| b == b'\n').collect();
        feed_reads(cols, lines, scrollback, &reads)
    }

    /// 読みの列を、読み手と同じように 1 回ずつ流す。
    fn feed_reads(cols: usize, lines: usize, scrollback: usize, reads: &[&[u8]]) -> (u64, usize) {
        let mut term = make_term(cols, lines, scrollback);
        let mut parser: Processor<StdSyncHandler> = Processor::new();
        let mut pushed = 0u64;
        for read in reads {
            let mut counting = Counting::new(&mut term, pushed, scrollback);
            parser.advance(&mut counting, read);
            pushed = counting.finish();
        }
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
        // 行ごとの道からは見えないが、上限より下なら履歴行数の差に出る。
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
        // 同じ台本を、上限の広い端末と狭い端末へ流す。広いほうは履歴行数の差で
        // 数え、狭いほうは行ごとの道で数える。両者は一致する。
        let mut script = plain_lines(10);
        script.extend(b"\r\n\x1b[3S"); // 改行とスクロールアップ
        script.extend(b"\x1bE\x1bE"); // NEL
        script.extend(plain_lines(5));
        script.extend(b"\x1b[2J"); // 画面消去
        script.extend(plain_lines(5));

        let (wide, history) = feed(20, 4, 10_000, &script);
        assert_eq!(wide, history as u64, "上限に届かなければ履歴行数と一致する");
        let (narrow, capped) = feed(20, 4, 3, &script);
        assert_eq!(capped, 3, "狭いほうは上限で止まっている");
        assert_eq!(narrow, wide, "上限に達しても同じ数になる");
    }

    #[test]
    fn 範囲を絞った送りは数えない() {
        // DECSTBM で画面の下半分だけを送り範囲にすると、その中の改行は履歴へ
        // 入らない。行ごとの道からは最下行の改行に見えるので、上限より下では
        // 履歴行数の差を答えにしないと数え過ぎる。
        let mut script = b"\x1b[3;6r\x1b[6;1H".to_vec();
        for i in 1..=10 {
            script.extend(format!("line {i}\r\n").as_bytes());
        }
        let (pushed, history) = feed(20, 6, 100, &script);
        assert_eq!(pushed, 0, "範囲の中で送るだけなので履歴へは出ない");
        assert_eq!(history, 0);
    }

    #[test]
    fn 上限に達してからは折り返しを数えない() {
        // 折り返しは `Term` の中で改行になるので、行ごとの道からは見えない。
        // 上限に達すると履歴行数の差も出ないので、そのぶんは数えられない。
        // 1 文字ごとに見れば数えられるが、読み手を遅くするので採らない。
        let mut script = plain_lines(4);
        script.extend(b"\r\n\r\n\r\n\r\n"); // 最下行での改行 4 回
        script.extend([b'x'; 35]); // 折り返し 3 回
        let (pushed, history) = feed(10, 4, 2, &script);
        assert_eq!(pushed, 4, "改行 4 回だけを数え、折り返し 3 回は数えない");
        assert_eq!(history, 2);
    }

    #[test]
    fn 読みをまたいでも数え続ける() {
        // 読み手は読みごとに `Counting` を作り直し、前の読みまでの数を渡して続ける。
        // 上限より下なら、1 回で流しても分けて流しても同じ数になる。
        let mut script = plain_lines(10);
        script.extend(b"\r\n");
        script.extend([b'z'; 47]);
        script.extend(b"\r\n");
        script.extend(plain_lines(5));
        let (one, history) = feed(10, 4, 100, &script);
        let (many, _) = feed_by_line(10, 4, 100, &script);
        assert_eq!(one, history as u64, "1 回で流しても履歴行数と一致する");
        assert_eq!(many, one, "読みに分けても同じ数になる");
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
        let mut ticks = Ticks::empty();
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
        assert_eq!(
            Ticks::empty().time_of(1),
            None,
            "目盛りが無ければ時刻も無い"
        );
    }

    #[test]
    fn 作った時点の時刻を種にする() {
        // 一度も行を押し出していない本でも、行から時刻が引けること。
        // 引けないと、その本の行が全セッション検索で時刻の無い件になってしまう。
        let ticks = Ticks::new();
        assert!(ticks.time_of(0).is_some(), "画面の行にも時刻が付く");
        assert!(ticks.time_of(-5).is_some(), "履歴側の行にも時刻が付く");
        assert_eq!(ticks.newest(), ticks.time_of(0), "目盛りは 1 つだけ");
        assert_eq!(Ticks::empty().newest(), None);
    }

    #[test]
    fn 間の空いた出力は前の出力の時刻にならない() {
        // 1 つめの出力で 76 行押し出し、10 分おいて 2 つめの出力で 100 行押し出した。
        // 目盛りに入れるのは読む前の数なので、2 つめで出た行は 2 つめの時刻になる。
        let t0 = Instant::now();
        let t1 = t0 + Duration::from_secs(600);
        let mut ticks = Ticks::empty();
        ticks.push(0, t0);
        ticks.push(76, t1);
        assert_eq!(ticks.time_of(150), Some(t1), "2 つめの出力で出た行");
        assert_eq!(ticks.time_of(30), Some(t0), "1 つめの出力で出た行");
    }

    #[test]
    fn 押し出さない読みは目盛りを取らない() {
        // 出力 → プロンプトの描き直し（押し出しなし）→ 10 分の間 → `ls` の出力。
        // 押し出さない読みが目盛りを取ると、同じ行数の目盛りは積まないので
        // `ls` の行が前の出力の時刻になってしまう。
        let t0 = Instant::now();
        let prompt = t0 + Duration::from_secs(1);
        let t1 = t0 + Duration::from_secs(600);
        let mut ticks = Ticks::empty();
        ticks.mark_read(0, 76, t0);
        ticks.mark_read(76, 76, prompt);
        ticks.mark_read(76, 176, t1);
        assert_eq!(ticks.time_of(150), Some(t1), "`ls` の出力で出た行");
        assert_eq!(ticks.time_of(30), Some(t0), "1 つめの出力で出た行");
    }

    #[test]
    fn 同じ行数の目盛りは積まない() {
        let t0 = Instant::now();
        let mut ticks = Ticks::empty();
        ticks.push(10, t0);
        ticks.push(10, t0 + Duration::from_secs(1));
        assert_eq!(ticks.time_of(10), Some(t0), "先に見た時刻を残す");
    }

    #[test]
    fn 目盛りは一秒おきに積む() {
        let t0 = Instant::now();
        let mut ticks = Ticks::empty();
        ticks.push(10, t0);
        ticks.push(20, t0 + Duration::from_millis(500));
        assert_eq!(ticks.time_of(20), Some(t0), "1 秒経っていないので積まない");
        ticks.push(30, t0 + Duration::from_secs(1));
        assert_eq!(
            ticks.time_of(30),
            Some(t0 + Duration::from_secs(1)),
            "1 秒経てば積む"
        );
    }

    #[test]
    fn 目盛りは古いものから捨てる() {
        let t0 = Instant::now();
        let mut ticks = Ticks::empty();
        for i in 0..(TICKS as u64 + 100) {
            ticks.push(i, t0 + Duration::from_secs(i));
        }
        assert_eq!(
            ticks.time_of(0),
            Some(t0 + Duration::from_secs(100)),
            "溢れた 100 件は捨て、残っている最古の時刻を返す"
        );
    }
}

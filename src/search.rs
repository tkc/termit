//! 画面とスクロールバックの中を探す。
//!
//! Warp の Find に合わせ、行を絞り込むのではなく、その場で見つけた箇所を
//! 強調して示す。探す範囲は選んでいるセッションだけである。
//! コマンド履歴の検索（`^R`）とは別で、あちらは過去に打った命令を探す。

use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Boundary, Column, Direction, Line, Point, Side};
use alacritty_terminal::term::search::{Match, RegexIter, RegexSearch};
use alacritty_terminal::term::Term;

pub struct ScreenSearch {
    pub query: String,
    /// 入力をそのまま正規表現として扱うか。
    pub regex_mode: bool,
    /// 大文字小文字を区別しないで探すか。
    ///
    /// `RegexSearch::new` は smart-case である（大文字を含む語だけ区別する）。
    /// `⌘F` 自身はそれを変えないので、既定は偽のままにする。
    ///
    /// 立てるのは、全セッション検索から語を受け取るときだけである。あちらは
    /// 無条件に区別しない（`scan::needle`）ので、`SES`・`AWS`・`TODO` のような
    /// 大文字を含む語が小文字の行に当たりうる。その件を区別する式で渡すと、
    /// 切り替えて飛んだあと `⌘F` が何も見つけず、強調が黙って点かない（仕様 §9）。
    pub ignore_case: bool,
    /// いま選んでいる一致。
    pub current: Option<Match>,
    /// 正規表現として組み立てられなかった。
    pub invalid: bool,
    regex: Option<RegexSearch>,
}

impl ScreenSearch {
    pub fn new() -> Self {
        Self {
            query: String::new(),
            regex_mode: false,
            ignore_case: false,
            current: None,
            invalid: false,
            regex: None,
        }
    }

    /// 入力が変わったら組み立て直す。
    pub fn rebuild(&mut self) {
        self.current = None;
        self.invalid = false;
        if self.query.is_empty() {
            self.regex = None;
            return;
        }
        let pattern = pattern(&self.query, self.regex_mode, self.ignore_case);
        match RegexSearch::new(&pattern) {
            Ok(r) => self.regex = Some(r),
            Err(_) => {
                self.regex = None;
                self.invalid = true;
            }
        }
    }

    pub fn is_ready(&self) -> bool {
        self.regex.is_some()
    }

    /// 次の一致へ進む。`direction` が `Left` なら古い方（上）へ。
    pub fn step<T>(&mut self, term: &Term<T>, direction: Direction) -> Option<Match>
    where
        T: alacritty_terminal::event::EventListener,
    {
        let regex = self.regex.as_mut()?;
        let origin = match &self.current {
            // いまの一致は飛ばす。含めると同じ場所に留まる。
            Some(m) => match direction {
                Direction::Left => m.start().sub(term, Boundary::Grid, 1),
                Direction::Right => m.end().add(term, Boundary::Grid, 1),
            },
            None => start_point(term, direction),
        };
        let found = term.search_next(regex, origin, direction, Side::Left, None);
        if found.is_some() {
            self.current = found.clone();
        }
        found
    }

    /// 見えている範囲の一致をすべて集める。強調の描画に使う。
    pub fn visible<T>(&mut self, term: &Term<T>) -> Vec<Match>
    where
        T: alacritty_terminal::event::EventListener,
    {
        let Some(regex) = self.regex.as_mut() else {
            return Vec::new();
        };
        let offset = term.grid().display_offset() as i32;
        let lines = term.grid().screen_lines() as i32;
        let cols = term.grid().columns();
        let start = Point::new(Line(-offset), Column(0));
        let end = Point::new(Line(lines - offset - 1), Column(cols.saturating_sub(1)));
        // 見えている範囲だけなので、数は画面の広さで頭打ちになる。
        RegexIter::new(start, end, Direction::Right, term, regex)
            .take(1000)
            .collect()
    }
}

impl Default for ScreenSearch {
    fn default() -> Self {
        Self::new()
    }
}

/// 探し始める位置。上へ探すなら見えている一番下から、下へ探すなら一番上から。
fn start_point<T>(term: &Term<T>, direction: Direction) -> Point {
    let offset = term.grid().display_offset() as i32;
    let lines = term.grid().screen_lines() as i32;
    let cols = term.grid().columns();
    match direction {
        Direction::Left => Point::new(Line(lines - offset - 1), Column(cols.saturating_sub(1))),
        Direction::Right => Point::new(Line(-offset), Column(0)),
    }
}

/// 打った語から、組み立てる式を作る。
///
/// `(?i)` を足すのは**打ち消したあと**である。先に足すと `(`・`?`・`)` が
/// 打ち消されて、ただの文字列になってしまう。打った語そのもの（`query`）は
/// 触らないので、帯に出る文字は変わらない。
fn pattern(query: &str, regex_mode: bool, ignore_case: bool) -> String {
    let mut out = if regex_mode {
        query.to_string()
    } else {
        escape(query)
    };
    if ignore_case {
        out.insert_str(0, "(?i)");
    }
    out
}

/// 正規表現の特殊文字を打ち消す。既定では入力をそのままの文字列として探す。
fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 2);
    for c in s.chars() {
        if "\\.+*?()|[]{}^$#&-~".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::term::{EventProxy, TermSize, UiSender};
    use alacritty_terminal::event::WindowSize;
    use alacritty_terminal::sync::FairMutex;
    use alacritty_terminal::vte::ansi::{Processor, StdSyncHandler};
    use std::sync::mpsc::channel;
    use std::sync::Arc;

    /// 台本を流した端末を 1 つ作る。
    fn fed(script: &str) -> Term<EventProxy> {
        let (pty_tx, _pty_rx) = channel();
        let (ui_tx, _ui_rx) = channel();
        let (cols, lines) = (60, 8);
        let ws = Arc::new(FairMutex::new(WindowSize {
            num_lines: lines as u16,
            num_cols: cols as u16,
            cell_width: 8,
            cell_height: 16,
        }));
        let proxy = EventProxy::new(1, pty_tx, UiSender::Channel(ui_tx), ws);
        let mut term = crate::term::new_term(TermSize::new(cols, lines), 1000, proxy);
        let mut parser: Processor<StdSyncHandler> = Processor::new();
        parser.advance(&mut term, script.as_bytes());
        term
    }

    #[test]
    fn 区別しない旗を立てると式の頭に付く() {
        assert_eq!(pattern("SES", false, false), "SES");
        assert_eq!(pattern("SES", false, true), "(?i)SES");
        // 打ち消したあとに付ける。先に付けると括弧まで打ち消される。
        assert_eq!(pattern("a.b", false, true), r"(?i)a\.b");
        assert_eq!(pattern("a(b)", true, true), "(?i)a(b)");
    }

    #[test]
    fn 旗を立てた語は小文字の行にも当たる() {
        // 全セッション検索から渡された語の経路。あちらは無条件に区別しないので、
        // 大文字を含む語が小文字の行に当たりうる。
        let term = fed("the ses bounce handler\r\n");
        let mut s = ScreenSearch::new();
        s.query = "SES".into();
        s.ignore_case = true;
        s.rebuild();
        assert!(
            s.step(&term, Direction::Left).is_some(),
            "区別しないので小文字の行に当たる"
        );
    }

    #[test]
    fn 旗を立てなければ_smart_case_のままである() {
        // `⌘F` 自身の振る舞いは変えない。大文字を含む語は区別する。
        let term = fed("the ses bounce handler\r\n");
        let mut s = ScreenSearch::new();
        s.query = "SES".into();
        s.rebuild();
        assert!(
            s.step(&term, Direction::Left).is_none(),
            "大文字を含む語は区別するので当たらない"
        );
        let mut lower = ScreenSearch::new();
        lower.query = "Ses".into();
        lower.rebuild();
        assert!(
            lower.step(&term, Direction::Left).is_none(),
            "大文字が 1 つでも有れば区別する"
        );
        let mut plain = ScreenSearch::new();
        plain.query = "ses".into();
        plain.rebuild();
        assert!(
            plain.step(&term, Direction::Left).is_some(),
            "小文字だけなら区別しない"
        );
    }

    #[test]
    fn 特殊文字を打ち消す() {
        assert_eq!(escape("a.b"), r"a\.b");
        assert_eq!(escape("cargo build"), "cargo build");
        assert_eq!(escape("[0-9]+"), r"\[0\-9\]\+");
    }

    #[test]
    fn 入力が空なら組み立てない() {
        let mut s = ScreenSearch::new();
        s.rebuild();
        assert!(!s.is_ready());
        assert!(!s.invalid);
    }

    #[test]
    fn 文字列として組み立てる() {
        let mut s = ScreenSearch::new();
        s.query = "a(b".into();
        s.rebuild();
        // そのままなら括弧が開いたままで壊れるが、打ち消すので通る。
        assert!(s.is_ready());
        assert!(!s.invalid);
    }

    #[test]
    fn 正規表現の誤りを知らせる() {
        let mut s = ScreenSearch::new();
        s.regex_mode = true;
        s.query = "a(b".into();
        s.rebuild();
        assert!(!s.is_ready());
        assert!(s.invalid);
    }

    #[test]
    fn 入力を変えたら選択を捨てる() {
        let mut s = ScreenSearch::new();
        s.query = "x".into();
        s.rebuild();
        s.current = Some(Point::new(Line(0), Column(0))..=Point::new(Line(0), Column(0)));
        s.query = "y".into();
        s.rebuild();
        assert!(s.current.is_none());
    }
}

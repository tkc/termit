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
        let pattern = if self.regex_mode {
            self.query.clone()
        } else {
            escape(&self.query)
        };
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

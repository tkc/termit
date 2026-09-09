//! OSC 8 のリンクを開く。
//!
//! `alacritty_terminal` が OSC 8 を解釈してコマに結び付けてくれる。
//! それに加えて、素で書かれた URL も見つけて押せるようにする。
//! どちらも、開いてよい相手かをここで決める。

use alacritty_terminal::index::{Column, Direction, Line, Point};
use alacritty_terminal::term::search::{Match, RegexIter, RegexSearch};
use alacritty_terminal::Term;

/// 開いてよい仕組み。ここに無いものは押しても何も起きない。
///
/// 端末に出た字はどこから来たものか分からない。
/// `tel:` や独自仕組みまで通すと、別のプログラムを黙って起こす道になる。
const SCHEMES: &[&str] = &["http://", "https://", "mailto:", "file://"];

/// リンクの長さの上限。これを超えるものは開かない。
const MAX_LEN: usize = 4096;

/// 押したときに開いてよいか。
pub fn openable(uri: &str) -> bool {
    if uri.is_empty() || uri.len() > MAX_LEN {
        return false;
    }
    // 制御文字が混ざっているものは扱わない。
    if uri.chars().any(|c| c.is_control()) {
        return false;
    }
    let lower = uri.to_ascii_lowercase();
    SCHEMES.iter().any(|s| lower.starts_with(s))
}

/// リンクを開く。開けない相手なら何もしない。
///
/// シェルを通さずに `open` へ渡す。仕組みを検めてあるので、
/// 引数が旗と読まれることもない。
pub fn open(uri: &str) {
    if !openable(uri) {
        log::warn!("開かないリンク: {uri}");
        return;
    }
    let _ = std::process::Command::new("open")
        .arg(uri)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

/// 素の URL を見つけるための形。
///
/// 開ける仕組みだけを拾う。終わりに句読点や閉じ括弧を含めない。
/// 文中に書かれた URL は、そのあとに `.` や `)` が続くことが多い。
const URL_PATTERN: &str =
    r#"(https?://|mailto:|file://)[^\s<>"'`(){}\[\]]*[^\s<>"'`(){}\[\].,;:!?]"#;

/// 素の URL を探す道具。作れなければ `None`。
pub fn url_search() -> Option<RegexSearch> {
    match RegexSearch::new(URL_PATTERN) {
        Ok(r) => Some(r),
        Err(e) => {
            log::warn!("URL を探す形を組めない: {e}");
            None
        }
    }
}

/// 指した位置を含む素の URL を探す。
///
/// 探す範囲は見えているところだけにする。履歴の端まで見ると、
/// マウスを動かすたびに走る処理としては重すぎる。
pub fn url_at<T>(term: &Term<T>, regex: &mut RegexSearch, point: Point) -> Option<Match>
where
    T: alacritty_terminal::event::EventListener,
{
    use alacritty_terminal::grid::Dimensions;
    let offset = term.grid().display_offset() as i32;
    let lines = term.grid().screen_lines() as i32;
    let cols = term.grid().columns();
    let start = Point::new(Line(-offset), Column(0));
    let end = Point::new(Line(lines - offset - 1), Column(cols.saturating_sub(1)));
    if point < start || point > end {
        return None;
    }
    RegexIter::new(start, end, Direction::Right, term, regex)
        .take(1000)
        .find(|m| m.contains(&point))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 素性の知れた仕組みだけ開く() {
        assert!(openable("https://example.com/a"));
        assert!(openable("http://example.com"));
        assert!(openable("mailto:tkc@example.com"));
        assert!(openable("file:///Users/tkc/a.txt"));
    }

    #[test]
    fn 大文字の仕組みも同じに見る() {
        assert!(openable("HTTPS://example.com"));
        assert!(openable("MailTo:a@b.c"));
    }

    #[test]
    fn 知らない仕組みは開かない() {
        // 端末に出た字がどこから来たかは分からない。
        // 別のプログラムを黙って起こす道にしない。
        for uri in [
            "tel:0312345678",
            "ssh://host",
            "javascript:alert(1)",
            "vscode://file/etc/passwd",
            "data:text/html,<script>",
            "/etc/passwd",
            "example.com",
        ] {
            assert!(!openable(uri), "{uri} は開かない");
        }
    }

    #[test]
    fn 旗と読まれる形も通さない() {
        // 仕組みを検めているので、先頭が `-` のものは弾かれる。
        assert!(!openable("-a"));
        assert!(!openable("--args"));
    }

    #[test]
    fn 制御文字が混ざるものは開かない() {
        assert!(!openable("https://example.com/\u{1b}]0;x\u{7}"));
        assert!(!openable("https://example.com/\n"));
        assert!(!openable("https://exa\u{0}mple.com"));
    }

    #[test]
    fn 空や長すぎるものは開かない() {
        assert!(!openable(""));
        let long = format!("https://example.com/{}", "a".repeat(MAX_LEN));
        assert!(!openable(&long));
    }
}

#[cfg(test)]
mod url_tests {
    use super::*;
    use crate::term::{new_term, EventProxy, TermSize, UiSender};
    use alacritty_terminal::event::WindowSize;
    use alacritty_terminal::sync::FairMutex;
    use alacritty_terminal::vte::ansi::Processor;
    use std::sync::Arc;

    /// 1 行書いた端末を作る。
    fn with_line(text: &str) -> alacritty_terminal::Term<EventProxy> {
        let (tx, _rx) = std::sync::mpsc::channel();
        let (ptx, _prx) = std::sync::mpsc::channel();
        let ws = Arc::new(FairMutex::new(WindowSize {
            num_lines: 10,
            num_cols: 120,
            cell_width: 8,
            cell_height: 16,
        }));
        let proxy = EventProxy::new(1, ptx, UiSender::Channel(tx), ws);
        let mut term = new_term(TermSize::new(120, 10), 100, proxy);
        let mut parser: Processor = Processor::new();
        parser.advance(&mut term, text.as_bytes());
        term
    }

    /// `col` 桁目を指したときに見つかる URL。
    fn at(text: &str, col: usize) -> Option<String> {
        let term = with_line(text);
        let mut regex = url_search().expect("形を組める");
        let point = Point::new(Line(0), Column(col));
        let span = url_at(&term, &mut regex, point)?;
        Some(term.bounds_to_string(*span.start(), *span.end()))
    }

    #[test]
    fn 素で書かれた_url_を見つける() {
        // 出力に混ざった URL は OSC 8 の印を持たない。それでも押せる。
        let line = "zsh: no such file or directory: https://github.com/tkc/termit/pull/22";
        assert_eq!(
            at(line, 40).as_deref(),
            Some("https://github.com/tkc/termit/pull/22")
        );
    }

    #[test]
    fn url_の外を指しても見つけない() {
        let line = "zsh: no such file or directory: https://example.com/";
        assert_eq!(at(line, 3), None, "前の字の上では何も出ない");
    }

    #[test]
    fn 文末の句読点は含めない() {
        // 文中に書かれた URL は、あとに `.` や `)` が続くことが多い。
        assert_eq!(
            at("see https://example.com/a. next", 10).as_deref(),
            Some("https://example.com/a")
        );
        assert_eq!(
            at("(https://example.com/b)", 10).as_deref(),
            Some("https://example.com/b")
        );
        assert_eq!(
            at("https://example.com/c, and", 5).as_deref(),
            Some("https://example.com/c")
        );
    }

    #[test]
    fn 開けない仕組みは拾わない() {
        // 形の側で仕組みを絞ってあるので、そもそも一致しない。
        for line in ["ftp://example.com/x", "ssh://host/x", "javascript:alert(1)"] {
            assert_eq!(at(line, 5), None, "{line} は拾わない");
        }
    }

    #[test]
    fn 開ける仕組みは拾う() {
        assert_eq!(
            at("http://example.com/x", 5).as_deref(),
            Some("http://example.com/x")
        );
        assert_eq!(
            at("mailto:tkc@example.com", 5).as_deref(),
            Some("mailto:tkc@example.com")
        );
        assert_eq!(
            at("file:///Users/tkc/a.txt", 5).as_deref(),
            Some("file:///Users/tkc/a.txt")
        );
    }

    #[test]
    fn 見つけた_url_は開ける相手である() {
        let line = "https://github.com/tkc/termit";
        let uri = at(line, 5).expect("見つかる");
        assert!(openable(&uri));
    }
}

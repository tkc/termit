//! OSC 8 のリンクを開く。
//!
//! `alacritty_terminal` が OSC 8 を解釈してコマに結び付けてくれるので、
//! こちらの仕事は「開いてよい相手か」を決めることと、開くことだけである。

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

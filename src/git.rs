//! いまの作業ディレクトリのブランチ名を読む。
//!
//! `git` を起動せずファイルを読むだけにする。左ペインは毎秒描き直すので、
//! そのたびにプロセスを起こすわけにはいかない。

use std::path::{Path, PathBuf};

/// `dir` から上へたどって最初に見つけた作業ツリーのブランチ名を返す。
///
/// 分離 HEAD のときは短縮した SHA を返す。
pub fn branch_for(dir: &Path) -> Option<String> {
    let git = find_git(dir)?;
    let head = std::fs::read_to_string(git.join("HEAD")).ok()?;
    parse_head(&head)
}

/// `.git` の実体を探す。作業ツリーでは `.git` がファイルになる。
fn find_git(dir: &Path) -> Option<PathBuf> {
    let mut cur = Some(dir);
    while let Some(d) = cur {
        let candidate = d.join(".git");
        if candidate.is_dir() {
            return Some(candidate);
        }
        if candidate.is_file() {
            let text = std::fs::read_to_string(&candidate).ok()?;
            let rest = text.trim().strip_prefix("gitdir:")?.trim();
            let path = PathBuf::from(rest);
            return Some(if path.is_absolute() {
                path
            } else {
                d.join(path)
            });
        }
        cur = d.parent();
    }
    None
}

/// `HEAD` の中身からブランチ名を取り出す。
fn parse_head(text: &str) -> Option<String> {
    let text = text.trim();
    if let Some(r) = text.strip_prefix("ref:") {
        let r = r.trim();
        let name = r.strip_prefix("refs/heads/").unwrap_or(r);
        return (!name.is_empty()).then(|| name.to_string());
    }
    // 分離 HEAD。SHA をそのまま出すと長いので縮める。
    if text.len() >= 7 && text.chars().all(|c| c.is_ascii_hexdigit()) {
        return Some(text[..7].to_string());
    }
    None
}

/// 表示用にパスを縮める。home は `~` に、長すぎるときは先頭を落とす。
pub fn short_path(path: &Path, max_cols: usize) -> String {
    let text = match dirs::home_dir() {
        Some(home) => match path.strip_prefix(&home) {
            Ok(rest) if rest.as_os_str().is_empty() => "~".to_string(),
            Ok(rest) => format!("~/{}", rest.display()),
            Err(_) => path.display().to_string(),
        },
        None => path.display().to_string(),
    };
    let cols: usize = text.chars().map(crate::render::char_cols).sum();
    if cols <= max_cols || max_cols < 2 {
        return text;
    }
    // 末尾のほうが手がかりになるので、先頭を落とす。
    let mut out = String::new();
    let mut used = 0;
    for c in text.chars().rev() {
        let w = crate::render::char_cols(c);
        if used + w > max_cols - 1 {
            break;
        }
        out.push(c);
        used += w;
    }
    let tail: String = out.chars().rev().collect();
    format!("…{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 通常のブランチ名を読む() {
        assert_eq!(parse_head("ref: refs/heads/main\n").unwrap(), "main");
        assert_eq!(
            parse_head("ref: refs/heads/mt5/nikkei-grid-lotto\n").unwrap(),
            "mt5/nikkei-grid-lotto"
        );
    }

    #[test]
    fn 分離_head_は短縮した_sha_になる() {
        let sha = "0123456789abcdef0123456789abcdef01234567";
        assert_eq!(parse_head(sha).unwrap(), "0123456");
    }

    #[test]
    fn 読めない中身は_none_になる() {
        assert_eq!(parse_head(""), None);
        assert_eq!(parse_head("ref:"), None);
        assert_eq!(parse_head("なにか"), None);
    }

    #[test]
    fn このリポジトリのブランチを読める() {
        let here = std::env::current_dir().unwrap();
        let branch = branch_for(&here).expect("このリポジトリのブランチが読める");
        assert!(!branch.is_empty());
    }

    #[test]
    fn git_のないところでは_none_になる() {
        assert_eq!(branch_for(Path::new("/")), None);
    }

    #[test]
    fn 長いパスは先頭を落とす() {
        let p = Path::new("/aaa/bbb/ccc/ddd/eee");
        assert_eq!(short_path(p, 100), "/aaa/bbb/ccc/ddd/eee");
        let s = short_path(p, 10);
        assert!(s.starts_with('…'), "{s}");
        assert!(s.ends_with("eee"), "{s}");
        assert!(
            s.chars().map(crate::render::char_cols).sum::<usize>() <= 10,
            "{s}"
        );
    }

    #[test]
    fn home_は波線にする() {
        let Some(home) = dirs::home_dir() else { return };
        assert_eq!(short_path(&home, 40), "~");
        assert_eq!(short_path(&home.join("x"), 40), "~/x");
    }
}

//! セッションの並びを覚えておき、次の起動で作り直す。
//!
//! プロセスそのものは復元できない。覚えるのは「どこで、どのプロファイルで、
//! 何を、どの名前で、どの分岐関係で動かしていたか」であり、
//! エージェントの会話は再開のコマンドに任せる。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// 覚えている 1 セッションぶん。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedSession {
    /// 再起動をまたいで残る鍵。コマンド履歴をこの単位で辿る。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    /// 利用者が付けた名前。付けていなければ空。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub cwd: String,
    pub profile: String,
    /// エージェントの会話 ID。分かっていれば、これで会話を再開する。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    /// 親の添字。先頭からの並び順で数える。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<usize>,
    /// プロファイルで包む前の起動コマンド。会話を再開できないときに使う。
    pub command: Vec<String>,
}

/// 覚えている並び全体。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedState {
    /// 形式の版。読めない版は捨てる。
    #[serde(default = "default_version")]
    pub version: u32,
    #[serde(default)]
    pub selected: usize,
    #[serde(default)]
    pub sessions: Vec<SavedSession>,
}

fn default_version() -> u32 {
    1
}

pub const VERSION: u32 = 1;

pub fn state_path() -> Option<PathBuf> {
    Some(
        crate::config::xdg_dir("XDG_DATA_HOME", ".local/share")?
            .join("termit")
            .join("sessions.toml"),
    )
}

impl SavedState {
    /// 既定の場所から読む。無い、壊れている、版が違うときは `None`。
    pub fn load() -> Option<SavedState> {
        let path = state_path()?;
        let text = std::fs::read_to_string(path).ok()?;
        Self::parse(&text)
    }

    pub fn parse(text: &str) -> Option<SavedState> {
        let state: SavedState = toml::from_str(text).ok()?;
        if state.version != VERSION || state.sessions.is_empty() {
            return None;
        }
        // 親の添字が自分より後ろを指していると、木を組めない。
        for (i, s) in state.sessions.iter().enumerate() {
            if let Some(p) = s.parent {
                if p >= i {
                    return None;
                }
            }
        }
        Some(state)
    }

    /// 既定の場所へ書く。書けなくても動作は続ける。
    pub fn save(&self) {
        let Some(path) = state_path() else { return };
        if let Some(parent) = path.parent() {
            if std::fs::create_dir_all(parent).is_err() {
                return;
            }
        }
        match toml::to_string_pretty(self) {
            Ok(text) => Self::write_atomic(&path, &text),
            Err(e) => log::warn!("cannot serialize session state: {e}"),
        }
    }

    /// 一時ファイルへ書いてから置き換える。
    /// 途中で落ちても、半分だけ書かれた状態を残さない。
    fn write_atomic(path: &Path, text: &str) {
        let tmp = path.with_extension("toml.tmp");
        if std::fs::write(&tmp, text).is_err() {
            return;
        }
        if let Err(e) = std::fs::rename(&tmp, path) {
            log::warn!("cannot save session state: {e}");
            let _ = std::fs::remove_file(&tmp);
        }
    }

    /// 覚えている並びを消す。
    pub fn clear() {
        if let Some(path) = state_path() {
            let _ = std::fs::remove_file(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> SavedState {
        SavedState {
            version: VERSION,
            selected: 1,
            sessions: vec![
                SavedSession {
                    key: Some("k1".into()),
                    name: None,
                    cwd: "/repo".into(),
                    profile: "host".into(),
                    agent_id: Some("aaa".into()),
                    parent: None,
                    command: vec!["claude".into()],
                },
                SavedSession {
                    key: Some("k2".into()),
                    name: Some("review".into()),
                    cwd: "/repo".into(),
                    profile: "sandbox".into(),
                    agent_id: None,
                    parent: Some(0),
                    command: vec!["zsh".into(), "-l".into()],
                },
            ],
        }
    }

    #[test]
    fn 書いて読み戻せる() {
        let s = sample();
        let text = toml::to_string_pretty(&s).unwrap();
        assert_eq!(SavedState::parse(&text).unwrap(), s);
    }

    #[test]
    fn 版が違えば捨てる() {
        let mut s = sample();
        s.version = 999;
        let text = toml::to_string_pretty(&s).unwrap();
        assert!(SavedState::parse(&text).is_none());
    }

    #[test]
    fn 空の並びは捨てる() {
        let s = SavedState {
            version: VERSION,
            selected: 0,
            sessions: vec![],
        };
        let text = toml::to_string_pretty(&s).unwrap();
        assert!(SavedState::parse(&text).is_none());
    }

    #[test]
    fn 壊れた本文は捨てる() {
        assert!(SavedState::parse("this is not toml {{{").is_none());
    }

    #[test]
    fn 親が自分より後ろを指していれば捨てる() {
        let mut s = sample();
        s.sessions[0].parent = Some(1);
        let text = toml::to_string_pretty(&s).unwrap();
        assert!(SavedState::parse(&text).is_none());
    }

    #[test]
    fn 名前や会話_id_が無くても読める() {
        let text = r#"
version = 1
selected = 0

[[sessions]]
cwd = "/repo"
profile = "host"
command = ["zsh"]
"#;
        let s = SavedState::parse(text).unwrap();
        assert_eq!(s.sessions.len(), 1);
        assert!(s.sessions[0].name.is_none());
        assert!(s.sessions[0].agent_id.is_none());
    }
}

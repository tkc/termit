//! コマンド履歴の保存と検索。
//!
//! シェルの `HISTFILE` は読まない。OSC 133 から端末自身が組み立てた記録だけを持つ。

use std::path::PathBuf;
use std::time::UNIX_EPOCH;

use rusqlite::{params, Connection};

use crate::session::CommandRecord;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub command: String,
    pub cwd: String,
    pub exit_code: Option<i32>,
    pub duration_ms: Option<i64>,
    pub started_at: i64,
}

/// 検索の対象範囲。`Ctrl+R` を続けて押すと切り替わる。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    All,
    Session,
    Cwd,
}

impl Scope {
    pub fn label(self) -> &'static str {
        match self {
            Scope::All => "all",
            Scope::Session => "session",
            Scope::Cwd => "cwd",
        }
    }
    pub fn next(self) -> Scope {
        match self {
            Scope::All => Scope::Session,
            Scope::Session => Scope::Cwd,
            Scope::Cwd => Scope::All,
        }
    }
}

pub struct History {
    conn: Connection,
}

/// 履歴の置き場所。設定と同じく XDG の作法に合わせる。
pub fn db_path() -> Option<PathBuf> {
    Some(
        crate::config::xdg_dir("XDG_DATA_HOME", ".local/share")?
            .join("termit")
            .join("history.db"),
    )
}

impl History {
    /// 既定の位置に開く。開けない場合は履歴だけを無効にする。
    pub fn open_default() -> Option<History> {
        let path = db_path()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok()?;
        }
        let conn = Connection::open(path).ok()?;
        let h = History { conn };
        h.migrate().ok()?;
        Some(h)
    }

    pub fn open_memory() -> rusqlite::Result<History> {
        let h = History {
            conn: Connection::open_in_memory()?,
        };
        h.migrate()?;
        Ok(h)
    }

    fn migrate(&self) -> rusqlite::Result<()> {
        self.conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS command (
               id          INTEGER PRIMARY KEY,
               session_id  INTEGER NOT NULL,
               session_key TEXT,
               agent_id    TEXT,
               cwd         TEXT NOT NULL,
               command     TEXT NOT NULL,
               exit_code   INTEGER,
               started_at  INTEGER NOT NULL,
               duration_ms INTEGER
             );
             CREATE INDEX IF NOT EXISTS idx_command_started ON command(started_at DESC);
             CREATE INDEX IF NOT EXISTS idx_command_session ON command(session_id, started_at DESC);",
        )?;
        // 先に作られた表には session_key が無い。あとから足す。
        // セッションの鍵は再起動をまたいで残るので、以前の並びも辿れる。
        let has_key = self
            .conn
            .prepare("SELECT session_key FROM command LIMIT 1")
            .is_ok();
        if !has_key {
            self.conn
                .execute("ALTER TABLE command ADD COLUMN session_key TEXT", [])?;
        }
        self.conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS idx_command_key
               ON command(session_key, started_at DESC);",
        )
    }

    pub fn record(&self, r: &CommandRecord) -> rusqlite::Result<()> {
        let started = r
            .started_at
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        self.conn.execute(
            "INSERT INTO command
               (session_id, session_key, agent_id, cwd, command, exit_code, started_at, duration_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                r.session_id,
                r.session_key,
                r.agent_id,
                r.cwd,
                r.command,
                r.exit_code,
                started,
                r.duration_ms
            ],
        )?;
        Ok(())
    }

    /// 部分一致で絞り込み、新しい順に返す。同じコマンドは最新の 1 件にまとめる。
    pub fn search(
        &self,
        query: &str,
        scope: Scope,
        session_key: &str,
        cwd: &str,
        limit: usize,
    ) -> rusqlite::Result<Vec<Entry>> {
        let pattern = format!("%{}%", escape_like(query));
        let (cond, extra): (&str, Vec<&dyn rusqlite::ToSql>) = match scope {
            Scope::All => ("", vec![]),
            Scope::Session => ("AND session_key = ?4", vec![&session_key]),
            Scope::Cwd => ("AND cwd = ?4", vec![&cwd]),
        };
        let sql = format!(
            "SELECT command, cwd, exit_code, duration_ms, MAX(started_at) AS ts
             FROM command
             WHERE command LIKE ?1 ESCAPE '\\' {cond}
             GROUP BY command
             ORDER BY ts DESC
             LIMIT ?2 OFFSET ?3"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let limit = limit as i64;
        let offset = 0i64;
        let mut args: Vec<&dyn rusqlite::ToSql> = vec![&pattern, &limit, &offset];
        args.extend(extra);
        let rows = stmt.query_map(args.as_slice(), |row| {
            Ok(Entry {
                command: row.get(0)?,
                cwd: row.get(1)?,
                exit_code: row.get(2)?,
                duration_ms: row.get(3)?,
                started_at: row.get(4)?,
            })
        })?;
        rows.collect()
    }

    /// 左ペインの下段に出す、そのセッションの直近のコマンド。
    pub fn recent(&self, session_key: &str, limit: usize) -> rusqlite::Result<Vec<Entry>> {
        let mut stmt = self.conn.prepare(
            "SELECT command, cwd, exit_code, duration_ms, started_at
             FROM command WHERE session_key = ?1
             ORDER BY started_at DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![session_key, limit as i64], |row| {
            Ok(Entry {
                command: row.get(0)?,
                cwd: row.get(1)?,
                exit_code: row.get(2)?,
                duration_ms: row.get(3)?,
                started_at: row.get(4)?,
            })
        })?;
        rows.collect()
    }
}

fn escape_like(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::SessionId;
    use std::time::Duration;

    fn rec(session: SessionId, cwd: &str, cmd: &str, code: i32, at: u64) -> CommandRecord {
        CommandRecord {
            session_id: session,
            session_key: format!("key-{session}"),
            agent_id: None,
            cwd: cwd.into(),
            command: cmd.into(),
            exit_code: Some(code),
            started_at: UNIX_EPOCH + Duration::from_millis(at),
            duration_ms: Some(10),
        }
    }

    fn seeded() -> History {
        let h = History::open_memory().unwrap();
        h.record(&rec(1, "/a", "cargo build", 0, 1000)).unwrap();
        h.record(&rec(1, "/a", "cargo test", 0, 2000)).unwrap();
        h.record(&rec(2, "/b", "git status", 0, 3000)).unwrap();
        h.record(&rec(2, "/b", "cargo clippy", 1, 4000)).unwrap();
        h
    }

    #[test]
    fn 部分一致で絞り込む() {
        let h = seeded();
        let got = h.search("cargo", Scope::All, "key-1", "/a", 10).unwrap();
        let cmds: Vec<_> = got.iter().map(|e| e.command.as_str()).collect();
        assert_eq!(cmds, vec!["cargo clippy", "cargo test", "cargo build"]);
    }

    #[test]
    fn 新しい順に返す() {
        let h = seeded();
        let got = h.search("", Scope::All, "key-1", "/a", 10).unwrap();
        assert_eq!(got[0].command, "cargo clippy");
        assert_eq!(got.last().unwrap().command, "cargo build");
    }

    #[test]
    fn セッションで範囲を絞る() {
        let h = seeded();
        let got = h.search("", Scope::Session, "key-2", "/a", 10).unwrap();
        let cmds: Vec<_> = got.iter().map(|e| e.command.as_str()).collect();
        assert_eq!(cmds, vec!["cargo clippy", "git status"]);
    }

    #[test]
    fn 作業ディレクトリで範囲を絞る() {
        let h = seeded();
        let got = h.search("", Scope::Cwd, "key-1", "/a", 10).unwrap();
        let cmds: Vec<_> = got.iter().map(|e| e.command.as_str()).collect();
        assert_eq!(cmds, vec!["cargo test", "cargo build"]);
    }

    #[test]
    fn 同じコマンドは最新の一件にまとまる() {
        let h = History::open_memory().unwrap();
        h.record(&rec(1, "/a", "ls", 0, 1000)).unwrap();
        h.record(&rec(1, "/a", "ls", 0, 5000)).unwrap();
        let got = h.search("ls", Scope::All, "key-1", "/a", 10).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].started_at, 5000);
    }

    #[test]
    fn 検索語のワイルドカードを文字として扱う() {
        let h = History::open_memory().unwrap();
        h.record(&rec(1, "/a", "echo 100%", 0, 1000)).unwrap();
        h.record(&rec(1, "/a", "ls", 0, 2000)).unwrap();
        let got = h.search("%", Scope::All, "key-1", "/a", 10).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].command, "echo 100%");
    }

    #[test]
    fn 直近の履歴をセッションごとに返す() {
        let h = seeded();
        let got = h.recent("key-1", 10).unwrap();
        let cmds: Vec<_> = got.iter().map(|e| e.command.as_str()).collect();
        assert_eq!(cmds, vec!["cargo test", "cargo build"]);
    }

    /// 再起動をまたいでも、同じ鍵なら同じ履歴が見えることを確かめる。
    /// 通し番号は起動のたびに振り直されるので、鍵で辿る必要がある。
    #[test]
    fn 鍵が同じなら通し番号が変わっても辿れる() {
        let h = History::open_memory().unwrap();
        // 1 回目の起動。通し番号は 1。
        h.record(&CommandRecord {
            session_id: 1,
            session_key: "same-key".into(),
            agent_id: None,
            cwd: "/a".into(),
            command: "before restart".into(),
            exit_code: Some(0),
            started_at: UNIX_EPOCH + Duration::from_millis(1000),
            duration_ms: None,
        })
        .unwrap();
        // 2 回目の起動。作り直されて通し番号は 7 になったが、鍵は同じ。
        h.record(&CommandRecord {
            session_id: 7,
            session_key: "same-key".into(),
            agent_id: None,
            cwd: "/a".into(),
            command: "after restart".into(),
            exit_code: Some(0),
            started_at: UNIX_EPOCH + Duration::from_millis(2000),
            duration_ms: None,
        })
        .unwrap();
        // 別のセッション。
        h.record(&rec(9, "/b", "other session", 0, 3000)).unwrap();

        let got = h.recent("same-key", 100).unwrap();
        let cmds: Vec<_> = got.iter().map(|e| e.command.as_str()).collect();
        assert_eq!(cmds, vec!["after restart", "before restart"]);

        let got = h.search("", Scope::Session, "same-key", "/a", 500).unwrap();
        let cmds: Vec<_> = got.iter().map(|e| e.command.as_str()).collect();
        assert_eq!(cmds, vec!["after restart", "before restart"]);
    }

    /// 遡れる件数まで返し、それを超えた分は返さないことを確かめる。
    #[test]
    fn 上限まで遡れる() {
        let h = History::open_memory().unwrap();
        for i in 0..600 {
            h.record(&CommandRecord {
                session_id: 1,
                session_key: "k".into(),
                agent_id: None,
                cwd: "/a".into(),
                command: format!("cmd {i:04}"),
                exit_code: Some(0),
                started_at: UNIX_EPOCH + Duration::from_millis(1000 + i as u64),
                duration_ms: None,
            })
            .unwrap();
        }
        let got = h.search("", Scope::Session, "k", "/a", 500).unwrap();
        assert_eq!(got.len(), 500, "上限まで返る");
        assert_eq!(got[0].command, "cmd 0599", "新しいものから並ぶ");
        assert_eq!(got[499].command, "cmd 0100");
    }

    #[test]
    fn 範囲の切り替えは三状態を巡回する() {
        assert_eq!(Scope::All.next(), Scope::Session);
        assert_eq!(Scope::Session.next(), Scope::Cwd);
        assert_eq!(Scope::Cwd.next(), Scope::All);
    }
}

//! コマンド履歴の保存と検索。
//!
//! シェルの `HISTFILE` は読まない。OSC 133 から端末自身が組み立てた記録だけを持つ。

use std::path::PathBuf;
use std::time::UNIX_EPOCH;

use rusqlite::{params, Connection};

use crate::session::{CommandRecord, SessionId};

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

pub fn db_path() -> Option<PathBuf> {
    dirs::data_dir().map(|d| d.join("tex").join("history.db"))
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
               agent_id    TEXT,
               cwd         TEXT NOT NULL,
               command     TEXT NOT NULL,
               exit_code   INTEGER,
               started_at  INTEGER NOT NULL,
               duration_ms INTEGER
             );
             CREATE INDEX IF NOT EXISTS idx_command_started ON command(started_at DESC);
             CREATE INDEX IF NOT EXISTS idx_command_session ON command(session_id, started_at DESC);",
        )
    }

    pub fn record(&self, r: &CommandRecord) -> rusqlite::Result<()> {
        let started = r
            .started_at
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        self.conn.execute(
            "INSERT INTO command (session_id, agent_id, cwd, command, exit_code, started_at, duration_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                r.session_id,
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
        session_id: SessionId,
        cwd: &str,
        limit: usize,
    ) -> rusqlite::Result<Vec<Entry>> {
        let pattern = format!("%{}%", escape_like(query));
        let (cond, extra): (&str, Vec<&dyn rusqlite::ToSql>) = match scope {
            Scope::All => ("", vec![]),
            Scope::Session => ("AND session_id = ?4", vec![&session_id]),
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
    pub fn recent(&self, session_id: SessionId, limit: usize) -> rusqlite::Result<Vec<Entry>> {
        let mut stmt = self.conn.prepare(
            "SELECT command, cwd, exit_code, duration_ms, started_at
             FROM command WHERE session_id = ?1
             ORDER BY started_at DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![session_id, limit as i64], |row| {
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
    use std::time::Duration;

    fn rec(session: SessionId, cwd: &str, cmd: &str, code: i32, at: u64) -> CommandRecord {
        CommandRecord {
            session_id: session,
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
        let got = h.search("cargo", Scope::All, 1, "/a", 10).unwrap();
        let cmds: Vec<_> = got.iter().map(|e| e.command.as_str()).collect();
        assert_eq!(cmds, vec!["cargo clippy", "cargo test", "cargo build"]);
    }

    #[test]
    fn 新しい順に返す() {
        let h = seeded();
        let got = h.search("", Scope::All, 1, "/a", 10).unwrap();
        assert_eq!(got[0].command, "cargo clippy");
        assert_eq!(got.last().unwrap().command, "cargo build");
    }

    #[test]
    fn セッションで範囲を絞る() {
        let h = seeded();
        let got = h.search("", Scope::Session, 2, "/a", 10).unwrap();
        let cmds: Vec<_> = got.iter().map(|e| e.command.as_str()).collect();
        assert_eq!(cmds, vec!["cargo clippy", "git status"]);
    }

    #[test]
    fn 作業ディレクトリで範囲を絞る() {
        let h = seeded();
        let got = h.search("", Scope::Cwd, 1, "/a", 10).unwrap();
        let cmds: Vec<_> = got.iter().map(|e| e.command.as_str()).collect();
        assert_eq!(cmds, vec!["cargo test", "cargo build"]);
    }

    #[test]
    fn 同じコマンドは最新の一件にまとまる() {
        let h = History::open_memory().unwrap();
        h.record(&rec(1, "/a", "ls", 0, 1000)).unwrap();
        h.record(&rec(1, "/a", "ls", 0, 5000)).unwrap();
        let got = h.search("ls", Scope::All, 1, "/a", 10).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].started_at, 5000);
    }

    #[test]
    fn 検索語のワイルドカードを文字として扱う() {
        let h = History::open_memory().unwrap();
        h.record(&rec(1, "/a", "echo 100%", 0, 1000)).unwrap();
        h.record(&rec(1, "/a", "ls", 0, 2000)).unwrap();
        let got = h.search("%", Scope::All, 1, "/a", 10).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].command, "echo 100%");
    }

    #[test]
    fn 直近の履歴をセッションごとに返す() {
        let h = seeded();
        let got = h.recent(1, 10).unwrap();
        let cmds: Vec<_> = got.iter().map(|e| e.command.as_str()).collect();
        assert_eq!(cmds, vec!["cargo test", "cargo build"]);
    }

    #[test]
    fn 範囲の切り替えは三状態を巡回する() {
        assert_eq!(Scope::All.next(), Scope::Session);
        assert_eq!(Scope::Session.next(), Scope::Cwd);
        assert_eq!(Scope::Cwd.next(), Scope::All);
    }
}

//! セッションの木と、その生成・分岐。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::SystemTime;

use alacritty_terminal::event::WindowSize;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line, Point};
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::cell::Flags;

use crate::config::{self, Config, ExpandError, Vars};
use crate::pty::{self, PtyHandle, SpawnError};
use crate::term::{EventProxy, TermSize, UiSender};

pub type SessionId = u32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunState {
    Running,
    Exited(i32),
}

pub struct Session {
    pub id: SessionId,
    pub parent: Option<SessionId>,
    pub title: String,
    /// OSC 7 で通知された現在の作業ディレクトリ。fork はこちらを引き継ぐ。
    pub cwd: PathBuf,
    pub profile: String,
    pub agent_id: Option<String>,
    /// 親の会話を引き継いだか。引き継いでいない分岐は左ペインで区別する。
    pub inherited: bool,
    /// プロファイルで包む前のコマンド。分岐のフォールバックに使う。
    pub base_command: Vec<String>,
    pub state: RunState,
    pub term: Arc<FairMutex<alacritty_terminal::Term<EventProxy>>>,
    pub pty: PtyHandle,
    pub size: TermSize,
    pub window_size: Arc<FairMutex<WindowSize>>,
    pub dirty: Arc<AtomicBool>,
    /// 作業ディレクトリのブランチ名。git の下にいなければ `None`。
    pub branch: Option<String>,
    /// ブランチ名を最後に読んだ時刻と、そのときの作業ディレクトリ。
    branch_read: Option<(std::time::Instant, PathBuf)>,
    /// 端末上のプログラムが OSC 0 や OSC 2 で名乗った題名。
    ///
    /// 左ペインの名前はセッションの識別なので置き換えない。
    /// ウィンドウの題名だけをこれにする。
    pub window_title: Option<String>,
    /// この時刻まで、描くたびにスクロールバックを捨てる。
    ///
    /// 画面消去でシェルへ Ctrl+L を送ると、シェルは画面を消すのではなく
    /// 上へ押し出す。押し出されたぶんが履歴に積まれるので、
    /// 押し出しが終わるまでのあいだ捨て続ける。
    pub clear_scrollback_until: Option<std::time::Instant>,
}

impl Session {
    pub fn is_running(&self) -> bool {
        matches!(self.state, RunState::Running)
    }
}

/// 左ペインに並べる 1 行分の情報。
pub struct TreeRow {
    pub index: usize,
    pub depth: usize,
}

pub struct Manager {
    sessions: Vec<Session>,
    next_id: SessionId,
    selected: usize,
    cell: (u16, u16),
    size: TermSize,
    ui_tx: UiSender,
}

#[derive(Debug)]
pub enum SessionError {
    Template(ExpandError),
    Spawn(SpawnError),
    NoDocker(String),
}

impl std::fmt::Display for SessionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SessionError::Template(e) => write!(f, "コマンドを組み立てられない: {e}"),
            SessionError::Spawn(e) => write!(f, "{e}"),
            SessionError::NoDocker(p) => {
                write!(f, "profile.{p} は docker を使うが、docker が見つからない")
            }
        }
    }
}

impl Manager {
    pub fn new(size: TermSize, cell: (u16, u16), ui_tx: UiSender) -> Self {
        Self {
            sessions: Vec::new(),
            next_id: 1,
            selected: 0,
            cell,
            size,
            ui_tx,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.sessions.is_empty()
    }
    pub fn selected_index(&self) -> usize {
        self.selected.min(self.sessions.len().saturating_sub(1))
    }
    pub fn selected(&self) -> Option<&Session> {
        self.sessions.get(self.selected_index())
    }
    pub fn sessions(&self) -> &[Session] {
        &self.sessions
    }
    pub fn sessions_mut(&mut self) -> &mut [Session] {
        &mut self.sessions
    }

    /// 左ペインに出すブランチ名を読み直す。
    ///
    /// 作業ディレクトリが変わったときと、しばらく経ったときだけ読む。
    /// 描くたびにファイルを開くほどの情報ではない。
    pub fn refresh_branches(&mut self) {
        let now = std::time::Instant::now();
        for s in &mut self.sessions {
            let stale = match &s.branch_read {
                None => true,
                Some((at, dir)) => {
                    dir != &s.cwd || now.duration_since(*at).as_secs() >= 2
                }
            };
            if stale {
                s.branch = crate::git::branch_for(&s.cwd);
                s.branch_read = Some((now, s.cwd.clone()));
            }
        }
    }

    /// 画面消去を頼まれたセッションの履歴を、期限まで捨て続ける。
    pub fn drain_pending_clears(&mut self) {
        use alacritty_terminal::vte::ansi::{ClearMode, Handler as _};
        let now = std::time::Instant::now();
        for s in &mut self.sessions {
            let Some(until) = s.clear_scrollback_until else {
                continue;
            };
            if now >= until {
                s.clear_scrollback_until = None;
                continue;
            }
            s.term.lock().clear_screen(ClearMode::Saved);
        }
    }
    pub fn get_mut(&mut self, id: SessionId) -> Option<&mut Session> {
        self.sessions.iter_mut().find(|s| s.id == id)
    }

    pub fn select(&mut self, index: usize) {
        if index < self.sessions.len() {
            self.selected = index;
        }
    }
    pub fn select_next(&mut self) {
        if !self.sessions.is_empty() {
            self.selected = (self.selected_index() + 1) % self.sessions.len();
        }
    }
    pub fn select_prev(&mut self) {
        if !self.sessions.is_empty() {
            let n = self.sessions.len();
            self.selected = (self.selected_index() + n - 1) % n;
        }
    }

    pub fn set_cell(&mut self, cell: (u16, u16)) {
        self.cell = cell;
    }

    /// 端末領域のセル数が変わったとき、全ペインへ伝える。
    pub fn resize(&mut self, size: TermSize) {
        if size == self.size {
            return;
        }
        self.size = size;
        for s in &mut self.sessions {
            s.size = size;
            s.term.lock().resize(size);
            {
                let mut ws = s.window_size.lock();
                ws.num_cols = size.cols as u16;
                ws.num_lines = size.lines as u16;
                ws.cell_width = self.cell.0;
                ws.cell_height = self.cell.1;
            }
            s.pty.resize(size, self.cell.0, self.cell.1);
        }
    }

    /// 深さ優先で親の直後に子が並ぶ順序を作る。
    pub fn tree_rows(&self) -> Vec<TreeRow> {
        let mut rows = Vec::with_capacity(self.sessions.len());
        for (i, s) in self.sessions.iter().enumerate() {
            if s.parent.is_none() {
                self.push_subtree(i, 0, &mut rows);
            }
        }
        // 親が先に閉じられた場合に取り残されたものを拾う。
        if rows.len() < self.sessions.len() {
            let listed: Vec<usize> = rows.iter().map(|r| r.index).collect();
            for i in 0..self.sessions.len() {
                if !listed.contains(&i) {
                    rows.push(TreeRow { index: i, depth: 0 });
                }
            }
        }
        rows
    }

    fn push_subtree(&self, index: usize, depth: usize, rows: &mut Vec<TreeRow>) {
        rows.push(TreeRow { index, depth });
        let id = self.sessions[index].id;
        for (i, s) in self.sessions.iter().enumerate() {
            if s.parent == Some(id) {
                self.push_subtree(i, depth + 1, rows);
            }
        }
    }

    /// 新規セッションを起動する。
    pub fn spawn_new(
        &mut self,
        config: &Config,
        profile_name: &str,
        cwd: &Path,
    ) -> Result<SessionId, SessionError> {
        let new_id = uuid::Uuid::new_v4().to_string();
        let vars = Vars {
            new_id: Some(new_id.clone()),
            parent_agent_id: None,
            cwd: Some(cwd.to_string_lossy().to_string()),
            parent_title: None,
        };
        let (base, agent_id) = match &config.agent.new {
            Some(t) => {
                let argv = config::expand_template(t, &vars).map_err(SessionError::Template)?;
                let knows_id = config::template_vars(t).iter().any(|v| v == "new_id");
                (argv, knows_id.then_some(new_id))
            }
            None => (shell_argv(config), None),
        };
        let title = if self.sessions.is_empty() {
            "main".to_string()
        } else {
            format!("s-{}", self.next_id)
        };
        self.launch(config, profile_name, cwd, base, None, agent_id, title, true)
    }

    /// 選択中のセッションから分岐する。
    ///
    /// 分岐用のテンプレートが親のエージェント ID を必要とするのに
    /// それが未取得なら、親と同じコマンドを起動して会話は引き継がない。
    pub fn fork(
        &mut self,
        config: &Config,
        index: usize,
        profile_override: Option<&str>,
    ) -> Result<SessionId, SessionError> {
        let Some(parent) = self.sessions.get(index) else {
            return Err(SessionError::Template(ExpandError::Empty));
        };
        let parent_id = parent.id;
        let parent_title = parent.title.clone();
        let cwd = parent.cwd.clone();
        let profile = profile_override
            .unwrap_or(&parent.profile)
            .to_string();
        let parent_base = parent.base_command.clone();
        let new_id = uuid::Uuid::new_v4().to_string();
        let vars = Vars {
            new_id: Some(new_id.clone()),
            parent_agent_id: parent.agent_id.clone(),
            cwd: Some(cwd.to_string_lossy().to_string()),
            parent_title: Some(parent_title.clone()),
        };

        // テンプレートが {new_id} を使うなら、分岐先の ID は起動時点で確定する。
        // 使わないなら、エージェントが OSC で知らせてくるまで未取得のままになる。
        let (base, inherited, agent_id) = match &config.agent.fork {
            Some(t) => match config::expand_template(t, &vars) {
                Ok(argv) => {
                    let knows_id = config::template_vars(t).iter().any(|v| v == "new_id");
                    (argv, true, knows_id.then_some(new_id))
                }
                Err(ExpandError::MissingValue(_)) => (parent_base, false, None),
                Err(e) => return Err(SessionError::Template(e)),
            },
            None => (parent_base, false, None),
        };

        let n = self
            .sessions
            .iter()
            .filter(|s| s.title.starts_with("fork-"))
            .count()
            + 1;
        let title = format!("fork-{n}");
        self.launch(
            config,
            &profile,
            &cwd,
            base,
            Some(parent_id),
            agent_id,
            title,
            inherited,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn launch(
        &mut self,
        config: &Config,
        profile_name: &str,
        cwd: &Path,
        base: Vec<String>,
        parent: Option<SessionId>,
        agent_id: Option<String>,
        title: String,
        inherited: bool,
    ) -> Result<SessionId, SessionError> {
        let profile = config.profile(profile_name);
        if !profile.is_host() && !docker_available() {
            return Err(SessionError::NoDocker(profile_name.to_string()));
        }
        let argv = config::build_argv(&profile, cwd, &base);
        let id = self.next_id;
        let spawned = pty::spawn(
            id,
            &argv,
            cwd,
            self.size,
            self.cell,
            config.window.scrollback,
            self.ui_tx.clone(),
        )
        .map_err(SessionError::Spawn)?;
        self.next_id += 1;

        self.sessions.push(Session {
            id,
            parent,
            title,
            cwd: cwd.to_path_buf(),
            profile: profile_name.to_string(),
            agent_id,
            inherited,
            base_command: base,
            state: RunState::Running,
            term: spawned.term,
            pty: spawned.handle,
            size: self.size,
            window_size: spawned.window_size,
            dirty: spawned.dirty,
            branch: None,
            branch_read: None,
            window_title: None,
            clear_scrollback_until: None,
        });
        self.selected = self.sessions.len() - 1;
        Ok(id)
    }

    /// 選択中のセッションを終了させる。実行中なら停止、停止済みなら一覧から外す。
    pub fn close_selected(&mut self) {
        let i = self.selected_index();
        let Some(s) = self.sessions.get_mut(i) else {
            return;
        };
        if s.is_running() {
            s.pty.kill();
        } else {
            self.sessions.remove(i);
            if self.selected >= self.sessions.len() {
                self.selected = self.sessions.len().saturating_sub(1);
            }
        }
    }

    /// 描画したので、変更ありの印を落とす。
    pub fn clear_dirty(&self) {
        for s in &self.sessions {
            s.dirty.store(false, Ordering::Release);
        }
    }

    pub fn mark_exited(&mut self, id: SessionId, code: i32) {
        if let Some(s) = self.get_mut(id) {
            s.state = RunState::Exited(code);
        }
    }
}

fn shell_argv(config: &Config) -> Vec<String> {
    let program = config.shell.program.clone().unwrap_or_else(|| {
        std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string())
    });
    let mut argv = vec![program];
    if config.shell.args.is_empty() {
        argv.push("-l".to_string());
    } else {
        argv.extend(config.shell.args.iter().cloned());
    }
    argv
}

fn docker_available() -> bool {
    std::process::Command::new("docker")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

// ------------------------------------------------------- コマンドの取り出し

/// 記録が確定したコマンド。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandRecord {
    pub session_id: SessionId,
    pub agent_id: Option<String>,
    pub cwd: String,
    pub command: String,
    pub exit_code: Option<i32>,
    pub started_at: SystemTime,
    pub duration_ms: Option<i64>,
}

impl Session {
    /// OSC の通知でセッションの属性を更新する。
    ///
    /// コマンドの記録は読み取りスレッドが組み立てる。ここへ届く頃には
    /// 画面が先へ進んでいるため、この位置でグリッドを読むわけにはいかない。
    pub fn on_osc(&mut self, ev: &crate::osc::OscEvent) {
        use crate::osc::OscEvent::*;
        match ev {
            Cwd(path) => self.cwd = PathBuf::from(path),
            AgentId(id) => {
                self.agent_id = Some(id.clone());
                self.inherited = true;
            }
            _ => {}
        }
    }
}

/// グリッドの 2 点のあいだの文字を読み出す。
///
/// 入力の途中で画面がスクロールすると開始点が現在行より下になる。
/// その場合は現在行だけを読む。
pub fn grid_text(
    term: &alacritty_terminal::Term<EventProxy>,
    start: Point,
    end: Point,
) -> String {
    let grid = term.grid();
    let (start, end) = if start.line > end.line
        || (start.line == end.line && start.column > end.column)
    {
        (Point::new(end.line, Column(0)), end)
    } else {
        (start, end)
    };
    let mut out = String::new();
    let mut line = start.line;
    while line <= end.line {
        let first = if line == start.line { start.column.0 } else { 0 };
        let last = if line == end.line {
            end.column.0
        } else {
            grid.columns()
        };
        let row = &grid[line];
        for col in first..last.min(grid.columns()) {
            let cell = &row[Column(col)];
            if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                continue;
            }
            out.push(cell.c);
        }
        if line < end.line {
            // 折り返しでない改行だけを空白に落とす。
            while out.ends_with(' ') {
                out.pop();
            }
        }
        line = Line(line.0 + 1);
    }
    while out.ends_with(' ') {
        out.pop();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    #[test]
    fn 既定ではログインシェルを起動する() {
        let c = Config::default();
        let argv = shell_argv(&c);
        assert_eq!(argv.len(), 2);
        assert_eq!(argv[1], "-l");
    }

    #[test]
    fn 設定したシェルと引数を使う() {
        let mut c = Config::default();
        c.shell.program = Some("/bin/zsh".into());
        c.shell.args = vec!["-i".into()];
        assert_eq!(shell_argv(&c), vec!["/bin/zsh", "-i"]);
    }
}

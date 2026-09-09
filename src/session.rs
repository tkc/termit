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
use crate::term::{now_ms, EventProxy, TermSize, UiSender};

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
    /// 最後に出力があった時刻（起動からのミリ秒）。
    pub activity: Arc<std::sync::atomic::AtomicU64>,
    /// 左ペインに「動いている」と描いてあるか。
    ///
    /// 描いた状態を覚えておき、変わるときだけ描き直す。
    /// これが無いと、背景で動いているセッションの通知を
    /// 一つ残らず描き直しに使うことになる。
    pub shown_working: bool,
    /// 再起動をまたいで残る鍵。コマンド履歴をこの単位で辿る。
    pub key: String,
    /// 利用者が付けた名前。付けていなければ作業ディレクトリを名前にする。
    pub name: Option<String>,
    /// 作業ディレクトリのブランチ名。git の下にいなければ `None`。
    pub branch: Option<String>,
    /// ブランチ名を最後に読んだ時刻と、そのときの作業ディレクトリ。
    branch_read: Option<(std::time::Instant, PathBuf)>,
    /// 作業ディレクトリを最後に OS へ尋ねた時刻。
    cwd_polled: Option<std::time::Instant>,
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

/// これだけ出力が途切れたら、止まっていると見なす（ミリ秒）。
///
/// エージェントの全画面 UI は考えているあいだ絵を回すので、
/// 出力が続いているかどうかが、そのまま働いているかどうかになる。
pub const WORKING_QUIET_MS: u64 = 500;

impl Session {
    pub fn is_running(&self) -> bool {
        matches!(self.state, RunState::Running)
    }

    /// いま何かを出しているか。
    pub fn is_working(&self) -> bool {
        self.is_running() && Self::working_at(self.activity.load(Ordering::Relaxed), now_ms())
    }

    /// 最後の出力から `now` までの間で、働いていると見なすか。
    fn working_at(last: u64, now: u64) -> bool {
        now.saturating_sub(last) < WORKING_QUIET_MS
    }

    /// 左ペインに出す名前と、それがパスかどうか。
    ///
    /// 付けた名前があればそれを、なければ作業ディレクトリを出す。
    /// パスは末尾のほうが手がかりになるので、切るときは先頭を落とす。
    /// 分岐の系統は字下げで示すので、名前には入れない。
    pub fn display_name(&self) -> (String, bool) {
        match &self.name {
            Some(n) => (n.clone(), false),
            None => (crate::git::short_path(&self.cwd, usize::MAX), true),
        }
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
    /// 引きずるあいだ、まだ全部へ伝えていない大きさ。
    pending: Option<TermSize>,
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
            SessionError::Template(e) => write!(f, "cannot build command: {e}"),
            SessionError::Spawn(e) => write!(f, "{e}"),
            SessionError::NoDocker(p) => {
                write!(f, "profile.{p} needs docker, but docker was not found")
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
            pending: None,
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

    /// 左ペインに出す作業ディレクトリとブランチ名を読み直す。
    ///
    /// 作業ディレクトリは OS に尋ねる。OSC 7 だけに頼ると、シェル統合を
    /// 入れていない利用者では起動時の位置から動かない。
    /// どちらかが先に変化を捉えれば、そこで更新される。
    ///
    /// 作業ディレクトリが変わったら `true` を返す。覚えている並びの
    /// 書き直しに使う。
    pub fn refresh_metadata(&mut self) -> bool {
        let now = std::time::Instant::now();
        let mut moved = false;
        for s in &mut self.sessions {
            // 走っているセッションだけ尋ねる。終わったものは動かない。
            let due = match s.cwd_polled {
                None => true,
                Some(at) => now.duration_since(at).as_millis() >= 400,
            };
            if s.is_running() && due {
                s.cwd_polled = Some(now);
                if let Some(pid) = s.pty.foreground_pid() {
                    if let Some(dir) = crate::cwd::of_pid(pid) {
                        if dir != s.cwd {
                            s.cwd = dir;
                            moved = true;
                        }
                    }
                }
            }
            let stale = match &s.branch_read {
                None => true,
                Some((at, dir)) => dir != &s.cwd || now.duration_since(*at).as_secs() >= 2,
            };
            if stale {
                s.branch = crate::git::branch_for(&s.cwd);
                s.branch_read = Some((now, s.cwd.clone()));
            }
        }
        moved
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
            self.ensure_visible_size();
        }
    }

    /// 出す前に、見えているセッションの桁数を合わせる。
    ///
    /// 引きずるあいだ待たせていたものへ切り替わることがある。
    /// 待たせたまま描くと、違う桁で組んだ画面が出る。
    fn ensure_visible_size(&mut self) {
        if let Some(size) = self.pending {
            self.apply_size(self.selected_index(), size);
        }
    }
    pub fn select_next(&mut self) {
        if !self.sessions.is_empty() {
            self.select((self.selected_index() + 1) % self.sessions.len());
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
        self.size = size;
        self.pending = None;
        for i in 0..self.sessions.len() {
            self.apply_size(i, size);
        }
    }

    /// 見えているセッションだけを先に合わせる。
    ///
    /// 桁数が変わるとグリッドを組み直す。行が多いほど高くつくので、
    /// 境目や窓の縁を掴んで引きずるあいだ全部へ伝えていると追いつかない。
    /// 残りは手が止まってから [`Manager::settle`] で合わせる。
    pub fn resize_visible(&mut self, size: TermSize) {
        if size == self.size {
            return;
        }
        self.size = size;
        self.pending = Some(size);
        self.apply_size(self.selected_index(), size);
    }

    /// 待たせていたセッションを、いまの大きさへ合わせる。
    pub fn settle(&mut self) {
        let Some(size) = self.pending.take() else {
            return;
        };
        for i in 0..self.sessions.len() {
            self.apply_size(i, size);
        }
    }

    /// まだ大きさが違うセッションだけを合わせる。
    fn apply_size(&mut self, index: usize, size: TermSize) {
        let cell = self.cell;
        let Some(s) = self.sessions.get_mut(index) else {
            return;
        };
        if s.size == size {
            return;
        }
        s.size = size;
        s.term.lock().resize(size);
        {
            let mut ws = s.window_size.lock();
            ws.num_cols = size.cols as u16;
            ws.num_lines = size.lines as u16;
            ws.cell_width = cell.0;
            ws.cell_height = cell.1;
        }
        s.pty.resize(size, cell.0, cell.1);
    }

    /// 掴んだ行が、その子孫ともども占める行数。
    fn group_len(rows: &[TreeRow], from_row: usize) -> usize {
        let depth = rows[from_row].depth;
        let mut n = 1;
        for r in &rows[from_row + 1..] {
            if r.depth <= depth {
                break;
            }
            n += 1;
        }
        n
    }

    /// 落とせる位置に丸める。行と行のあいだを指す 0..=行数 を返す。
    ///
    /// 根は根のあいだへ、子は同じ親の下へしか動かせない。
    /// 木の形は並べ替えでは変えない。丸めた結果を目印として描くので、
    /// 見えている位置と落ちる位置が食い違わない。
    pub fn drop_row(&self, from_row: usize, want: usize) -> Option<usize> {
        let rows = self.tree_rows();
        if from_row >= rows.len() {
            return None;
        }
        let len = Self::group_len(&rows, from_row);
        let depth = rows[from_row].depth;
        // 自分の中は落とし先にならない。
        if want > from_row && want <= from_row + len {
            return None;
        }
        // 同じ深さの行の頭だけが境目になる。末尾も含める。
        let mut stops: Vec<usize> = rows
            .iter()
            .enumerate()
            .filter(|(_, r)| r.depth == depth)
            .map(|(i, _)| i)
            .collect();
        if depth == 0 {
            stops.push(rows.len());
        } else {
            // 子は親の連なりの終わりまで。
            let parent = rows[..from_row].iter().rposition(|r| r.depth < depth)?;
            let end = parent + Self::group_len(&rows, parent);
            stops.push(end);
            stops.retain(|&i| i > parent && i <= end);
        }
        stops
            .into_iter()
            .min_by_key(|&i| want.abs_diff(i))
            .filter(|&i| !(i > from_row && i <= from_row + len))
    }

    /// 行を掴んで並べ替える。掴んだ行にぶら下がるものは一緒に動く。
    ///
    /// `insert_at` は [`Manager::drop_row`] で丸めた境目である。
    /// 動かすのは並びだけで、親子の関係は変えない。
    pub fn reorder(&mut self, from_row: usize, insert_at: usize) -> bool {
        let rows = self.tree_rows();
        if from_row >= rows.len() || insert_at > rows.len() {
            return false;
        }
        let len = Self::group_len(&rows, from_row);
        if insert_at > from_row && insert_at <= from_row + len {
            return false;
        }
        let before: Vec<usize> = rows.iter().map(|r| r.index).collect();
        let mut order = before.clone();
        let group: Vec<usize> = order.drain(from_row..from_row + len).collect();
        let at = if insert_at > from_row {
            insert_at - len
        } else {
            insert_at
        }
        .min(order.len());
        for (k, idx) in group.into_iter().enumerate() {
            order.insert(at + k, idx);
        }
        if order == before {
            return false;
        }
        // 並びのとおりに作り直す。選んでいたセッションは追いかける。
        let selected_id = self.sessions.get(self.selected_index()).map(|s| s.id);
        let mut taken: Vec<Option<Session>> = self.sessions.drain(..).map(Some).collect();
        self.sessions = order.iter().filter_map(|&i| taken[i].take()).collect();
        // 拾い残しがあれば末尾へ。並びが崩れてもセッションは失わない。
        for s in taken.into_iter().flatten() {
            self.sessions.push(s);
        }
        if let Some(id) = selected_id {
            if let Some(p) = self.sessions.iter().position(|s| s.id == id) {
                self.selected = p;
            }
        }
        self.ensure_visible_size();
        true
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
            agent_id: None,
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
        self.launch(
            config,
            profile_name,
            cwd,
            base,
            None,
            agent_id,
            title,
            true,
            None,
        )
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
        let profile = profile_override.unwrap_or(&parent.profile).to_string();
        let parent_base = parent.base_command.clone();
        let new_id = uuid::Uuid::new_v4().to_string();
        let vars = Vars {
            new_id: Some(new_id.clone()),
            parent_agent_id: parent.agent_id.clone(),
            agent_id: None,
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
            None,
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
        key: Option<String>,
    ) -> Result<SessionId, SessionError> {
        let profile = config.profile(profile_name);
        if !profile.is_host() && !docker_available() {
            return Err(SessionError::NoDocker(profile_name.to_string()));
        }
        let argv = config::build_argv(&profile, cwd, &base);
        let id = self.next_id;
        let key = key.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let spawned = pty::spawn(
            pty::SpawnOptions {
                id,
                argv: &argv,
                cwd,
                size: self.size,
                cell: self.cell,
                scrollback: config.window.scrollback,
                session_key: key.clone(),
            },
            self.ui_tx.clone(),
        )
        .map_err(SessionError::Spawn)?;
        self.next_id += 1;

        self.sessions.push(Session {
            id,
            key,
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
            activity: spawned.activity,
            shown_working: false,
            name: None,
            branch: None,
            branch_read: None,
            cwd_polled: None,
            window_title: None,
            clear_scrollback_until: None,
        });
        self.selected = self.sessions.len() - 1;
        Ok(id)
    }

    /// いまの並びを、次の起動で作り直せる形に写し取る。
    pub fn snapshot(&self) -> crate::state::SavedState {
        let index_of: std::collections::HashMap<SessionId, usize> = self
            .sessions
            .iter()
            .enumerate()
            .map(|(i, s)| (s.id, i))
            .collect();
        crate::state::SavedState {
            version: crate::state::VERSION,
            selected: self.selected_index(),
            sessions: self
                .sessions
                .iter()
                .map(|s| crate::state::SavedSession {
                    key: Some(s.key.clone()),
                    name: s.name.clone(),
                    cwd: s.cwd.to_string_lossy().to_string(),
                    profile: s.profile.clone(),
                    agent_id: s.agent_id.clone(),
                    parent: s.parent.and_then(|p| index_of.get(&p).copied()),
                    command: s.base_command.clone(),
                })
                .collect(),
        }
    }

    /// 覚えていた並びを作り直す。作れたセッションの数を返す。
    ///
    /// 会話 ID が分かっていて再開のテンプレートがあれば、それで会話を続ける。
    /// なければ、覚えていたコマンドをそのまま動かす。
    pub fn restore(&mut self, config: &Config, saved: &crate::state::SavedState) -> usize {
        let mut ids: Vec<Option<SessionId>> = Vec::with_capacity(saved.sessions.len());
        for s in &saved.sessions {
            let parent = s.parent.and_then(|p| ids.get(p).copied().flatten());
            match self.restore_one(config, s, parent) {
                Ok(id) => ids.push(Some(id)),
                Err(e) => {
                    log::warn!("cannot restore session in {}: {e}", s.cwd);
                    ids.push(None);
                }
            }
        }
        let made = ids.iter().filter(|i| i.is_some()).count();
        if let Some(Some(_)) = ids.get(saved.selected) {
            self.selected = saved.selected.min(self.sessions.len().saturating_sub(1));
        }
        made
    }

    fn restore_one(
        &mut self,
        config: &Config,
        saved: &crate::state::SavedSession,
        parent: Option<SessionId>,
    ) -> Result<SessionId, SessionError> {
        let cwd = PathBuf::from(&saved.cwd);
        // 覚えていた作業ディレクトリが無くなっていることがある。
        let cwd = if cwd.is_dir() {
            cwd
        } else {
            dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"))
        };
        let vars = Vars {
            new_id: Some(uuid::Uuid::new_v4().to_string()),
            parent_agent_id: None,
            agent_id: saved.agent_id.clone(),
            cwd: Some(cwd.to_string_lossy().to_string()),
            parent_title: None,
        };
        let (base, agent_id) = match (&config.agent.resume, &saved.agent_id) {
            (Some(t), Some(_)) => match config::expand_template(t, &vars) {
                Ok(argv) => (argv, saved.agent_id.clone()),
                Err(_) => (saved.command.clone(), saved.agent_id.clone()),
            },
            _ => (saved.command.clone(), saved.agent_id.clone()),
        };
        let base = if base.is_empty() {
            shell_argv(config)
        } else {
            base
        };
        let title = if parent.is_none() {
            "main".to_string()
        } else {
            format!("s-{}", self.next_id)
        };
        let id = self.launch(
            config,
            &saved.profile,
            &cwd,
            base,
            parent,
            agent_id,
            title,
            true,
            saved.key.clone(),
        )?;
        if let Some(s) = self.sessions.last_mut() {
            s.name = saved.name.clone();
        }
        Ok(id)
    }

    /// 選択中のセッションを閉じる。
    pub fn close_selected(&mut self) {
        self.close(self.selected_index());
    }

    /// 番号で指定したセッションを閉じる。走っていれば止め、一覧から外す。
    ///
    /// 止めるだけで行を残すと、消せない行が居座っているように見える。
    /// 閉じる操作は一度で閉じきる。
    pub fn close(&mut self, index: usize) {
        let Some(s) = self.sessions.get_mut(index) else {
            return;
        };
        s.pty.kill();
        self.sessions.remove(index);
        // 消した行が選択より前なら、選択はその分だけ前へずれる。
        if index < self.selected {
            self.selected -= 1;
        } else if self.selected >= self.sessions.len() {
            self.selected = self.sessions.len().saturating_sub(1);
        }
        self.ensure_visible_size();
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
    let program = config
        .shell
        .program
        .clone()
        .unwrap_or_else(|| std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string()));
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
    /// 再起動をまたいで残るセッションの鍵。履歴を辿るのに使う。
    pub session_key: String,
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
pub fn grid_text(term: &alacritty_terminal::Term<EventProxy>, start: Point, end: Point) -> String {
    let grid = term.grid();
    let (start, end) =
        if start.line > end.line || (start.line == end.line && start.column > end.column) {
            (Point::new(end.line, Column(0)), end)
        } else {
            (start, end)
        };
    let mut out = String::new();
    let mut line = start.line;
    while line <= end.line {
        let first = if line == start.line {
            start.column.0
        } else {
            0
        };
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

    fn probe_manager() -> Manager {
        let (tx, _rx) = std::sync::mpsc::channel();
        Manager::new(
            TermSize::new(40, 10),
            (8, 16),
            crate::term::UiSender::Channel(tx),
        )
    }

    fn probe_config() -> Config {
        let mut c = Config::default();
        c.shell.program = Some("/bin/sh".into());
        c.shell.args = vec!["-c".into(), "sleep 20".into()];
        c
    }

    /// その pid がまだ生きているか。合図は送らずに存在だけ見る。
    fn alive(pid: i32) -> bool {
        unsafe { libc::kill(pid, 0) == 0 }
    }

    /// pid が消えるまで待つ。消えなければ `false`。
    fn wait_gone(pid: i32) -> bool {
        for _ in 0..200 {
            if !alive(pid) {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        false
    }

    fn resize_manager(n: usize) -> Option<(Manager, Config)> {
        if !std::path::Path::new("/bin/sh").exists() {
            return None;
        }
        let config = probe_config();
        let cwd = std::env::current_dir().unwrap();
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut m = Manager::new(
            TermSize::new(80, 24),
            (8, 17),
            crate::term::UiSender::Channel(tx),
        );
        for _ in 0..n {
            m.spawn_new(&config, "host", &cwd).expect("作れる");
        }
        Some((m, config))
    }

    /// 引きずるあいだは見えているものだけ組み直すことを確かめる。
    #[test]
    fn 引きずるあいだは見えているものだけ合わせる() {
        let Some((mut m, _)) = resize_manager(4) else {
            return;
        };
        m.select(1);
        let want = TermSize::new(70, 24);
        m.resize_visible(want);
        assert_eq!(m.sessions()[1].size, want, "見えているものは合っている");
        for i in [0, 2, 3] {
            assert_eq!(
                m.sessions()[i].size,
                TermSize::new(80, 24),
                "{i} 本目はまだ待っている"
            );
        }
        for s in m.sessions_mut() {
            s.pty.kill();
        }
    }

    /// 手が止まったら残りも合うことを確かめる。
    #[test]
    fn 手が止まれば残りも合わせる() {
        let Some((mut m, _)) = resize_manager(4) else {
            return;
        };
        let want = TermSize::new(70, 24);
        m.resize_visible(want);
        m.settle();
        for i in 0..4 {
            assert_eq!(m.sessions()[i].size, want, "{i} 本目も合っている");
        }
        assert!(m.pending.is_none(), "待たせているものは無い");
        // 二度目の settle は何もしない。
        m.settle();
        for s in m.sessions_mut() {
            s.pty.kill();
        }
    }

    /// 待たせたセッションへ切り替えたら、出す前に合うことを確かめる。
    #[test]
    fn 待たせたセッションは選んだ時点で合わせる() {
        let Some((mut m, _)) = resize_manager(3) else {
            return;
        };
        m.select(0);
        let want = TermSize::new(70, 24);
        m.resize_visible(want);
        assert_ne!(m.sessions()[2].size, want, "まだ待っている");

        m.select(2);
        assert_eq!(
            m.sessions()[2].size,
            want,
            "選んだ時点で合っていないと、違う桁で描いてしまう"
        );
        for s in m.sessions_mut() {
            s.pty.kill();
        }
    }

    /// 同じ大きさで呼んでも組み直さないことを確かめる。
    #[test]
    fn 大きさが変わらなければ何もしない() {
        let Some((mut m, _)) = resize_manager(2) else {
            return;
        };
        let same = TermSize::new(80, 24);
        m.resize_visible(same);
        assert!(m.pending.is_none(), "待たせるものが出ない");
        for s in m.sessions_mut() {
            s.pty.kill();
        }
    }

    #[test]
    fn 出力が続いているあいだは動いていると見なす() {
        // 全画面 UI は考えているあいだ絵を回す。出力が続く。
        assert!(Session::working_at(1000, 1000), "出したばかり");
        assert!(Session::working_at(1000, 1000 + WORKING_QUIET_MS - 1));
    }

    #[test]
    fn 出力が途切れたら止まっていると見なす() {
        assert!(!Session::working_at(1000, 1000 + WORKING_QUIET_MS));
        assert!(!Session::working_at(1000, 10_000));
    }

    #[test]
    fn 時計が戻っても動いている扱いにしない() {
        // 引き算があふれると、いつまでも動いていることになる。
        assert!(Session::working_at(5000, 1000), "戻ったぶんは 0 と見る");
    }

    #[test]
    fn 終わったセッションは動いていない() {
        let Some((mut m, _)) = ordered(1) else { return };
        assert!(m.sessions()[0].is_working() || !m.sessions()[0].is_working());
        let id = m.sessions()[0].id;
        m.mark_exited(id, 0);
        assert!(!m.sessions()[0].is_working(), "終わっていれば動いていない");
        for s in m.sessions_mut() {
            s.pty.kill();
        }
    }

    /// 並べ替え用に n 本立ち上げ、名前で見分けられるようにする。
    fn ordered(n: usize) -> Option<(Manager, Config)> {
        if !std::path::Path::new("/bin/sh").exists() {
            return None;
        }
        let config = probe_config();
        let cwd = std::env::current_dir().unwrap();
        let mut m = probe_manager();
        for i in 0..n {
            m.spawn_new(&config, "host", &cwd).expect("作れる");
            m.sessions_mut()[i].name = Some(format!("s{i}"));
        }
        Some((m, config))
    }

    /// 上から順の名前。
    fn names(m: &Manager) -> Vec<String> {
        m.tree_rows()
            .iter()
            .map(|r| m.sessions()[r.index].name.clone().unwrap_or_default())
            .collect()
    }

    #[test]
    fn 行を掴んで先頭へ動かせる() {
        let Some((mut m, _)) = ordered(4) else { return };
        assert_eq!(names(&m), ["s0", "s1", "s2", "s3"]);
        assert!(m.reorder(2, 0));
        assert_eq!(names(&m), ["s2", "s0", "s1", "s3"]);
        for s in m.sessions_mut() {
            s.pty.kill();
        }
    }

    #[test]
    fn 行を掴んで末尾へ動かせる() {
        let Some((mut m, _)) = ordered(4) else { return };
        assert!(m.reorder(0, 4));
        assert_eq!(names(&m), ["s1", "s2", "s3", "s0"]);
        for s in m.sessions_mut() {
            s.pty.kill();
        }
    }

    #[test]
    fn 同じ場所へ落としても何も起きない() {
        let Some((mut m, _)) = ordered(3) else { return };
        assert!(!m.reorder(1, 1), "自分の頭は動かない");
        assert!(!m.reorder(1, 2), "自分の後ろも動かない");
        assert_eq!(names(&m), ["s0", "s1", "s2"]);
        for s in m.sessions_mut() {
            s.pty.kill();
        }
    }

    #[test]
    fn 選んでいたセッションは動いても選ばれたまま() {
        let Some((mut m, _)) = ordered(4) else { return };
        m.select(2);
        let id = m.sessions()[2].id;
        assert!(m.reorder(2, 0));
        assert_eq!(m.selected().map(|s| s.id), Some(id));
        assert_eq!(m.selected_index(), 0);
        for s in m.sessions_mut() {
            s.pty.kill();
        }
    }

    #[test]
    fn 分岐は親ごと動き親子の関係は変わらない() {
        let Some((mut m, config)) = ordered(3) else {
            return;
        };
        // s1 の下に子を 2 つ作る。
        m.fork(&config, 1, None).expect("子を作れる");
        m.sessions_mut()[3].name = Some("c1".into());
        m.fork(&config, 1, None).expect("子をもう 1 つ");
        m.sessions_mut()[4].name = Some("c2".into());
        assert_eq!(names(&m), ["s0", "s1", "c1", "c2", "s2"]);

        // s1 の行を掴むと、子ごと先頭へ動く。
        assert!(m.reorder(1, 0));
        assert_eq!(names(&m), ["s1", "c1", "c2", "s0", "s2"]);
        // 親子の関係はそのまま。
        let parent = m.sessions()[0].id;
        assert_eq!(m.sessions()[1].parent, Some(parent));
        assert_eq!(m.sessions()[2].parent, Some(parent));
        for s in m.sessions_mut() {
            s.pty.kill();
        }
    }

    #[test]
    fn 自分の中へは落とせない() {
        let Some((mut m, config)) = ordered(2) else {
            return;
        };
        m.fork(&config, 0, None).expect("子を作れる");
        m.sessions_mut()[2].name = Some("c".into());
        assert_eq!(names(&m), ["s0", "c", "s1"]);
        // 行 0 は s0 とその子。行 1 は自分の中。
        assert_eq!(m.drop_row(0, 1), None, "自分の中は落とし先にならない");
        assert!(!m.reorder(0, 1));
        for s in m.sessions_mut() {
            s.pty.kill();
        }
    }

    #[test]
    fn 子は親の下から出られない() {
        let Some((mut m, config)) = ordered(2) else {
            return;
        };
        m.fork(&config, 0, None).expect("子を作れる");
        m.sessions_mut()[2].name = Some("c".into());
        assert_eq!(names(&m), ["s0", "c", "s1"]);
        // 子（行 1）を先頭へ落とそうとしても、親の下へ丸められる。
        let at = m.drop_row(1, 0).expect("落とし先はある");
        assert_eq!(at, 1, "親の直後より前へは行かない");
        for s in m.sessions_mut() {
            s.pty.kill();
        }
    }

    #[test]
    fn 並べ替えた順は覚えて作り直せる() {
        let Some((mut m, config)) = ordered(3) else {
            return;
        };
        assert!(m.reorder(2, 0));
        assert_eq!(names(&m), ["s2", "s0", "s1"]);

        let saved = m.snapshot();
        for s in m.sessions_mut() {
            s.pty.kill();
        }
        // 親の添字は自分より前を指す、という決まりを守っている。
        let text = toml::to_string_pretty(&saved).unwrap();
        let reloaded = crate::state::SavedState::parse(&text).expect("読み戻せる");

        let mut m2 = probe_manager();
        assert_eq!(m2.restore(&config, &reloaded), 3);
        assert_eq!(names(&m2), ["s2", "s0", "s1"], "並べ替えた順で戻る");
        for s in m2.sessions_mut() {
            s.pty.kill();
        }
    }

    #[test]
    fn 分岐を並べ替えても親の添字は前を指す() {
        let Some((mut m, config)) = ordered(2) else {
            return;
        };
        m.fork(&config, 0, None).expect("子を作れる");
        m.sessions_mut()[2].name = Some("c".into());
        assert!(m.reorder(0, 3), "親と子を末尾へ動かす");
        assert_eq!(names(&m), ["s1", "s0", "c"]);

        let saved = m.snapshot();
        for s in m.sessions_mut() {
            s.pty.kill();
        }
        for (i, s) in saved.sessions.iter().enumerate() {
            if let Some(p) = s.parent {
                assert!(p < i, "{i} 番目の親 {p} は自分より前");
            }
        }
        let text = toml::to_string_pretty(&saved).unwrap();
        let reloaded = crate::state::SavedState::parse(&text).expect("読み戻せる");
        let mut m2 = probe_manager();
        assert_eq!(m2.restore(&config, &reloaded), 3);
        assert_eq!(names(&m2), ["s1", "s0", "c"]);
        for s in m2.sessions_mut() {
            s.pty.kill();
        }
    }

    /// 走っているセッションでも一度で閉じられることを確かめる。
    #[test]
    fn 走っているセッションも一度で閉じる() {
        if !std::path::Path::new("/bin/sh").exists() {
            return;
        }
        let config = probe_config();
        let cwd = std::env::current_dir().unwrap();
        let mut m = probe_manager();
        m.spawn_new(&config, "host", &cwd).expect("1 つめを作れる");
        m.spawn_new(&config, "host", &cwd).expect("2 つめを作れる");
        m.select(1);
        assert!(m.sessions()[1].is_running());

        m.close_selected();
        assert_eq!(m.sessions().len(), 1, "走っていても一度で消える");
        assert_eq!(m.selected_index(), 0, "選択が残った側へ寄る");
        for s in m.sessions_mut() {
            s.pty.kill();
        }
    }

    /// 止まったセッションも同じように外れることを確かめる。
    #[test]
    fn 止まったセッションを一覧から外せる() {
        if !std::path::Path::new("/bin/sh").exists() {
            return;
        }
        let config = probe_config();
        let cwd = std::env::current_dir().unwrap();
        let mut m = probe_manager();
        m.spawn_new(&config, "host", &cwd).expect("1 つめを作れる");
        m.spawn_new(&config, "host", &cwd).expect("2 つめを作れる");
        let id = m.sessions()[1].id;
        m.mark_exited(id, 130);
        m.select(1);
        assert!(!m.sessions()[1].is_running());

        m.close_selected();
        assert_eq!(m.sessions().len(), 1, "止まっていても一度で消える");
        for s in m.sessions_mut() {
            s.pty.kill();
        }
    }

    /// 閉じたあとにプロセスが残らないことを確かめる。
    ///
    /// 一度で行が消えるようになったぶん、殺し損ねると気付けない。
    /// シェルだけでなく、その下で走っているものまで落ちること。
    #[test]
    fn 閉じたセッションのプロセスは残らない() {
        if !std::path::Path::new("/bin/sh").exists() {
            return;
        }
        // 孫の pid をファイルへ書かせる。シェルは wait で居座る。
        let note = std::env::temp_dir().join(format!("termit-close-{}.pid", std::process::id()));
        let _ = std::fs::remove_file(&note);
        let mut config = probe_config();
        config.shell.args = vec![
            "-c".into(),
            format!("sleep 30 & echo $! > {}; wait", note.display()),
        ];

        let cwd = std::env::current_dir().unwrap();
        let mut m = probe_manager();
        m.spawn_new(&config, "host", &cwd).expect("作れる");
        let shell = m.sessions()[0].pty.foreground_pid().expect("pid が取れる");

        let mut child = 0;
        for _ in 0..200 {
            if let Ok(t) = std::fs::read_to_string(&note) {
                if let Ok(p) = t.trim().parse::<i32>() {
                    child = p;
                    break;
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let _ = std::fs::remove_file(&note);
        assert!(child > 0, "孫の pid を受け取れる");
        assert!(alive(shell) && alive(child), "閉じる前は両方走っている");

        m.close_selected();
        assert!(m.is_empty(), "一覧から消える");
        assert!(wait_gone(shell), "シェル {shell} が残っている");
        assert!(wait_gone(child), "その下の {child} が残っている");
    }

    /// 選択より前の行を閉じても、選択が別のセッションへ移らないことを確かめる。
    #[test]
    fn 前の行を閉じても選択はずれない() {
        if !std::path::Path::new("/bin/sh").exists() {
            return;
        }
        let config = probe_config();
        let cwd = std::env::current_dir().unwrap();
        let mut m = probe_manager();
        for _ in 0..3 {
            m.spawn_new(&config, "host", &cwd).expect("作れる");
        }
        m.select(2);
        let want = m.sessions()[2].id;

        // 鼠で「×」を押す道は、選択と関係ない行を閉じる。
        m.close(0);
        assert_eq!(m.sessions().len(), 2);
        assert_eq!(
            m.selected().map(|s| s.id),
            Some(want),
            "選んでいたセッションのまま"
        );
        for s in m.sessions_mut() {
            s.pty.kill();
        }
    }

    /// 覚えた並びから作り直したとき、木の形と名前が戻ることを確かめる。
    #[test]
    fn 覚えた並びを作り直せる() {
        if !std::path::Path::new("/bin/sh").exists() {
            return;
        }
        let config = probe_config();
        let cwd = std::env::current_dir().unwrap();
        let mut m = probe_manager();
        m.spawn_new(&config, "host", &cwd).expect("親を作れる");
        m.fork(&config, 0, None).expect("子を作れる");
        m.sessions_mut()[1].name = Some("child".into());
        m.sessions_mut()[0].agent_id = Some("abc-123".into());
        m.select(1);

        let saved = m.snapshot();
        assert_eq!(saved.sessions.len(), 2);
        assert_eq!(saved.selected, 1);
        assert_eq!(saved.sessions[0].parent, None);
        assert_eq!(saved.sessions[1].parent, Some(0));
        assert_eq!(saved.sessions[0].agent_id.as_deref(), Some("abc-123"));
        assert_eq!(saved.sessions[1].name.as_deref(), Some("child"));
        for s in m.sessions_mut() {
            s.pty.kill();
        }

        // 書いて読み戻しても同じであること。
        let text = toml::to_string_pretty(&saved).unwrap();
        let reloaded = crate::state::SavedState::parse(&text).expect("読み戻せる");
        assert_eq!(reloaded, saved);

        let mut m2 = probe_manager();
        let made = m2.restore(&config, &reloaded);
        assert_eq!(made, 2, "2 つとも作り直せる");
        let rows = m2.tree_rows();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].depth, 0);
        assert_eq!(rows[1].depth, 1, "親子の関係が戻る");
        assert_eq!(m2.sessions()[1].name.as_deref(), Some("child"));
        assert_eq!(m2.sessions()[0].agent_id.as_deref(), Some("abc-123"));
        assert_eq!(m2.selected_index(), 1, "選んでいた位置が戻る");
        for s in m2.sessions_mut() {
            s.pty.kill();
        }
    }

    /// 覚えていた作業ディレクトリが無くなっていても、起動そのものは通ること。
    #[test]
    fn 消えた作業ディレクトリでも作り直せる() {
        if !std::path::Path::new("/bin/sh").exists() {
            return;
        }
        let config = probe_config();
        let saved = crate::state::SavedState {
            version: crate::state::VERSION,
            selected: 0,
            sessions: vec![crate::state::SavedSession {
                key: None,
                name: None,
                cwd: "/no/such/directory/at/all".into(),
                profile: "host".into(),
                agent_id: None,
                parent: None,
                command: vec!["/bin/sh".into(), "-c".into(), "sleep 20".into()],
            }],
        };
        let mut m = probe_manager();
        assert_eq!(m.restore(&config, &saved), 1);
        for s in m.sessions_mut() {
            s.pty.kill();
        }
    }

    #[test]
    fn 設定したシェルと引数を使う() {
        let mut c = Config::default();
        c.shell.program = Some("/bin/zsh".into());
        c.shell.args = vec!["-i".into()];
        assert_eq!(shell_argv(&c), vec!["/bin/zsh", "-i"]);
    }
}

#[cfg(test)]
mod cwd_tests {
    use super::*;
    use crate::config::Config;
    use std::time::{Duration, Instant};

    fn manager() -> Manager {
        let (tx, _rx) = std::sync::mpsc::channel();
        Manager::new(
            TermSize::new(60, 12),
            (8, 16),
            crate::term::UiSender::Channel(tx),
        )
    }

    /// シェル統合を入れていなくても、cd に追従することを確かめる。
    ///
    /// OSC 7 は出さないシェルを使う。作業ディレクトリは OS へ尋ねて得る。
    #[test]
    fn シェル統合が無くても_cd_に追従する() {
        if !std::path::Path::new("/bin/sh").exists() {
            return;
        }
        let mut config = Config::default();
        config.shell.program = Some("/bin/sh".into());
        config.shell.args = vec!["-i".into()];
        let start = std::env::current_dir().unwrap();
        let mut m = manager();
        m.spawn_new(&config, "host", &start).expect("起動できる");
        assert_eq!(m.sessions()[0].cwd, start);

        // OSC 7 を出さないまま cd する。
        m.sessions()[0].pty.write(b"cd /usr/lib\n".to_vec());

        let deadline = Instant::now() + Duration::from_secs(10);
        let mut moved = false;
        while Instant::now() < deadline && !moved {
            std::thread::sleep(Duration::from_millis(100));
            // 実際の描画と同じく、間隔を空けて尋ね直す。
            m.sessions_mut()[0].cwd_polled = None;
            moved = m.refresh_metadata();
        }
        assert!(moved, "cd を捉えられる");
        assert_eq!(m.sessions()[0].cwd, std::path::PathBuf::from("/usr/lib"));

        for s in m.sessions_mut() {
            s.pty.kill();
        }
    }

    /// 移動したあとの位置が、覚える並びにも入ることを確かめる。
    #[test]
    fn 移動したあとの位置を覚える() {
        if !std::path::Path::new("/bin/sh").exists() {
            return;
        }
        let mut config = Config::default();
        config.shell.program = Some("/bin/sh".into());
        config.shell.args = vec!["-i".into()];
        let start = std::env::current_dir().unwrap();
        let mut m = manager();
        m.spawn_new(&config, "host", &start).expect("起動できる");
        m.sessions()[0].pty.write(b"cd /usr/share\n".to_vec());

        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(100));
            m.sessions_mut()[0].cwd_polled = None;
            if m.refresh_metadata() {
                break;
            }
        }
        let saved = m.snapshot();
        assert_eq!(saved.sessions[0].cwd, "/usr/share", "移動先が覚えられる");

        for s in m.sessions_mut() {
            s.pty.kill();
        }
    }

    /// git の下へ移ると、ブランチ名も付いてくることを確かめる。
    #[test]
    fn 移動先が_git_ならブランチ名も出る() {
        if !std::path::Path::new("/bin/sh").exists() {
            return;
        }
        let repo = std::env::current_dir().unwrap();
        if crate::git::branch_for(&repo).is_none() {
            return;
        }
        let mut config = Config::default();
        config.shell.program = Some("/bin/sh".into());
        config.shell.args = vec!["-i".into()];
        let mut m = manager();
        // git の外から始める。
        m.spawn_new(&config, "host", std::path::Path::new("/usr/lib"))
            .expect("起動できる");
        m.refresh_metadata();
        assert!(m.sessions()[0].branch.is_none(), "はじめは枝が無い");

        m.sessions()[0]
            .pty
            .write(format!("cd {}\n", repo.display()).into_bytes());
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline && m.sessions()[0].branch.is_none() {
            std::thread::sleep(Duration::from_millis(100));
            m.sessions_mut()[0].cwd_polled = None;
            m.refresh_metadata();
        }
        assert!(
            m.sessions()[0].branch.is_some(),
            "移動先が git なら枝が出る"
        );

        for s in m.sessions_mut() {
            s.pty.kill();
        }
    }
}

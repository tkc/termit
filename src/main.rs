//! tex: エージェント向けの軽量ターミナル。

mod bench;
mod clipboard;
mod config;
mod cwd;
mod git;
mod history;
mod input;
mod keytest;
mod latency;
mod lines;
mod link;
mod mouse;
mod osc;
mod probe;
mod pty;
mod rect;
mod render;
mod scan;
mod search;
mod secret;
mod session;
mod state;
mod term;
mod theme;
mod throughput;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use alacritty_terminal::index::{Column, Direction, Point, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{viewport_to_point, TermMode};
use alacritty_terminal::vte::ansi::CursorShape;
use alacritty_terminal::vte::ansi::{ClearMode, Handler as _, Rgb};
use winit::application::ApplicationHandler;
use winit::event::{ElementState, Ime, MouseButton, MouseScrollDelta, StartCause, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::platform::modifier_supplement::KeyEventExtModifierSupplement;
use winit::window::{Window, WindowId};

use crate::config::{Config, HOST_PROFILE};
use crate::history::{Entry, History, Scope};
use crate::input::Action;
use crate::render::{char_cols, Renderer, TextStyle};
use crate::session::{Manager, RunState};
use crate::term::{TermSize, UiEvent};
use crate::theme::Theme;

/// 履歴の一覧で一度に見える行数。
const SEARCH_ROWS: usize = 10;
/// 履歴を遡れる件数。これを超えた分は出さない。
const SEARCH_LIMIT: usize = 500;

/// 押したキーを記録する。キーバインドが届かないときの切り分けに使う。
/// `TERMIT_KEYLOG` に書き出し先を指定したときだけ動く。
fn keylog(event: &winit::event::KeyEvent, mods: ModifiersState) {
    use std::io::Write;
    let Ok(path) = std::env::var("TERMIT_KEYLOG") else {
        return;
    };
    let base = event.key_without_modifiers();
    let action = input::action_for(&base, event.physical_key, mods);
    let line = format!(
        "state={:?} logical={:?} base={:?} physical={:?} text={:?} \
         ctrl={} shift={} alt={} super={} action={:?}\n",
        event.state,
        event.logical_key,
        base,
        event.physical_key,
        event.text.as_deref(),
        mods.control_key(),
        mods.shift_key(),
        mods.alt_key(),
        mods.super_key(),
        action,
    );
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = f.write_all(line.as_bytes());
    }
}

fn main() {
    env_logger::init();

    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--latency-test") {
        latency::run();
        return;
    }
    if args.iter().any(|a| a == "--throughput") {
        throughput::run();
        return;
    }
    if args.iter().any(|a| a == "--bench") {
        bench::run();
        return;
    }
    if args.iter().any(|a| a == "--keytest") {
        let c = Config::load().unwrap_or_default();
        keytest::run(&c.window.font, c.window.font_size);
        return;
    }
    if args.iter().any(|a| a == "--shell-integration") {
        print!("{}", osc::ZSH_INTEGRATION);
        return;
    }
    if let Some(i) = args.iter().position(|a| a == "--probe") {
        let out = args
            .get(i + 1)
            .cloned()
            .unwrap_or_else(|| "probe.rgba".into());
        probe::run(&out);
        return;
    }

    let config = match Config::load() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("termit: {e}");
            std::process::exit(1);
        }
    };

    // 画面で伏せる式は、ここで 1 度だけ組み立てる。
    // 設定の検査も同じものを通しているので、ここまで来れば必ず成功する。
    let redactor = match secret::Redactor::new(&config.screen.redact) {
        Ok(r) if config.screen.mask => r,
        // 伏せない設定なら、式を持たない。判定そのものが走らなくなる。
        Ok(_) => secret::Redactor::default(),
        Err(e) => {
            eprintln!("termit: {e}");
            std::process::exit(1);
        }
    };

    let event_loop = EventLoop::<UiEvent>::with_user_event()
        .build()
        .expect("イベントループを作れない");
    event_loop.set_control_flow(ControlFlow::Wait);
    let proxy = event_loop.create_proxy();

    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
    let mut app = App {
        config,
        cwd,
        proxy,
        state: None,
        counters: std::env::var("TERMIT_FRAME_LOG")
            .is_ok()
            .then(Counters::default),
        resize_quiet_since: None,
        agent_state_at: None,
        // 走査スレッドへも同じものを渡す。式を 2 度組み立てはしない。
        redactor_seed: Arc::new(redactor),
    };
    if let Err(e) = event_loop.run_app(&mut app) {
        eprintln!("termit: {e}");
        std::process::exit(1);
    }
}

/// 車輪の動きを行数に直す。端数は次の通知まで持ち越す。
///
/// 触覚板は 1 回の通知が数画素しかない。行に満たないぶんを捨てると、
/// ゆっくり動かしているあいだ何も起こらず、動かないように見える。
#[derive(Default)]
struct WheelAccum {
    /// まだ行にならずに残っている分。符号は向きを表す。
    carry: f32,
}

impl WheelAccum {
    /// 通知 1 回ぶんを足し、動かせる行数を返す。
    fn push(&mut self, delta: MouseScrollDelta, cell_height: f32) -> i32 {
        let add = match delta {
            MouseScrollDelta::LineDelta(_, y) => y,
            MouseScrollDelta::PixelDelta(p) => p.y as f32 / cell_height.max(1.0),
        };
        if add == 0.0 {
            return 0;
        }
        // 向きが変わったら持ち越しは捨てる。逆向きの端数が残っていると、
        // 折り返した最初のひと押しがそれに食われる。
        if self.carry != 0.0 && add.signum() != self.carry.signum() {
            self.carry = 0.0;
        }
        self.carry += add;
        let whole = self.carry.trunc();
        self.carry -= whole;
        whole as i32
    }
}

/// マウスの状態。
#[derive(Default)]
struct MouseState {
    x: f32,
    y: f32,
    /// 端末領域でドラッグ中か（端末側の選択）。
    dragging: bool,
    /// 左ペインの幅を変えている最中か。
    resizing: bool,
    /// 端末上のプログラムへ報告中の釦。
    reporting: Option<mouse::Button>,
    /// 直前に報告したセル。同じセルの中の動きは送らない。
    last_reported: Option<(usize, usize)>,
    /// 直前のクリックの時刻、セル、連続回数。
    last_click: Option<(std::time::Instant, (usize, usize), u8)>,
    /// 車輪の端数。
    wheel: WheelAccum,
    /// 左ペインで行を掴んでいる最中の状態。
    sidebar_drag: Option<SidebarDrag>,
}

/// 左ペインの行を掴んで動かしている最中。
struct SidebarDrag {
    /// 掴んだ行（`tree_rows` の添字）。
    from_row: usize,
    /// 掴んだときの縦の位置。動いたと見なす閾値に使う。
    start_y: f32,
    /// 落とす境目。落とせる位置が無ければ `None`。
    to: Option<usize>,
    /// 掴んだだけか、動かしたか。
    moved: bool,
}

/// 掴んだと見なすまでに動かす距離（pt）。
///
/// これが無いと、押しただけで目印が一瞬出る。
const DRAG_SLOP: f32 = 5.0;

/// 名前を付けるための入力。
pub(crate) struct RenameState {
    pub(crate) input: String,
}

/// 重ねて出すプロファイルの一覧。
pub(crate) struct PickerState {
    pub(crate) names: Vec<String>,
    pub(crate) selected: usize,
}

pub(crate) struct SearchState {
    pub(crate) query: String,
    pub(crate) scope: Scope,
    pub(crate) results: Vec<Entry>,
    pub(crate) selected: usize,
    /// 一覧の先頭に出している位置。選んでいる行が外へ出ないよう追う。
    pub(crate) offset: usize,
}

impl SearchState {
    /// 選んでいる行が見える範囲に入るよう、出す位置を合わせる。
    fn follow(&mut self) {
        if self.selected < self.offset {
            self.offset = self.selected;
        } else if self.selected >= self.offset + SEARCH_ROWS {
            self.offset = self.selected + 1 - SEARCH_ROWS;
        }
    }
}

/// 打鍵が止まったと見なすまでの間。
///
/// 打鍵ごとには走らせない（仕様 §10）。走査は別スレッドへ出すが、
/// 出したところで 1 字ごとに始めれば、14 本ぶんの錠を取り合うだけになる。
const SCAN_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(150);

/// 全セッションを横断する検索。左ペインの箱と、そこに出す当たりの一覧。
///
/// `⌘⇧F` では空のまま置く。語が変わるたびに `typed_at` を更新し、
/// 手が止まったところで走らせる。受けた結果は次の打鍵まで固める（仕様 §12）。
#[derive(Default)]
pub(crate) struct CrossSearch {
    pub(crate) query: String,
    /// いま待っている走査の世代。これと違う世代の結果は捨てる。
    pub(crate) generation: u64,
    /// 最後に語が変わった時刻。走っていないあいだは `None`。
    pub(crate) typed_at: Option<std::time::Instant>,
    /// いま出している当たり。新しい順に並んでいる。
    pub(crate) hits: Vec<scan::Hit>,
    pub(crate) selected: usize,
    /// 一覧の先頭に出している位置。選んでいる件が外へ出ないよう追う。
    pub(crate) offset: usize,
    /// 走査を投げて、結果がまだ帰っていない。
    ///
    /// `typed_at` は投げた瞬間に消えるので、走っているあいだと、走り終えて
    /// 0 件だったときを区別するには別に持つ必要がある。
    pub(crate) scanning: bool,
}

/// 結果の一覧の下に出す一言。
#[derive(Debug, PartialEq, Eq)]
enum ScanNote {
    None,
    Searching,
    NoMatch,
}

/// 結果の一覧の下に何を出すか。
///
/// 当たりが残っているあいだは出さない（前の語の結果を見ながら打ち継げる）。
/// 走っているあいだ、または手がまだ止まっていないあいだは「無い」と言わない。
fn scan_note(scanning: bool, typed: bool, hits: usize) -> ScanNote {
    if hits > 0 {
        ScanNote::None
    } else if scanning || typed {
        ScanNote::Searching
    } else {
        ScanNote::NoMatch
    }
}

/// 結果の一覧の代わりに出す字。置く件があれば `None`。
///
/// 件が 1 つも置けないなら必ず何かを出す。当たりがあるのに高さが足りないとき、
/// 何も出さないと空の帯だけが残る。
fn across_note_text(no_blocks: bool, note: &ScanNote) -> Option<&'static str> {
    match note {
        ScanNote::Searching => Some("searching…"),
        ScanNote::NoMatch => Some("no match"),
        ScanNote::None if no_blocks => Some("too narrow"),
        ScanNote::None => None,
    }
}

impl CrossSearch {
    /// 走査の結果を受ける。入れ替えたなら `true`。
    ///
    /// 世代が合わないのは、この結果が帰るより先に次の走査を始めた場合である。
    /// 走査は別スレッドで走るので、結果は打った順に帰るとは限らない。
    /// 遅れて届いた古い結果で、新しい結果を上書きしない。
    fn accept(&mut self, generation: u64, hits: Vec<scan::Hit>) -> bool {
        if generation != self.generation {
            return false;
        }
        self.hits = hits;
        self.selected = 0;
        self.offset = 0;
        self.scanning = false;
        true
    }

    /// 出ている当たりを捨てる。捨てるものがあったなら `true`。
    ///
    /// 語を消し切ったときに呼ぶ。`hits` を書くのは `accept` とここだけなので、
    /// ここで捨てなければ、消した語の当たりがいつまでも一覧に残る。
    fn clear_hits(&mut self) -> bool {
        let had = !self.hits.is_empty();
        self.scanning = false;
        self.hits.clear();
        self.selected = 0;
        self.offset = 0;
        had
    }

    /// 貼り付けた文字列を語へ足す。改行は空白にする（検索語に改行は要らない）。
    fn push_pasted(&mut self, text: &str) {
        self.query.extend(
            text.chars()
                .map(|c| if matches!(c, '\r' | '\n') { ' ' } else { c })
                .filter(|c| !c.is_control()),
        );
    }

    /// 選んでいる件が見える範囲に入るよう、出す位置を合わせる。
    ///
    /// `rows` は結果を置ける件数。1 件が 2 段を取るので、左ペインの高さで変わる。
    /// 呼ぶ側は `across_window` で出すこと。割り付けと食い違うと件が隠れる。
    fn follow(&mut self, rows: usize) {
        // 1 件も置けない高さでも、選んでいる件を追い越さない。
        let rows = rows.max(1);
        if self.selected < self.offset {
            self.offset = self.selected;
        } else if self.selected >= self.offset + rows {
            self.offset = self.selected + 1 - rows;
        }
    }
}

/// 走査を始める頃合いか、まだ待つか。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScanWhen {
    /// 語は変わっていない。起こしてもらう必要も無い。
    Idle,
    /// もう始める。始めれば待ちは解けるので、この腕は起こす時刻を持たない。
    Start,
    /// この時刻に起こしてもらう。
    WakeAt(std::time::Instant),
}

/// 最後の打鍵からの間を見て、走らせるか・待つかを決める。
///
/// 決めるのは描画の最後だが、判定はここに出してある。窓の無いところでは
/// `State` を組み立てられないので、組み立てずに確かめられるようにする。
fn scan_due(
    typed_at: Option<std::time::Instant>,
    now: std::time::Instant,
    debounce: std::time::Duration,
) -> ScanWhen {
    let Some(at) = typed_at else {
        return ScanWhen::Idle;
    };
    let due = at + debounce;
    if due <= now {
        ScanWhen::Start
    } else {
        ScanWhen::WakeAt(due)
    }
}

pub(crate) struct State {
    renderer: Renderer,
    manager: Manager,
    history: Option<History>,
    theme: Theme,
    sidebar: bool,
    search: Option<SearchState>,
    picker: Option<PickerState>,
    /// 名前を付ける入力。
    rename: Option<RenameState>,
    /// 左ペインの幅（pt）。境目を掴んで変えられる。
    sidebar_width: f32,
    /// 一覧の先頭に出している行（`Manager::tree_rows` の添字）。
    ///
    /// セッションが増えると、入りきらない行が下で見切れる。車輪で送り、
    /// 選んだ行が窓の外へ出たときは追いかける。
    sidebar_offset: usize,
    /// 送り位置を合わせた選択行。これが変わったときだけ合わせ直す。
    ///
    /// 毎フレーム合わせると、車輪で送った位置をその場で戻してしまう。
    /// `shown_title` と同じ、描いた状態の控えである。
    sidebar_followed: Option<usize>,
    /// 画面の中の検索。
    find: Option<search::ScreenSearch>,
    /// 全セッションを横断する検索。開いていなければ `None`。
    pub(crate) across: Option<CrossSearch>,
    /// 次に走らせる走査の世代。1 つずつ増やすだけで、戻さない。
    ///
    /// `CrossSearch` の中に置くと `⌘⇧F` で箱を開き直すたびに 0 へ戻る。
    /// 走り続けている前の走査が、新しい箱の世代と一致して受け取られてしまう。
    scan_generation: u64,
    /// 見えている範囲で強調するセル。
    find_cells: std::collections::HashSet<(i32, usize)>,
    /// そのうち、いま選んでいる一致のセル。
    find_current: std::collections::HashSet<(i32, usize)>,
    mouse: MouseState,
    /// いま指しているリンク。下線を引く相手と、押したときに開く相手。
    hovered_link: Option<HoveredLink>,
    /// 素の URL を探す道具。作るのが重いので使い回す。
    url_regex: Option<alacritty_terminal::term::search::RegexSearch>,
    /// 最後に選び終えた文字と、その持ち主。
    ///
    /// 全画面 UI は絶えず描き直す。選んだ行を書き直された時点で
    /// `Term` は選択を捨てるので、`⌘C` を押す頃には何も残っていない。
    /// 選び終えた時点で控えておき、消えていたらこちらを使う。
    picked: Option<(crate::session::SessionId, String)>,
    mods: ModifiersState,
    status: Option<String>,
    recent: Vec<Entry>,
    recent_for: Option<u32>,
    /// 未表示の更新のうち、最も古いものが読み取られた時刻。
    pending_since: Option<std::time::Instant>,
    /// 描こうとして描けなかった。間を置いて描き直す。
    needs_redraw: bool,
    /// 窓が完全に隠れているか。隠れているあいだは組み立てもしない。
    occluded: bool,
    /// 画面で伏せる式。起動時に 1 度だけ組み立てる。
    ///
    /// 横断検索の走査スレッドにも同じものを渡すので `Arc` で持つ。
    pub(crate) redactor: Arc<crate::secret::Redactor>,
    /// セッションの並びが変わった。少し置いてから書き出す。
    state_dirty: bool,
    /// 最後に書き出した時刻。
    state_saved_at: Option<std::time::Instant>,
    /// いまウィンドウに出している題名。変わったときだけ設定し直す。
    shown_title: String,
    /// 変換中の文字列。確定するまでは入力にも検索にも渡さない。
    preedit: String,
    /// いま知らせてある候補窓の位置。変わったときだけ設定し直す。
    ime_area: Option<(i32, i32)>,
    /// 変換中の文字列を出す位置（画素）。候補窓もここへ寄せる。
    cursor_px: Option<(f32, f32)>,
    window: Option<Arc<Window>>,
}

impl State {
    /// セッションの並びが変わったことを覚えておく。
    fn mark_state_dirty(&mut self) {
        self.state_dirty = true;
    }

    /// 覚えている並びを書き出す。頻繁に呼ばれるので間隔を空ける。
    fn save_state(&mut self, force: bool) {
        if !self.state_dirty {
            return;
        }
        let now = std::time::Instant::now();
        let due = match self.state_saved_at {
            None => true,
            Some(t) => now.duration_since(t).as_millis() >= 500,
        };
        if !(due || force) {
            return;
        }
        self.state_dirty = false;
        self.state_saved_at = Some(now);
        if self.manager.sessions().is_empty() {
            state::SavedState::clear();
        } else {
            self.manager.snapshot().save();
        }
    }

    fn request_redraw(&self) {
        if let Some(w) = &self.window {
            w.request_redraw();
        }
    }
}

/// 描画されない原因を工程ごとに切り分けるための計数。
/// `TERMIT_FRAME_LOG` を指定したときだけ動く。
#[derive(Default)]
struct Counters {
    wakeup: u64,
    osc: u64,
    other_user_event: u64,
    redraw_requested: u64,
    key: u64,
    drew: u64,
    /// 更新を読み取ってから画面に出すまでの時間。
    latency_us: Vec<u64>,
    last: Option<std::time::Instant>,
}

impl Counters {
    fn tick(&mut self) {
        let now = std::time::Instant::now();
        let due = match self.last {
            None => {
                self.last = Some(now);
                false
            }
            Some(t) => now.duration_since(t).as_millis() >= 1000,
        };
        if !due {
            return;
        }
        self.last = Some(now);
        self.latency_us.sort_unstable();
        let pct = |v: &Vec<u64>, p: usize| {
            if v.is_empty() {
                0.0
            } else {
                v[(v.len() - 1) * p / 100] as f64 / 1000.0
            }
        };
        log::info!(
            "1s: wakeup={} redraw_req={} drawn={} keys={} | read→present p50 {:.1}ms p90 {:.1}ms max {:.1}ms",
            self.wakeup,
            self.redraw_requested,
            self.drew,
            self.key,
            pct(&self.latency_us, 50),
            pct(&self.latency_us, 90),
            pct(&self.latency_us, 100),
        );
        let last = self.last;
        *self = Counters {
            last,
            ..Default::default()
        };
    }
}

/// 動いている印を見直す間隔。
///
/// 出力が止まったことは通知として届かない。止まったと見なせる頃に
/// 一度描き直して、印を落とす。
const WORKING_REFRESH: std::time::Duration = std::time::Duration::from_millis(250);
/// 返事待ちを調べる間隔。印を出すのに要るだけで、これより短くしても見た目は変わらない。
const AGENT_STATE_INTERVAL: std::time::Duration = std::time::Duration::from_millis(250);

/// 引きずる手が止まったと見なすまでの間。
///
/// これを過ぎたら、待たせていたセッションの桁数も合わせる。
const RESIZE_SETTLE: std::time::Duration = std::time::Duration::from_millis(120);

struct App {
    config: Config,
    cwd: PathBuf,
    proxy: EventLoopProxy<UiEvent>,
    state: Option<State>,
    counters: Option<Counters>,
    /// 最後に大きさが変わった時刻。合わせ残しがあるときだけ入る。
    resize_quiet_since: Option<std::time::Instant>,
    /// 返事待ちを最後に調べた時刻。毎フレーム画面を読まないための間隔。
    agent_state_at: Option<std::time::Instant>,
    /// 画面を作るときに State へ渡す式。組み立ては起動時に済ませてある。
    redactor_seed: Arc<crate::secret::Redactor>,
}

/// 画面の割り付け。すべてセル単位で扱う。
#[derive(Clone, Copy, Debug)]
pub(crate) struct Layout {
    cols: usize,
    rows: usize,
    /// 左ペインの幅（画素）。0 なら出さない。
    sidebar_px: f32,
    /// 左ペインが占める桁数。端末はその次の桁から始まる。
    sidebar_cols: usize,
    term_col: usize,
    term_cols: usize,
    term_rows: usize,
}

impl App {
    fn layout(&self, s: &State) -> Layout {
        layout_of(&self.config, s)
    }
}

impl ApplicationHandler<UiEvent> for App {
    fn new_events(&mut self, event_loop: &ActiveEventLoop, cause: StartCause) {
        // 描けずに待っていた場合は、ここで描き直す。
        if matches!(cause, StartCause::ResumeTimeReached { .. }) {
            event_loop.set_control_flow(ControlFlow::Wait);
            if let Some(s) = &self.state {
                // 描けずに待っていたときのほか、大きさの合わせ残しと
                // 「動いている」印の描き直しも、ここを起点にする。
                // 起こしてもらっても描かなければ、どれも先へ進まない。
                if s.needs_redraw
                    || s.manager.has_pending_size()
                    || s.manager.sessions().iter().any(|x| x.shown_working)
                    // 横断検索の走査を始めるのは描画の最後なので、
                    // 頃合いになっても描き直さなければ始まらない。
                    || s.across.as_ref().is_some_and(|a| a.typed_at.is_some())
                {
                    s.request_redraw();
                }
            }
        }
    }

    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.state.is_some() {
            return;
        }
        let attrs = Window::default_attributes()
            .with_title("termit")
            .with_inner_size(winit::dpi::LogicalSize::new(1100.0, 720.0));
        let window = Arc::new(
            event_loop
                .create_window(attrs)
                .expect("ウィンドウを作れない"),
        );
        window.set_ime_allowed(true);

        let renderer = pollster::block_on(Renderer::new(
            window.clone(),
            event_loop,
            &self.config.window.font,
            self.config.window.font_size,
            self.config.window.vsync,
        ));

        let mut state = State {
            renderer,
            manager: Manager::new(TermSize::new(80, 24), (8, 16), self.proxy.clone().into()),
            history: History::open_default(),
            theme: Theme::default(),
            sidebar: true,
            search: None,
            picker: None,
            rename: None,
            sidebar_width: self.config.window.sidebar_width,
            sidebar_offset: 0,
            sidebar_followed: None,
            find: None,
            across: None,
            scan_generation: 0,
            find_cells: Default::default(),
            find_current: Default::default(),
            mouse: MouseState::default(),
            hovered_link: None,
            url_regex: link::url_search(),
            picked: None,
            mods: ModifiersState::empty(),
            status: None,
            recent: Vec::new(),
            recent_for: None,
            pending_since: None,
            needs_redraw: false,
            occluded: false,
            redactor: std::mem::take(&mut self.redactor_seed),
            state_dirty: false,
            state_saved_at: None,
            shown_title: String::new(),
            preedit: String::new(),
            ime_area: None,
            cursor_px: None,
            window: Some(window.clone()),
        };
        if state.history.is_none() {
            state.status = Some("history database unavailable; history disabled".into());
        }

        let layout = self.layout(&state);
        let cell = state.renderer.cell();
        state
            .manager
            .set_cell((cell.width as u16, cell.height as u16));
        state
            .manager
            .resize(TermSize::new(layout.term_cols, layout.term_rows));

        let cwd = self.cwd.clone();
        // 前回の並びを作り直す。作れなければ、いつもどおり 1 つ立ち上げる。
        let restored = if self.config.window.restore_sessions {
            state::SavedState::load()
                .map(|saved| state.manager.restore(&self.config, &saved))
                .unwrap_or(0)
        } else {
            0
        };
        if restored == 0 {
            if let Err(e) = state.manager.spawn_new(&self.config, HOST_PROFILE, &cwd) {
                state.status = Some(e.to_string());
            }
        }
        state.mark_state_dirty();

        self.state = Some(state);
        window.request_redraw();
    }

    fn user_event(&mut self, _event_loop: &ActiveEventLoop, event: UiEvent) {
        let selected = self
            .state
            .as_ref()
            .and_then(|s| s.manager.selected())
            .map(|s| s.id);
        // すでに「動いている」と描いてあるセッションなら、
        // 出力が続いているだけなので描き直さない。
        let shown_working = match (&event, &self.state) {
            (UiEvent::Wakeup(id, _), Some(st)) => st
                .manager
                .sessions()
                .iter()
                .any(|s| s.id == *id && s.shown_working),
            _ => false,
        };
        let redraw = wants_redraw(&event, selected, shown_working);
        if let Some(c) = &mut self.counters {
            match &event {
                UiEvent::Wakeup(_, _) => c.wakeup += 1,
                UiEvent::Osc(_, _) => c.osc += 1,
                _ => c.other_user_event += 1,
            }
            c.tick();
        }
        let Some(state) = &mut self.state else { return };
        match event {
            UiEvent::Wakeup(id, at) => {
                // 最も古い更新の時刻を覚えておき、表示までの時間を測る。
                // 測るのは見えているものだけである。
                if state.pending_since.is_none() && selected == Some(id) {
                    state.pending_since = Some(at);
                }
            }
            UiEvent::Title(id, title) => {
                // 題名の先頭に回る絵を出すエージェントがある。
                // 出力が途切れても動いていることが、これで分かる。
                let chars = self.config.agent.working_title.clone();
                if let Some(s) = state.manager.get_mut(id) {
                    let t = (!title.is_empty()).then_some(title);
                    s.title_working = crate::session::Session::title_signal(t.as_deref(), &chars);
                    s.window_title = t;
                }
            }
            UiEvent::ChildExit(id, code) => {
                state.manager.mark_exited(id, code);
                state.mark_state_dirty();
            }
            UiEvent::ClipboardStore(_, text) => clipboard::copy(&text),
            UiEvent::ClipboardLoad(id, format) => {
                let text = clipboard::paste().unwrap_or_default();
                if let Some(s) = state.manager.get_mut(id) {
                    s.pty.write(format(&text).into_bytes());
                }
            }
            UiEvent::Osc(id, ev) => {
                if let Some(s) = state.manager.get_mut(id) {
                    s.on_osc(&ev);
                }
                // 作業ディレクトリと会話 ID は、次の起動で作り直すのに要る。
                if matches!(
                    ev,
                    crate::osc::OscEvent::Cwd(_) | crate::osc::OscEvent::AgentId(_)
                ) {
                    state.mark_state_dirty();
                }
            }
            UiEvent::Command(_, record) => {
                if let Some(h) = &state.history {
                    if let Err(e) = h.record(&record) {
                        log::warn!("cannot write history: {e}");
                    }
                }
                state.recent_for = None;
            }
            UiEvent::Scanned(generation, hits) => {
                // 描き直すかどうかは `wants_redraw` では決められない。
                // 古い世代の結果なら、出ているものは何も変わらない。
                let took = state
                    .across
                    .as_mut()
                    .is_some_and(|across| across.accept(generation, hits));
                if took {
                    state.request_redraw();
                }
            }
        }
        if redraw {
            state.request_redraw();
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        let Some(state) = &mut self.state else { return };
        match event {
            WindowEvent::CloseRequested => {
                state.save_state(true);
                event_loop.exit();
            }
            WindowEvent::Occluded(hidden) => {
                // 隠れているあいだは、組み立てても捨てるだけになる。
                // 出てきたら描き直す。時計で起こす必要はない。
                state.occluded = hidden;
                if !hidden {
                    state.request_redraw();
                }
            }
            WindowEvent::Resized(size) => {
                state.renderer.resize(size.width, size.height);
                self.reflow();
                if let Some(s) = &self.state {
                    s.request_redraw();
                }
            }
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                state.renderer.set_scale(scale_factor as f32);
                self.reflow();
            }
            WindowEvent::ModifiersChanged(m) => state.mods = m.state(),
            WindowEvent::CursorMoved { position, .. } => {
                state.mouse.x = position.x as f32;
                state.mouse.y = position.y as f32;
                let dragging = state.mouse.dragging;
                if state.mouse.sidebar_drag.is_some() {
                    self.drag_sidebar_row();
                    if let Some(s) = &self.state {
                        s.request_redraw();
                    }
                    return;
                }
                if state.mouse.resizing {
                    // 掴んでいるあいだは、その位置を幅にする。
                    let w = (state.mouse.x / state.renderer.scale()).clamp(80.0, 800.0);
                    if (w - state.sidebar_width).abs() > 0.5 {
                        state.sidebar_width = w;
                        self.reflow();
                    }
                    return;
                }
                let layout = layout_of(&self.config, state);
                // 指しているリンクを覚える。変わったときだけ描き直す。
                let link = hyperlink_at(state, &layout);
                let link_changed = link != state.hovered_link;
                if link_changed {
                    state.hovered_link = link;
                }
                // 境目の上では掴める形、リンクの上では指の形にする。
                let over = on_divider(state, &layout, state.mouse.x);
                if let Some(win) = &state.window {
                    win.set_cursor(if over {
                        winit::window::CursorIcon::ColResize
                    } else if state.hovered_link.is_some() {
                        winit::window::CursorIcon::Pointer
                    } else {
                        winit::window::CursorIcon::Default
                    });
                }
                if link_changed {
                    state.request_redraw();
                }
                // 左ペインの上では、乗っている行の見た目が変わる。
                if (position.x as f32) < layout.sidebar_px {
                    state.request_redraw();
                }
                if self.report_mouse_motion() {
                    return;
                }
                if dragging {
                    self.drag_selection();
                    if let Some(s) = &self.state {
                        s.request_redraw();
                    }
                }
            }
            WindowEvent::Focused(focused) => {
                // 全画面のプログラムは、窓の出入りを知りたがることがある。
                if let Some(s) = state.manager.selected() {
                    let mode = *s.term.lock().mode();
                    if mode.contains(TermMode::FOCUS_IN_OUT) {
                        s.pty.write(if focused {
                            b"\x1b[I".to_vec()
                        } else {
                            b"\x1b[O".to_vec()
                        });
                    }
                }
            }
            WindowEvent::MouseInput {
                state: button_state,
                button,
                ..
            } => {
                let btn = match button {
                    MouseButton::Left => Some(mouse::Button::Left),
                    MouseButton::Middle => Some(mouse::Button::Middle),
                    MouseButton::Right => Some(mouse::Button::Right),
                    _ => None,
                };
                if let Some(btn) = btn {
                    let pressed = button_state == ElementState::Pressed;
                    let reported = self.report_mouse_button(btn, pressed);
                    if btn == mouse::Button::Left {
                        if pressed {
                            if !reported {
                                self.on_click();
                            }
                        } else {
                            // 離したときは、報告したかどうかによらず終える。
                            // Shift を先に離すと報告側へ回り、掴んだままになる。
                            self.on_release();
                        }
                    }
                }
                if let Some(s) = &self.state {
                    s.request_redraw();
                }
            }
            WindowEvent::DroppedFile(path) => {
                self.on_dropped_file(&path);
                if let Some(s) = &self.state {
                    s.request_redraw();
                }
            }
            WindowEvent::HoveredFile(_) => {
                state.status = Some(DROP_HINT.into());
                state.request_redraw();
            }
            WindowEvent::HoveredFileCancelled => {
                state.status = None;
                state.request_redraw();
            }
            WindowEvent::MouseWheel { delta, .. } => {
                self.on_wheel(delta);
                if let Some(s) = &self.state {
                    s.request_redraw();
                }
            }
            WindowEvent::Ime(Ime::Preedit(text, _)) => {
                state.preedit = text;
                state.request_redraw();
            }
            WindowEvent::Ime(Ime::Disabled) => {
                state.preedit.clear();
                state.request_redraw();
            }
            WindowEvent::Ime(Ime::Commit(text)) => {
                state.preedit.clear();
                // 確定した文字列は、開いている入り口へ渡す。
                // 変換で入れた語も、検索や履歴の絞り込みに使える必要がある。
                if state.rename.is_some() {
                    if let Some(r) = &mut state.rename {
                        r.input.push_str(&text);
                    }
                } else if state.across.is_some() {
                    if let Some(a) = &mut state.across {
                        a.query.push_str(&text);
                    }
                    self.queue_scan();
                } else if state.find.is_some() {
                    if let Some(f) = &mut state.find {
                        f.query.push_str(&text);
                        f.rebuild();
                    }
                    self.find_step(Direction::Left);
                } else if state.search.is_some() {
                    if let Some(s) = &mut state.search {
                        s.query.push_str(&text);
                    }
                    self.refresh_search();
                } else if state.picker.is_some() {
                    // 一覧は数字と j/k で選ぶ。文字は受けない。
                } else if let Some(s) = state.manager.selected() {
                    s.pty.write(text.into_bytes());
                }
                if let Some(s) = &self.state {
                    s.request_redraw();
                }
            }
            WindowEvent::KeyboardInput { event, .. } => {
                keylog(&event, state.mods);
                if event.state.is_pressed() {
                    if let Some(c) = &mut self.counters {
                        c.key += 1;
                    }
                    self.on_key(event);
                }
            }
            WindowEvent::RedrawRequested => {
                if let Some(c) = &mut self.counters {
                    c.redraw_requested += 1;
                    c.tick();
                }
                self.draw(event_loop);
            }
            _ => {}
        }
    }
}

impl App {
    /// セル寸法や窓の大きさが変わったとき、全ペインへ伝える。
    fn reflow(&mut self) {
        let Some(state) = &mut self.state else { return };
        let layout = layout_of(&self.config, state);
        let (term_cols, term_rows) = (layout.term_cols, layout.term_rows);
        let cell = state.renderer.cell();
        state
            .manager
            .set_cell((cell.width as u16, cell.height as u16));
        // 引きずっている最中かもしれない。見えているものだけ先に合わせる。
        state
            .manager
            .resize_visible(TermSize::new(term_cols, term_rows));
        self.resize_quiet_since = Some(std::time::Instant::now());
        if let Some(s) = &self.state {
            s.request_redraw();
        }
    }

    /// 待たせていたセッションを、いま合わせる。
    fn settle_resize(&mut self) {
        self.resize_quiet_since = None;
        if let Some(state) = &mut self.state {
            state.manager.settle();
        }
    }

    fn on_key(&mut self, event: winit::event::KeyEvent) {
        let Some(state) = &mut self.state else { return };
        let mods = state.mods;
        let key = event.logical_key.clone();
        // 修飾を外したキー。Ctrl を押すと logical_key が制御文字になる環境が
        // あるため、キーバインドの照合と制御文字への変換はこちらを使う。
        let base = event.key_without_modifiers();

        // 重ねた一覧が開いているあいだは、すべてのキーをそちらが受ける。
        if state.picker.is_some() {
            self.on_picker_key(&base, mods);
            if let Some(s) = &self.state {
                s.request_redraw();
            }
            return;
        }
        if state.rename.is_some() {
            self.on_rename_key(&base, &event);
            if let Some(s) = &self.state {
                s.request_redraw();
            }
            return;
        }
        if state.across.is_some() {
            self.on_across_key(&base, &event, mods);
            if let Some(s) = &self.state {
                s.request_redraw();
            }
            return;
        }
        if state.find.is_some() {
            self.on_find_key(&base, &event, mods);
            if let Some(s) = &self.state {
                s.request_redraw();
            }
            return;
        }
        if state.search.is_some() {
            self.on_search_key(&base, &event, mods);
            if let Some(s) = &self.state {
                s.request_redraw();
            }
            return;
        }

        if let Some(action) = input::action_for(&base, event.physical_key, mods) {
            self.on_action(action);
            if let Some(s) = &self.state {
                s.request_redraw();
            }
            return;
        }

        let Some(session) = state.manager.selected() else {
            return;
        };
        let mode = *session.term.lock().mode();
        // 変換中の文字は確定するまで渡さない。Ime::Commit で受ける。
        let text = if state.preedit.is_empty() {
            event.text.as_deref()
        } else {
            None
        };
        if let Some(bytes) = input::encode(&key, &base, text, mods, mode) {
            // 入力があったら最下部へ戻す。
            session
                .term
                .lock()
                .scroll_display(alacritty_terminal::grid::Scroll::Bottom);
            session.pty.write(bytes);
            state.request_redraw();
        }
    }

    fn on_action(&mut self, action: Action) {
        self.on_action_inner(action);
        if let Some(s) = &mut self.state {
            s.mark_state_dirty();
        }
    }

    fn on_action_inner(&mut self, action: Action) {
        let cwd = self.cwd.clone();
        let config = self.config.clone();
        let Some(state) = &mut self.state else { return };
        state.status = None;
        match action {
            Action::NewSession => {
                let dir = state
                    .manager
                    .selected()
                    .map(|s| s.cwd.clone())
                    .unwrap_or(cwd);
                if let Err(e) = state.manager.spawn_new(&config, HOST_PROFILE, &dir) {
                    state.status = Some(e.to_string());
                }
            }
            Action::Fork => {
                let i = state.manager.selected_index();
                match state.manager.fork(&config, i, None) {
                    Ok(_) => {}
                    Err(e) => state.status = Some(e.to_string()),
                }
            }
            Action::ForkWithProfile => {
                let names = config.profile_names();
                if names.len() < 2 {
                    state.status = Some("no profile other than host is configured".into());
                } else {
                    state.search = None;
                    state.picker = Some(PickerState { names, selected: 1 });
                }
            }
            Action::SelectNext => state.manager.select_next(),
            Action::SelectPrev => state.manager.select_prev(),
            Action::SelectIndex(i) => {
                let rows = state.manager.tree_rows();
                if let Some(row) = rows.get(i) {
                    let index = row.index;
                    state.manager.select(index);
                }
            }
            Action::CloseSession => state.manager.close_selected(),
            Action::ToggleSidebar => {
                state.sidebar = !state.sidebar;
                self.reflow();
            }
            Action::RenameSession => {
                let current = state
                    .manager
                    .selected()
                    .and_then(|s| s.name.clone())
                    .unwrap_or_default();
                state.picker = None;
                state.search = None;
                state.find = None;
                state.rename = Some(RenameState { input: current });
            }
            Action::FindInScreen => {
                state.picker = None;
                state.search = None;
                state.find = Some(search::ScreenSearch::new());
            }
            Action::FindAcrossSessions => self.enter_across(),
            Action::SearchHistory => {
                state.picker = None;
                state.find = None;
                state.search = Some(SearchState {
                    query: String::new(),
                    // まずはこのセッションの中を遡る。他へ広げるのは ^R の押し直し。
                    scope: Scope::Session,
                    results: Vec::new(),
                    selected: 0,
                    offset: 0,
                });
                self.refresh_search();
            }
            Action::Copy => {
                let live = state.manager.selected().and_then(|s| {
                    let term = s.term.lock();
                    term.selection_to_string()
                });
                let id = state.manager.selected().map(|s| s.id);
                if let Some(text) = copy_text(live, state.picked.as_ref(), id) {
                    clipboard::copy(&text);
                }
            }
            Action::Paste => {
                // 貼り付けは素通しである。認証情報は画面の上で伏せるだけで、
                // PTY へは本物が流れる。`export AWS_SECRET_ACCESS_KEY=…` は普通に効く。
                if let (Some(s), Some(text)) = (state.manager.selected(), clipboard::paste()) {
                    let mode = *s.term.lock().mode();
                    s.pty.write(bracketed(&text, mode));
                }
            }
            Action::ClearScreen => {
                // 画面の消去とプロンプトの出し直しはシェルに任せる。端末が
                // 画面を消すだけでは、プロンプトが消えたまま戻らない。
                // \x0c は Ctrl+L で、シェルも全画面のプログラムも
                // 「描き直せ」と解釈する。
                //
                // ただしシェルは画面を消すのではなく上へ押し出すため、
                // 押し出されたぶんが履歴に積まれる。少しのあいだ捨て続ける。
                let i = state.manager.selected_index();
                if let Some(s) = state.manager.sessions_mut().get_mut(i) {
                    {
                        let mut term = s.term.lock();
                        term.clear_screen(ClearMode::Saved);
                        term.scroll_display(alacritty_terminal::grid::Scroll::Bottom);
                    }
                    s.pty.write(vec![0x0c]);
                    s.clear_scrollback_until =
                        Some(std::time::Instant::now() + std::time::Duration::from_millis(250));
                }
            }
            Action::FontBigger => {
                let size = state.renderer.font_size() + 1.0;
                state.renderer.set_font_size(size);
                self.reflow();
            }
            Action::FontSmaller => {
                let size = state.renderer.font_size() - 1.0;
                state.renderer.set_font_size(size);
                self.reflow();
            }
            Action::ScrollUp => {
                if let Some(s) = state.manager.selected() {
                    s.term
                        .lock()
                        .scroll_display(alacritty_terminal::grid::Scroll::PageUp);
                }
            }
            Action::ScrollDown => {
                if let Some(s) = state.manager.selected() {
                    s.term
                        .lock()
                        .scroll_display(alacritty_terminal::grid::Scroll::PageDown);
                }
            }
        }
    }

    // ------------------------------------------------------------ マウス
}

/// マウス位置をセル座標に直す。戻り値はセルの桁と行、桁内の左右。
fn mouse_cell(state: &State) -> (usize, usize, Side) {
    let cell = state.renderer.cell();
    let col_f = state.mouse.x / cell.width;
    let col = col_f.max(0.0) as usize;
    let row = (state.mouse.y / cell.height).max(0.0) as usize;
    let side = if col_f - col_f.floor() < 0.5 {
        Side::Left
    } else {
        Side::Right
    };
    (col, row, side)
}

/// 端末領域の中での 0 起点の位置。領域の外なら `None`。
fn terminal_cell(layout: &Layout, col: usize, row: usize) -> Option<(usize, usize)> {
    if col < layout.term_col || row >= layout.term_rows {
        return None;
    }
    Some((
        (col - layout.term_col).min(layout.term_cols.saturating_sub(1)),
        row,
    ))
}

/// 画面の状態を 1 秒に一度だけ記録する。`TERMIT_FRAME_LOG` を指定したときだけ動く。
///
/// 「下が空いている」「行が飛んでいる」という報告は、絵だけでは
/// 描き方の間違いか中身のとおりかを見分けられない。数えて残す。
fn log_grid(
    term: &alacritty_terminal::Term<crate::term::EventProxy>,
    display_offset: usize,
    layout: &Layout,
    drawn: std::collections::HashSet<usize>,
) {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if !*ON.get_or_init(|| std::env::var("TERMIT_FRAME_LOG").is_ok()) {
        return;
    }
    static LAST: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let now = crate::term::now_ms();
    let last = LAST.load(std::sync::atomic::Ordering::Relaxed);
    if now.saturating_sub(last) < 1000 {
        return;
    }
    LAST.store(now, std::sync::atomic::Ordering::Relaxed);
    use alacritty_terminal::grid::Dimensions;
    let grid = term.grid();
    let missing: Vec<usize> = (0..layout.term_rows)
        .filter(|r| !drawn.contains(r))
        .collect();
    log::info!(
        "画面: 描く枠 {} 行 / グリッド {} 行 / 履歴 {} 行 / 遡り {} 行 / カーソル {:?} | 字を出した行 {} / 出さなかった行 {:?}",
        layout.term_rows,
        grid.screen_lines(),
        grid.history_size(),
        display_offset,
        (grid.cursor.point.line.0, grid.cursor.point.column.0),
        drawn.len(),
        &missing[..missing.len().min(12)],
    );
}

/// マウスの下にあるリンク。無ければ `None`。
///
/// OSC 8 は `alacritty_terminal` がコマに結び付けてくれる。
/// ここでは、いま指している 1 コマぶんを読むだけである。
fn hyperlink_at(state: &mut State, layout: &Layout) -> Option<HoveredLink> {
    if state.mouse.x < layout.sidebar_px {
        return None;
    }
    let (col, row, _) = mouse_cell(state);
    let point = terminal_point(state, layout, col, row)?;
    let session = state.manager.selected()?;
    let term = session.term.lock();
    use alacritty_terminal::grid::Dimensions;
    if point.line.0 < -(term.grid().history_size() as i32)
        || point.line.0 >= term.grid().screen_lines() as i32
    {
        return None;
    }
    // OSC 8 で印が付いていれば、それを使う。
    if let Some(h) = term.grid()[point].hyperlink() {
        return Some(HoveredLink {
            uri: h.uri().to_string(),
            id: Some(h.id().to_string()),
            span: None,
        });
    }
    // 印が無ければ、素で書かれた URL を探す。
    let regex = state.url_regex.as_mut()?;
    let span = link::url_at(&term, regex, point)?;
    let uri = term.bounds_to_string(*span.start(), *span.end());
    link::openable(&uri).then_some(HoveredLink {
        uri,
        id: None,
        span: Some(span),
    })
}

/// 画面の上で伏せる文字。丸 1 つで 1 コマ。
///
/// 同じ幅のものに置き換える。桁がずれると、全画面 UI の枠線が崩れる。
const MASK_CHAR: char = '\u{2022}';

/// 画面のうち、伏せるコマの位置を返す。鍵は履歴を含む行番号と桁。
///
/// 行ごとに文字を組み立てて式に当て、当たったバイトの範囲を桁へ直す。
/// 認証情報は「名前と値」の形で行として現れるので、1 コマずつでは判定できない。
fn masked_cells(
    term: &alacritty_terminal::Term<crate::term::EventProxy>,
    display_offset: usize,
    rows: usize,
    cols: usize,
    redactor: &crate::secret::Redactor,
) -> std::collections::HashSet<(i32, usize)> {
    use alacritty_terminal::grid::Dimensions;
    let mut out = std::collections::HashSet::new();
    if redactor.is_empty() {
        return out;
    }
    let grid = term.grid();
    let cols = grid.columns().min(cols);
    // 見えている行だけを見る。遡っていれば、その位置の行になる。
    let first = -(display_offset as i32);
    for row in 0..rows.min(grid.screen_lines()) {
        let line = first + row as i32;
        let grid_line = alacritty_terminal::index::Line(line);
        let mut text = String::with_capacity(cols);
        // バイトの位置から桁へ戻すための対応表。
        let mut at: Vec<usize> = Vec::with_capacity(cols + 1);
        for col in 0..cols {
            let cell = &grid[grid_line][alacritty_terminal::index::Column(col)];
            if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                continue;
            }
            for _ in 0..cell.c.len_utf8() {
                at.push(col);
            }
            text.push(cell.c);
        }
        at.push(cols);
        for span in redactor.spans(&text) {
            let from = at.get(span.start).copied().unwrap_or(cols);
            let to = at.get(span.end).copied().unwrap_or(cols);
            for col in from..to.min(cols) {
                out.insert((line, col));
            }
        }
    }
    out
}

/// 表示行を画面の何行目かに直す。範囲の外なら `None`。
///
/// `display_iter` が返す行番号は履歴を含む座標で、
/// 遡っているあいだは `-遡った行数` から始まる。
/// 直し方は `alacritty_terminal` の `point_to_viewport` に合わせる。
/// alacritty 本体もコマとカーソルの両方をこれで直しており、
/// 自前で足し引きすると、いつか食い違う。
fn screen_row(line: i32, display_offset: usize, rows: usize) -> Option<usize> {
    let point = Point::new(alacritty_terminal::index::Line(line), Column(0));
    let row = alacritty_terminal::term::point_to_viewport(display_offset, point)?.line;
    (row < rows).then_some(row)
}

/// 端末領域のセルをグリッドの位置に直す。領域の外なら `None`。
fn terminal_point(state: &State, layout: &Layout, col: usize, row: usize) -> Option<Point> {
    if col < layout.term_col || row >= layout.term_rows {
        return None;
    }
    let term_col = (col - layout.term_col).min(layout.term_cols.saturating_sub(1));
    let session = state.manager.selected()?;
    let display_offset = session.term.lock().grid().display_offset();
    Some(viewport_to_point(
        display_offset,
        Point::new(row, Column(term_col)),
    ))
}

impl App {
    /// 端末上のプログラムがマウスを要求していれば、釦の出来事を渡す。
    ///
    /// Shift を押しているあいだは渡さず、端末側の選択に使う。
    /// これは xterm からの作法で、報告中でも文字を選べるようにするためである。
    fn report_mouse_button(&mut self, button: mouse::Button, pressed: bool) -> bool {
        let Some(state) = &mut self.state else {
            return false;
        };
        if state.picker.is_some() || state.search.is_some() {
            return false;
        }
        if state.mods.shift_key() {
            return false;
        }
        let layout = layout_of(&self.config, state);
        let (col, row, _) = mouse_cell(state);
        let Some((tcol, trow)) = terminal_cell(&layout, col, row) else {
            return false;
        };
        let Some(session) = state.manager.selected() else {
            return false;
        };
        let mode = *session.term.lock().mode();
        let kind = if pressed {
            mouse::Kind::Press
        } else {
            mouse::Kind::Release
        };
        let Some(bytes) = mouse::encode(kind, button, tcol, trow, state.mods, mode) else {
            return false;
        };
        session.pty.write(bytes);
        state.mouse.reporting = pressed.then_some(button);
        state.mouse.last_reported = Some((tcol, trow));
        // 釦がプログラムへ渡ったということは、引いても選択にならない。
        // 文字をコピーできない、と見えるのはここである。逃げ道を出す。
        if pressed && button == mouse::Button::Left {
            state.status = Some(SELECT_HINT.into());
        }
        true
    }

    /// 移動を報告する。送ったら `true`。
    fn report_mouse_motion(&mut self) -> bool {
        let Some(state) = &mut self.state else {
            return false;
        };
        if state.mods.shift_key() || state.picker.is_some() || state.search.is_some() {
            return false;
        }
        let layout = layout_of(&self.config, state);
        let (col, row, _) = mouse_cell(state);
        let Some((tcol, trow)) = terminal_cell(&layout, col, row) else {
            return false;
        };
        // 同じセルの中で動いただけなら送らない。送ると数が多すぎる。
        if state.mouse.last_reported == Some((tcol, trow)) {
            return false;
        }
        let Some(session) = state.manager.selected() else {
            return false;
        };
        let mode = *session.term.lock().mode();
        let (kind, button) = match state.mouse.reporting {
            Some(b) => (mouse::Kind::Drag, b),
            None => (mouse::Kind::Move, mouse::Button::Left),
        };
        let Some(bytes) = mouse::encode(kind, button, tcol, trow, state.mods, mode) else {
            // 報告しない設定でも、報告中の釦があれば選択には使わない。
            return state.mouse.reporting.is_some();
        };
        session.pty.write(bytes);
        state.mouse.last_reported = Some((tcol, trow));
        true
    }

    fn on_click(&mut self) {
        let config = self.config.clone();
        let Some(state) = &mut self.state else { return };
        let layout = layout_of(&self.config, state);
        let (col, row, side) = mouse_cell(state);

        // 重ねた一覧が開いているときは、まず閉じる。
        if state.picker.is_some() || state.search.is_some() || state.rename.is_some() {
            state.picker = None;
            state.search = None;
            state.rename = None;
            return;
        }

        // 境目を掴んだら、幅を変える操作に入る。
        if on_divider(state, &layout, state.mouse.x) {
            state.mouse.resizing = true;
            return;
        }
        if let Some(hit) = sidebar_hit(state, &layout, state.mouse.x, state.mouse.y) {
            match hit {
                SidebarHit::NewSession => {
                    let dir = state
                        .manager
                        .selected()
                        .map(|s| s.cwd.clone())
                        .unwrap_or_else(|| self.cwd.clone());
                    state.status = None;
                    if let Err(e) = state.manager.spawn_new(&config, HOST_PROFILE, &dir) {
                        state.status = Some(e.to_string());
                    }
                }
                SidebarHit::Select(i) => {
                    state.manager.select(i);
                    // 押した行はそのまま掴んでいる。動かせば並べ替えになる。
                    let sl = sidebar_layout(state, &layout);
                    if let Some(row) = sl.blocks.iter().position(|b| b.index == i) {
                        state.mouse.sidebar_drag = Some(SidebarDrag {
                            // 掴んだのは窓の中の位置。送っているぶんを足して
                            // 木の行に直す。
                            from_row: sl.offset + row,
                            start_y: state.mouse.y,
                            to: None,
                            moved: false,
                        });
                    }
                }
                SidebarHit::Close(i) => state.manager.close(i),
                SidebarHit::SearchBox => {
                    // 窓に入る（仕様 §9）。入っているなら、打った語を捨てない。
                    if state.across.is_none() {
                        self.enter_across();
                    }
                    // 並び順は変わらない。保存を予約しない。
                    return;
                }
                SidebarHit::Hit(n) => {
                    // 選び直すだけ。飛ぶのは別の仕事である。
                    // 掴んだことにはしない（仕様 §13）。
                    let rows = across_window(state, &layout);
                    if let Some(across) = &mut state.across {
                        if n < across.hits.len() {
                            across.selected = n;
                            across.follow(rows);
                        }
                    }
                    return;
                }
            }
            state.mark_state_dirty();
            return;
        }

        // 左ペインの外を押したら検索窓から出る。
        // 出ないと、端末を押したあとに打った字が窓へ入っていく。
        if state.across.is_some() && state.mouse.x >= layout.sidebar_px {
            state.across = None;
        }

        // 端末領域。リンクの上なら開く。選択はしない。
        if let Some(hit) = hyperlink_at(state, &layout) {
            link::open(&hit.uri);
            return;
        }
        // 連続クリックの回数で選択の単位を変える。
        let Some(point) = terminal_point(state, &layout, col, row) else {
            return;
        };
        let now = std::time::Instant::now();
        let count = match state.mouse.last_click {
            Some((at, cell, n))
                if cell == (col, row) && now.duration_since(at).as_millis() < 400 =>
            {
                n % 3 + 1
            }
            _ => 1,
        };
        state.mouse.last_click = Some((now, (col, row), count));
        let ty = match count {
            2 => SelectionType::Semantic,
            3 => SelectionType::Lines,
            _ => SelectionType::Simple,
        };
        if let Some(session) = state.manager.selected() {
            session.term.lock().selection = Some(Selection::new(ty, point, side));
        }
        state.mouse.dragging = true;
    }

    /// 掴んでいる行の落とし先を、いまの位置から決める。
    ///
    /// ペインの外へ出ても、最後に決めた落とし先は残す。
    /// 端末の座標で決め直すと、離した瞬間に飛ぶ。
    fn drag_sidebar_row(&mut self) {
        let Some(state) = &mut self.state else { return };
        let layout = layout_of(&self.config, state);
        let sl = sidebar_layout(state, &layout);
        let sc = state.renderer.scale();
        let (x, y) = (state.mouse.x, state.mouse.y);
        let Some(drag) = &mut state.mouse.sidebar_drag else {
            return;
        };
        if !drag.moved && (y - drag.start_y).abs() < DRAG_SLOP * sc {
            return;
        }
        drag.moved = true;
        if x >= sl.width {
            return;
        }
        let want = sidebar_boundary(&sl, y);
        let from_row = drag.from_row;
        let to = state.manager.drop_row(from_row, want);
        if let Some(drag) = &mut state.mouse.sidebar_drag {
            if to.is_some() {
                drag.to = to;
            }
        }
    }

    fn drag_selection(&mut self) {
        let Some(state) = &mut self.state else { return };
        let layout = layout_of(&self.config, state);
        let (col, row, side) = mouse_cell(state);
        let Some(point) = terminal_point(state, &layout, col, row) else {
            return;
        };
        if let Some(session) = state.manager.selected() {
            let mut term = session.term.lock();
            if let Some(sel) = term.selection.as_mut() {
                sel.update(point, side);
            }
        }
    }

    fn on_release(&mut self) {
        let Some(state) = &mut self.state else { return };
        // 左ペインで掴んでいた行を落とす。
        if let Some(drag) = state.mouse.sidebar_drag.take() {
            if drag.moved {
                if let Some(to) = drag.to {
                    if state.manager.reorder(drag.from_row, to) {
                        state.mark_state_dirty();
                    }
                }
                return;
            }
        }
        if state.mouse.resizing {
            state.mouse.resizing = false;
            // 離した時点で、残りも合わせる。次のできごとを待たせない。
            self.settle_resize();
            return;
        }
        state.mouse.dragging = false;
        // 動かさずに離したときは選択を消す。
        // 選べていれば、その文字を控える。画面が塗り替わっても取り出せる。
        let picked = match state.manager.selected() {
            Some(session) => {
                let id = session.id;
                let mut term = session.term.lock();
                let empty = term
                    .selection
                    .as_ref()
                    .map(|s| s.is_empty())
                    .unwrap_or(true);
                if empty {
                    term.selection = None;
                    None
                } else {
                    term.selection_to_string()
                        .filter(|t| !t.is_empty())
                        .map(|t| (id, t))
                }
            }
            None => None,
        };
        state.picked = picked;
    }

    /// 落とされたファイルの場所を、打ったのと同じように渡す。
    ///
    /// 複数まとめて落とすと 1 つずつ届く。後ろに空白を置いて、
    /// 続けて並ぶようにする。
    fn on_dropped_file(&mut self, path: &Path) {
        let Some(state) = &mut self.state else { return };
        state.status = None;
        let Some(session) = state.manager.selected() else {
            return;
        };
        let mode = *session.term.lock().mode();
        let mut text = quote_path(path);
        text.push(' ');
        session
            .term
            .lock()
            .scroll_display(alacritty_terminal::grid::Scroll::Bottom);
        session.pty.write(bracketed(&text, mode));
    }

    fn on_wheel(&mut self, delta: MouseScrollDelta) {
        let Some(state) = &mut self.state else { return };
        let cell_height = state.renderer.cell().height.max(1.0);
        let lines = state.mouse.wheel.push(delta, cell_height);
        if lines == 0 {
            return;
        }
        let layout = layout_of(&self.config, state);
        // 左ペインの上で回したら、一覧を送る。ここで返さないと、下の経路が
        // 選んでいるセッションのスクロールバックを遡らせる。
        if state.mouse.x < layout.sidebar_px {
            scroll_sidebar(state, &layout, lines);
            return;
        }
        let (col, row, _) = mouse_cell(state);
        let cell = terminal_cell(&layout, col, row);
        let Some(session) = state.manager.selected() else {
            return;
        };
        let mode = *session.term.lock().mode();

        // まず、プログラムが要求していれば車輪をそのまま渡す。
        if let Some((tcol, trow)) = cell {
            if !state.mods.shift_key() {
                let kind = if lines > 0 {
                    mouse::Kind::WheelUp
                } else {
                    mouse::Kind::WheelDown
                };
                let mut sent = false;
                for _ in 0..lines.abs() {
                    if let Some(bytes) =
                        mouse::encode(kind, mouse::Button::Left, tcol, trow, state.mods, mode)
                    {
                        session.pty.write(bytes);
                        sent = true;
                    }
                }
                if sent {
                    return;
                }
            }
        }
        // 次に、代替画面なら矢印キーに変える。less や man はこれで動く。
        if let Some(bytes) = mouse::alternate_scroll(lines, mode) {
            session.pty.write(bytes);
            return;
        }
        // どちらでもなければ、端末のスクロールバックを動かす。
        session
            .term
            .lock()
            .scroll_display(alacritty_terminal::grid::Scroll::Delta(lines));
    }
}

/// この通知で見えているものが変わるか。
///
/// 窓に出ているのは、選んでいるセッションの画面と、左ペインの各行だけである。
/// 背景のセッションが字を出すたびに描き直すと、見えていないもののために
/// 窓ぜんぶを塗り直すことになる。エージェントを何本も走らせるほど重くなる。
fn wants_redraw(
    event: &UiEvent,
    selected: Option<crate::session::SessionId>,
    shown_working: bool,
) -> bool {
    let shown = |id: crate::session::SessionId| selected == Some(id);
    match event {
        // 画面の中身。見えているのは選んでいるセッションのものだけ。
        // 背景でも、左ペインの印が変わるときは描き直す。
        UiEvent::Wakeup(id, _) => shown(*id) || !shown_working,
        // 左ペインに出るもの。どのセッションのものでも見えている。
        UiEvent::Title(_, _) | UiEvent::ChildExit(_, _) => true,
        // 作業ディレクトリは左ペインの 1 段目に出る。ほかの OSC は
        // 画面の中でしか効かない。
        UiEvent::Osc(id, ev) => shown(*id) || matches!(ev, crate::osc::OscEvent::Cwd(_)),
        // 直近のコマンドは、選んでいるセッションのぶんだけ出す。
        UiEvent::Command(id, _) => shown(*id),
        // 目に見えるものは変わらない。
        UiEvent::ClipboardStore(_, _) | UiEvent::ClipboardLoad(_, _) => false,
        // 古い世代なら一覧は変わらない。受け取れたかどうかで分かれるので、
        // 描き直しは受け取る分岐が決める。
        UiEvent::Scanned(_, _) => false,
    }
}

fn layout_of(config: &Config, state: &State) -> Layout {
    let (cols, rows) = state.renderer.grid_size();
    let cell = state.renderer.cell();
    // 左ペインは画素で幅を決める。端末はその次の桁から始まるので、
    // 端数の 1 桁ぶんだけ隙間ができる。
    let _ = config;
    let sidebar_px = if state.sidebar {
        (state.sidebar_width * state.renderer.scale())
            .min((cols as f32 - 20.0) * cell.width)
            .max(0.0)
    } else {
        0.0
    };
    let sidebar_cols = (sidebar_px / cell.width).ceil() as usize;
    let sep = usize::from(sidebar_cols > 0);
    let term_col = sidebar_cols + sep;
    Layout {
        cols,
        rows,
        sidebar_px,
        sidebar_cols,
        term_col,
        term_cols: cols.saturating_sub(term_col).max(2),
        term_rows: rows.saturating_sub(1).max(1),
    }
}

impl App {
    fn on_picker_key(&mut self, key: &Key, _mods: ModifiersState) {
        let config = self.config.clone();
        let Some(state) = &mut self.state else { return };
        let Some(picker) = &mut state.picker else {
            return;
        };
        let n = picker.names.len();
        match key {
            Key::Named(NamedKey::Escape) => state.picker = None,
            Key::Named(NamedKey::ArrowDown) => picker.selected = (picker.selected + 1) % n,
            Key::Named(NamedKey::ArrowUp) => picker.selected = (picker.selected + n - 1) % n,
            Key::Named(NamedKey::Enter) => {
                let name = picker.names[picker.selected].clone();
                state.picker = None;
                let i = state.manager.selected_index();
                if let Err(e) = state.manager.fork(&config, i, Some(&name)) {
                    state.status = Some(e.to_string());
                }
                state.mark_state_dirty();
            }
            Key::Character(c) => match c.as_str() {
                "j" => picker.selected = (picker.selected + 1) % n,
                "k" => picker.selected = (picker.selected + n - 1) % n,
                d if d.len() == 1 && d.chars().next().unwrap().is_ascii_digit() => {
                    let i = d.chars().next().unwrap().to_digit(10).unwrap() as usize;
                    if i >= 1 && i <= n {
                        let name = picker.names[i - 1].clone();
                        state.picker = None;
                        let idx = state.manager.selected_index();
                        if let Err(e) = state.manager.fork(&config, idx, Some(&name)) {
                            state.status = Some(e.to_string());
                        }
                    }
                }
                _ => {}
            },
            _ => {}
        }
    }

    fn on_rename_key(&mut self, key: &Key, event: &winit::event::KeyEvent) {
        let Some(state) = &mut self.state else { return };
        let Some(rename) = &mut state.rename else {
            return;
        };
        match key {
            Key::Named(NamedKey::Escape) => state.rename = None,
            Key::Named(NamedKey::Enter) => {
                let name = rename.input.trim().to_string();
                state.rename = None;
                let i = state.manager.selected_index();
                if let Some(s) = state.manager.sessions_mut().get_mut(i) {
                    // 空にすると、既定の作業ディレクトリ表示へ戻る。
                    s.name = (!name.is_empty()).then_some(name);
                }
                state.mark_state_dirty();
            }
            Key::Named(NamedKey::Backspace) => {
                rename.input.pop();
            }
            _ => {
                let Some(text) = event.text.as_deref() else {
                    return;
                };
                if text.chars().any(|c| c.is_control()) || !state.preedit.is_empty() {
                    return;
                }
                rename.input.push_str(text);
            }
        }
    }

    fn on_find_key(&mut self, key: &Key, event: &winit::event::KeyEvent, mods: ModifiersState) {
        let Some(state) = &mut self.state else { return };
        let Some(find) = &mut state.find else { return };

        let step: Option<Direction> = match key {
            Key::Named(NamedKey::Escape) => {
                state.find = None;
                state.find_cells.clear();
                state.find_current.clear();
                return;
            }
            // 以降はすべて step を決めてから抜ける。
            Key::Named(NamedKey::Enter) | Key::Named(NamedKey::ArrowUp) => {
                // 既定は古い方（上）へ。端末では下ほど新しいので、
                // 探したいものはたいてい上にある。
                Some(if mods.shift_key() {
                    Direction::Right
                } else {
                    Direction::Left
                })
            }
            Key::Named(NamedKey::ArrowDown) => Some(Direction::Right),
            Key::Named(NamedKey::Backspace) => {
                find.query.pop();
                find.rebuild();
                Some(Direction::Left)
            }
            Key::Named(NamedKey::Tab) => {
                find.regex_mode = !find.regex_mode;
                find.rebuild();
                Some(Direction::Left)
            }
            _ => {
                let Some(text) = event.text.as_deref() else {
                    return;
                };
                if text.chars().any(|c| c.is_control()) {
                    return;
                }
                // 変換中は、確定した文字列だけを Ime::Commit から受ける。
                // ここでも入れると同じ文字が二度入る。
                if !state.preedit.is_empty() {
                    return;
                }
                find.query.push_str(text);
                find.rebuild();
                Some(Direction::Left)
            }
        };
        let Some(direction) = step else { return };
        self.find_step(direction);
    }

    /// 検索を一歩進め、見つかった位置まで画面を送る。
    fn find_step(&mut self, direction: Direction) {
        let Some(state) = &mut self.state else { return };
        let Some(find) = &mut state.find else { return };
        let Some(session) = state.manager.selected() else {
            return;
        };
        let found = {
            let term = session.term.lock();
            find.step(&term, direction)
        };
        if let Some(m) = found {
            session.term.lock().scroll_to_point(*m.start());
        }
    }

    /// 全セッション検索の窓に入る（仕様 §9）。
    ///
    /// 入る口は `⌘⇧F` と、窓を押すことの 2 つある。窓を押す側は `on_click` が
    /// 先に重ねた一覧を閉じて帰るので、前提が違っても今は無事だが、閉じる順を
    /// いつか動かせばそこが破れる。片方だけ直すことにならないよう、入る手順は
    /// ここに 1 つだけ置く。
    fn enter_across(&mut self) {
        let Some(state) = &mut self.state else { return };
        // 重ねた一覧は閉じる。`on_key` は picker・rename → across → find・search の
        // 順に見るので、どちらに残しても「覆いは描かれているのに鍵が届かない」側が
        // 出る。`rename` だけは残す（名前を打ち終える Esc がそちらへ行く）。
        state.picker = None;
        state.search = None;
        state.find = None;
        // 掴んでいた行は放す。結果の一覧に並べ替えは効かせない（仕様 §13）。
        state.mouse.sidebar_drag = None;
        state.across = Some(CrossSearch::default());
        // ペインを閉じていたら開く。窓が見えないまま鍵を奪わない。
        if !state.sidebar {
            state.sidebar = true;
            self.reflow();
        }
    }

    /// 結果の `n` 件目を開く。そのセッションへ切り替え、行が残っていればそこへ飛ぶ。
    ///
    /// 名前・題名・枝の件と、行が履歴から消えた件は、送り先が無いので切り替えて
    /// 語を `⌘F` に渡すだけにする（仕様 §12）。
    fn open_hit(&mut self, n: usize) {
        let Some(state) = &mut self.state else { return };
        let Some(across) = &state.across else { return };
        let Some(hit) = across.hits.get(n).cloned() else {
            return;
        };
        let query = across.query.clone();
        // 閉じたセッションの件なら、何も動かさず窓も閉じない。
        let Some(index) = state.manager.sessions().iter().position(|s| s.id == hit.id) else {
            return;
        };
        state.manager.select(index);
        let mut find = search::ScreenSearch::new();
        find.query = query.clone();
        // 全セッション検索は無条件に大文字小文字を無視する（`scan::needle`）。
        // `RegexSearch::new` は smart-case なので、打った語をそのまま渡すと
        // `SES` で当てた小文字の行を `⌘F` が見つけられず、強調が黙って点かない。
        // 旗を立てるのはこの受け渡しのときだけで、`⌘F` 自身は smart-case のままである。
        find.ignore_case = true;
        find.rebuild();
        state.find = Some(find);
        state.across = None;

        let serial = hit.serial.filter(|_| hit.kind == scan::HitKind::Line);
        let needle = scan::needle(&query);
        use alacritty_terminal::grid::Dimensions as _;
        // 錠は 1 度の短い読みだけで放す。`pushed` は走査と同じ読み方で、錠の中で読む。
        // 読み手も錠の中で書くので、グリッドと総数は食い違わない。
        // それでも下で行を再確認するのは、走査してから選ぶまでに行が書き換わる
        // （その場で描き直す UI、`resize` の折り返し直し）ことへの備えである。
        let landed = state.manager.selected().map(|session| {
            let term = session.term.lock();
            let pushed = session.pushed.load(std::sync::atomic::Ordering::Relaxed);
            let grid = term.grid();
            let land = landing(
                serial,
                pushed,
                grid.history_size(),
                grid.screen_lines() as i32,
            );
            let row = match land {
                Landing::Row(line) => {
                    // 走査と同じ述語（素の行でも伏せ字のあとでも当たる）で確かめる。
                    let mut text = String::new();
                    scan::row_text(grid, line, &mut text);
                    scan::matched(&state.redactor, &text, &needle).map(|_| line)
                }
                _ => None,
            };
            (land, row)
        });
        match landed {
            Some((Landing::Row(_), Some(line))) => {
                if let Some(session) = state.manager.selected() {
                    session.term.lock().scroll_to_point(Point::new(
                        alacritty_terminal::index::Line(line),
                        Column(0),
                    ));
                }
            }
            // 数え違い・書き換え。
            Some((Landing::Search | Landing::Row(_), _)) => self.find_step(Direction::Left),
            // 送り先が無い。語を渡すだけで画面は動かさない。
            Some((Landing::Nothing, _)) | None => {}
        }
    }

    fn on_search_key(&mut self, key: &Key, event: &winit::event::KeyEvent, mods: ModifiersState) {
        let Some(state) = &mut self.state else { return };
        let Some(search) = &mut state.search else {
            return;
        };
        match key {
            Key::Named(NamedKey::Escape) => {
                state.search = None;
                return;
            }
            Key::Named(NamedKey::Enter) => {
                let picked = search.results.get(search.selected).cloned();
                state.search = None;
                if let (Some(entry), Some(s)) = (picked, state.manager.selected()) {
                    // 実行はしない。利用者が内容を確認して Enter を押す。
                    s.pty.write(entry.command.into_bytes());
                }
                return;
            }
            Key::Named(NamedKey::Backspace) => {
                search.query.pop();
            }
            Key::Named(NamedKey::ArrowDown) => {
                if !search.results.is_empty() {
                    search.selected = (search.selected + 1) % search.results.len();
                    search.follow();
                }
                return;
            }
            Key::Named(NamedKey::ArrowUp) => {
                if !search.results.is_empty() {
                    let n = search.results.len();
                    search.selected = (search.selected + n - 1) % n;
                    search.follow();
                }
                return;
            }
            Key::Named(NamedKey::PageDown) => {
                if !search.results.is_empty() {
                    let n = search.results.len();
                    search.selected = (search.selected + SEARCH_ROWS).min(n - 1);
                    search.follow();
                }
                return;
            }
            Key::Named(NamedKey::PageUp) => {
                if !search.results.is_empty() {
                    search.selected = search.selected.saturating_sub(SEARCH_ROWS);
                    search.follow();
                }
                return;
            }
            Key::Character(c) if mods.control_key() => {
                if c.eq_ignore_ascii_case("r") {
                    search.scope = search.scope.next();
                } else {
                    return;
                }
            }
            _ => {
                let Some(text) = event.text.as_deref() else {
                    return;
                };
                if text.chars().any(|c| c.is_control()) {
                    return;
                }
                if !state.preedit.is_empty() {
                    return;
                }
                search.query.push_str(text);
            }
        }
        self.refresh_search();
    }

    fn refresh_search(&mut self) {
        let Some(state) = &mut self.state else { return };
        let (key, cwd) = match state.manager.selected() {
            Some(s) => (s.key.clone(), s.cwd.to_string_lossy().to_string()),
            None => (String::new(), String::new()),
        };
        let Some(search) = &mut state.search else {
            return;
        };
        search.results = match &state.history {
            Some(h) => h
                .search(&search.query, search.scope, &key, &cwd, SEARCH_LIMIT)
                .unwrap_or_default(),
            None => Vec::new(),
        };
        search.selected = 0;
        search.offset = 0;
    }

    /// 検索窓の鍵（仕様 §9）。
    ///
    /// `Esc` で窓を出る。`↑` `↓` で件を選ぶ。語を変えたら走らせ直す。
    /// 変換中の文字列は受けない。確定した分だけを `Ime::Commit` から受ける。
    fn on_across_key(&mut self, key: &Key, event: &winit::event::KeyEvent, mods: ModifiersState) {
        let Some(state) = &mut self.state else { return };
        let layout = layout_of(&self.config, state);
        // 窓に入る件数。割り付けと同じ関数で出す。
        let rows = across_window(state, &layout);
        let Some(across) = &mut state.across else {
            return;
        };
        match key {
            Key::Named(NamedKey::Escape) => {
                // 窓を出て、元の一覧へ戻る。走っている走査の結果は、
                // 受け手が居なくなるので `Scanned` の腕で捨てられる。
                state.across = None;
                return;
            }
            Key::Named(NamedKey::Enter) => {
                let n = across.selected;
                self.open_hit(n);
                return;
            }
            Key::Named(NamedKey::ArrowDown) => {
                if !across.hits.is_empty() {
                    across.selected = (across.selected + 1) % across.hits.len();
                    across.follow(rows);
                }
                return;
            }
            Key::Named(NamedKey::ArrowUp) => {
                if !across.hits.is_empty() {
                    let n = across.hits.len();
                    across.selected = (across.selected + n - 1) % n;
                    across.follow(rows);
                }
                return;
            }
            Key::Named(NamedKey::Backspace) => {
                across.query.pop();
            }
            _ => {
                // 貼り付けだけは `⌘` の和音でも通す。語を貼るのは普通の操作である。
                if input::action_for(key, event.physical_key, mods) == Some(Action::Paste) {
                    if let Some(text) = clipboard::paste() {
                        across.push_pasted(&text);
                        self.queue_scan();
                    }
                    return;
                }
                let Some(text) = event.text.as_deref() else {
                    return;
                };
                if text.chars().any(|c| c.is_control()) {
                    return;
                }
                // 変換中は、確定した文字列だけを `Ime::Commit` から受ける（仕様 §9）。
                // ここでも入れると同じ文字が二度入る。
                if !state.preedit.is_empty() {
                    return;
                }
                // `⌘` の和音は字として入れない。入れると `⌘⇧F` を
                // 押し直すたびに "F" が語へ足される。
                if mods.super_key() {
                    return;
                }
                across.query.push_str(text);
            }
        }
        self.queue_scan();
    }

    /// 語が変わったことを覚える。走らせるのは手が止まってからである（仕様 §10）。
    fn queue_scan(&mut self) {
        let Some(across) = self.state.as_mut().and_then(|s| s.across.as_mut()) else {
            return;
        };
        across.typed_at = Some(std::time::Instant::now());
    }

    /// 走査を別スレッドで始める。
    ///
    /// UI スレッドは `scan` を呼ばない（仕様 §4）。1 フレームの余白は 0.3ms ほどで、
    /// 14 本 × 10000 行の走査はそこに入らない。別スレッドへ渡すのは `Arc` の複製と
    /// 短い文字列だけで、`Term` の中身は写さない（仕様 §10）。
    /// 結果は `UiEvent::Scanned` で帰ってくる。
    fn start_scan(&mut self) {
        let ui_tx: crate::term::UiSender = self.proxy.clone().into();
        let Some(state) = &mut self.state else { return };
        let Some(across) = &mut state.across else {
            return;
        };
        // 走らせるかどうかに関わらず、待ちは解く。残すと過ぎた時刻で起こされ続ける。
        across.typed_at = None;
        // 世代は、走らせない場合でも進める。進めずに抜けると、まだ走っている
        // 前の走査の結果が `accept` に一致して、消した語の当たりが入ってしまう。
        state.scan_generation += 1;
        across.generation = state.scan_generation;
        // 語が空白だけなら走らせない。出ている当たりは、ここで捨てる。
        if across.query.trim().is_empty() {
            let dropped = across.clear_hits();
            // ここへ来るのは描画の最後である。いま出ている一覧は描き終わっていて、
            // 捨てたことを知らせる機会が他に無い。捨てたときだけ描き直しを頼む。
            if dropped {
                state.request_redraw();
            }
            return;
        }
        let generation = across.generation;
        let query = across.query.clone();
        across.scanning = true;
        let targets: Vec<scan::Target> = state
            .manager
            .sessions()
            .iter()
            .map(|s| scan::Target {
                id: s.id,
                term: s.term.clone(),
                pushed: s.pushed.clone(),
                ticks: s.ticks.clone(),
                // 探すのは左ペインに出ている名前である（仕様 §7）。
                name: s.display_name().0,
                title: s.window_title.clone(),
                branch: s.branch.clone(),
            })
            .collect();
        let redactor = state.redactor.clone();
        let spawned = std::thread::Builder::new()
            .name("termit-scan".into())
            .spawn(move || {
                let hits = scan::scan(&targets, &query, &redactor, scan::Limits::default());
                // 窓が閉じていれば送り先はもう無い。結果は捨てる。
                let _ = ui_tx.send_event(UiEvent::Scanned(generation, hits));
            });
        if let Err(e) = spawned {
            // スレッドが作れないなら、この走査だけ諦める。端末は動き続ける。
            log::warn!("cannot start the cross-session scan: {e}");
            if let Some(across) = state.across.as_mut() {
                across.scanning = false;
            }
        }
    }

    // ---------------------------------------------------------------- 描画

    fn draw(&mut self, event_loop: &ActiveEventLoop) {
        // 引きずる手が止まったら、待たせていたセッションを合わせる。
        if self
            .resize_quiet_since
            .is_some_and(|at| at.elapsed() >= RESIZE_SETTLE)
        {
            self.settle_resize();
        }
        let Some(state) = &mut self.state else { return };
        // 最後の 1 つを閉じたら窓ごと閉じる。鍵盤でも鼠でもここを通る。
        if state.manager.is_empty() {
            event_loop.exit();
            return;
        }
        // 窓が隠れているなら、何も組み立てない。
        // 組み立ててから描く面を取りに行くと、取れなかったぶんが丸ごと無駄になる。
        if state.occluded {
            state.needs_redraw = true;
            return;
        }
        self.refresh_agent_state();
        let Some(state) = &mut self.state else { return };
        // 出すセッションの大きさを、描く前に必ず合わせる。
        // 引きずるあいだ待たせたものを、そのまま描くと下半分が空く。
        state.manager.ensure_visible_size();
        // 「描いた」印は、画面の内容を読む前に落とす。あとで落とすと、
        // 読んでから落とすまでのあいだに届いた更新が、通知を出さないまま
        // 捨てられる。出力が止まる直前（プロンプトが出た瞬間）に当たると、
        // その一画面ぶんが永久に描かれない。
        state.manager.clear_dirty();
        let layout = layout_of(&self.config, state);

        self.refresh_recent();
        let Some(state) = &mut self.state else { return };
        state.manager.drain_pending_clears();
        if state.manager.refresh_metadata() {
            // cd で動いたら、覚えている並びも書き直す。
            state.mark_state_dirty();
        }
        state.save_state(false);
        collect_find_cells(state);
        // 選んでいるセッションが名乗った題名を、ウィンドウに出す。
        let want_title = state
            .manager
            .selected()
            .and_then(|s| s.window_title.clone())
            .unwrap_or_else(|| "termit".to_string());
        if want_title != state.shown_title {
            if let Some(w) = &state.window {
                w.set_title(&want_title);
            }
            state.shown_title = want_title;
        }
        let theme = state.theme;
        state.renderer.begin();

        if layout.sidebar_cols > 0 {
            // 縦の区切り線は引かない。地の色が違うので境目は分かる。
            draw_sidebar(state, &layout, &theme);
        }
        draw_terminal(state, &layout, &theme);
        draw_bottom(state, &layout, &theme);

        // 候補窓を、いま文字が入る位置へ寄せる。
        if let (Some((cx, cy)), Some(window)) = (state.cursor_px, state.window.clone()) {
            let cell = state.renderer.cell();
            let pos = (cx as i32, cy as i32);
            if state.ime_area != Some(pos) {
                state.ime_area = Some(pos);
                window.set_ime_cursor_area(
                    winit::dpi::PhysicalPosition::new(pos.0, pos.1),
                    winit::dpi::PhysicalSize::new(
                        cell.width.ceil() as u32,
                        cell.height.ceil() as u32,
                    ),
                );
            }
        }
        let drew = state.renderer.render(theme.bg);
        if drew {
            state.needs_redraw = false;
        } else {
            // 隠れているなどで描けなかった。間を置いて試す。
            // すぐ要求し直すと、隠れているあいだ空回りする。
            state.needs_redraw = true;
            event_loop.set_control_flow(ControlFlow::WaitUntil(
                std::time::Instant::now() + std::time::Duration::from_millis(32),
            ));
            return;
        }
        let latency = state.pending_since.take().map(|t| t.elapsed());
        if let Some(c) = &mut self.counters {
            c.drew += 1;
            if let Some(d) = latency {
                c.latency_us.push(d.as_micros() as u64);
            }
        }
        // 次に起こしてもらう時刻。近いほうを採る。
        let mut wake: Option<std::time::Instant> = None;
        let mut want = |at: std::time::Instant| {
            wake = Some(match wake {
                Some(w) if w <= at => w,
                _ => at,
            });
        };
        // 合わせ残しがあるなら、手が止まったころに。
        // 引きずり終わりが最後のできごとになることがある。
        if let Some(at) = self.resize_quiet_since {
            want(at + RESIZE_SETTLE);
        }
        // 動いているセッションがあるなら、印を落とすために時々。
        // 出力が止まったことは、それ自体では何も知らせてくれない。
        if let Some(state) = &self.state {
            if state.manager.sessions().iter().any(|s| s.shown_working) {
                want(std::time::Instant::now() + WORKING_REFRESH);
            }
        }
        // 横断検索の語が変わっているなら、手が止まったころに走らせる。
        let typed_at = self
            .state
            .as_ref()
            .and_then(|s| s.across.as_ref())
            .and_then(|across| across.typed_at);
        match scan_due(typed_at, std::time::Instant::now(), SCAN_DEBOUNCE) {
            ScanWhen::Idle => {}
            ScanWhen::Start => self.start_scan(),
            ScanWhen::WakeAt(due) => want(due),
        }
        if let Some(at) = wake {
            event_loop.set_control_flow(ControlFlow::WaitUntil(at));
        }
    }

    /// 返事待ちの印を更新する。
    ///
    /// 承認や質問が出ていると、エージェントは出力を止めて待つ。
    /// 「動いていない」だけでは、暇なのか返事待ちなのか区別できない。
    /// 手がかりの表は設定にあり、termit は文字を照合するだけである。
    ///
    /// 読むのは出力が止まっているセッションの末尾数行だけで、間隔も置く。
    /// 毎フレーム全部の画面を読むと、費用が印に見合わない。
    fn refresh_agent_state(&mut self) {
        if self.config.agent.blocked_when.is_empty() {
            return;
        }
        let now = std::time::Instant::now();
        if let Some(at) = self.agent_state_at {
            if now.duration_since(at) < AGENT_STATE_INTERVAL {
                return;
            }
        }
        self.agent_state_at = Some(now);
        let lines = self.config.agent.blocked_lines;
        let phrases = &self.config.agent.blocked_when;
        let Some(state) = &mut self.state else { return };
        for s in state.manager.sessions_mut() {
            s.blocked = if !s.is_running() || s.is_working() {
                false
            } else {
                let tail = {
                    let term = s.term.lock();
                    crate::session::screen_tail(&term, lines)
                };
                crate::session::Session::blocked_in(&tail, phrases)
            };
        }
    }

    fn refresh_recent(&mut self) {
        let Some(state) = &mut self.state else { return };
        let Some((id, key)) = state.manager.selected().map(|s| (s.id, s.key.clone())) else {
            state.recent.clear();
            return;
        };
        if state.recent_for == Some(id) {
            return;
        }
        state.recent = match &state.history {
            Some(h) => h.recent(&key, 10).unwrap_or_default(),
            None => Vec::new(),
        };
        state.recent_for = Some(id);
    }
}

/// 左ペインの寸法。単位は pt で、描くときに表示の倍率を掛ける。
///
/// Warp の一覧を画素から測った値に合わせてある。
/// 実測は docs/warp-metrics.md に記録した。
mod sidebar {
    /// 行の矩形の左余白（ペインの左端から）。
    pub const MARGIN_L: f32 = 6.0;
    /// 行の矩形の右余白（ペインの右端まで）。
    pub const MARGIN_R: f32 = 8.0;
    /// 矩形と矩形のあいだ。
    pub const GAP: f32 = 4.0;
    /// 矩形の上端から 1 段目まで。
    pub const PAD_TOP: f32 = 9.0;
    /// 最後の段から矩形の下端まで。
    pub const PAD_BOTTOM: f32 = 10.0;
    /// 実行状態の印の左端（矩形の左端から）。
    pub const MARK_INSET: f32 = 7.0;
    /// 文字の左端（矩形の左端から）。印のぶん右へ寄せる。
    pub const TEXT_INSET: f32 = 20.0;
    /// 分岐 1 段ぶんの字下げ。
    pub const INDENT: f32 = 12.0;
    /// 矩形の角を丸める半径。
    pub const RADIUS: f32 = 6.0;
    /// 上端に `+` を置く帯の高さ。
    pub const HEADER: f32 = 34.0;
    /// 帯の右端で `+` が取る幅。残りが検索窓になる（仕様 §6）。
    ///
    /// 描く側と当たり判定で同じ値を使う。食い違うと、窓を押したのに
    /// セッションが増える。
    pub const PLUS_ZONE: f32 = 24.0;
    /// 変換中に、検索窓の中で変換中の文字列へ必ず残す幅。
    ///
    /// 確定した語が窓いっぱいに伸びていると、変換中の文字列に幅が残らず、
    /// いま打っている字が見えなくなる。全角で 4 字ぶん（12pt）より少し広く取る。
    pub const MIN_PREEDIT: f32 = 56.0;
    /// 区切り線の上下の余白。
    pub const SEP_PAD: f32 = 8.0;

    /// 1 段目（名前）。
    pub const SIZE_TITLE: f32 = 13.0;
    /// 2 段目（名乗った題名）。
    pub const SIZE_SUB: f32 = 12.0;
    /// 3 段目（ブランチ名）。
    pub const SIZE_BRANCH: f32 = 10.0;
    /// 直近のコマンド。
    pub const SIZE_RECENT: f32 = 11.0;
    /// 検索窓に打った語。
    pub const SIZE_QUERY: f32 = 12.0;

    /// 当たった語に敷く色を、文字の両端へ広げる量。
    ///
    /// 字にぴったり敷くと、前後の字と地続きに見えて語の切れ目が読めない。
    pub const HIT_PAD: f32 = 4.0;
}

/// `⌘C` で写す文字を決める。
///
/// いま選ばれているものがあればそれを使う。無ければ、最後に選び終えた
/// 文字を使う。全画面 UI は選んだ行を書き直した時点で選択を捨てるため、
/// これが無いと、選べているのに何も写らないという形になる。
/// 控えは持ち主のセッションでだけ使う。
fn copy_text(
    live: Option<String>,
    picked: Option<&(crate::session::SessionId, String)>,
    selected: Option<crate::session::SessionId>,
) -> Option<String> {
    if let Some(text) = live.filter(|t| !t.is_empty()) {
        return Some(text);
    }
    match (picked, selected) {
        (Some((owner, text)), Some(id)) if *owner == id && !text.is_empty() => Some(text.clone()),
        _ => None,
    }
}

/// いま指しているリンク。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HoveredLink {
    /// 押したときに開く相手。
    pub(crate) uri: String,
    /// OSC 8 の識別子。素の URL を見つけたときは `None`。
    pub(crate) id: Option<String>,
    /// 素の URL のときの範囲。OSC 8 のときは `None`。
    pub(crate) span: Option<alacritty_terminal::term::search::Match>,
}

/// ファイルを持ってきたときに下の帯へ出す案内。
const DROP_HINT: &str = "drop to paste the path";

/// 端末上のプログラムがマウスを掴んでいるときに出す逃げ道。
///
/// Claude Code のような全画面 UI は起動時に `?1000` `?1002` `?1003` を
/// 設定する。以後、引いた動きはプログラムのものになり、端末側の選択は
/// できない。Shift を足すと端末が受け取る。
const SELECT_HINT: &str = "the program is using the mouse — hold ⇧ to select text";

/// 左ペインと端末の境目を掴める幅（pt）。
const DIVIDER_GRAB: f32 = 4.0;

/// 境目の上にカーソルがあるか。
fn on_divider(state: &State, layout: &Layout, px: f32) -> bool {
    layout.sidebar_px > 0.0
        && (px - layout.sidebar_px).abs() <= DIVIDER_GRAB * state.renderer.scale()
}

/// 実行状態の印の色。セッションの一覧と結果の一覧で同じ組みにする。
///
/// 待っている＞動いている＞止まっている、の順に強い。
/// 待ちは黄色。動いている緑とも、異常終了の赤とも違う色にする。
fn mark_color(theme: &Theme, exit: Option<i32>, working: bool, blocked: bool) -> Rgb {
    match exit {
        None if blocked => theme.ansi[3],
        None if working => theme.accent,
        None => theme.fg_tertiary,
        Some(0) => theme.fg_tertiary,
        Some(_) => theme.warn,
    }
}

/// 左ペインの文字の体裁。桁に縛られないので、等幅でない書体で組む。
///
/// 同じ幅に多く入り、Warp の一覧の見え方に近くなる。
fn style(size: f32, bold: bool) -> TextStyle {
    let s = TextStyle::sans(size);
    if bold {
        s.bold()
    } else {
        s
    }
}

/// 左ペインの 1 件ぶんの領域。描画と当たり判定で同じ値を使う。
struct Block {
    /// セッションの一覧では `sessions()` の添字。結果の一覧では
    /// `across.hits` の添字で、送った位置（`offset`）を足した絶対の値。
    index: usize,
    depth: usize,
    /// 矩形の上端（画素）。
    top: f32,
    /// 矩形の高さ（画素）。
    height: f32,
}

struct SidebarLayout {
    /// ペインの幅（画素）。
    width: f32,
    /// 行の矩形の左端と幅（画素）。
    rect_x: f32,
    rect_w: f32,
    blocks: Vec<Block>,
    /// 一覧の先頭に出している行。`blocks` の添字に足すと木の行になる。
    ///
    /// 結果の一覧では 0 のままにする。`Block::index` が当たりの絶対の
    /// 位置を持っており、送った位置はそちらに入っている。
    offset: usize,
    /// 区切り線の位置（画素）。
    sep_y: f32,
}

/// 一覧に使える高さ（画素）。下段に直近のコマンドを出す余地を残す。
fn sidebar_limit(state: &State, layout: &Layout) -> f32 {
    let height_px = layout.rows as f32 * state.renderer.cell().height;
    height_px - 120.0 * state.renderer.scale()
}

/// 各行の矩形の高さ（画素）。渡された行と同じ並びで返す。
///
/// 高さは段の数で変わる（題名や枝があれば高い）。送り位置を決める側と
/// 割り付ける側で同じ値を使うため、ここで 1 度だけ出す。
/// 行の並びは呼ぶ側から渡す。`tree_rows` は毎回組み直すので、
/// 1 フレームのうちに何度も作らせない。
fn sidebar_row_heights(state: &State, rows: &[crate::session::TreeRow]) -> Vec<f32> {
    let sc = state.renderer.scale();
    let title = state.renderer.line_height_px(sidebar::SIZE_TITLE);
    let sub = state.renderer.line_height_px(sidebar::SIZE_SUB);
    let branch = state.renderer.line_height_px(sidebar::SIZE_BRANCH);
    rows.iter()
        .map(|tree| {
            // `tree_rows` はセッションを数え上げて作るので、添字は必ずある。
            let s = &state.manager.sessions()[tree.index];
            let mut content = title;
            if s.window_title.is_some() {
                content += sub;
            }
            if s.branch.is_some() {
                content += branch;
            }
            (sidebar::PAD_TOP + sidebar::PAD_BOTTOM) * sc + content
        })
        .collect()
}

/// `offset` から `limit` までに置ける行数。
///
/// 高さが行ごとに違うので、件数では出せない。割り付け（`sidebar_layout`）と
/// 同じ順で足していく。
fn sidebar_rows(heights: &[f32], offset: usize, top: f32, limit: f32, gap: f32) -> usize {
    let mut y = top;
    let mut rows = 0;
    for h in heights.iter().skip(offset) {
        if y + h > limit {
            break;
        }
        rows += 1;
        y += h + gap;
    }
    rows
}

/// 送れる上限。最後の行が下端に来る位置より先へは送らない。
///
/// 送りすぎて空白だけを出さないための歯止めである。末尾から積み上げて、
/// まだ入る限り先頭へ戻す。
fn sidebar_max_offset(heights: &[f32], top: f32, limit: f32, gap: f32) -> usize {
    let mut y = top;
    let mut offset = heights.len();
    for (i, h) in heights.iter().enumerate().rev() {
        if y + h > limit {
            break;
        }
        y += h + gap;
        offset = i;
    }
    // 送り位置は行の範囲に収める。`len` を返すと、どこへ送っても空になる。
    offset.min(heights.len().saturating_sub(1))
}

/// 選んだ行が窓の中に入るよう、送り位置を出す。
///
/// 上に隠れていればその行へ戻す。下に隠れていれば、入るまで 1 行ずつ送る。
/// 行の高さが違うので、引き算では出せない。
fn sidebar_follow(
    heights: &[f32],
    offset: usize,
    selected: usize,
    top: f32,
    limit: f32,
    gap: f32,
) -> usize {
    if selected < offset {
        return selected;
    }
    let mut offset = offset;
    while selected >= offset + sidebar_rows(heights, offset, top, limit, gap) {
        // 1 行も入らない高さでは、送っても入らない。選んだ行で止める。
        if offset >= selected {
            break;
        }
        offset += 1;
    }
    offset
}

/// 車輪の行数を送り位置に足す。
///
/// 上へ回すと `lines` は正で、先頭（古い添字）へ戻る。
fn wheel_offset(offset: usize, lines: i32, max: usize) -> usize {
    if lines > 0 {
        offset.saturating_sub(lines as usize)
    } else {
        (offset + lines.unsigned_abs() as usize).min(max)
    }
}

/// 送り位置を、いまの選択と高さに合わせ直す。
///
/// 選択行が変わったときだけ追いかける。毎フレーム追いかけると、車輪で
/// 送った位置をその場で戻してしまい、車輪が効かなくなる。
/// 上限の丸めは毎フレームかける。セッションを閉じれば上限は下がる。
fn settle_sidebar_offset(state: &mut State, layout: &Layout) {
    let sc = state.renderer.scale();
    let top = sidebar::HEADER * sc;
    let gap = sidebar::GAP * sc;
    let limit = sidebar_limit(state, layout);
    let rows = state.manager.tree_rows();
    let heights = sidebar_row_heights(state, &rows);
    let selected = state.manager.selected_index();
    let row = rows.iter().position(|r| r.index == selected);
    if state.sidebar_followed != row {
        state.sidebar_followed = row;
        if let Some(row) = row {
            state.sidebar_offset =
                sidebar_follow(&heights, state.sidebar_offset, row, top, limit, gap);
        }
    }
    state.sidebar_offset = state
        .sidebar_offset
        .min(sidebar_max_offset(&heights, top, limit, gap));
}

/// 左ペインの一覧を車輪で送る。いま出している側の一覧だけを動かす。
fn scroll_sidebar(state: &mut State, layout: &Layout, lines: i32) {
    if showing_hits(state.across.as_ref()) {
        // 窓に入る件数は割り付けと同じ関数で出す。
        let rows = across_window(state, layout);
        if let Some(across) = &mut state.across {
            let max = across.hits.len().saturating_sub(rows);
            across.offset = wheel_offset(across.offset, lines, max);
        }
        return;
    }
    let sc = state.renderer.scale();
    let rows = state.manager.tree_rows();
    let heights = sidebar_row_heights(state, &rows);
    let max = sidebar_max_offset(
        &heights,
        sidebar::HEADER * sc,
        sidebar_limit(state, layout),
        sidebar::GAP * sc,
    );
    state.sidebar_offset = wheel_offset(state.sidebar_offset, lines, max);
}

/// セッションの一覧を、結果の一覧に差し替えるか。
///
/// 語が空、または空白だけなら元の一覧へ戻る（仕様 §6）。
/// 走査を始めない判定（`start_scan`）と同じ規則にしてある。違えてしまうと、
/// 語を消し切って当たりを捨てたあとも、空の一覧が出たままになる。
fn showing_hits(across: Option<&CrossSearch>) -> bool {
    across.is_some_and(|a| !a.query.trim().is_empty())
}

/// 結果 1 件ぶんの高さ（画素）。1 件は 2 段を取る（仕様 §6）。
///
/// 測った段の高さを受け取るので、`Renderer` を立てずに試験できる。
fn across_block_height(sc: f32, title_h: f32, sub_h: f32) -> f32 {
    (sidebar::PAD_TOP + sidebar::PAD_BOTTOM) * sc + title_h + sub_h
}

/// 当たりが 1 件も無いときに、代わりの字を出す高さ（画素）。
///
/// 1 件目と同じ位置から出す。割り付けもこの字のぶんを空ける。
fn across_note_y(sc: f32) -> f32 {
    sidebar::HEADER * sc + sidebar::PAD_TOP * sc
}

/// `top` から `limit` までに置ける件数。
fn across_rows(top: f32, limit: f32, height: f32, gap: f32) -> usize {
    // 高さの無い件を数え続けない。ここは描画の経路にある。
    if height <= 0.0 {
        return 0;
    }
    let mut rows = 0;
    let mut y = top;
    while y + height <= limit {
        rows += 1;
        y += height + gap;
    }
    rows
}

/// いま結果を置ける件数。
///
/// 割り付け（`across_blocks`）と窓送り（`CrossSearch::follow`）で同じ値を使う。
/// 食い違うと、選んだ件が見えない位置へ出る。
fn across_window(state: &State, layout: &Layout) -> usize {
    let sc = state.renderer.scale();
    let height = across_block_height(
        sc,
        state.renderer.line_height_px(sidebar::SIZE_TITLE),
        state.renderer.line_height_px(sidebar::SIZE_SUB),
    );
    across_rows(
        sidebar::HEADER * sc,
        sidebar_limit(state, layout),
        height,
        sidebar::GAP * sc,
    )
}

/// 結果の一覧の領域を割り付ける。入りきらない件は `offset` から出す。
fn across_blocks(state: &State, layout: &Layout) -> Vec<Block> {
    let Some(across) = &state.across else {
        return Vec::new();
    };
    let sc = state.renderer.scale();
    let height = across_block_height(
        sc,
        state.renderer.line_height_px(sidebar::SIZE_TITLE),
        state.renderer.line_height_px(sidebar::SIZE_SUB),
    );
    let last = (across.offset + across_window(state, layout)).min(across.hits.len());
    let mut blocks = Vec::new();
    let mut y = sidebar::HEADER * sc;
    for index in across.offset..last {
        // 結果の一覧に系統樹の字下げは無い（仕様 §13）。
        blocks.push(Block {
            index,
            depth: 0,
            top: y,
            height,
        });
        y += height + sidebar::GAP * sc;
    }
    blocks
}

fn sidebar_layout(state: &State, layout: &Layout) -> SidebarLayout {
    let sc = state.renderer.scale();
    let width = layout.sidebar_px;
    let rect_x = sidebar::MARGIN_L * sc;
    let rect_w = (width - (sidebar::MARGIN_L + sidebar::MARGIN_R) * sc).max(0.0);
    let limit = sidebar_limit(state, layout);

    let mut blocks = Vec::new();
    let mut offset = 0;
    let mut y = sidebar::HEADER * sc;
    if showing_hits(state.across.as_ref()) {
        // 打っているあいだ、一覧は結果の一覧に差し替わる（仕様 §6）。
        blocks = across_blocks(state, layout);
        if let Some(b) = blocks.last() {
            y = b.top + b.height + sidebar::GAP * sc;
        } else {
            // 1 件も置かないときは、代わりに出す字のぶんを空ける。
            // 空けないと、区切り線と直近のコマンドがその字に重なる。
            y = across_note_y(sc)
                + state.renderer.line_height_px(sidebar::SIZE_SUB)
                + sidebar::PAD_BOTTOM * sc
                + sidebar::GAP * sc;
        }
    } else {
        // 入りきらない行は下で見切れる。`offset` から出し、残りは車輪で送る。
        let rows = state.manager.tree_rows();
        let heights = sidebar_row_heights(state, &rows);
        offset =
            state
                .sidebar_offset
                .min(sidebar_max_offset(&heights, y, limit, sidebar::GAP * sc));
        for (row, tree) in rows.iter().enumerate().skip(offset) {
            let height = heights[row];
            if y + height > limit {
                break;
            }
            blocks.push(Block {
                index: tree.index,
                depth: tree.depth,
                top: y,
                height,
            });
            y += height + sidebar::GAP * sc;
        }
    }
    let sep_y = y - sidebar::GAP * sc + sidebar::SEP_PAD * sc;
    SidebarLayout {
        width,
        rect_x,
        rect_w,
        blocks,
        offset,
        sep_y,
    }
}

/// 縦の位置が、行と行のどの境目を指しているか。
///
/// 返すのは木の行の境目（`Manager::drop_row` と同じ単位）で、窓に出ている
/// 位置ではない。送っているあいだ、この 2 つは `offset` だけずれる。
/// 混ぜると、掴んだ行が送った数だけ離れた位置へ落ちる。
fn sidebar_boundary(sl: &SidebarLayout, py: f32) -> usize {
    for (i, b) in sl.blocks.iter().enumerate() {
        if py < b.top + b.height / 2.0 {
            return sl.offset + i;
        }
    }
    sl.offset + sl.blocks.len()
}

/// 境目を描く高さ。`at` は木の行の境目。
fn sidebar_boundary_y(sl: &SidebarLayout, at: usize, sc: f32) -> f32 {
    let half = sidebar::GAP * sc / 2.0;
    // 窓より上の境目は先頭の行の上端、下の境目は末尾の行の下端に寄せる。
    // 窓の外に線を引いても見えない。
    match sl.blocks.get(at.saturating_sub(sl.offset)) {
        Some(b) => b.top - half,
        None => match sl.blocks.last() {
            Some(b) => b.top + b.height + half,
            None => sidebar::HEADER * sc,
        },
    }
}

/// 左ペインのどこを押したか。
enum SidebarHit {
    NewSession,
    /// 見出しの帯の検索窓。押すと窓に入る（仕様 §9）。
    SearchBox,
    Select(usize),
    Close(usize),
    /// 結果の一覧の件。添字は `across.hits` の絶対の位置。
    Hit(usize),
}

/// 画素の位置で当たりを見る。
fn sidebar_hit(state: &State, layout: &Layout, px: f32, py: f32) -> Option<SidebarHit> {
    if layout.sidebar_px <= 0.0 || px >= layout.sidebar_px {
        return None;
    }
    let sc = state.renderer.scale();
    let sl = sidebar_layout(state, layout);
    if py < sidebar::HEADER * sc {
        // 見出しの帯の右端に `+` を置き、残りを検索窓にしている（仕様 §6）。
        let plus_right = sl.rect_x + sl.rect_w;
        if px > plus_right - sidebar::PLUS_ZONE * sc {
            return Some(SidebarHit::NewSession);
        }
        return Some(SidebarHit::SearchBox);
    }
    // 結果の一覧に × も並べ替えも効かせない（仕様 §13）。
    let hits = showing_hits(state.across.as_ref());
    for b in &sl.blocks {
        if py < b.top || py >= b.top + b.height {
            continue;
        }
        if hits {
            return Some(SidebarHit::Hit(b.index));
        }
        // × は、カーソルが乗っているときだけ 1 段目の右端に出る。
        // 出ていない位置で閉じないよう、同じ範囲でだけ受ける。
        let first_line = state.renderer.line_height_px(sidebar::SIZE_TITLE);
        let close_right = sl.rect_x + sl.rect_w - 6.0 * sc;
        let close_left = close_right - 18.0 * sc;
        if py < b.top + sidebar::PAD_TOP * sc + first_line && px >= close_left && px < close_right {
            return Some(SidebarHit::Close(b.index));
        }
        return Some(SidebarHit::Select(b.index));
    }
    None
}

/// 検索窓の目印。
///
/// 絵文字は書体によって豆腐になる。`--probe` で出した画を見て選んである。
const GLASS: &str = "🔍 ";

/// 窓に入っていないときに出す案内。
///
/// 窓は常に見えていて、`⌘⇧F` は近道にすぎない（仕様 §12）。
/// 何に使う窓かが読めなければ、見えていても見えていないのと同じである。
const ACROSS_HINT: &str = "search all sessions  ⌘⇧F";

/// 当たりを拾ったあとに終わったセッションの代わりに出す字。
///
/// `Hit.id` は、走査が走っているあいだに終わったセッションを指しうる。
/// 1 段目は引けないが、当たった文は残っているので、件ごと捨てはしない。
const CLOSED_SESSION: &str = "(closed)";

/// 見出しの帯に置く検索窓。
///
/// `across` が `None` のあいだも薄く出す（仕様 §6「窓は常に見えている」）。
/// 入っているあいだは縁と棒を出し、変換中の文字列もここへ出す（仕様 §9）。
fn draw_search_box(state: &mut State, sl: &SidebarLayout, theme: &Theme) {
    let sc = state.renderer.scale();
    let st = style(sidebar::SIZE_QUERY, false);
    let line_h = state.renderer.line_height_px(sidebar::SIZE_QUERY);
    let box_h = line_h + 8.0 * sc;
    let box_y = ((sidebar::HEADER * sc - box_h) / 2.0).max(0.0);
    // `+` のぶんを右端に残す。当たり判定と同じ値で切る。
    let box_w = sl.rect_w - sidebar::PLUS_ZONE * sc;
    if box_w <= 0.0 {
        return;
    }
    let radius = sidebar::RADIUS * sc;
    let focused = state.across.is_some();
    if focused {
        // 縁は、縁の色で塗ったうえへ地の色を 1 画素内側から重ねて作る。
        let t = sc.max(1.0);
        state
            .renderer
            .fill_px(sl.rect_x, box_y, box_w, box_h, theme.accent, radius);
        state.renderer.fill_px(
            sl.rect_x + t,
            box_y + t,
            box_w - t * 2.0,
            box_h - t * 2.0,
            theme.surface,
            radius,
        );
    } else {
        state
            .renderer
            .fill_px(sl.rect_x, box_y, box_w, box_h, theme.surface, radius);
    }

    let pad = 6.0 * sc;
    let ty = box_y + (box_h - line_h) / 2.0;
    let mut x = sl.rect_x + pad;
    x += state
        .renderer
        .put_text_px(x, ty, GLASS, st, theme.fg_tertiary);
    let avail = (sl.rect_x + box_w - pad - x).max(0.0);
    if !focused {
        let hint = state.renderer.fit_tail(ACROSS_HINT, st, avail);
        state
            .renderer
            .put_text_px(x, ty, &hint, st, theme.fg_tertiary);
        return;
    }
    let query = state
        .across
        .as_ref()
        .map(|a| a.query.clone())
        .unwrap_or_default();
    // 打っている末尾を残す。先頭が落ちる。
    // 変換中は、変換中の文字列のぶんを先に取り分ける。
    let (query_w, _) = split_box_width(avail, !state.preedit.is_empty(), sidebar::MIN_PREEDIT * sc);
    let shown = state.renderer.fit_head(&query, st, query_w);
    let used = state
        .renderer
        .put_text_px(x, ty, &shown, st, theme.fg_primary);
    x += used;
    // 箱の右端までに収める。下敷きは窓自身の色にする。
    let room = preedit_room(avail, used);
    x += draw_preedit_px(state, x, ty, st, theme, Some((room, theme.surface)));
    state
        .renderer
        .fill_px(x, ty, 2.0 * sc, line_h, theme.cursor, 0.0);
    // 候補窓はここへ寄せる。`draw_terminal` は箱に入っているあいだ書かない。
    state.cursor_px = Some((x, ty));
}

/// 窓の中の幅を、確定した語と変換中の文字列へ分ける。
/// 変換中は、いま打っている末尾が必ず見えるだけの幅を残す。
fn split_box_width(avail: f32, composing: bool, min_preedit: f32) -> (f32, f32) {
    let avail = avail.max(0.0);
    if !composing {
        return (avail, 0.0);
    }
    let preedit = min_preedit.min(avail);
    (avail - preedit, preedit)
}

/// 語を描いたあとに、変換中の文字列へ残る幅。
///
/// 取り分は語の予算にだけ掛ける（`split_box_width`）。変換中の文字列は、
/// 語が実際に使わなかった幅をすべて使える。
fn preedit_room(avail: f32, query_used: f32) -> f32 {
    (avail - query_used).max(0.0)
}

/// 結果の 1 件を描くのに要るもの。`renderer` を触る前に写し取る。
struct HitRow {
    top: f32,
    height: f32,
    selected: bool,
    /// 1 段目に出す名前。引けなかったセッションでは `CLOSED_SESSION`。
    name: String,
    is_path: bool,
    /// `⌘` の札。引けなかったセッションでは出さない。
    label: Option<String>,
    exit: Option<i32>,
    /// 走査のあいだに終わって消えたセッション。終了コードは分からない。
    closed: bool,
    working: bool,
    blocked: bool,
    kind: scan::HitKind,
    text: String,
    /// `text` の中の当たりのバイト位置。
    hit: std::ops::Range<usize>,
}

/// 結果の一覧の 1 件ずつを、描く前に写し取る。
///
/// `Hit.id` は走査中に終わったセッションを指しうる。位置ではなく番号で引き、
/// 引けなければ 1 段目だけを代わりの字にする（当たった文は残す）。
fn collect_hit_rows(state: &State, sl: &SidebarLayout) -> Vec<HitRow> {
    let Some(across) = &state.across else {
        return Vec::new();
    };
    let tree = state.manager.tree_rows();
    sl.blocks
        .iter()
        .filter_map(|b| {
            let hit = across.hits.get(b.index)?;
            let found = state
                .manager
                .sessions()
                .iter()
                .enumerate()
                .find(|(_, s)| s.id == hit.id);
            let (name, is_path, label, exit, closed, working, blocked) = match found {
                Some((i, s)) => {
                    let (name, is_path) = s.display_name();
                    let exit = match s.state {
                        RunState::Running => None,
                        RunState::Exited(code) => Some(code),
                    };
                    // 札は一覧に出ているのと同じ番号にする。
                    let label = tree
                        .iter()
                        .position(|r| r.index == i)
                        .and_then(input::index_label);
                    (name, is_path, label, exit, false, s.is_working(), s.blocked)
                }
                // 終わって消えたセッション。止まっている印で出す。
                None => (
                    CLOSED_SESSION.to_string(),
                    false,
                    None,
                    None,
                    true,
                    false,
                    false,
                ),
            };
            Some(HitRow {
                top: b.top,
                height: b.height,
                selected: b.index == across.selected,
                name,
                is_path,
                label,
                exit,
                closed,
                working,
                blocked,
                kind: hit.kind,
                text: hit.text.clone(),
                hit: hit.hit.clone(),
            })
        })
        .collect()
}

/// 結果の一覧を描く。
///
/// 1 段目は既存の行と同じ組み（状態の点、名前、`⌘` の札）。
/// 2 段目は当たった文で、当たりを中心に詰め、当たった語に色を敷く（仕様 §6）。
fn draw_across_hits(state: &mut State, sl: &SidebarLayout, theme: &Theme) {
    let sc = state.renderer.scale();
    let title_h = state.renderer.line_height_px(sidebar::SIZE_TITLE);
    let sub_h = state.renderer.line_height_px(sidebar::SIZE_SUB);
    let small_h = state.renderer.line_height_px(sidebar::SIZE_BRANCH);
    let text_x = sl.rect_x + sidebar::TEXT_INSET * sc;
    let right = sl.rect_x + sl.rect_w;
    let title_st = style(sidebar::SIZE_TITLE, false);
    let sub_st = style(sidebar::SIZE_SUB, false);
    let small_st = style(sidebar::SIZE_BRANCH, false);

    // 当たりが 1 件も無いときは、何も無い理由を出す。
    // 当たりは語より 1 走査ぶん遅れることがあるので（`queue_scan` は時刻を
    // 押すだけ）、走らせる前に「無い」と言わない。
    // 1 件も置けない高さのときも、割り付けは一言のぶんを空けている。
    // 空の帯を残さず、理由を出す。
    let note = state.across.as_ref().and_then(|a| {
        let n = scan_note(a.scanning, a.typed_at.is_some(), a.hits.len());
        across_note_text(sl.blocks.is_empty(), &n)
    });
    if let Some(note) = note {
        state
            .renderer
            .put_text_px(text_x, across_note_y(sc), note, sub_st, theme.fg_tertiary);
        return;
    }

    for row in collect_hit_rows(state, sl) {
        if row.selected {
            state.renderer.fill_px(
                sl.rect_x,
                row.top,
                sl.rect_w,
                row.height,
                theme.surface,
                sidebar::RADIUS * sc,
            );
        }
        let mut y = row.top + sidebar::PAD_TOP * sc;

        // ---- 1 段目：状態の点、名前、`⌘` の札
        let label_w = match &row.label {
            Some(l) => state.renderer.measure_px(l, small_st),
            None => 0.0,
        };
        let avail = (right - 12.0 * sc - label_w - text_x).max(0.0);
        let name = if row.is_path {
            state.renderer.fit_head(&row.name, title_st, avail)
        } else {
            state.renderer.fit_tail(&row.name, title_st, avail)
        };
        let ended = row.closed || row.exit.is_some();
        let fg = if !ended {
            theme.fg_primary
        } else {
            theme.fg_tertiary
        };
        state.renderer.put_text_px(text_x, y, &name, title_st, fg);
        state.renderer.put_text_px(
            sl.rect_x + sidebar::MARK_INSET * sc,
            y + (title_h - small_h) / 2.0,
            if ended { "○" } else { "●" },
            small_st,
            mark_color(theme, row.exit, row.working, row.blocked),
        );
        if let Some(label) = &row.label {
            state.renderer.put_text_px(
                right - 6.0 * sc - label_w,
                y + (title_h - small_h) / 2.0,
                label,
                small_st,
                theme.fg_tertiary,
            );
        }
        y += title_h;

        // ---- 2 段目：当たった文
        // 題名と枝に当たったときは、一覧と同じ印を同じ色で置く。
        // 置かないと、出力の行に当たったのと見分けが付かない。
        let (mark, mark_fg) = match row.kind {
            scan::HitKind::Title => ("✻ ", theme.accent),
            scan::HitKind::Branch => ("⋔ ", theme.fg_tertiary),
            scan::HitKind::Line | scan::HitKind::Name => ("", theme.fg_tertiary),
        };
        let mut x = text_x;
        x += state.renderer.put_text_px(x, y, mark, sub_st, mark_fg);
        let limit = (right - 8.0 * sc - x).max(0.0);
        // 当たりを中心にして詰める。末尾も先頭も落とす詰め方では、
        // 探した語そのものが落ちうる（仕様 §6）。
        let (shown, at) = state.renderer.fit_center(&row.text, sub_st, limit, row.hit);
        // `at` は `shown` のバイト位置。境を外した範囲で落ちないようにする。
        let head = shown.get(..at.start).unwrap_or("");
        let word = shown.get(at.start..at.end).unwrap_or("");
        let tail = shown.get(at.end..).unwrap_or("");
        x += state
            .renderer
            .put_text_px(x, y, head, sub_st, theme.fg_secondary);
        if !word.is_empty() {
            let word_w = state.renderer.measure_px(word, sub_st);
            let pad = sidebar::HIT_PAD * sc / 2.0;
            state.renderer.fill_px(
                x - pad,
                y,
                word_w + pad * 2.0,
                sub_h,
                theme.search_hit,
                2.0 * sc,
            );
        }
        x += state
            .renderer
            .put_text_px(x, y, word, sub_st, theme.search_fg);
        state
            .renderer
            .put_text_px(x, y, tail, sub_st, theme.fg_secondary);
    }
}

pub(crate) fn draw_sidebar(state: &mut State, layout: &Layout, theme: &Theme) {
    let sc = state.renderer.scale();
    // 割り付ける前に送り位置を決める。選択が動いていれば追いかける。
    settle_sidebar_offset(state, layout);
    let sl = sidebar_layout(state, layout);
    let height_px = layout.rows as f32 * state.renderer.cell().height;
    state
        .renderer
        .fill_px(0.0, 0.0, sl.width, height_px, theme.chrome_bg, 0.0);

    // 帯の左は検索窓。`+` より先に描く（仕様 §6）。
    draw_search_box(state, &sl, theme);

    // 押せる目印。キーが効かない環境でもここから増やせる。
    let plus_w = state
        .renderer
        .measure_px("+", style(sidebar::SIZE_TITLE, true));
    let plus_x = sl.rect_x + sl.rect_w - plus_w - 6.0 * sc;
    let plus_y = (sidebar::HEADER * sc - state.renderer.line_height_px(sidebar::SIZE_TITLE)) / 2.0;
    state.renderer.put_text_px(
        plus_x,
        plus_y,
        "+",
        style(sidebar::SIZE_TITLE, true),
        theme.accent,
    );

    if showing_hits(state.across.as_ref()) {
        draw_across_hits(state, &sl, theme);
        // 一覧を出していないあいだも「描いた状態」は合わせておく。
        // 残しておくと、背景のセッションが字を出すたびに
        // `wants_redraw` が描き直しを頼み続ける。
        for s in state.manager.sessions_mut() {
            let working = s.is_working();
            s.shown_working = working;
            s.shown_blocked = s.blocked;
        }
        draw_sidebar_recent(state, &sl, theme, height_px);
        return;
    }

    let selected = state.manager.selected_index();
    for (n, b) in sl.blocks.iter().enumerate() {
        let s = &state.manager.sessions()[b.index];
        let running = s.is_running();
        let indent = b.depth as f32 * sidebar::INDENT * sc;
        let text_x = sl.rect_x + sidebar::TEXT_INSET * sc + indent;
        let forked = !s.inherited && s.parent.is_some();
        let (raw_name, is_path) = s.display_name();
        let osc_title = s.window_title.clone();
        let branch = s.branch.clone();
        let exit = match s.state {
            RunState::Running => None,
            RunState::Exited(code) => Some(code),
        };
        let working = s.is_working();
        let blocked = s.blocked;

        let hovered =
            state.mouse.x < sl.width && state.mouse.y >= b.top && state.mouse.y < b.top + b.height;
        if b.index == selected {
            state.renderer.fill_px(
                sl.rect_x,
                b.top,
                sl.rect_w,
                b.height,
                theme.surface,
                sidebar::RADIUS * sc,
            );
        }

        let mut y = b.top + sidebar::PAD_TOP * sc;
        let title_h = state.renderer.line_height_px(sidebar::SIZE_TITLE);
        // 右端に並べる印の左端。名前はここまでで切る。
        let right = sl.rect_x + sl.rect_w;
        // 印は行ごとに幅が違う。実際に出すものではかる。
        // 札は木の行で振る。窓の中の位置で振ると、送ったとき
        // `⌘1` が別の行に付き、押すと窓の外の行へ飛ぶ。
        let hint = input::index_label(sl.offset + n);
        let hint_w = match &hint {
            Some(h) => state
                .renderer
                .measure_px(h, style(sidebar::SIZE_BRANCH, false)),
            None => 0.0,
        };
        let name_limit = right - 12.0 * sc - hint_w - text_x;
        let mark_w = state
            .renderer
            .measure_px("*", style(sidebar::SIZE_TITLE, false));
        let avail = (name_limit - if forked { mark_w } else { 0.0 }).max(0.0);
        let mut name = if is_path {
            state
                .renderer
                .fit_head(&raw_name, style(sidebar::SIZE_TITLE, false), avail)
        } else {
            state
                .renderer
                .fit_tail(&raw_name, style(sidebar::SIZE_TITLE, false), avail)
        };
        if forked {
            name.push('*');
        }

        // 分岐の系統は字下げで示す。つなぎの印は、状態の印に重ならない
        // 位置へ寄せる。
        if b.depth > 0 {
            state.renderer.put_text_px(
                sl.rect_x + indent - 5.0 * sc,
                y + (title_h - state.renderer.line_height_px(sidebar::SIZE_BRANCH)) / 2.0,
                "└",
                style(sidebar::SIZE_BRANCH, false),
                theme.fg_tertiary,
            );
        }
        let fg = if running {
            theme.fg_primary
        } else {
            theme.fg_tertiary
        };
        state
            .renderer
            .put_text_px(text_x, y, &name, style(sidebar::SIZE_TITLE, false), fg);

        // 実行状態は左端に置く。Warp がアバターを置いている位置にあたる。
        // 動いているあいだは明るく、待っているあいだは沈める。
        let mark = if exit.is_none() { "●" } else { "○" };
        let small_h = state.renderer.line_height_px(sidebar::SIZE_BRANCH);
        state.renderer.put_text_px(
            sl.rect_x + sidebar::MARK_INSET * sc + indent,
            y + (title_h - small_h) / 2.0,
            mark,
            style(sidebar::SIZE_BRANCH, false),
            mark_color(theme, exit, working, blocked),
        );
        // 右端は、ふだんは ⌘ の番号、カーソルが乗っているときは閉じる印。
        // 両方を常に置くと名前の幅が足りない。
        if hovered {
            let xw = state
                .renderer
                .measure_px("×", style(sidebar::SIZE_SUB, false));
            state.renderer.put_text_px(
                right - 6.0 * sc - xw,
                y + (title_h - state.renderer.line_height_px(sidebar::SIZE_SUB)) / 2.0,
                "×",
                style(sidebar::SIZE_SUB, false),
                theme.fg_secondary,
            );
        } else if let Some(hint) = &hint {
            state.renderer.put_text_px(
                right - 6.0 * sc - hint_w,
                y + (title_h - small_h) / 2.0,
                hint,
                style(sidebar::SIZE_BRANCH, false),
                theme.fg_tertiary,
            );
        }
        y += title_h;

        // 2 段目は端末上のプログラムが名乗った題名。
        if let Some(title) = osc_title {
            let w = state.renderer.put_text_px(
                text_x,
                y,
                "✻ ",
                style(sidebar::SIZE_SUB, false),
                theme.accent,
            );
            let limit = (right - 8.0 * sc - (text_x + w)).max(0.0);
            let clipped = state
                .renderer
                .fit_tail(&title, style(sidebar::SIZE_SUB, false), limit);
            state.renderer.put_text_px(
                text_x + w,
                y,
                &clipped,
                style(sidebar::SIZE_SUB, false),
                theme.fg_secondary,
            );
            y += state.renderer.line_height_px(sidebar::SIZE_SUB);
        }

        // 3 段目はブランチ名。
        if let Some(branch) = branch {
            let w = state.renderer.put_text_px(
                text_x,
                y,
                "⋔ ",
                style(sidebar::SIZE_BRANCH, false),
                theme.fg_tertiary,
            );
            let limit = (right - 8.0 * sc - (text_x + w)).max(0.0);
            let clipped =
                state
                    .renderer
                    .fit_tail(&branch, style(sidebar::SIZE_BRANCH, false), limit);
            state.renderer.put_text_px(
                text_x + w,
                y,
                &clipped,
                style(sidebar::SIZE_BRANCH, false),
                theme.fg_tertiary,
            );
        }
    }

    // 描いた「動いている」状態は、窓の外の行も含めて合わせる。
    // 外の行を残すと、見えていないセッションが字を出すたびに
    // `wants_redraw` が窓ぜんぶの描き直しを頼み続ける。
    for s in state.manager.sessions_mut() {
        let working = s.is_working();
        s.shown_working = working;
        s.shown_blocked = s.blocked;
    }

    // 掴んでいる行の落とし先。
    if let Some(drag) = &state.mouse.sidebar_drag {
        if let (true, Some(at)) = (drag.moved, drag.to) {
            let y = sidebar_boundary_y(&sl, at, sc);
            state.renderer.fill_px(
                sl.rect_x,
                y - (1.0 * sc).max(1.0),
                sl.rect_w,
                (2.0 * sc).max(2.0),
                theme.accent,
                0.0,
            );
        }
    }

    draw_sidebar_recent(state, &sl, theme, height_px);
}

/// 下段の直近のコマンド。何もなければ区切りごと出さない。
fn draw_sidebar_recent(state: &mut State, sl: &SidebarLayout, theme: &Theme, height_px: f32) {
    let sc = state.renderer.scale();
    if state.recent.is_empty() {
        return;
    }
    let line_h = state.renderer.line_height_px(sidebar::SIZE_RECENT);
    if sl.sep_y + line_h > height_px {
        return;
    }
    state.renderer.fill_px(
        sl.rect_x,
        sl.sep_y,
        sl.rect_w,
        (1.0 * sc).max(1.0),
        theme.fg_tertiary,
        0.0,
    );
    let mut y = sl.sep_y + sidebar::SEP_PAD * sc;
    // 別の欄なので、描きながら読める。毎フレーム写し取る必要はない。
    for i in 0..state.recent.len() {
        let entry = &state.recent[i];
        if y + line_h > height_px {
            break;
        }
        let code = entry.exit_code.unwrap_or(0);
        let color = if code == 0 {
            theme.fg_tertiary
        } else {
            theme.warn
        };
        let w = state.renderer.put_text_px(
            sl.rect_x + 4.0 * sc,
            y,
            &format!("{code:>3} "),
            style(sidebar::SIZE_RECENT, false),
            color,
        );
        let limit = (sl.rect_w - 8.0 * sc - w).max(0.0);
        let clipped =
            state
                .renderer
                .fit_tail(&entry.command, style(sidebar::SIZE_RECENT, false), limit);
        state.renderer.put_text_px(
            sl.rect_x + 4.0 * sc + w,
            y,
            &clipped,
            style(sidebar::SIZE_RECENT, false),
            theme.fg_secondary,
        );
        y += line_h;
    }
}

pub(crate) fn draw_terminal(state: &mut State, layout: &Layout, theme: &Theme) {
    let Some(session) = state.manager.selected() else {
        return;
    };
    let term = session.term.lock();
    let content = term.renderable_content();
    let colors = content.colors;
    let selection = content.selection;
    let cursor = content.cursor;
    let display_offset = content.display_offset;
    let mut cursor_cols = 1usize;
    let default_bg = theme.bg;
    // 探していないときは、1 コマごとに二つの集合を引く必要がない。
    // 空でも鍵を混ぜる費用はかかる。画面の広さぶん、まるごと無駄になる。
    let searching = !state.find_cells.is_empty() || !state.find_current.is_empty();
    // 出した行を数える。表示の食い違いを突き合わせるための記録に使う。
    let mut drawn_rows = std::collections::HashSet::new();
    // 指しているリンクは、押せることが分かるよう下線を引く。
    let hovered = state.hovered_link.clone();
    // 伏せるコマを先に出しておく。1 コマずつ式に当てることはできない。
    // 認証情報は「名前と値」のように行として現れるので、行ごとに見る。
    let masked = masked_cells(
        &term,
        display_offset,
        layout.term_rows,
        layout.term_cols,
        &state.redactor,
    );

    for indexed in content.display_iter {
        let cell = indexed.cell;
        if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
            continue;
        }
        // 行番号は履歴を含む座標で来る。遡っているあいだは負になるので、
        // 遡った分を足して画面の行に直す。足さずに負を捨てると、
        // 上から遡った分だけ行が抜け、画面の下がそのぶん空く。
        let line = indexed.point.line.0;
        let Some(row) = screen_row(line, display_offset, layout.term_rows) else {
            continue;
        };
        let col = indexed.point.column.0;
        if col >= layout.term_cols {
            continue;
        }
        let mut fg = theme.resolve(cell.fg, colors);
        let mut bg = theme.resolve(cell.bg, colors);
        if cell.flags.contains(Flags::INVERSE) {
            std::mem::swap(&mut fg, &mut bg);
        }
        if cell.flags.contains(Flags::HIDDEN) {
            fg = bg;
        }
        if selection.is_some_and(|s| s.contains(indexed.point)) {
            bg = theme.selection;
        }
        // 検索の強調は選択より優先する。探しているものを見失わないため。
        // 印の位置は履歴を含む座標で覚えてあるので、直す前の行番号で引く。
        if searching {
            let key = (line, col);
            if state.find_current.contains(&key) {
                bg = theme.search_current;
                fg = theme.search_fg;
            } else if state.find_cells.contains(&key) {
                bg = theme.search_hit;
                fg = theme.search_fg;
            }
        }
        let wide = cell.flags.contains(Flags::WIDE_CHAR);
        if wide && indexed.point == cursor.point {
            cursor_cols = 2;
        }
        let mut underline = cell.flags.intersects(Flags::ALL_UNDERLINES);
        if let Some(link) = &hovered {
            let hit = match (&link.id, &link.span) {
                (Some(id), _) => cell.hyperlink().is_some_and(|h| h.id() == *id),
                (None, Some(span)) => span.contains(&indexed.point),
                _ => false,
            };
            if hit {
                underline = true;
            }
        }
        // 既定の地色のままの空白は、何も出すものがない。
        if cell.c == ' ' && bg == default_bg && !underline {
            continue;
        }
        drawn_rows.insert(row);
        // 伏せるコマは、同じ幅の丸に置き換える。桁がずれると全画面 UI が崩れる。
        // 置き換えるのは描くものだけで、グリッドの中身は本物のままである。
        let c = if masked.contains(&(line, col)) {
            MASK_CHAR
        } else {
            cell.c
        };
        let x = layout.term_col + col;
        let span = if wide { 2 } else { 1 };
        let span = span.min(layout.term_cols.saturating_sub(col)).max(1);
        if bg != default_bg {
            state.renderer.fill_cells(x, row, span, 1, bg);
        }
        state.renderer.put_char(
            x,
            row,
            c,
            fg,
            cell.flags.contains(Flags::BOLD),
            cell.flags.contains(Flags::ITALIC),
        );
        if underline {
            state.renderer.underline_cells(x, row, span, fg);
        }
    }
    // カーソルも同じ道で直す。遡っていても、見える範囲にいれば出す。
    let cursor_row = screen_row(cursor.point.line.0, display_offset, layout.term_rows);
    let show_cursor = cursor_row.is_some() && cursor.shape != CursorShape::Hidden;
    // 何が出ているかを数えて、詰まったときに突き合わせられるようにする。
    log_grid(&term, display_offset, layout, drawn_rows);
    let cursor_col = cursor.point.column.0;
    let cursor_shape = cursor.shape;
    drop(term);

    if show_cursor {
        if let Some(y) = cursor_row {
            let x = layout.term_col + cursor_col.min(layout.term_cols - 1);
            let cell = state.renderer.cell();
            // 検索窓に入っているあいだ、変換中の文字列と候補窓は窓の側が持つ。
            // `draw_sidebar` は `draw_terminal` より先に走るので、ここで
            // 書き直すと候補窓が端末のカーソルへ戻ってしまう。
            let owns_ime = state.across.is_none();
            if owns_ime {
                state.cursor_px = Some((x as f32 * cell.width, y as f32 * cell.height));
            }
            // 変換中の文字列は、確定するまで子プロセスへ渡さない。
            // 見えないと何を打っているか分からないので、カーソルの位置に出す。
            let preedit = (owns_ime && state.find.is_none() && state.search.is_none())
                .then(|| state.preedit.clone())
                .filter(|p| !p.is_empty());
            if let Some(text) = preedit {
                // 変換中はカーソルの塗りを出さない。
                // 塗りが一文字目に重なると、何を打っているか読めなくなる。
                let width = layout.term_cols.saturating_sub(x - layout.term_col);
                let cols: usize = text.chars().map(char_cols).sum();
                let shown = cols.min(width);
                state.renderer.fill_cells(x, y, shown, 1, theme.surface);
                let used = state
                    .renderer
                    .put_str_clipped(x, y, &text, width, theme.fg_primary);
                state.renderer.underline_cells(x, y, used, theme.accent);
                // 文字が入る位置は変換中の文字列の末尾なので、細い棒で示す。
                let caret = (x + used).min(layout.cols.saturating_sub(1));
                state.renderer.cursor_beam(caret, y, theme.cursor);
                state.cursor_px = Some((caret as f32 * cell.width, y as f32 * cell.height));
            } else {
                match cursor_shape {
                    CursorShape::Beam => state.renderer.cursor_beam(x, y, theme.cursor),
                    CursorShape::Underline => {
                        state
                            .renderer
                            .underline_cells(x, y, cursor_cols, theme.cursor)
                    }
                    _ => state
                        .renderer
                        .fill_cells_alpha(x, y, cursor_cols, 1, theme.cursor, 0.55),
                }
            }
        }
    }
}

/// 最下段の帯。端末の文字より小さく、別の書体で組む。
///
/// 端末の中身ではなく端末についての表示なので、桁に揃える必要がない。
/// 同じ大きさで並べると、本文より目立ってしまう。
const BAR_SIZE: f32 = 11.0;

pub(crate) fn draw_bottom(state: &mut State, layout: &Layout, theme: &Theme) {
    let sc = state.renderer.scale();
    let cell = state.renderer.cell();
    let bar_h = cell.height;
    let bar_y = (layout.rows.saturating_sub(1)) as f32 * bar_h;
    let x0 = layout.term_col as f32 * cell.width;
    let w = (layout.cols.saturating_sub(layout.term_col)) as f32 * cell.width;
    let st = style(BAR_SIZE, false);
    let line_h = state.renderer.line_height_px(BAR_SIZE);
    let ty = bar_y + (bar_h - line_h) / 2.0;
    let pad = 8.0 * sc;

    // 重ねた一覧は、帯の上へ同じ高さの行で積む。
    let overlay = |state: &mut State, rows: usize| -> f32 {
        let top = bar_y - rows as f32 * line_h;
        state.renderer.fill_px(
            x0,
            top,
            w,
            rows as f32 * line_h + bar_h,
            theme.chrome_bg,
            0.0,
        );
        top
    };

    if let Some(rename) = &state.rename {
        let input = rename.input.clone();
        state
            .renderer
            .fill_px(x0, bar_y, w, bar_h, theme.surface, 0.0);
        let mut x = x0 + pad;
        x += state
            .renderer
            .put_text_px(x, ty, "name: ", st, theme.fg_secondary);
        x += state
            .renderer
            .put_text_px(x, ty, &input, st, theme.fg_primary);
        x += draw_preedit_px(state, x, ty, st, theme, None);
        state
            .renderer
            .fill_px(x, ty, 2.0 * sc, line_h, theme.cursor, 0.0);
        state.cursor_px = Some((x, ty));
        let note = "Enter to set   empty resets to path   Esc to cancel";
        let nw = state.renderer.measure_px(note, st);
        if x0 + w - pad - nw > x + pad {
            state
                .renderer
                .put_text_px(x0 + w - pad - nw, ty, note, st, theme.fg_tertiary);
        }
        return;
    }

    if let Some(find) = &state.find {
        let (query, regex_mode, invalid, has_match) = (
            find.query.clone(),
            find.regex_mode,
            find.invalid,
            find.current.is_some(),
        );
        state
            .renderer
            .fill_px(x0, bar_y, w, bar_h, theme.surface, 0.0);
        let label = if regex_mode {
            "find (regex): "
        } else {
            "find: "
        };
        let mut x = x0 + pad;
        x += state
            .renderer
            .put_text_px(x, ty, label, st, theme.fg_secondary);
        let fg = if invalid {
            theme.warn
        } else {
            theme.fg_primary
        };
        x += state.renderer.put_text_px(x, ty, &query, st, fg);
        x += draw_preedit_px(state, x, ty, st, theme, None);
        state
            .renderer
            .fill_px(x, ty, 2.0 * sc, line_h, theme.cursor, 0.0);
        state.cursor_px = Some((x, ty));
        let note = if invalid {
            "invalid regex"
        } else if !query.is_empty() && !has_match {
            "no match"
        } else {
            "Enter prev   ⇧Enter next   Tab regex   Esc close"
        };
        let nw = state.renderer.measure_px(note, st);
        if x0 + w - pad - nw > x + pad {
            state
                .renderer
                .put_text_px(x0 + w - pad - nw, ty, note, st, theme.fg_tertiary);
        }
        return;
    }

    if let Some(picker) = &state.picker {
        let names = picker.names.clone();
        let sel = picker.selected;
        let top = overlay(state, names.len());
        for (i, name) in names.iter().enumerate() {
            let ry = top + i as f32 * line_h;
            if i == sel {
                state
                    .renderer
                    .fill_px(x0, ry, w, line_h, theme.surface, 0.0);
            }
            let fg = if i == sel {
                theme.fg_primary
            } else {
                theme.fg_secondary
            };
            state
                .renderer
                .put_text_px(x0 + pad, ry, &format!("{}  {}", i + 1, name), st, fg);
        }
        state
            .renderer
            .fill_px(x0, bar_y, w, bar_h, theme.surface, 0.0);
        state.renderer.put_text_px(
            x0 + pad,
            ty,
            "fork profile: j/k or number, Enter to confirm, Esc to cancel",
            st,
            theme.fg_primary,
        );
        return;
    }

    if let Some(search) = &state.search {
        // 見えるのは一部で、選択を動かすと窓が付いてくる。
        let results: Vec<crate::history::Entry> = search
            .results
            .iter()
            .skip(search.offset)
            .take(SEARCH_ROWS)
            .cloned()
            .collect();
        let sel = search.selected.saturating_sub(search.offset);
        let scope = search.scope.label().to_string();
        let query = search.query.clone();
        let position = if search.results.is_empty() {
            String::new()
        } else {
            format!("{}/{}", search.selected + 1, search.results.len())
        };
        let top = overlay(state, results.len());
        for (i, entry) in results.iter().enumerate() {
            let ry = top + i as f32 * line_h;
            if i == sel {
                state
                    .renderer
                    .fill_px(x0, ry, w, line_h, theme.surface, 0.0);
            }
            let fg = if i == sel {
                theme.fg_primary
            } else {
                theme.fg_secondary
            };
            let text = state.renderer.fit_tail(&entry.command, st, w - pad * 3.0);
            state
                .renderer
                .put_text_px(x0 + pad * 2.0, ry, &text, st, fg);
        }
        state
            .renderer
            .fill_px(x0, bar_y, w, bar_h, theme.surface, 0.0);
        let mut x = x0 + pad;
        x += state.renderer.put_text_px(
            x,
            ty,
            &format!("history [{scope}]: "),
            st,
            theme.fg_secondary,
        );
        x += state
            .renderer
            .put_text_px(x, ty, &query, st, theme.fg_primary);
        x += draw_preedit_px(state, x, ty, st, theme, None);
        state
            .renderer
            .fill_px(x, ty, 2.0 * sc, line_h, theme.cursor, 0.0);
        state.cursor_px = Some((x, ty));
        // 何件目を見ているかと、範囲の切り替え方を右端に出す。
        let note = if position.is_empty() {
            "^R scope   Esc close".to_string()
        } else {
            format!("{position}   ↑↓ move   ⇞⇟ page   ^R scope   Esc close")
        };
        let nw = state.renderer.measure_px(&note, st);
        if x0 + w - pad - nw > x + pad {
            state
                .renderer
                .put_text_px(x0 + w - pad - nw, ty, &note, st, theme.fg_tertiary);
        }
        return;
    }

    if let Some(msg) = &state.status {
        let msg = msg.clone();
        state
            .renderer
            .fill_px(x0, bar_y, w, bar_h, theme.chrome_bg, 0.0);
        let text = state.renderer.fit_tail(&msg, st, w - pad * 2.0);
        state
            .renderer
            .put_text_px(x0 + pad, ty, &text, st, theme.warn);
        return;
    }

    // ふだんは操作のヒントだけを薄く出す。
    let hint = "^O new   ^\\ fork   ^] fork as…   ^^ next   ⌘F find   ⌘I rename   ^R history   ^B pane   ⌘K clear   ⌘W close";
    let text = state.renderer.fit_tail(hint, st, w - pad * 2.0);
    state
        .renderer
        .put_text_px(x0 + pad, ty, &text, st, theme.fg_tertiary);
}

/// 変換中の文字列を帯の中に出す。返す値は描いた幅。
///
/// `bound` は窓の中で使うときの、残りの幅と下敷きの色。幅を越える分は切る。
fn draw_preedit_px(
    state: &mut State,
    x: f32,
    y: f32,
    st: TextStyle,
    theme: &Theme,
    bound: Option<(f32, Rgb)>,
) -> f32 {
    if state.preedit.is_empty() {
        return 0.0;
    }
    let (text, under) = match bound {
        // 窓の中では、いま打っている末尾を残す。他の呼び手は切らない。
        Some((room, color)) => (state.renderer.fit_head(&state.preedit, st, room), color),
        None => (state.preedit.clone(), theme.chrome_bg),
    };
    let w = state.renderer.measure_px(&text, st);
    let h = state.renderer.line_height_px(st.size);
    state.renderer.fill_px(x, y, w, h, under, 0.0);
    state
        .renderer
        .put_text_px(x, y, &text, st, theme.fg_primary);
    // 未確定であることを下線で示す。
    let t = (state.renderer.scale()).max(1.0);
    state
        .renderer
        .fill_px(x, y + h - t, w, t, theme.accent, 0.0);
    w
}

/// 括弧付き貼り付けに対応している端末には印を付けて送る。
/// 落とされたファイルの場所を、そのまま打ったのと同じ形にする。
///
/// 空白や引用符を含む場所は、そのまま渡すとシェルが分けてしまう。
/// 素直な字だけで出来ているときは、囲まずにそのまま返す。
/// 囲むときは単引用符を使い、中の単引用符だけ `'\''` で継ぐ。
fn quote_path(path: &Path) -> String {
    let text = path.to_string_lossy();
    if text.is_empty() {
        return "''".to_string();
    }
    let plain = text
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "._-/+=:@,%".contains(c));
    if plain {
        return text.into_owned();
    }
    let mut out = String::with_capacity(text.len() + 2);
    out.push('\'');
    for c in text.chars() {
        if c == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
}

fn bracketed(text: &str, mode: TermMode) -> Vec<u8> {
    let cleaned: String = text.replace("\r\n", "\r").replace('\n', "\r");
    if mode.contains(TermMode::BRACKETED_PASTE) {
        let mut out = b"\x1b[200~".to_vec();
        out.extend_from_slice(cleaned.as_bytes());
        out.extend_from_slice(b"\x1b[201~");
        out
    } else {
        cleaned.into_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::osc::OscEvent;
    use crate::term::UiEvent;

    fn now() -> std::time::Instant {
        std::time::Instant::now()
    }

    /// 画面の上で伏せるコマを、行から正しく割り出せること。
    ///
    /// バイトの位置を桁へ戻すところが要である。全角が混ざると両者はずれる。
    mod 伏せる位置 {
        use super::*;
        use alacritty_terminal::grid::Dimensions;
        use alacritty_terminal::index::{Column, Line as GLine};
        use alacritty_terminal::vte::ansi::Processor;
        use std::sync::mpsc::channel;

        fn term_of(script: &[u8]) -> alacritty_terminal::Term<crate::term::EventProxy> {
            let (tx, _rx) = channel();
            let (ptx, _prx) = channel();
            let ws = std::sync::Arc::new(alacritty_terminal::sync::FairMutex::new(
                alacritty_terminal::event::WindowSize {
                    num_lines: 6,
                    num_cols: 80,
                    cell_width: 8,
                    cell_height: 16,
                },
            ));
            let proxy =
                crate::term::EventProxy::new(1, ptx, crate::term::UiSender::Channel(tx), ws);
            let mut term = crate::term::new_term(crate::term::TermSize::new(80, 6), 50, proxy);
            let mut parser: Processor = Processor::new();
            parser.advance(&mut term, script);
            term
        }

        /// 伏せた結果を、目で読める 1 行にする。
        fn shown(script: &[u8]) -> String {
            let term = term_of(script);
            let r = crate::secret::Redactor::new(&crate::config::ScreenConfig::default().redact)
                .expect("既定の式は組み立てられる");
            let masked = masked_cells(&term, 0, 6, 80, &r);
            let grid = term.grid();
            let mut out = String::new();
            for col in 0..grid.columns() {
                let cell = &grid[GLine(0)][Column(col)];
                // 全角の 2 桁目は詰め物である。読める形にするため飛ばす。
                if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                    continue;
                }
                out.push(if masked.contains(&(0, col)) {
                    MASK_CHAR
                } else {
                    cell.c
                });
            }
            out.trim_end().to_string()
        }

        #[test]
        #[ignore]
        fn 計測_伏せる位置の費用() {
            // 現実に近い画面：47 行 x 163 桁、ところどころに認証情報。
            let mut script = Vec::new();
            for i in 0..47 {
                if i % 8 == 3 {
                    script.extend_from_slice(
                        b"export AWS_SECRET_ACCESS_KEY=wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
                    );
                } else {
                    script.extend_from_slice(
                        b"    let mut out = String::with_capacity(text.len()); // fill the row with code",
                    );
                }
                script.extend_from_slice(b"\r\n");
            }
            let (tx, _rx) = channel();
            let (ptx, _prx) = channel();
            let ws = std::sync::Arc::new(alacritty_terminal::sync::FairMutex::new(
                alacritty_terminal::event::WindowSize {
                    num_lines: 47,
                    num_cols: 163,
                    cell_width: 8,
                    cell_height: 17,
                },
            ));
            let proxy =
                crate::term::EventProxy::new(1, ptx, crate::term::UiSender::Channel(tx), ws);
            let mut term = crate::term::new_term(crate::term::TermSize::new(163, 47), 100, proxy);
            let mut parser: Processor = Processor::new();
            parser.advance(&mut term, &script);
            let r = crate::secret::Redactor::new(&crate::config::ScreenConfig::default().redact)
                .unwrap();

            for _ in 0..20 {
                let _ = masked_cells(&term, 0, 47, 163, &r);
            }
            let mut ts = Vec::new();
            for _ in 0..100 {
                let t = std::time::Instant::now();
                let m = masked_cells(&term, 0, 47, 163, &r);
                ts.push(t.elapsed().as_secs_f64() * 1000.0);
                assert!(!m.is_empty());
            }
            ts.sort_by(|a, b| a.partial_cmp(b).unwrap());
            println!(
                "47 行 x 163 桁: 中央 {:.3}ms  p90 {:.3}ms  最大 {:.3}ms",
                ts[50], ts[90], ts[99]
            );
        }

        #[test]
        fn 値だけを伏せ_名前は残す() {
            let line =
                shown(b"export AWS_SECRET_ACCESS_KEY=wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY");
            assert!(line.starts_with("export AWS_SECRET_ACCESS_KEY="), "{line}");
            assert!(!line.contains("wJalr"), "{line}");
            assert_eq!(
                line.matches(MASK_CHAR).count(),
                40,
                "値の桁数だけ伏せる: {line}"
            );
        }

        /// 全角が前にあると、バイトの位置と桁がずれる。
        /// ずれたまま使うと、値ではなく別の場所を消してしまう。
        #[test]
        fn 全角があっても位置がずれない() {
            let line = shown("鍵は AKIAIOSFODNN7EXAMPLE です".as_bytes());
            assert!(line.starts_with("鍵は "), "{line}");
            assert!(line.ends_with(" です"), "{line}");
            assert!(!line.contains("AKIA"), "{line}");
            assert_eq!(line.matches(MASK_CHAR).count(), 20, "{line}");
        }

        #[test]
        fn 秘密でない行は触らない() {
            assert_eq!(
                shown(b"export AWS_REGION=us-east-1"),
                "export AWS_REGION=us-east-1"
            );
        }

        #[test]
        fn 式が無ければ何も伏せない() {
            let term = term_of(b"AKIAIOSFODNN7EXAMPLE");
            let off = crate::secret::Redactor::default();
            assert!(masked_cells(&term, 0, 6, 80, &off).is_empty());
        }
    }

    #[test]
    fn 遡っていなければ行番号はそのまま() {
        assert_eq!(screen_row(0, 0, 24), Some(0));
        assert_eq!(screen_row(23, 0, 24), Some(23));
        assert_eq!(screen_row(24, 0, 24), None);
        assert_eq!(screen_row(-1, 0, 24), None);
    }

    #[test]
    fn 遡っているあいだの負の行番号を画面へ直す() {
        // 5 行遡ると、いちばん上は -5 行目として来る。
        // 直さずに捨てると、上から 5 行ぶん抜けて画面の下が空く。
        assert_eq!(screen_row(-5, 5, 24), Some(0));
        assert_eq!(screen_row(-1, 5, 24), Some(4));
        assert_eq!(screen_row(0, 5, 24), Some(5));
        assert_eq!(screen_row(18, 5, 24), Some(23));
        assert_eq!(screen_row(19, 5, 24), None, "画面の外");
        assert_eq!(screen_row(-6, 5, 24), None, "画面の外");
    }

    #[test]
    fn どこまで遡っても画面ぶんの行が埋まる() {
        // 遡った量にかかわらず、画面の行は 0..行数 で埋まる。
        for offset in [0usize, 1, 5, 23, 24, 100, 9999] {
            let rows = 24;
            let got: Vec<usize> = (0..rows)
                .map(|i| -(offset as i32) + i as i32)
                .filter_map(|line| screen_row(line, offset, rows))
                .collect();
            assert_eq!(got, (0..rows).collect::<Vec<_>>(), "{offset} 行遡ったとき");
        }
    }

    #[test]
    fn 選んでいるものがあればそれを写す() {
        assert_eq!(
            copy_text(Some("live".into()), Some(&(3, "old".into())), Some(3)).as_deref(),
            Some("live")
        );
    }

    #[test]
    fn 選択が消えていれば控えを写す() {
        // 全画面 UI が描き直すと Term は選択を捨てる。
        // 控えが無いと、選べているのに何も写らない。
        assert_eq!(
            copy_text(None, Some(&(3, "picked".into())), Some(3)).as_deref(),
            Some("picked")
        );
        assert_eq!(
            copy_text(Some(String::new()), Some(&(3, "picked".into())), Some(3)).as_deref(),
            Some("picked")
        );
    }

    #[test]
    fn 控えは持ち主のセッションでだけ使う() {
        // 別のセッションへ移ってから ⌘C を押しても、前の画面の文字は出さない。
        assert_eq!(copy_text(None, Some(&(3, "picked".into())), Some(4)), None);
        assert_eq!(copy_text(None, Some(&(3, "picked".into())), None), None);
    }

    #[test]
    fn 何も選んでいなければ写さない() {
        assert_eq!(copy_text(None, None, Some(3)), None);
        assert_eq!(copy_text(Some(String::new()), None, Some(3)), None);
        assert_eq!(copy_text(None, Some(&(3, String::new())), Some(3)), None);
    }

    #[test]
    fn 背景のセッションが字を出し続けても描き直さない() {
        // 見えているのは選んでいるセッションの画面だけである。
        // ここで描き直すと、エージェントを何本も走らせるほど重くなる。
        assert!(!wants_redraw(&UiEvent::Wakeup(7, now()), Some(3), true));
        assert!(wants_redraw(&UiEvent::Wakeup(3, now()), Some(3), true));
    }

    #[test]
    fn 背景が動き始めたときは印のために一度描き直す() {
        // まだ「動いている」と描いていないなら、印を点けるために描き直す。
        // 点けたあとは、出力が続いても描き直さない（上のテスト）。
        assert!(wants_redraw(&UiEvent::Wakeup(7, now()), Some(3), false));
    }

    #[test]
    fn 左ペインに出るものはどのセッションでも描き直す() {
        assert!(wants_redraw(&UiEvent::Title(7, "x".into()), Some(3), false));
        assert!(wants_redraw(&UiEvent::ChildExit(7, 0), Some(3), false));
        // 作業ディレクトリは 1 段目に出る。
        assert!(wants_redraw(
            &UiEvent::Osc(7, OscEvent::Cwd("/tmp".into())),
            Some(3),
            false
        ));
    }

    #[test]
    fn 画面の中でしか効かない_osc_は背景では描き直さない() {
        for ev in [
            OscEvent::PromptStart,
            OscEvent::CommandStart,
            OscEvent::CommandExecuted,
            OscEvent::CommandFinished(Some(0)),
            OscEvent::AgentId("a".into()),
        ] {
            assert!(
                !wants_redraw(&UiEvent::Osc(7, ev.clone()), Some(3), false),
                "{ev:?} は背景では見えない"
            );
            assert!(
                wants_redraw(&UiEvent::Osc(3, ev.clone()), Some(3), false),
                "{ev:?} は選んでいれば見える"
            );
        }
    }

    #[test]
    fn 直近のコマンドは選んでいるぶんだけ描き直す() {
        let rec = |id| crate::session::CommandRecord {
            session_id: id,
            session_key: String::new(),
            agent_id: None,
            cwd: "/tmp".into(),
            command: "ls".into(),
            exit_code: Some(0),
            started_at: std::time::SystemTime::now(),
            duration_ms: None,
        };
        assert!(!wants_redraw(&UiEvent::Command(7, rec(7)), Some(3), false));
        assert!(wants_redraw(&UiEvent::Command(3, rec(3)), Some(3), false));
    }

    #[test]
    fn 見た目の変わらない通知では描き直さない() {
        assert!(!wants_redraw(
            &UiEvent::ClipboardStore(3, "x".into()),
            Some(3),
            false
        ));
        assert!(!wants_redraw(
            &UiEvent::ClipboardLoad(3, std::sync::Arc::new(|s: &str| s.to_string())),
            Some(3),
            false
        ));
        // 走査の結果は、受け取れたかどうかで変わる。ここでは決めない。
        assert!(!wants_redraw(
            &UiEvent::Scanned(1, Vec::new()),
            Some(3),
            false
        ));
    }

    fn px(y: f64) -> MouseScrollDelta {
        MouseScrollDelta::PixelDelta(winit::dpi::PhysicalPosition::new(0.0, y))
    }

    #[test]
    fn 触覚板の細かい動きを取りこぼさない() {
        // 1 回 3 画素、行は 17 画素。切り捨てると永久に 0 行のままになる。
        let mut w = WheelAccum::default();
        let mut moved = 0;
        for _ in 0..12 {
            moved += w.push(px(3.0), 17.0);
        }
        assert_eq!(moved, 2, "36 画素ぶんは 2 行になる");
    }

    #[test]
    fn 行に足りない一回では動かない() {
        let mut w = WheelAccum::default();
        assert_eq!(w.push(px(3.0), 17.0), 0);
    }

    #[test]
    fn 端数は次の通知へ持ち越す() {
        let mut w = WheelAccum::default();
        assert_eq!(w.push(px(10.0), 17.0), 0);
        assert_eq!(w.push(px(10.0), 17.0), 1, "20 画素で 1 行");
        assert_eq!(w.push(px(10.0), 17.0), 0);
        assert_eq!(w.push(px(10.0), 17.0), 1, "残りの 3 画素が効いている");
    }

    #[test]
    fn 向きを変えたら持ち越しを捨てる() {
        let mut w = WheelAccum::default();
        assert_eq!(w.push(px(10.0), 17.0), 0, "上へ 10 画素、まだ行にならない");
        // 逆向きの端数が残っていると、折り返した最初のひと押しが食われる。
        assert_eq!(w.push(px(-17.0), 17.0), -1, "下へ 1 行");
    }

    #[test]
    fn 行単位の通知はそのまま通る() {
        let mut w = WheelAccum::default();
        assert_eq!(w.push(MouseScrollDelta::LineDelta(0.0, 3.0), 17.0), 3);
        assert_eq!(w.push(MouseScrollDelta::LineDelta(0.0, -1.0), 17.0), -1);
    }

    #[test]
    fn まとめて来た大きな動きも行数に直す() {
        let mut w = WheelAccum::default();
        assert_eq!(w.push(px(170.0), 17.0), 10);
    }

    #[test]
    fn 素直な場所はそのまま渡す() {
        assert_eq!(
            quote_path(Path::new("/Users/tkc/a.txt")),
            "/Users/tkc/a.txt"
        );
        assert_eq!(quote_path(Path::new("src/main.rs")), "src/main.rs");
        assert_eq!(quote_path(Path::new("a-b_c.2.tar.gz")), "a-b_c.2.tar.gz");
    }

    #[test]
    fn 空白を含む場所は囲む() {
        // 囲まずに渡すと、シェルが二つの引数に分けてしまう。
        assert_eq!(
            quote_path(Path::new("/Users/tkc/My Documents/a.txt")),
            "'/Users/tkc/My Documents/a.txt'"
        );
    }

    #[test]
    fn 単引用符は継いで囲む() {
        // 単引用符の中では単引用符を書けない。いったん閉じて継ぐ。
        assert_eq!(
            quote_path(Path::new("/tmp/it's here")),
            "'/tmp/it'\\''s here'"
        );
    }

    #[test]
    fn シェルに読まれる字は囲む() {
        for raw in [
            "/tmp/a;rm -rf b",
            "/tmp/$(whoami)",
            "/tmp/a`id`b",
            "/tmp/a&b",
            "/tmp/a|b",
            "/tmp/a>b",
            "/tmp/a*b",
            "/tmp/a\\b",
            "/tmp/a\"b",
        ] {
            let got = quote_path(Path::new(raw));
            assert!(got.starts_with('\'') && got.ends_with('\''), "{raw} は囲む");
        }
    }

    #[test]
    fn 日本語の場所も囲んで渡す() {
        assert_eq!(quote_path(Path::new("/tmp/資料.txt")), "'/tmp/資料.txt'");
    }

    #[test]
    fn 空の場所は空の引数にする() {
        assert_eq!(quote_path(Path::new("")), "''");
    }

    #[test]
    fn 貼り付けは改行を復帰に揃える() {
        let got = bracketed("a\r\nb\nc", TermMode::empty());
        assert_eq!(got, b"a\rb\rc");
    }

    #[test]
    fn 括弧付き貼り付けに印を付ける() {
        let got = bracketed("x", TermMode::BRACKETED_PASTE);
        assert_eq!(got, b"\x1b[200~x\x1b[201~");
    }

    /// 走査は別スレッドで走るので、結果は打った順に帰ってくるとは限らない。
    /// 遅れて来た古い結果で、新しい結果を上書きしないこと。
    mod 横断検索の受け口 {
        use super::*;

        fn 当たり(text: &str) -> crate::scan::Hit {
            crate::scan::Hit {
                id: 1,
                kind: crate::scan::HitKind::Line,
                serial: Some(0),
                at: None,
                text: text.into(),
                hit: 0..text.len(),
            }
        }

        #[test]
        fn 古い世代の結果は捨てる() {
            // 世代 2 を待っているあいだに、1 本前の走査が結果を持って帰ってくる。
            let mut across = CrossSearch {
                query: "ses".into(),
                generation: 2,
                ..CrossSearch::default()
            };
            assert!(!across.accept(1, vec![当たり("古い")]), "古い世代を受けた");
            assert!(across.hits.is_empty(), "古い世代の結果で入れ替わった");

            // 待っていた世代なら受ける。選んでいた位置は先頭へ戻す。
            across.selected = 4;
            across.offset = 2;
            assert!(across.accept(2, vec![当たり("新しい")]));
            assert_eq!(across.hits.len(), 1);
            assert_eq!(across.hits[0].text, "新しい");
            assert_eq!((across.selected, across.offset), (0, 0));

            // 受け取ったあとで届いた古い世代も捨てる。
            assert!(!across.accept(1, vec![当たり("もっと古い")]));
            assert_eq!(across.hits[0].text, "新しい", "古い結果に戻された");
        }

        /// 語を消し切ったら走査は始まらない。出ている当たりを捨てるのは
        /// このときだけで、捨てなければ、消した語の当たりが一覧に残り続ける。
        #[test]
        fn 語を消したら当たりも捨てる() {
            let mut across = CrossSearch {
                hits: vec![当たり("古い")],
                selected: 3,
                offset: 2,
                ..CrossSearch::default()
            };
            assert!(across.clear_hits(), "捨てたことを知らせない");
            assert!(across.hits.is_empty());
            assert_eq!((across.selected, across.offset), (0, 0));
            // 空のまま呼ばれても、描き直しは頼まない。
            assert!(!across.clear_hits());
        }

        /// 打鍵のたびには走らせない（仕様 §10）。
        #[test]
        fn 打鍵が止まるまで走らせない() {
            let now = std::time::Instant::now();
            let 間 = SCAN_DEBOUNCE;
            // 語が変わっていなければ、起こしてもらう必要も無い。
            assert_eq!(scan_due(None, now, 間), ScanWhen::Idle);
            // まだ間が空いていなければ、その時刻に起こしてもらう。
            let typed = now - 間 / 2;
            assert_eq!(scan_due(Some(typed), now, 間), ScanWhen::WakeAt(typed + 間));
            // ちょうど過ぎたところから始める。
            assert_eq!(scan_due(Some(now - 間), now, 間), ScanWhen::Start);
            assert_eq!(scan_due(Some(now - 間 * 2), now, 間), ScanWhen::Start);
        }

        #[test]
        fn 選んでいる件が窓から出ないよう追う() {
            // 窓は 3 件。下へ出たら、下端に付いてくる。
            let mut across = CrossSearch {
                selected: 5,
                ..CrossSearch::default()
            };
            across.follow(3);
            assert_eq!(across.offset, 3);
            // 窓の中にいるあいだは動かさない。
            across.selected = 4;
            across.follow(3);
            assert_eq!(across.offset, 3);
            // 上へ出たら、上端に付いてくる。
            across.selected = 1;
            across.follow(3);
            assert_eq!(across.offset, 1);
            // 1 件も置けない高さでも、選んでいる件を追い越さない。
            across.selected = 7;
            across.follow(0);
            assert_eq!(across.offset, 7);
        }
    }
}

/// `Enter` を押したとき、その件をどう扱うか。
#[derive(Debug, PartialEq, Eq)]
enum Landing {
    /// 送り先が無い。セッションを切り替えて語を渡すだけ（仕様 §12）。
    /// 名前・題名・枝の件と、履歴から消えた行。
    Nothing,
    /// 数え違い。`⌘F` に探させる。
    Search,
    /// この行を見に行く。まだ語が有れば送り、無ければ `Search` と同じにする。
    Row(i32),
}

/// 通し番号を、いまの `pushed` で行位置へ引き直して扱いを決める。
fn landing(serial: Option<i64>, pushed: u64, history: usize, screen_lines: i32) -> Landing {
    let Some(serial) = serial else {
        return Landing::Nothing;
    };
    match lines::line_of(serial, pushed, history) {
        None => Landing::Nothing,
        Some(line) if line > screen_lines - 1 => Landing::Search,
        Some(line) => Landing::Row(line),
    }
}

#[cfg(test)]
mod search_view_tests {
    use super::*;

    #[test]
    fn 消えた行は送らずに語だけ渡す() {
        assert_eq!(landing(None, 100, 10000, 24), Landing::Nothing);
        assert_eq!(landing(Some(97), 120, 10, 24), Landing::Nothing);
    }

    #[test]
    fn 流れた行は通し番号から引き直す() {
        assert_eq!(landing(Some(97), 120, 10000, 24), Landing::Row(-23));
        assert_eq!(landing(Some(120), 120, 10000, 24), Landing::Row(0));
    }

    #[test]
    fn 画面より下に出た件は数え違いとして扱う() {
        assert_eq!(landing(Some(200), 120, 10000, 24), Landing::Search);
    }

    fn view(selected: usize, offset: usize) -> SearchState {
        SearchState {
            query: String::new(),
            scope: Scope::Session,
            results: Vec::new(),
            selected,
            offset,
        }
    }

    #[test]
    fn 見える範囲の中なら位置を動かさない() {
        let mut v = view(3, 0);
        v.follow();
        assert_eq!(v.offset, 0);
    }

    #[test]
    fn 下へ出たら窓が付いてくる() {
        let mut v = view(SEARCH_ROWS, 0);
        v.follow();
        assert_eq!(v.offset, 1, "1 行ぶんだけ送る");

        let mut v = view(499, 0);
        v.follow();
        assert_eq!(v.offset, 499 + 1 - SEARCH_ROWS);
    }

    #[test]
    fn 上へ出たら窓が戻る() {
        let mut v = view(2, 10);
        v.follow();
        assert_eq!(v.offset, 2);
    }

    #[test]
    fn 選んだ行はつねに見える範囲に入る() {
        for selected in 0..600usize {
            for offset in [0usize, 5, 100, 590] {
                let mut v = view(selected, offset);
                v.follow();
                assert!(
                    v.offset <= selected && selected < v.offset + SEARCH_ROWS,
                    "selected={selected} offset={offset} -> {}",
                    v.offset
                );
            }
        }
    }

    fn cross(query: &str) -> CrossSearch {
        CrossSearch {
            query: query.into(),
            ..CrossSearch::default()
        }
    }

    #[test]
    fn 語が空白だけなら元の一覧に戻る() {
        // 結果 0 件の一覧を出すのではなく、セッションの一覧を組む（仕様 §6）。
        // 走査を始めない判定（`start_scan`）と同じ規則でなければ、
        // 当たりが捨てられたあとも空の一覧が出たままになる。
        assert!(!showing_hits(None));
        assert!(!showing_hits(Some(&cross(""))));
        assert!(!showing_hits(Some(&cross(" "))));
        assert!(!showing_hits(Some(&cross("\t "))));
        // 全角の空白も空白として扱う。
        assert!(!showing_hits(Some(&cross("　"))));
        assert!(showing_hits(Some(&cross("ses"))));
        // 前後の空白は語の一部ではないが、語そのものは残っている。
        assert!(showing_hits(Some(&cross(" ses "))));
    }

    #[test]
    fn 結果の行は二段ぶんの高さを取る() {
        // 1 件は 2 段（仕様 §6）。1 段目と 2 段目の高さに、上下の余白を足す。
        // 余白は PAD_TOP 9 + PAD_BOTTOM 10 = 19pt で、倍率 2 なら 38 画素。
        assert_eq!(across_block_height(2.0, 34.0, 32.0), 38.0 + 34.0 + 32.0);
        assert_eq!(across_block_height(1.0, 17.0, 16.0), 19.0 + 17.0 + 16.0);
        // 3 段目（枝）のぶんは取らない。結果の行に枝は出さない。
        let three = 19.0 + 17.0 + 16.0 + 13.0;
        assert!(across_block_height(1.0, 17.0, 16.0) < three);
    }

    #[test]
    fn 走っているあいだは当たりが無いと言わない() {
        assert_eq!(scan_note(true, false, 0), ScanNote::Searching);
        assert_eq!(scan_note(false, true, 0), ScanNote::Searching);
        assert_eq!(scan_note(false, false, 0), ScanNote::NoMatch);
        assert_eq!(scan_note(true, false, 3), ScanNote::None);
        assert_eq!(scan_note(false, false, 3), ScanNote::None);
    }

    #[test]
    fn 貼り付けた語の改行は空白になる() {
        let mut a = CrossSearch::default();
        a.query.push_str("ab ");
        a.push_pasted("cd\r\nef\tg\n");
        assert_eq!(a.query, "ab cd  efg ");
    }

    #[test]
    fn 変換中は窓の幅から文字列のぶんを先に取り分ける() {
        // 変換中でなければ、幅は全部が語のもの。
        assert_eq!(split_box_width(100.0, false, 56.0), (100.0, 0.0));
        // 広い窓は、指定のぶんだけ変換中へ割く。
        assert_eq!(split_box_width(100.0, true, 56.0), (44.0, 56.0));
        // 狭い窓でも、語の幅が負にならない。
        assert_eq!(split_box_width(30.0, true, 56.0), (0.0, 30.0));
        assert_eq!(split_box_width(-5.0, true, 56.0), (0.0, 0.0));
    }

    #[test]
    fn 変換中の文字列は語が使わなかった幅をすべて使える() {
        let (query_w, _) = split_box_width(100.0, true, 56.0);
        // 短い語なら、取り分ではなく実際の余りが残る。
        assert_eq!(preedit_room(100.0, 20.0), 80.0);
        // 語が予算いっぱいなら、残るのはちょうど取り分。
        assert_eq!(preedit_room(100.0, query_w), 56.0);
        // 負にならない。
        assert_eq!(preedit_room(100.0, 130.0), 0.0);
        assert_eq!(preedit_room(-5.0, 0.0), 0.0);
    }

    #[test]
    fn 件が置けない高さでも理由を出す() {
        assert_eq!(across_note_text(true, &ScanNote::None), Some("too narrow"));
        assert_eq!(across_note_text(false, &ScanNote::None), None);
        assert_eq!(across_note_text(true, &ScanNote::NoMatch), Some("no match"));
    }

    #[test]
    fn 高さに入る件数だけ席を作る() {
        // 帯の下（34）から `limit` まで。1 件 50 画素、あいだ 4 画素。
        assert_eq!(across_rows(34.0, 34.0, 50.0, 4.0), 0, "1 件も置けない");
        assert_eq!(across_rows(34.0, 84.0, 50.0, 4.0), 1, "ちょうど 1 件");
        assert_eq!(
            across_rows(34.0, 137.0, 50.0, 4.0),
            1,
            "2 件目に 1 画素足りない"
        );
        assert_eq!(across_rows(34.0, 138.0, 50.0, 4.0), 2);
        // 帯が `limit` より下にあっても数えない。
        assert_eq!(across_rows(900.0, 100.0, 50.0, 4.0), 0);
        // 高さの無い件を数え続けない。これは描画の経路にある。
        assert_eq!(across_rows(0.0, 1000.0, 0.0, 0.0), 0);
    }

    #[test]
    fn 送った位置から置ける行数を数える() {
        // 高さの違う 3 行。帯の下（34）から、あいだ 4 画素で置く。
        let h = [54.0, 71.0, 54.0];
        // 54 + 4 + 71 で 129。34 から置いて 163 がちょうど 2 行。
        assert_eq!(sidebar_rows(&h, 0, 34.0, 163.0, 4.0), 2);
        assert_eq!(
            sidebar_rows(&h, 0, 34.0, 162.0, 4.0),
            1,
            "2 行目に 1 画素足りない"
        );
        // 送れば、先頭の行が落ちた先から数える。
        assert_eq!(sidebar_rows(&h, 1, 34.0, 163.0, 4.0), 2);
        assert_eq!(
            sidebar_rows(&h, 3, 34.0, 1000.0, 4.0),
            0,
            "末尾より先は 0 行"
        );
    }

    #[test]
    fn 最後の行が下端に来る位置より先へは送らない() {
        let h = [54.0, 71.0, 54.0];
        // 3 行とも入るなら送らない。
        assert_eq!(sidebar_max_offset(&h, 34.0, 1000.0, 4.0), 0);
        // 末尾の 2 行（71 + 4 + 54）が入る高さ。
        assert_eq!(sidebar_max_offset(&h, 34.0, 163.0, 4.0), 1);
        // 末尾の 1 行しか入らない高さ。
        assert_eq!(sidebar_max_offset(&h, 34.0, 162.0, 4.0), 2);
        // 1 行も入らない高さでも、行の範囲は出ない。
        assert_eq!(sidebar_max_offset(&h, 34.0, 34.0, 4.0), 2);
    }

    #[test]
    fn 選んだ行が下に隠れていれば送り位置を進める() {
        let h = [54.0, 71.0, 54.0];
        // 2 行しか入らない高さで、3 行目を選ぶ。
        assert_eq!(sidebar_follow(&h, 0, 2, 34.0, 163.0, 4.0), 1);
        // 入っているあいだは動かさない。車輪で送った位置を奪わない。
        assert_eq!(sidebar_follow(&h, 0, 1, 34.0, 163.0, 4.0), 0);
    }

    #[test]
    fn 選んだ行が上に隠れていればその行まで戻す() {
        let h = [54.0, 71.0, 54.0];
        assert_eq!(sidebar_follow(&h, 2, 0, 34.0, 163.0, 4.0), 0);
    }

    #[test]
    fn 一行も入らない高さでも送り続けない() {
        // ここは描画の経路にある。入らない高さで回り続けてはならない。
        let h = [54.0, 71.0, 54.0];
        assert_eq!(sidebar_follow(&h, 0, 2, 34.0, 34.0, 4.0), 2);
    }

    #[test]
    fn 車輪は上で戻し下で進める() {
        // 上へ回すと `lines` は正。既存の経路と同じ向きである。
        assert_eq!(wheel_offset(3, 2, 10), 1);
        assert_eq!(wheel_offset(1, 3, 10), 0, "先頭より前へは行かない");
        assert_eq!(wheel_offset(3, -2, 10), 5);
        assert_eq!(wheel_offset(9, -5, 10), 10, "上限で止まる");
    }

    #[test]
    fn 送った一覧でも境目は木の行で返る() {
        // 窓の中の位置と木の行を混ぜると、掴んだ行が送った数だけ
        // 離れた位置へ落ちる。
        let sl = SidebarLayout {
            width: 200.0,
            rect_x: 6.0,
            rect_w: 186.0,
            blocks: vec![
                Block {
                    index: 7,
                    depth: 0,
                    top: 34.0,
                    height: 50.0,
                },
                Block {
                    index: 8,
                    depth: 0,
                    top: 88.0,
                    height: 50.0,
                },
            ],
            offset: 5,
            sep_y: 150.0,
        };
        // 1 本目の上半分はその行の前の境目、下半分は次の境目。
        assert_eq!(sidebar_boundary(&sl, 40.0), 5);
        assert_eq!(sidebar_boundary(&sl, 80.0), 6);
        // 窓の末尾より下は、出ている最後の行の後ろ。
        assert_eq!(sidebar_boundary(&sl, 300.0), 7);
        // 描く高さは窓の中へ戻す。
        assert_eq!(sidebar_boundary_y(&sl, 6, 1.0), 86.0);
    }
}

#[cfg(test)]
mod path_tests {
    /// macOS の `dirs::config_dir()` は `~/.config` ではなく
    /// `~/Library/Application Support` を返す。README に書いた場所と
    /// 実装がずれないよう、XDG の作法に従っていることを確かめる。
    #[test]
    fn 設定と履歴は_xdg_の場所にある() {
        let config = crate::config::config_path().expect("設定の場所が決まる");
        let db = crate::history::db_path().expect("履歴の場所が決まる");
        assert!(
            config.ends_with(".config/termit/config.toml"),
            "設定の場所が想定と違う: {}",
            config.display()
        );
        assert!(
            db.ends_with(".local/share/termit/history.db"),
            "履歴の場所が想定と違う: {}",
            db.display()
        );
    }

    #[test]
    fn 環境変数で置き場所を移せる() {
        // 環境変数を書き換えるテストは並行実行と相性が悪いので、
        // 変数を読む関数の方を直接確かめる。
        let d = crate::config::xdg_dir("XDG_CONFIG_HOME_TEX_TEST_UNSET", ".config")
            .expect("既定へ落ちる");
        assert!(d.ends_with(".config"));
    }
}

/// 見えている範囲の一致を、セルの集まりに直す。
///
/// 一致は点の範囲で返るが、描くときはセルごとに引きたい。
/// 見えている範囲だけなので、数は画面の広さで頭打ちになる。
pub(crate) fn collect_find_cells(state: &mut State) {
    state.find_cells.clear();
    state.find_current.clear();
    let Some(find) = &mut state.find else { return };
    if !find.is_ready() {
        return;
    }
    let Some(session) = state.manager.selected() else {
        return;
    };
    let term = session.term.lock();
    let cols = {
        use alacritty_terminal::grid::Dimensions;
        term.grid().columns()
    };
    let matches = find.visible(&term);
    let current = find.current.clone();
    drop(term);

    let push = |set: &mut std::collections::HashSet<(i32, usize)>,
                m: &alacritty_terminal::term::search::Match| {
        let (start, end) = (m.start(), m.end());
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
                cols - 1
            };
            for c in first..=last.min(cols.saturating_sub(1)) {
                set.insert((line.0, c));
            }
            line = alacritty_terminal::index::Line(line.0 + 1);
        }
    };
    for m in &matches {
        push(&mut state.find_cells, m);
    }
    if let Some(m) = &current {
        push(&mut state.find_current, m);
    }
}

//! 全セッションのスクロールバックを走り、当たりを新しい順に並べる。
//!
//! 1 行の中の当たりと、当たりを中心にした詰め方もここに置く。
//!
//! 左ペインは 200pt ほどの幅しかなく、結果の 1 行には 20 字前後しか載らない。
//! 末尾を落とす `fit_tail` も先頭を落とす `fit_head` も、探した語そのものを
//! 落としかねない。そこで当たりを残して、前後から落とす。
//!
//! 単位に注意する。`find_hit` が返すのは元の文字列の**バイト**位置、
//! `window_around` が受け取り返すのは**文字**の添字である。

use std::cmp::Ordering;
use std::ops::Range;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::Line;
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::TermMode;
use alacritty_terminal::Term;

use crate::lines::{self, Ticks};
use crate::secret::Redactor;
use crate::session::SessionId;
use crate::term::EventProxy;

/// 大文字小文字を無視した素の一致。`needle_lower` は呼ぶ側が小文字にして渡す。
///
/// 返す範囲は `text` の**バイト**位置。小文字にすると長さが変わる字（'İ' など）が
/// あるので、小文字側のバイト位置から元のバイト位置へ戻す対応表を作る。
pub fn find_hit(text: &str, needle_lower: &str) -> Option<Range<usize>> {
    if needle_lower.is_empty() {
        return None;
    }
    let mut lower = String::with_capacity(text.len());
    // 小文字側の各バイトが、元の文字のどこから（start）どこまで（end）かを持つ。
    let mut start_at: Vec<usize> = Vec::with_capacity(text.len());
    let mut end_at: Vec<usize> = Vec::with_capacity(text.len());
    for (i, c) in text.char_indices() {
        let end = i + c.len_utf8();
        for lc in c.to_lowercase() {
            for _ in 0..lc.len_utf8() {
                start_at.push(i);
                end_at.push(end);
            }
            lower.push(lc);
        }
    }
    let at = lower.find(needle_lower)?;
    let from = start_at[at];
    let to = end_at[at + needle_lower.len() - 1];
    Some(from..to)
}

/// 当たりを中心に `keep` 字だけ残す範囲を返す。添字はすべて**文字**。
///
/// 当たりの前後へ均等に伸ばし、片側が尽きたら残りをもう片側へ回す。
/// 当たりが `keep` より長ければ、当たりの頭から `keep` 字を残す。
/// 戻り値の 2 つの真偽は、頭を落としたか・尾を落としたか。
pub fn window_around(chars: &[char], hit: Range<usize>, keep: usize) -> (Range<usize>, bool, bool) {
    let len = chars.len();
    if keep >= len {
        return (0..len, false, false);
    }
    let hit_len = hit.end.saturating_sub(hit.start);
    let before = keep.saturating_sub(hit_len) / 2;
    let start = hit.start.saturating_sub(before).min(len - keep);
    let end = start + keep;
    (start..end, start > 0, end < len)
}

/// 走査するセッション 1 本ぶんの持ち物。
///
/// `Term` は `Arc` の複製だけを受け取る。中身を写してから走る案は採らない。
/// 14 本 × 10000 行 × 80 桁で 10M 文字を超え、常駐 85MB の看板に合わない（仕様 §10）。
pub struct Target {
    pub id: SessionId,
    pub term: Arc<FairMutex<Term<EventProxy>>>,
    /// これまでに履歴へ押し出した行の数。行位置に足すと通し番号になる。
    pub pushed: Arc<AtomicU64>,
    /// 通し番号から時刻を引く目盛り。
    pub ticks: Arc<Mutex<Ticks>>,
    pub name: String,
    pub title: Option<String>,
    pub branch: Option<String>,
}

/// 何に当たったか。行のほかに、左ペインが出している 3 つの手がかりも探す（仕様 §7）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HitKind {
    Line,
    Name,
    Title,
    Branch,
}

/// 1 件の当たり。1 件は 1 当たりで、セッション単位にはまとめない（仕様 §5）。
#[derive(Debug, Clone)]
pub struct Hit {
    pub id: SessionId,
    pub kind: HitKind,
    /// 当たった行の通し番号。名前・題名・枝と、代替画面の行なら `None`。
    pub serial: Option<i64>,
    /// その行が現れた時刻。行でない件（名前・題名・枝）だけが `None` である。
    ///
    /// `Ticks` はどの行にも必ず時刻を答えるので、行の件が `None` になることは無い。
    pub at: Option<Instant>,
    /// 伏せ字を通したあとの行の文字列。
    pub text: String,
    /// `text` の中の当たりの**バイト**位置。
    pub hit: Range<usize>,
}

/// 走査の上限。
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// 1 本あたりの採用数。1 本の当たりで一覧を埋めさせない（仕様 §8）。
    pub per_session: usize,
    /// 全体の採用数。
    pub total: usize,
    /// 錠を放すまでに見る行数。
    pub chunk: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            per_session: 20,
            total: 200,
            chunk: 256,
        }
    }
}

/// 打った語から `find_hit` に渡す針を作る。
///
/// 針も 1 文字ずつ小文字にする。`find_hit` は干し草を `char::to_lowercase` で
/// 小文字にするので、こちらだけ `str::to_lowercase` を使うと食い違う。
/// ギリシャ語の語末シグマがそれで、`str` は語末の 'Σ' を 'ς' にするが
/// `char` は必ず 'σ' にする。打った語が黙って当たらなくなる。
pub fn needle(query: &str) -> String {
    query.chars().flat_map(char::to_lowercase).collect()
}

/// 全セッションのスクロールバックを、新しい方から探す。
///
/// 走るのは別スレッドである（仕様 §10）。1 フレームの余白は 0.3ms ほどしかなく、
/// 10000 行の走査はそこに入らない。
pub fn scan(targets: &[Target], query: &str, redactor: &Redactor, limits: Limits) -> Vec<Hit> {
    let needle = needle(query);
    if needle.trim().is_empty() {
        return Vec::new();
    }
    let mut hits = Vec::new();
    for target in targets {
        scan_one(target, &needle, redactor, limits, &mut hits);
    }
    // 安定な並べ替えなので、同じ時刻のあいだは走った順＝本ごとの新しい順が残る。
    hits.sort_by(newest_first);
    hits.truncate(limits.total);
    hits
}

/// 一覧の並び。名前・題名・枝を先頭に置き、残りを新しい順に並べる。
///
/// 先頭に置くのは、それが時刻を持たないからではなく、行ではないからである。
/// 時刻を見るのは行の件どうしだけで、そこには必ず時刻が付いている
/// （`Ticks::time_of` は答えを返さないことがない）。
fn newest_first(a: &Hit, b: &Hit) -> Ordering {
    let pinned = |h: &Hit| h.kind != HitKind::Line;
    pinned(b).cmp(&pinned(a)).then_with(|| b.at.cmp(&a.at))
}

/// 1 本を走る。名前・題名・枝を見てから、行を遡る。
fn scan_one(
    target: &Target,
    needle: &str,
    redactor: &Redactor,
    limits: Limits,
    out: &mut Vec<Hit>,
) {
    // 名前・題名・枝は 1 本につき 3 つしかないので、`per_session` には数えない。
    // 上限は「よく喋る本が一覧を埋めてしまう」ことへの備えである（仕様 §8）。
    for (kind, text) in [
        (HitKind::Name, Some(target.name.as_str())),
        (HitKind::Title, target.title.as_deref()),
        (HitKind::Branch, target.branch.as_deref()),
    ] {
        let Some((text, hit)) = text.and_then(|t| matched(redactor, t, needle)) else {
            continue;
        };
        out.push(Hit {
            id: target.id,
            kind,
            serial: None,
            at: None,
            text,
            hit,
        });
    }
    scan_lines(target, needle, redactor, limits, out);
}

/// 行を新しい方から遡る。`limits.chunk` 行ごとに錠を放す。
///
/// 錠は 2 つある。`term` は pty の読み手が 64KiB の読みごとに取るので、
/// 10000 行を取りっぱなしで走れば、その本の出力そのものが止まる（仕様 §10）。
/// `ticks` は読み手が 1 読みにつき 1 回取る。読み手は `term` を持ったまま
/// `ticks` を取らないので、こちらもその順を守る。塊を読み終えて `term` を
/// 放してから、その塊ぶんの時刻をまとめて引く。
fn scan_lines(
    target: &Target,
    needle: &str,
    redactor: &Redactor,
    limits: Limits,
    out: &mut Vec<Hit>,
) {
    // 0 行ずつ走ると塊が進まない。
    let chunk = limits.chunk.max(1);
    // 直前に採った行。全画面 UI の描き直しは同じ行を何度も履歴へ入れるので、
    // 続けて同じ文が当たるぶんは 1 件にまとめる（仕様 §8）。見ているのは行の
    // 並びではなく一覧に並ぶ文なので、当たらない行を挟んでいても畳む。
    let mut last: Option<String> = None;
    let mut taken = 0usize;
    // 次に見る行の通し番号。行位置ではなく通し番号で覚える。
    // 塊のあいだに出力が流れると、同じ行位置はもう同じ行を指さない。
    let mut next: Option<i64> = None;
    // 走り始めた時点でいちばん古かった行の通し番号。ここまで遡ったら終わり。
    let mut floor = i64::MIN;
    loop {
        if taken >= limits.per_session {
            return;
        }
        // この塊で当たったぶん。時刻は錠を放してから付ける。
        let mut found: Vec<(Option<i64>, String, Range<usize>)> = Vec::new();
        // この塊の先にまだ遡る行があるか。
        let more;
        {
            let term = target.term.lock();
            // 代替画面に履歴は無い（`history_size()` が 0 になる）。`vim` や `less` が
            // 開いているあいだは、いま出ている画面しか探せない。穴として受け入れる
            // （仕様 §7）。そのうえ `pushed` は代替画面では進まないので、この画面の行に
            // 通し番号を付けると本画面の無関係な履歴行を指してしまう。付けずに置く。
            // 番号の無い件は、そのセッションへ切り替えて `⌘F` に語を渡すだけになる。
            let alt = term.mode().contains(TermMode::ALT_SCREEN);
            let grid = term.grid();
            let history = grid.history_size();
            let top = grid.screen_lines() as i32 - 1;
            // 行位置に足して通し番号にする数。グリッドと同じ錠の下で読む。
            // 読み手も錠の中で書くので、この総数は手に持ったグリッドのものである。
            let pushed = target.pushed.load(Relaxed);
            let mut at = match next {
                None => {
                    floor = lines::serial(pushed, -(history as i32));
                    lines::serial(pushed, top)
                }
                Some(at) => at,
            };
            // 覚えた行がもう履歴に無ければ（流れ去った・画面消去で履歴が縮んだ）、
            // そこで遡りを止める。
            let Some(mut line) = lines::line_of(at, pushed, history) else {
                break;
            };
            if line > top {
                // 画面が小さくなっていれば、覚えた行はもう画面の外にある。
                line = top;
                at = lines::serial(pushed, line);
            }
            let bottom = -(history as i32);
            let mut text = String::with_capacity(grid.columns());
            let mut walked = 0usize;
            while walked < chunk && taken < limits.per_session && line >= bottom && at >= floor {
                let row = line;
                let row_serial = at;
                line -= 1;
                at -= 1;
                walked += 1;

                row_text(grid, row, &mut text);
                if text.is_empty() {
                    continue;
                }
                let Some((shown, hit)) = matched(redactor, &text, needle) else {
                    continue;
                };
                if last.as_deref() == Some(shown.as_str()) {
                    continue;
                }
                last = Some(shown.clone());
                found.push(((!alt).then_some(row_serial), shown, hit));
                taken += 1;
            }
            next = Some(at);
            more = line >= bottom && at >= floor;
        }
        // 錠を放してから時刻を引く。`term` を持ったまま `ticks` は取らない。
        if !found.is_empty() {
            let ticks = target
                .ticks
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            for (serial, text, hit) in found {
                out.push(Hit {
                    id: target.id,
                    kind: HitKind::Line,
                    serial,
                    // 番号の引けない行には、この本が最後に押し出した時刻を付ける。
                    // 行には必ず時刻が付く。`None` になるのは行でない件だけである。
                    at: Some(match serial {
                        Some(serial) => ticks.time_of(serial),
                        None => ticks.newest(),
                    }),
                    text,
                    hit,
                });
            }
        }
        if !more {
            return;
        }
    }
}

/// グリッドの 1 行を文字列にする。行末の空白は落とす。
///
/// 全角の後ろ半分は実体を持たないコマなので飛ばす（`session.rs` の `grid_text` と
/// 同じ規則）。走査と、選んで飛ぶときの確かめが同じ読み方をするよう、ここに 1 つだけ置く。
pub fn row_text(
    grid: &alacritty_terminal::grid::Grid<alacritty_terminal::term::cell::Cell>,
    row: i32,
    out: &mut String,
) {
    out.clear();
    for cell in &grid[Line(row)][..] {
        if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
            continue;
        }
        out.push(cell.c);
    }
    while out.ends_with(' ') {
        out.pop();
    }
}

/// 伏せ字を通してから当たりを探す。返すのは伏せたあとの文字列と、その中の位置。
///
/// 先に素の文字列で見るのは速さのためである。`Redactor` は式を 10 本ほど走らせる。
/// 14 本 × 10000 行の全部に掛ければ秒の単位になるので、当たらない行は式を
/// 1 本も走らせずに捨てる。
///
/// 当たった行だけ伏せ字を通し、伏せたあとの文字列でもう一度探す。鍵の中にしか
/// ない当たりはここで消える。端末で `••••` に見えている鍵が、結果の一覧に
/// 素のまま出ることはない（仕様 §6）。
pub(crate) fn matched(
    redactor: &Redactor,
    text: &str,
    needle: &str,
) -> Option<(String, Range<usize>)> {
    find_hit(text, needle)?;
    let (shown, _) = redactor.redact(text);
    let hit = find_hit(&shown, needle)?;
    Some((shown, hit))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ScreenConfig;
    use crate::term::{TermSize, UiSender};
    use alacritty_terminal::event::WindowSize;
    use alacritty_terminal::term::TermMode;
    use alacritty_terminal::vte::ansi::{Processor, StdSyncHandler};
    use std::sync::mpsc::channel;
    use std::time::Duration;

    #[test]
    fn 語末のシグマを文字ごとに小文字にする() {
        assert_eq!(needle("ΑΣ"), "ασ");
        assert_eq!(needle("SES"), "ses");
    }

    const COLS: usize = 60;
    const ROWS: usize = 6;

    fn make_term(cols: usize, lines: usize, scrollback: usize) -> Term<EventProxy> {
        let (pty_tx, _pty_rx) = channel();
        let (ui_tx, _ui_rx) = channel();
        let size = TermSize::new(cols, lines);
        let ws = Arc::new(FairMutex::new(WindowSize {
            num_lines: lines as u16,
            num_cols: cols as u16,
            cell_width: 8,
            cell_height: 16,
        }));
        let proxy = EventProxy::new(1, pty_tx, UiSender::Channel(ui_tx), ws);
        crate::term::new_term(size, scrollback, proxy)
    }

    /// 台本を流した `Target` を 1 本作る。
    ///
    /// `at` を渡すとその時刻の目盛りを 1 つだけ置く。目盛りが 1 つならどの行も
    /// その時刻になるので、本どうしの新しさを比べるのに足りる。`None` なら
    /// 目盛りの無い本になり、行の時刻は `Ticks` の作った時刻になる。
    /// 押し出した行数は走査では読むだけなので、試験では 0 のままにする。
    fn target(id: SessionId, play: &[u8], at: Option<Instant>) -> Target {
        let mut term = make_term(COLS, ROWS, 10_000);
        let mut parser: Processor<StdSyncHandler> = Processor::new();
        parser.advance(&mut term, play);
        let mut ticks = Ticks::new();
        if let Some(at) = at {
            ticks.mark_read(0, 1, 0, at);
        }
        Target {
            id,
            term: Arc::new(FairMutex::new(term)),
            pushed: Arc::new(AtomicU64::new(0)),
            ticks: Arc::new(Mutex::new(ticks)),
            name: String::new(),
            title: None,
            branch: None,
        }
    }

    /// 伏せる式を 1 つも持たない `Redactor`。行の走りだけを見たいとき。
    fn plain() -> Redactor {
        Redactor::new(&[]).expect("式が無ければ組み立てられる")
    }

    /// 行の列を台本にする。
    fn play(lines: &[&str]) -> Vec<u8> {
        let mut out = String::new();
        for line in lines {
            out.push_str(line);
            out.push_str("\r\n");
        }
        out.into_bytes()
    }

    /// 1 行ずつ違う文の台本。連続の畳み込みに掛からないようにする。
    fn numbered(prefix: &str, n: usize) -> Vec<u8> {
        let lines: Vec<String> = (1..=n).map(|i| format!("{prefix} {i}")).collect();
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        play(&refs)
    }

    #[test]
    fn 新しい順に並ぶ() {
        let t0 = Instant::now();
        let a = target(1, &play(&["古い ses の話"]), Some(t0));
        let b = target(
            2,
            &play(&["新しい ses の話"]),
            Some(t0 + Duration::from_secs(60)),
        );
        let hits = scan(&[a, b], "ses", &plain(), Limits::default());
        assert_eq!(hits.len(), 2, "{hits:?}");
        assert_eq!(hits[0].id, 2, "新しい本が先に来る");
        assert_eq!(hits[1].id, 1);
    }

    #[test]
    fn 名前は新しい行より先に来る() {
        // 行の時刻がいくら新しくても、名前・題名・枝の件を押し下げない。
        let t0 = Instant::now();
        let mut named = target(1, &play(&["ses 新しい行"]), Some(t0));
        named.name = "ses の本".to_string();
        let hits = scan(
            std::slice::from_ref(&named),
            "ses",
            &plain(),
            Limits::default(),
        );
        assert_eq!(hits.len(), 2, "{hits:?}");
        assert_eq!(hits[0].kind, HitKind::Name);
        assert_eq!(hits[1].at, Some(t0));
    }

    #[test]
    fn 一度も流れていない本の行にも時刻が付く() {
        // 開いたばかりの本は 1 行も押し出していない。目盛りが無くても、
        // `Ticks` が本を開いた時刻を答えるので行から時刻が引ける。
        // 引けないと一覧の並びが決まらない。
        let t = target(1, &play(&["ses one"]), None);
        let hits = scan(std::slice::from_ref(&t), "ses", &plain(), Limits::default());
        assert_eq!(hits.len(), 1, "{hits:?}");
        assert!(hits[0].at.is_some(), "{hits:?}");
        assert!(hits[0].serial.is_some(), "本画面の行には通し番号が付く");
    }

    #[test]
    fn 一件は一当たり() {
        let t = target(1, &play(&["ses one", "ses two", "ses three"]), None);
        let hits = scan(&[t], "ses", &plain(), Limits::default());
        assert_eq!(hits.len(), 3, "{hits:?}");
        // 1 本の中でも新しい方から並ぶ。
        assert_eq!(hits[0].text, "ses three");
        assert_eq!(hits[2].text, "ses one");
        assert!(
            hits.iter()
                .all(|h| h.kind == HitKind::Line && h.serial.is_some()),
            "{hits:?}"
        );
    }

    #[test]
    fn 連続する同じ文は一件にまとめる() {
        let t = target(1, &play(&["待っています"; 5]), None);
        let hits = scan(&[t], "待って", &plain(), Limits::default());
        assert_eq!(hits.len(), 1, "{hits:?}");
    }

    #[test]
    fn 一本あたりの上限で他の本の席を残す() {
        let a = target(1, &numbered("ses", 50), None);
        let b = target(2, &play(&["ses だけの本"]), None);
        let limits = Limits {
            per_session: 20,
            ..Limits::default()
        };
        let hits = scan(&[a, b], "ses", &plain(), limits);
        assert_eq!(hits.iter().filter(|h| h.id == 1).count(), 20, "{hits:?}");
        assert_eq!(hits.iter().filter(|h| h.id == 2).count(), 1, "{hits:?}");
    }

    #[test]
    fn 全体の上限で止まる() {
        // 全行が当たる語で走らせる。2 本で 40 件ぶん拾えるところを 10 件で切る。
        let a = target(1, &numbered("ses a", 50), None);
        let b = target(2, &numbered("ses b", 50), None);
        let limits = Limits {
            total: 10,
            ..Limits::default()
        };
        let hits = scan(&[a, b], "ses", &plain(), limits);
        assert_eq!(hits.len(), 10, "{hits:?}");
    }

    #[test]
    fn 塊ごとに錠を放しても遡りを続ける() {
        // 塊の境目では、覚えた通し番号から行位置を出し直す。4 行ずつ放しても、
        // 一息に走ったときと同じ件数・同じ並びになること。
        let rows = 50;
        let t = target(1, &numbered("ses", rows), None);
        let limits = Limits {
            per_session: 100,
            chunk: 4,
            ..Limits::default()
        };
        let hits = scan(std::slice::from_ref(&t), "ses", &plain(), limits);
        assert_eq!(hits.len(), rows, "{hits:?}");
        assert_eq!(hits[0].text, "ses 50", "新しい方から並ぶ");
        assert_eq!(hits[rows - 1].text, "ses 1");
    }

    #[test]
    fn 空白だけの語では何も探さない() {
        let t = target(1, &play(&["ses one"]), None);
        let one = std::slice::from_ref(&t);
        assert!(scan(one, "", &plain(), Limits::default()).is_empty());
        assert!(scan(one, "   ", &plain(), Limits::default()).is_empty());
    }

    #[test]
    fn 伏せ字の中の当たりは出さない() {
        let r = Redactor::new(&ScreenConfig::default().redact).expect("既定の式は組み立てられる");
        let t = target(1, &play(&["key is AKIAIOSFODNN7EXAMPLE here"]), None);
        let one = std::slice::from_ref(&t);
        // 鍵の一部を語にしても当たらない。
        let inside = scan(one, "iosfodnn", &r, Limits::default());
        assert!(inside.is_empty(), "{inside:?}");
        // 同じ行の別の語なら当たるが、行の文字列に鍵は残っていない。
        let hits = scan(one, "key is", &r, Limits::default());
        assert_eq!(hits.len(), 1, "{hits:?}");
        assert!(
            !hits[0].text.contains("AKIAIOSFODNN7EXAMPLE"),
            "{}",
            hits[0].text
        );
        assert!(hits[0].text.contains("[redacted]"), "{}", hits[0].text);
        // 当たりの位置は、伏せたあとの文字列の中の位置である。
        assert_eq!(&hits[0].text[hits[0].hit.clone()], "key is");
    }

    #[test]
    fn 名前と題名と枝も探す() {
        let mut t = target(1, b"", None);
        t.name = "SESマッチ".to_string();
        t.title = Some("ses の一覧".to_string());
        t.branch = Some("feat/ses".to_string());
        let hits = scan(std::slice::from_ref(&t), "ses", &plain(), Limits::default());
        assert_eq!(hits.len(), 3, "{hits:?}");
        assert_eq!(hits[0].kind, HitKind::Name);
        assert_eq!(hits[0].text, "SESマッチ");
        assert_eq!(hits[0].hit, 0..3);
        assert_eq!(hits[1].kind, HitKind::Title);
        assert_eq!(hits[2].kind, HitKind::Branch);
        assert!(
            hits.iter().all(|h| h.serial.is_none() && h.at.is_none()),
            "{hits:?}"
        );
    }

    #[test]
    fn 代替画面でも落ちない() {
        let mut script = play(&["main hit one", "main hit two"]);
        script.extend_from_slice(b"\x1b[?1049h");
        script.extend_from_slice(&play(&["alt hit one", "alt hit two"]));
        let t0 = Instant::now();
        let t = target(1, &script, Some(t0));
        // 本画面で押し出した行のぶん、目盛りを 2 つ持たせる。
        let newest = t0 + Duration::from_secs(60);
        t.ticks
            .lock()
            .expect("目盛りの錠")
            .mark_read(1, 2, 0, newest);
        assert!(t.term.lock().mode().contains(TermMode::ALT_SCREEN));
        assert_eq!(
            t.term.lock().grid().history_size(),
            0,
            "代替画面に履歴は無い"
        );
        // いま出ている画面のぶんだけが出る（仕様 §7）。
        let hits = scan(std::slice::from_ref(&t), "hit", &plain(), Limits::default());
        assert_eq!(hits.len(), 2, "{hits:?}");
        assert!(hits.iter().all(|h| h.text.starts_with("alt")), "{hits:?}");
        // 代替画面の行に通し番号は付けない。`pushed` が進まないので、付けると
        // 本画面の無関係な履歴行を指す。時刻は最後の目盛りから取る。
        assert!(hits.iter().all(|h| h.serial.is_none()), "{hits:?}");
        assert!(hits.iter().all(|h| h.at == Some(newest)), "{hits:?}");
    }

    #[test]
    fn 大文字小文字を無視して探す() {
        assert_eq!(find_hit("この SESマッチ機能", "ses"), Some(7..10));
        assert_eq!(find_hit("ses", "SES"), None); // needle は小文字で渡す約束
        assert_eq!(find_hit("abc", "x"), None);
    }

    #[test]
    fn 小文字にして長さが変わっても位置を戻せる() {
        // 'İ'（U+0130）の小文字は 'i' と結合点の 2 文字で、バイト数が増える。
        // 元の文字列のバイト位置が返ること。
        let t = "aİb";
        assert_eq!(find_hit(t, "b"), Some(t.len() - 1..t.len()));
        // 増えた側の途中にかかる一致でも、元の文字の境で返る。
        assert_eq!(find_hit(t, "i"), Some(1..3));
    }

    #[test]
    fn 当たりを真ん中に残す() {
        let chars: Vec<char> = "0123456789ABCDEFGHIJ".chars().collect();
        // 当たりは 10..12。20 字のうち 8 字だけ残すなら、前後を均等に落とす。
        let (keep, head, tail) = window_around(&chars, 10..12, 8);
        assert!(keep.start <= 10 && keep.end >= 12);
        assert_eq!(keep.end - keep.start, 8);
        assert!(head && tail);
    }

    #[test]
    fn 当たりが端にあるときは片側だけ落とす() {
        let chars: Vec<char> = "0123456789".chars().collect();
        let (keep, head, tail) = window_around(&chars, 0..2, 5);
        assert_eq!(keep, 0..5);
        assert!(!head && tail);
    }

    #[test]
    fn 当たりが残す幅より長ければ当たりの頭から残す() {
        let chars: Vec<char> = "0123456789".chars().collect();
        let (keep, _, _) = window_around(&chars, 2..9, 3);
        assert_eq!(keep, 2..5);
    }
}

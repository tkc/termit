# 全セッション検索 実装計画

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 左ペインの左上の検索窓に打った語で全セッションのスクロールバックを探し、当たった文を新しい順に左ペインへ並べ、選べばその場所へ飛ぶ。

**Architecture:** 当たりの新旧を決めるため、pty の読み手で `Handler` を包み、履歴へ押し出した行の総数（`pushed`）を数える。行の通し番号は `pushed + line` で出し、`(pushed, Instant)` の環から時刻を引く。走査は別スレッドで、各セッションの `FairMutex<Term>` を数百行ごとに取り直しながら走る。結果は `UiEvent` で UI スレッドへ返し、左ペインの一覧を差し替える。

**Tech Stack:** Rust 2021、`alacritty_terminal` 0.26（`vte` 0.15 の `Handler`）、`winit`、既存の `Renderer`（wgpu + glyphon）。新しい依存は足さない。

**Spec:** [`docs/superpowers/specs/2026-10-28-cross-session-search-design.md`](../specs/2026-10-28-cross-session-search-design.md)

## Global Constraints

- 走査は別スレッド。UI スレッドで 10000 行を走らせない（仕様 §10）。測った余白は 1 フレーム 16.61ms 中 0.3ms。
- 各セッションの `Term` の錠は数百行ごとに放す。全文を写してから走る案は採らない（§10）。
- 素の文字列一致、大文字小文字は無視、正規表現なし（§7）。
- 1 本あたりの採用は 20 件、全体 200 件、連続する同じ文は 1 件（§8）。
- 結果の 2 段目は `Redactor` を通してから出す（§6）。
- 代替画面のセッションは、いま出ている画面しか探せない。穴として受け入れる（§7）。
- `Ctrl` の鍵は増やさない。鍵は `⌘⇧F` だけ（§9）。
- 変換中の文字列は検索に渡さない。`Ime::Commit` だけを受ける（§9）。
- コードのコメントと `docs/` は日本語、コミットと PR は英語。試験の名前は日本語の文。
- 1 つの Task = 1 ブランチ = 1 PR ではない。**この計画全体で 1 本のブランチ**とし、Task ごとにコミットする。

## Review Focus

仕様が黙っているが、使えば当たる入力。各行の試験は、その行を持つ Task の中に置いてある。

- **語が空白だけ**のとき、結果 0 件の一覧ではなく元のセッション一覧へ戻ること（Task 6）。
- **代替画面のセッション**（`vim`、`less`）を含めて走っても落ちず、そのセッションからは画面のぶんだけ出ること（Task 4）。
- **名前・題名・枝に当たった件**で `Enter` を押したとき、通し番号が無いので送り先が無い。切り替えるだけで終わること（Task 7）。
- **走査中に閉じられたセッション**の件を選んだとき、添字がずれている。`SessionId` で引き直し、無ければ何もしないこと（Task 7）。
- **全行が当たる語**（`e` の 1 文字など）で、上限で止まり、UI が固まらないこと（Task 4）。

---

### Task 1: `⌘⇧F` を `⌘F` から分ける

いまは物理キー側の分岐が Cmd のとき Shift を見ていないため、`⌘⇧F` が `FindInScreen` に落ちる（仕様 §9）。新しい鍵を足す前に、ここを分ける。

**Files:**
- Modify: `src/input.rs:9-33`（`Action`）、`src/input.rs:118-121`（文字側の Cmd+Shift）、`src/input.rs:166-193`（物理キー側の Cmd）
- Test: `src/input.rs` の `mod tests`（同じファイル内）

**Interfaces:**
- Produces: `Action::FindAcrossSessions`

- [ ] **Step 1: 失敗する試験を書く**

```rust
#[test]
fn コマンドとシフトのFは横断検索になる() {
    let cs = ModifiersState::SUPER | ModifiersState::SHIFT;
    // 文字側（通常の配列）。
    assert_eq!(action_for(&ch("f"), PhysicalKey::Code(KeyCode::KeyF), cs), Some(Action::FindAcrossSessions));
    // 物理キー側（文字が取れない配列）。
    assert_eq!(action_for(&Key::Named(NamedKey::Shift), PhysicalKey::Code(KeyCode::KeyF), cs), Some(Action::FindAcrossSessions));
}

#[test]
fn コマンドだけのFは画面内検索のまま() {
    let cmd = ModifiersState::SUPER;
    assert_eq!(action_for(&ch("f"), PhysicalKey::Code(KeyCode::KeyF), cmd), Some(Action::FindInScreen));
}

#[test]
fn コマンドとシフトはFとRだけ受ける() {
    let cs = ModifiersState::SUPER | ModifiersState::SHIFT;
    let phys = PhysicalKey::Code(KeyCode::KeyR);
    assert_eq!(action_for(&ch("r"), phys, cs), Some(Action::RenameSession));
    // ⌘⇧N は、いまは物理キー側で NewSession に落ちている。落ちないこと。
    let n = PhysicalKey::Code(KeyCode::KeyN);
    assert_eq!(action_for(&Key::Named(NamedKey::Shift), n, cs), None);
}
```

- [ ] **Step 2: 落ちることを確かめる**

Run: `cargo test --lib input`
Expected: FAIL（`FindAcrossSessions` が無い）

- [ ] **Step 3: `Action::FindAcrossSessions` を足し、両方の経路で Shift を分ける**

`Action` に `/// 全セッションの出力を探す。` を添えて追加する。
`action_from_char` の Cmd+Shift の分岐（いま `"r"` だけを見ている箇所）を `"f" => FindAcrossSessions`、`"r" => RenameSession`、他は `None` にする。
`action_from_physical` の Cmd の分岐の先頭で Shift を分け、Shift が付いているときは `KeyF => FindAcrossSessions`、`KeyR => RenameSession`、他は `None` を返す。`INDEX_KEYS` の照合も Shift のときは通さない。

- [ ] **Step 4: 通ることを確かめる**

Run: `cargo test --lib input && cargo clippy --all-targets -- -D warnings`
Expected: PASS（既存の `⌘F`・`⌘⇧R`・`^` 側の試験も通ったまま）

- [ ] **Step 5: コミット**

```bash
git add src/input.rs
git commit -m "feat: give cmd-shift-f its own action instead of falling through to find"
```

---

### Task 2: 行の通し番号を数える

`Grid::history_size()` はスクロールバック上限で頭打ちになり、行から時刻が引けない（仕様 §8）。`Term` へ渡す前に `Handler` を包み、履歴へ押し出した行を自分で数える。

**Files:**
- Create: `src/lines.rs`
- Modify: `src/main.rs:3-25`（`mod lines;` を足す）、`src/pty.rs:68-80`（`Spawned`）、`src/pty.rs:170-205`（`spawn`）、`src/pty.rs:323-416`（`spawn_reader`）、`src/session.rs:26-88`（`Session`）、`src/session.rs:497-668`（`spawn_new` と `fork` の組み立て）
- Test: `src/lines.rs` の `mod tests`

**Interfaces:**
- Produces:
  - `pub struct Counting<'a>` — `Counting::new(inner: &'a mut Term<EventProxy>, pushed: u64) -> Self`、`fn pushed(&self) -> u64`、`impl vte::ansi::Handler`
  - `pub struct Ticks` — `Ticks::new() -> Self`、`fn push(&mut self, pushed: u64, at: Instant)`、`fn time_of(&self, serial: i64) -> Option<Instant>`
  - `pub fn serial(pushed: u64, line: i32) -> i64`
  - `pub fn line_of(serial: i64, pushed: u64, history_size: usize) -> Option<i32>`
  - `Session.pushed: Arc<AtomicU64>`、`Session.ticks: Arc<Mutex<Ticks>>`（`Spawned` にも同じ 2 つ）

- [ ] **Step 1: 失敗する試験を書く**

`Counting` は `Term` を要るので、試験は `crate::term::new_term` で 1 本立てて `Processor` から流す（`src/pty.rs:721-780` の既存の試験が同じ形を取っている）。

```rust
#[test]
fn 本画面の改行で通し番号が増える() {
    // 24 行の Term に 30 行流す。6 行が履歴へ出る。
    // pushed == 6
}

#[test]
fn 代替画面にいるあいだは数えない() {
    // DECSET 1049 → 30 行 → DECRST 1049。pushed は 0 のまま。
}

#[test]
fn 上限に達しても増え続ける() {
    // scrollback 4 行の Term に 40 行流す。history_size は 4 で止まるが
    // pushed は 36 まで伸びる。
}

#[test]
fn 通し番号から行位置を出す() {
    assert_eq!(serial(100, 0), 100);
    assert_eq!(serial(100, -3), 97);
    // 20 行流れたあと、通し番号 97 は 23 行上にある。
    assert_eq!(line_of(97, 120, 10000), Some(-23));
    // 履歴から押し出されていれば無い。
    assert_eq!(line_of(97, 120, 10), None);
}

#[test]
fn 目盛りから時刻を引く() {
    // (10, t0) (20, t1) (30, t2) を入れ、通し番号 25 で t1 が出ること。
    // 一番古いものより前の通し番号では、残っている最古の時刻が出ること。
}
```

- [ ] **Step 2: 落ちることを確かめる**

Run: `cargo test --lib lines`
Expected: FAIL（`src/lines.rs` が無い）

- [ ] **Step 3: `src/lines.rs` を書く**

`Counting` は `inner: &'a mut Term<EventProxy>` と `pushed: u64` を持つ。`Handler` の 71 本は**すべて unit を返す**ので、1 つの macro で畳める。一覧は
`~/.cargo/registry/src/*/vte-0.15.0/src/ansi.rs` の `pub trait Handler` から取る
（`awk '/^pub trait Handler/,/^}/' src/ansi.rs | grep -E '^\s+fn '`）。

```rust
macro_rules! forward {
    ($( fn $name:ident ( $( $arg:ident : $ty:ty ),* ); )*) => {
        $( #[inline] fn $name(&mut self $(, $arg: $ty)*) { self.inner.$name($($arg),*) } )*
    };
}
```

`linefeed` と `scroll_up` だけ手で書き、押し出した行を数える。数え方は、
**上限に届くまでは `history_size()` の差が正確**なので差を使い、差が 0 かつ
上限に達しているときだけ見積もる。

```rust
fn linefeed(&mut self) {
    let alt = self.inner.mode().contains(TermMode::ALT_SCREEN);
    let before = self.inner.grid().history_size();
    let bottom = self.inner.grid().cursor.point.line.0 as usize + 1
        == self.inner.grid().screen_lines();
    self.inner.linefeed();
    if !alt {
        let d = self.inner.grid().history_size() - before;
        self.pushed += if d > 0 { d as u64 } else { bottom as u64 };
    }
}
```

`scroll_up(n)` も同じ形で、差が 0 なら `n` を足す。
`Ticks` は `VecDeque<(u64, Instant)>` の環で、容量は `pub const TICKS: usize = 4096`（約 32KB）。
`push` は前の `pushed` と同じなら積まない。`time_of` は `partition_point` で引く。
`line_of` は `serial - pushed as i64` を出し、`-(history_size as i64)` より小さければ `None`。

- [ ] **Step 4: 通ることを確かめる**

Run: `cargo test --lib lines`
Expected: PASS

- [ ] **Step 5: 読み手に繋ぐ**

`spawn_reader` に `pushed: Arc<AtomicU64>` と `ticks: Arc<Mutex<Ticks>>` を渡す（`ReaderFlags` に 2 つ足す）。
`parser.advance(&mut *term, ..)` を `Counting` 越しにする。錠を持っている区間の外で
`pushed.store(..)` と `ticks.lock().push(..)`（`Instant::now()`）を 1 読みにつき 1 回行う。
`Spawned` と `Session` に同じ 2 つを持たせ、`spawn_new` と `fork` の両方で埋める。

- [ ] **Step 6: 動かして確かめる**

Run: `cargo test --all-targets && cargo clippy --all-targets -- -D warnings`
Expected: PASS（`src/pty.rs` の既存の pty 試験も通ったまま）

- [ ] **Step 7: コミット**

```bash
git add src/lines.rs src/main.rs src/pty.rs src/session.rs
git commit -m "feat: count the lines pushed into history so a row can be dated"
```

---

### Task 3: 1 行の中の当たりと、当たりを中心に詰める

**Files:**
- Create: `src/scan.rs`
- Modify: `src/main.rs`（`mod scan;`）、`src/render.rs:575-610`（`fit_center` を足す）
- Test: `src/scan.rs` の `mod tests`

**Interfaces:**
- Produces:
  - `pub fn find_hit(text: &str, needle_lower: &str) -> Option<Range<usize>>` — 大文字小文字を無視した素の一致。返す範囲は `text` のバイト位置。
  - `pub fn window_around(chars: &[char], hit: Range<usize>, keep: usize) -> (Range<usize>, bool, bool)` — 残す範囲（文字の添字）と、頭・尾を落としたか。`hit` も**文字の添字**で渡す（`find_hit` が返すのはバイト位置なので、呼ぶ側で直す）。
  - `Renderer::fit_center(&mut self, text: &str, style: TextStyle, max_px: f32, hit: Range<usize>) -> (String, Range<usize>)`

- [ ] **Step 1: 失敗する試験を書く**

```rust
#[test]
fn 大文字小文字を無視して探す() {
    assert_eq!(find_hit("この SESマッチ機能", "ses"), Some(3..6));
    assert_eq!(find_hit("ses", "SES"), None); // needle は小文字で渡す約束
    assert_eq!(find_hit("abc", "x"), None);
}

#[test]
fn 小文字にして長さが変わっても位置を戻せる() {
    // 'İ'（U+0130、3 バイト）の小文字は 2 文字 4 バイト。
    // 元の文字列のバイト位置が返ること。
    let t = "aİb";
    assert_eq!(find_hit(t, "b"), Some(4..5));
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
```

- [ ] **Step 2: 落ちることを確かめる**

Run: `cargo test --lib scan`
Expected: FAIL

- [ ] **Step 3: `find_hit` と `window_around` を書く**

`find_hit` は `text.char_indices()` を回しながら小文字の文字列と「小文字側のバイト位置 → 元のバイト位置」の対応表を作り（`src/main.rs:1205` の `masked_cells` が同じ作りをしている）、`lower.find(needle_lower)` の結果を対応表で戻す。
`window_around` は、当たりの中心から前後へ均等に伸ばし、片側が尽きたら残りをもう片側へ回す。

- [ ] **Step 4: 通ることを確かめる**

Run: `cargo test --lib scan`
Expected: PASS

- [ ] **Step 5: `Renderer::fit_center` を足す**

既存の `fit`/`build` と同じ組みにする。収まるならそのまま返す。収まらないなら
`hit` のバイト位置を文字の添字に直し、比から残す字数の当たりを付け、`window_around`
で組み、`…` を落とした側に付けて `measure_px` で合わせ込む。返す `Range` は、
組み立てた文字列の中での当たりの**バイト位置**（`…` のぶんずれる）。
試験は `window_around` にあるので、ここは薄く保つ。

- [ ] **Step 6: コミット**

```bash
git add src/scan.rs src/render.rs src/main.rs
git commit -m "feat: match a line case-insensitively and keep the hit centered"
```

---

### Task 4: 全セッションを走る

**Files:**
- Modify: `src/scan.rs`、`src/secret.rs:109-123`（`redact` の `#[cfg(test)]` を外す）
- Test: `src/scan.rs` の `mod tests`

**Interfaces:**
- Consumes: Task 2 の `lines::{serial, Ticks}`、Task 3 の `find_hit`
- Produces:
  - `pub struct Target { pub id: SessionId, pub term: Arc<FairMutex<Term<EventProxy>>>, pub pushed: Arc<AtomicU64>, pub ticks: Arc<Mutex<Ticks>>, pub name: String, pub title: Option<String>, pub branch: Option<String> }`
  - `pub enum HitKind { Line, Name, Title, Branch }`
  - `pub struct Hit { pub id: SessionId, pub kind: HitKind, pub serial: Option<i64>, pub at: Option<Instant>, pub text: String, pub hit: Range<usize> }`
  - `pub struct Limits { pub per_session: usize, pub total: usize, pub chunk: usize }` — `Default` は 20 / 200 / 256
  - `pub fn scan(targets: &[Target], query: &str, redactor: &Redactor, limits: Limits) -> Vec<Hit>`

- [ ] **Step 1: 失敗する試験を書く**

`Target` を 2 本作り（`crate::term::new_term` に `Processor` で流し込む）、`scan` にかける。

```rust
#[test]
fn 新しい順に並ぶ() {
    // 本 A に古い当たり、本 B に新しい当たりを入れ、B が先に来ること。
}

#[test]
fn 一件は一当たり() {
    // 同じセッションの別の行 3 本が当たれば 3 件返ること。
}

#[test]
fn 連続する同じ文は一件にまとめる() {
    // 同じ文を 5 行続けて流し、1 件になること。
}

#[test]
fn 一本あたりの上限で他の本の席を残す() {
    // A に 50 当たり、B に 1 当たり。per_session=20 なら A は 20 件、B は 1 件。
}

#[test]
fn 全体の上限で止まる() {
    // total=10 なら 10 件で返ること（Review Focus: 全行が当たる語）。
}

#[test]
fn 伏せ字の中の当たりは出さない() {
    // AKIA から始まる鍵を含む行を流し、鍵の一部を語にしても 0 件。
    // 行の文字列にも鍵が残っていないこと。
}

#[test]
fn 名前と題名と枝も探す() {
    // name="SESマッチ" に対して "ses" が HitKind::Name で当たり、serial は None。
}

#[test]
fn 代替画面でも落ちない() {
    // DECSET 1049 の Term を混ぜても panic せず、画面のぶんが出ること
    //（Review Focus: 代替画面のセッション）。
}
```

- [ ] **Step 2: 落ちることを確かめる**

Run: `cargo test --lib scan`
Expected: FAIL

- [ ] **Step 3: `scan` を書く**

形：

1. `query` を小文字にする。空か空白だけなら `Vec::new()`。
2. 各 `Target` について、名前・題名・枝を `find_hit` にかける（`serial: None`、`at: None`）。
3. 行は**新しい方から**、`Line(screen_lines-1)` から `Line(-(history_size))` へ遡る。
   `limits.chunk` 行ごとに錠を取り直す（`term.lock()` を抜けてから次の塊を取る）。
   塊の境目で `history_size()` を取り直し、縮んでいれば遡りを止める。
4. 1 行の文字列は `WIDE_CHAR_SPACER` を飛ばして組む（`src/session.rs:900-930` と同じ規則）。
   末尾の空白は落とす。空行は飛ばす。
5. `redactor.redact(&text)` を通したうえで `find_hit` にかける。
   当たらなければ捨てる（伏せ字に隠れた当たりは出さない）。
6. 直前に採った行と同じ文字列なら積まない（連続の畳み込み）。
7. `serial = lines::serial(pushed.load(Relaxed), line)`、`at = ticks.lock().time_of(serial)`。
8. 1 本で `per_session` に達したら、その本を打ち切る。
9. 全本を集めてから `at` の新しい順に並べ、`total` で切る。
   `at` が無い件（名前・題名・枝）は先頭に置く。

- [ ] **Step 4: 通ることを確かめる**

Run: `cargo test --lib scan && cargo clippy --all-targets -- -D warnings`
Expected: PASS

- [ ] **Step 5: コミット**

```bash
git add src/scan.rs src/secret.rs
git commit -m "feat: scan every session newest first, under a per-session cap"
```

---

### Task 5: 別スレッドで走らせ、結果を受ける

**Files:**
- Modify: `src/term.rs`（`UiEvent` に 1 つ足す）、`src/main.rs:268-325`（`State`）、`src/main.rs:568-600`（`user_event`）、`src/main.rs:1980-2005`（起こす時刻）
- Test: `src/main.rs` の `mod tests`

**Interfaces:**
- Consumes: `scan::{scan, Target, Hit, Limits}`
- Produces:
  - `UiEvent::Scanned(u64, Vec<scan::Hit>)` — 第 1 要素は世代。
  - `pub(crate) struct CrossSearch { pub query: String, pub generation: u64, pub typed_at: Option<Instant>, pub hits: Vec<scan::Hit>, pub selected: usize, pub offset: usize }` — `#[derive(Default)]`（`⌘⇧F` で空のまま置く）
  - `CrossSearch::accept(&mut self, generation: u64, hits: Vec<scan::Hit>) -> bool`、`CrossSearch::follow(&mut self)`
  - `State.across: Option<CrossSearch>`
  - `const SCAN_DEBOUNCE: Duration = Duration::from_millis(150)`
  - `fn start_scan(&mut self)` / `fn queue_scan(&mut self)`

- [ ] **Step 1: 失敗する試験を書く**

```rust
#[test]
fn 古い世代の結果は捨てる() {
    // generation=2 を待っているあいだに届いた generation=1 の結果で
    // hits が入れ替わらないこと。
    // CrossSearch::accept(&mut self, generation: u64, hits: Vec<Hit>) -> bool を試す。
}

#[test]
fn 選んでいる件が窓から出ないよう追う() {
    // CrossSearch::follow() が offset を動かすこと（SearchState::follow と同じ約束）。
}
```

- [ ] **Step 2: 落ちることを確かめる**

Run: `cargo test --lib`
Expected: FAIL

- [ ] **Step 3: `CrossSearch` と受け口を書く**

`accept` は `generation` が自分のものと同じときだけ `hits` を入れ替え、`selected`/`offset` を 0 に戻して `true` を返す。
`queue_scan` は `typed_at = Some(Instant::now())` を置くだけ。
`start_scan` は `generation += 1` し、`Target` の列（`Arc` の複製のみ。`Term` の中身は写さない）と `redactor.clone()` と語を `std::thread::spawn` へ渡し、終わったら `EventLoopProxy` で `UiEvent::Scanned` を送る。
`Redactor` が `Clone` でなければ `Arc<Redactor>` にして `State` と共有する。

- [ ] **Step 4: デバウンスを繋ぐ**

描画の最後の `want(at)` に、`across` の `typed_at` があれば `typed_at + SCAN_DEBOUNCE` を足す。
その時刻を過ぎていて語が空でなければ `start_scan` を呼び、`typed_at` を `None` にする。
`user_event` に `UiEvent::Scanned` を足し、`accept` が `true` なら `request_redraw`。

- [ ] **Step 5: 通ることを確かめる**

Run: `cargo test --all-targets && cargo clippy --all-targets -- -D warnings`
Expected: PASS

- [ ] **Step 6: コミット**

```bash
git add src/term.rs src/main.rs
git commit -m "feat: run the scan off the UI thread and drop stale generations"
```

---

### Task 6: 検索窓と結果の一覧を描く

**Files:**
- Modify: `src/main.rs:2064-2096`（`mod sidebar` に定数）、`src/main.rs:2180-2220`（`sidebar_layout`）、`src/main.rs:2250-2280`（`sidebar_hit`）、`src/main.rs:2283-2300`（`draw_sidebar` の帯）、`src/main.rs:890-915`（鍵の振り分け）、`src/main.rs:802-828`（`Ime::Commit`）、`src/main.rs:1005-1020`（`on_action`）
- Test: `src/main.rs` の `mod search_view_tests`（既にある）

**Interfaces:**
- Consumes: `CrossSearch`、`scan::Hit`、`Renderer::fit_center`
- Produces:
  - `sidebar::SIZE_QUERY: f32 = 12.0`、`sidebar::HIT_PAD: f32 = 4.0`
  - `fn showing_hits(across: Option<&CrossSearch>) -> bool` — 語が空か空白だけなら偽
  - `fn across_blocks(state: &State, layout: &Layout) -> Vec<Block>`（`SidebarLayout.blocks` を結果で作る）
  - `fn on_across_key(&mut self, key: &Key, event: &KeyEvent, mods: ModifiersState)`
  - `SidebarHit::SearchBox`、`SidebarHit::Hit(usize)`

- [ ] **Step 1: 失敗する試験を書く**

```rust
#[test]
fn 語が空白だけなら元の一覧に戻る() {
    // Review Focus: across.query が " " のとき、結果の一覧ではなく
    // セッションの一覧を組むこと。判定は fn showing_hits(across: Option<&CrossSearch>) -> bool
    assert!(!showing_hits(Some(&cross(" "))));
    assert!(!showing_hits(Some(&cross(""))));
    assert!(showing_hits(Some(&cross("ses"))));
}

#[test]
fn 結果の行は二段ぶんの高さを取る() {
    // across_blocks の各 Block の height が
    // PAD_TOP + PAD_BOTTOM + line_height(SIZE_TITLE) + line_height(SIZE_SUB) であること。
}
```

- [ ] **Step 2: 落ちることを確かめる**

Run: `cargo test --lib search_view`
Expected: FAIL

- [ ] **Step 3: 帯に窓を描く**

`draw_sidebar` の `+` を描く前に、`across` があれば帯（`sidebar::HEADER`）の左側へ
`🔍 ` と語を `SIZE_QUERY` で描き、`+` の左までを窓の幅にする。`surface` を敷き、
入っているあいだは `accent` の縁と `cursor` の棒を出す。`state.cursor_px` を窓の
位置にして、変換中の文字列（`draw_preedit_px`）がここへ出るようにする。
窓は `across` が `None` のあいだも薄く見せる（仕様 §12「窓は常に見えている」）。

- [ ] **Step 4: 一覧を結果に差し替える**

`showing_hits` が真のとき、`sidebar_layout` の `blocks` を `across_blocks` で作る。
各件の 1 段目は既存の行と同じ組み（状態の点、`Session` の名前、`⌘` の札）を、
`hit.id` から引いたセッションで描く。2 段目は `fit_center` に通し、返った
`Range` のぶんに `theme.search_hit` を敷いてから文字を重ねる。
入りきらない件は `offset` から `blocks` の数だけ出す。並べ替えとドラッグは効かせない
（`state.mouse.sidebar_drag` を作らない）。

- [ ] **Step 5: 鍵と当たり判定を繋ぐ**

`Action::FindAcrossSessions` で `picker`/`search`/`find` を `None` にし、
`across = Some(CrossSearch::default())` を置く。
`on_key` の振り分けに `across.is_some()` の分岐を足し、`on_across_key` へ渡す。
`on_across_key`：`Esc` で `across = None`、`↑`/`↓` で `selected` を動かして `follow()`、
`Backspace` と文字で `query` を変えて `queue_scan()`、`Enter` は Task 7。
`event.text` が制御文字、または `preedit` が空でないときは受けない（`on_find_key` と同じ）。
`Ime::Commit` の分岐に `across` を足す（`rename` の次、`find` の前）。
`sidebar_hit` は、帯のうち `+` の左を `SearchBox`、結果の行を `Hit(n)` として返す。

- [ ] **Step 6: 通ることを確かめる**

Run: `cargo test --all-targets && cargo clippy --all-targets -- -D warnings && cargo fmt --check`
Expected: PASS

- [ ] **Step 7: 目で確かめる**

Run: `cargo run --release -- --probe /tmp/across.png`（画面の取り込みは塞がれているため `--probe` を使う）
Expected: 左上に窓が出ていて、結果が 2 段で並び、当たった語に色が敷かれている

- [ ] **Step 8: コミット**

```bash
git add src/main.rs
git commit -m "feat: put the search box in the left pane and list the hits there"
```

---

### Task 7: 選んで飛ぶ

**Files:**
- Modify: `src/main.rs`（`on_across_key` の `Enter`、`sidebar_hit` の `Hit`）
- Test: `src/main.rs` の `mod search_view_tests`

**Interfaces:**
- Consumes: `lines::line_of`、`search::ScreenSearch`
- Produces: `fn open_hit(&mut self, n: usize)`

- [ ] **Step 1: 失敗する試験を書く**

```rust
#[test]
fn 流れた行は通し番号から引き直す() {
    // 当たりを取ったあと 30 行流し、line_of が -30 ずれた行を返すこと。
}

#[test]
fn 消えた行なら送らずに語だけ渡す() {
    // line_of が None のとき、送り先を None にして query だけ渡す決め方
    // fn jump_target(serial: Option<i64>, pushed: u64, history: usize) -> Option<i32>
    assert_eq!(jump_target(None, 100, 10000), None);          // 名前・題名・枝の件
    assert_eq!(jump_target(Some(97), 120, 10), None);         // 履歴から消えた
    assert_eq!(jump_target(Some(97), 120, 10000), Some(-23));
}

#[test]
fn 閉じたセッションの件は何もしない() {
    // Review Focus: hit.id が sessions に無いとき、selected を動かさないこと。
}
```

- [ ] **Step 2: 落ちることを確かめる**

Run: `cargo test --lib search_view`
Expected: FAIL

- [ ] **Step 3: `open_hit` を書く**

1. `hits[n].id` を `manager.sessions()` から探す。無ければ何もしない。
2. その添字へ `manager.select(i)` で切り替える。
3. `state.find = Some(ScreenSearch::new())` に語を入れて `rebuild()` する
   （`⌘F` と同じ強調が点く）。
4. `jump_target` が `Some(line)` を返したら、`line` の文字列に語がまだ有るかを
   確かめ、有れば `scroll_to_point(Point::new(Line(line), Column(0)))`。
   無ければ（数え違い・書き換え）`find_step(Direction::Left)` に任せる。
5. `across = None` にして窓を閉じる。

- [ ] **Step 4: 通ることを確かめる**

Run: `cargo test --all-targets && cargo clippy --all-targets -- -D warnings`
Expected: PASS

- [ ] **Step 5: コミット**

```bash
git add src/main.rs
git commit -m "feat: jump to a hit by its serial and fall back to find"
```

---

### Task 8: 測って、書く

仕様 §10 は「先に測る」を要求している。走査の費用を `--bench` に足し、
数字を `docs/performance.md` に残す。

**Files:**
- Modify: `src/bench.rs`、`docs/performance.md`、`README.md`
- Test: なし（測る道具そのもの）

- [ ] **Step 1: `--bench` に走査を足す**

`bench::run` の末尾に、`14 本 × 10000 行` を組み立てて `scan::scan` を 1 語で走らせ、
1 回あたりの中央と p90 を出す節を足す。行の中身は毎行変え、当たりは 1 本に 1 つだけ置く
（上限で早じまいする効果が消えないようにする）。出力の体裁は既存の節に合わせる。

- [ ] **Step 2: 測る**

Run: `cargo run --release -- --bench 2>&1 | tail -20`
Expected: 「n 本 × m 行」の行に数字が出る

- [ ] **Step 3: フレームを落としていないことを確かめる**

Run: `RUST_LOG=info TERMIT_FRAME_LOG=1 cargo run --release`（打ってから 1 回検索する）
Expected: 毎フレームの記録に、検索の前後で落ちたフレームが無い

- [ ] **Step 4: 書く**

`docs/performance.md` に節を足し、測った数字と、どのセッションの出力も止まらなかったことを書く。
`README.md` の鍵の表に `⌘⇧F` を足し、代替画面のセッションでは画面しか探せないことを
穴として書く（仕様 §7）。

- [ ] **Step 5: コミット**

```bash
git add src/bench.rs docs/performance.md README.md
git commit -m "docs: measure the scan and record the alt-screen hole"
```

---

## 最後に

全部の Task が終わったら、仕様の各節に当たる実装が有ることを確かめる。
`cargo test --all-targets`、`cargo clippy --all-targets -- -D warnings`、`cargo fmt --check` を
通してから PR を作る（`superpowers:finishing-a-development-branch`）。

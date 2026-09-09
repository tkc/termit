# 遡り表示の実装例を五つ

作成日：2026-09-09

「遡ると画面の下が空く」「行が飛ぶ」という報告に対し、
自分の直感ではなく、動いている実装がどうしているかを集めた。
termit は `alacritty_terminal` のグリッドを使うので、1 と 2 と 3 は直接効く。

## 1. alacritty — 座標の直し方は本体側でも同じ関数を通す

`alacritty/src/display/content.rs` で、コマもカーソルも同じ関数で直している。

```rust
// コマ
let point = term::point_to_viewport(display_offset, cell.point).unwrap();
// カーソル
let cursor_point = term::point_to_viewport(display_offset, terminal_content.cursor.point).unwrap();
```

`point_to_viewport` の中身は `alacritty_terminal/src/term/mod.rs` にある。

```rust
pub fn point_to_viewport(display_offset: usize, point: Point) -> Option<Point<usize>> {
    let viewport_line = point.line.0 + display_offset as i32;
    usize::try_from(viewport_line).ok().map(|line| Point::new(line, point.column))
}
```

**効いたこと。** termit はコマの行番号をそのまま画面の行として使い、負を捨てていた。
遡ると上から抜け、画面の下が地の色のまま残る。
自前で足し引きせず、この関数を通すようにした。
カーソルも同じ道で直す。遡っていても見える範囲にいれば出す。

## 2. alacritty_terminal — 表示の繰り返しは負の行から始まる

`Grid::display_iter` の始点。

```rust
let start = Point::new(Line(-(self.display_offset() as i32) - 1), last_column);
let end_line = min(start.line + self.screen_lines(), self.bottommost_line());
```

**読み取れること。** 返ってくる行番号は履歴を含む座標である。
遡っているあいだは `-遡った行数` から始まり、負になる。
`0..行数` だと思って扱うと、遡った分だけ行が消える。

## 3. alacritty_terminal — 窓を広げたときは履歴から引き上げる

`Grid::grow_lines`。

```rust
let from_history = min(history_size, lines_added);
if from_history != lines_added {
    self.scroll_up(&(Line(0)..Line(target as i32)), lines_added - from_history);
}
self.cursor.point.line += from_history;
self.display_offset = self.display_offset.saturating_sub(lines_added);
```

**読み取れること。** 行数を増やすと、足りない分は履歴から引き上げ、
引き上げた分だけカーソルを下げ、遡り量をその分減らす。
だから窓を広げても中身は下に張り付いたままで、穴は空かない。
穴が見えるなら、原因はグリッドの外にある。

## 4. wezterm — 行に消えない番号を振る

`StableRowIndex` は履歴の先頭から数える通し番号で、
履歴があふれて古い行が捨てられても、残っている行の番号は変わらない。
捨てた本数を `stable_row_index_offset` に足し、物理位置との橋渡しにする。

**効くこと。** 遡って見ているあいだに新しい出力が届いても、
見ている場所が動かない。alacritty も `scroll_up` の中で
`display_offset` を増やして同じことをするが、上限は履歴の長さである。
履歴が満杯になると、遡って見ている中身は下へ流れていく。

## 5. kitty — 触覚板は行未満で動かし、位置を目に見せる

高精度な入力装置では、行単位ではなく画素単位で遡る。
また、右端に遡り位置を示す帯を出し、掴んで動かせる。

**termit との差。** 端数の持ち越しは入れた（7.8 の車輪の端数）。
遡り位置を目に見せる帯は無い。どこまで遡ったか分からないので、
「途切れた」のか「端まで来た」のかを利用者が区別できない。

## 6.（おまけ）foot — 送りは画素を動かして済ませる

前のフレームの画素を `memmove` でずらし、
新しく出た行だけを描く。CPU で描く実装ならではの手だが、
「画面全部を毎回作り直さない」という考え方は共通である。

**termit との差。** こちらは GPU へ毎フレーム作り直して渡している。
実測（`docs/performance.md` 7.9）では全面 7661 コマで
組み立て 0.25ms、垂直同期待ちのほうが 16ms あり、
いまのところ作り直しが問題になっていない。

## 参考

- alacritty `display/content.rs` — <https://github.com/alacritty/alacritty/blob/master/alacritty/src/display/content.rs>
- alacritty_terminal `grid/mod.rs`, `grid/resize.rs`, `term/mod.rs`（0.26.0）
- wezterm `term/src/screen.rs` — <https://github.com/wezterm/wezterm/blob/main/term/src/screen.rs>
- kitty 概要 — <https://sw.kovidgoyal.net/kitty/overview/>
- foot 性能 — <https://codeberg.org/dnkl/foot/wiki/Performance>

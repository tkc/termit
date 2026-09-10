# 端末を軽くする手法の棚卸し

作成日：2026-09-10

他の端末や描画ライブラリが実際に使っている手法を集め、
termit に**効くか、効かないか、すでに入っているか**を測った結果とともに並べる。
「web で見つけたから入れる」ではなく、測って採否を決める。
数値の出どころは `docs/performance.md`（節番号を添える）。

## 入れたもの

### 隠れているあいだは組み立てない

| 出どころ | 実測 | 節 |
|---|---|---|
| 自分で見つけた（`WindowEvent::Occluded` を扱っていなかった） | 捨てていた 1 フレーム 0.57ms × 毎秒 26〜31 回 | 7.11 |

窓が完全に隠れると macOS は描く面を渡さない。それが分かるのは最後の段なので、
組み立てと `prepare` を丸ごと捨てていた。

### 平文のあいだは memchr で飛ばす

| 出どころ | 実測 | 節 |
|---|---|---|
| ripgrep / vte などで一般的な手（`memchr`） | OSC の走査 16MiB あたり 8.7ms → 0.35ms（25 倍） | 7.13 |

OSC 7 と OSC 133 を拾うために、VT パーサへ流すのと同じバイト列を自分でも走査している。
出力のほとんどは普通の文字なので、次の `ESC` まで飛ばせば走査はほぼ無料になった。

### 字形は添字で引く

| 出どころ | 実測 | 節 |
|---|---|---|
| Alacritty の字形置き場（1024x1024 の atlas）の考え方 | 描画の CPU −15% | 7.8 |

ASCII は `(文字, 太字, 斜体)` を添字に直して配列で引く。ハッシュを引かない。

### 出したフレームの待ち行列を最短にする

| 出どころ | 実測 | 節 |
|---|---|---|
| [wgpu SurfaceConfiguration](https://docs.rs/wgpu-types/latest/wgpu_types/struct.SurfaceConfiguration.html)、[Apple の議論](https://developer.apple.com/forums/thread/711033) | 読み取りから present まで 0.5〜1.3ms | 3, 9 |

`desired_maximum_frame_latency: 1`。`CAMetalLayer` の `maximumDrawableCount` が 3 だと
50/30/16ms と揺れる。1 を頼めば次の走査で出る。

### 通知を重ねない、見えないものを組み直さない

| 出どころ | 実測 | 節 |
|---|---|---|
| kitty の `repaint_delay` / `input_delay` の考え方 | 背景セッションの描き直し 8 分の 1、引きずり 124ms → 1.3ms | 7.5, 7.7 |

## 測って却下したもの

### 1 行を 1 領域にまとめる

| 出どころ | 測った結果 | 節 |
|---|---|---|
| [Ghostty: Let's talk about performance](https://github.com/ghostty-org/ghostty/discussions/4837)（「同時に使われる書式は多くて 16 種、64 種を前提に最適化した」） | **1.9 倍遅い**。行ごと 1.06ms 対 いまの 0.57ms | 7.12 |

いまは 1 コマ 1 領域なので、glyphon へ渡す領域が画面のコマ数（7,661）だけ並ぶ。
行ごとにまとめれば 47 個で済み、`prepare` は 0.55ms → 0.22ms に下がる。
しかし行の中身は毎フレーム変わるので、**行を毎フレーム整形し直す**ことになり、
その整形が 0.84ms かかる。1 文字ずつなら整形は初回だけで済む。
「領域の数」より「整形の回数」のほうが高い。
（変わらない行を使い回す実装なら打鍵中は整形が起きない。
それでも却下するのは、CPU が問題になる場面では全部の行が毎フレーム変わるからである。）

### 走査に合わせない（vsync を切る）

| 出どころ | 判断 |
|---|---|
| 同 Ghostty の議論（「linux でも macos でも既定では走査に合わせない。できるだけ速く回す」） | 既定にしない。`--no-vsync` で選べるようにだけしてある |

`Immediate` はこの機械でも使える。しかし上限を設けずに回すと、
出力が滝のように出るセッションで CPU を食う。
利用者の求めは「小メモリ・小 CPU」なので、既定は `Fifo` のままにする。
（なお、この 2 つを公平に比べる測定はこの環境では取れていない。7.11 に理由を書いた。）

### 画素をずらして送りを済ませる

| 出どころ | 判断 |
|---|---|
| [foot Performance](https://codeberg.org/dnkl/foot/wiki/Performance) | 効かない |

前のフレームの画素を `memmove` でずらし、新しい行だけ描く。
CPU で描く実装の手であり、毎フレーム GPU へ渡す構造では持ち込めない。

## まだ入れていない候補

### 損傷追跡（変わったコマだけ描く）

| 出どころ | いまの数値からの見立て |
|---|---|
| [Alacritty PR #5773](https://github.com/alacritty/alacritty/pull/5773)、[Alacritty Rendering Pipeline](https://deepwiki.com/alacritty/alacritty/3.4-rendering-pipeline) | 効き幅は小さい |

全面を組み立て直して 0.57ms、走査待ちが 16ms。組み立てを半分にしても、
出るまでの時間は変わらない。窓が隠れているあいだの丸ごと省略（7.11）のほうが
桁が大きかった。将来 4K や大きな窓で `prepare` が 1.2ms を超えてきたら見直す。

### 描画専用スレッド

| 出どころ | いまの数値からの見立て |
|---|---|
| [Ghostty Rendering System](https://deepwiki.com/ghostty-org/ghostty/5-rendering-system) | 遅延の性質が変わる。要検討 |

いまは UI スレッドが `get_current_texture` の中で走査を 16.27ms 待つ（3 節）。
そのあいだ入力は処理されない。押鍵から表示までの実測は問題になっていないが、
描画を別スレッドへ出せば、待つのは描く側だけになる。
入れるなら、グリッドの錠（`FairMutex`）の持ち方から設計し直す必要がある。

### スクロール領域の中の送り

| 測った結果 | 直せる場所 |
|---|---|
| 領域を切った送りは 25 MiB/s。切らない送りは 88 MiB/s（**3.5 倍遅い**） | `alacritty_terminal` の中 |

`DECSTBM` で領域を切ると、送りは履歴への押し出しではなく行の入れ替えになる。
tmux や vim、エージェントの表示はこれを使う。
termit のコードでは直せないので、記録だけ残す。

## 測る道具

| 道具 | 何が分かるか |
|---|---|
| `termit --bench` | 組み立て・`prepare`・`submit` の内訳。行ごとにまとめる案の見積もりも出す |
| `termit --throughput` | PTY を読む速さ。負荷 9 種の走査と解釈を別々に出す |
| `termit --probe out.png` | 窓を開かずに 1 フレーム描く。画面を撮れない環境用 |
| `TERMIT_FRAME_LOG=1` | 毎フレームの時間、描く面を取れなかった回数と理由、読み取りの合体回数 |
| `cargo build --profile profiling` + `sample` | 記号を残した release。関数ごとの内訳 |

### vtebench について

[alacritty/vtebench](https://github.com/alacritty/vtebench) は端末の中で走らせ、
標準出力へ escape 列を流して「PTY を読む速さ」を測る。
作者自身が「これは PTY を読む速さだけを測るもので、フレーム率も遅延も含まない」と断っている。

termit で走らせるには窓の中でコマンドを打つ必要があり、この環境では窓を操作できない。
そこで `benchmarks/` にある負荷（`light_cells`、`dense_cells`、`scrolling`、
`scrolling_*_region`、`cursor_motion`、`unicode`、`sync_*`）に倣った列を自前で作り、
窓を開かずに実際の読み取り経路へ流すようにした（`termit --throughput`）。
他の端末との比較はできないが、変更の前後は比べられる。

## 参考

- [alacritty/vtebench](https://github.com/alacritty/vtebench)
- [Ghostty: Let's talk about performance](https://github.com/ghostty-org/ghostty/discussions/4837)
- [Ghostty Rendering System](https://deepwiki.com/ghostty-org/ghostty/5-rendering-system)
- [Alacritty PR #5773（損傷追跡）](https://github.com/alacritty/alacritty/pull/5773)
- [Alacritty Rendering Pipeline](https://deepwiki.com/alacritty/alacritty/3.4-rendering-pipeline)
- [foot Performance](https://codeberg.org/dnkl/foot/wiki/Performance)
- [kitty performance](https://sw.kovidgoyal.net/kitty/performance/)
- [glyphon](https://github.com/grovesNL/glyphon) — `prepare` で CPU 側の整形と焼き付けを済ませる構造
- [wgpu SurfaceConfiguration](https://docs.rs/wgpu-types/latest/wgpu_types/struct.SurfaceConfiguration.html)
- [How to schedule CAMetalLayer rendering for lowest CPU to Display latency?](https://developer.apple.com/forums/thread/711033)

# termit の応答速度を調べた記録

作成日：2026-09-09
対象：`ec5f0b8` 以降の実装、macOS（Apple Silicon）、release ビルド

## 1. 結論

「反応が遅い」という感触に対して工程ごとに測った結果、**端末自身の取り分は合計 1ms 前後**であり、
遅さの原因は描画でも VT 解釈でもなく、**描いたフレームが画面に出るまでの待ち**にあった。

| 工程 | 実測値 |
|---|---|
| 打鍵から画面内容が変わるまで（PTY の往復とグリッド反映） | 中央 **0.073ms** |
| 画面の更新を読み取ってから `present` を呼び終えるまで | 中央 **0.5〜1.3ms** |
| `present` から実際に表示されるまで | 直接は測れない。設定次第で **16〜50ms** |

最後の 1 行が支配的だった。
`desired_maximum_frame_latency` を 2 から 1 へ下げたのが、この記録で行った唯一の実質的な変更である。

## 2. 測り方

いずれも本体に組み込んであり、再現できる。

### 2.1 描画そのものの費用

```
termit --bench
```

ウィンドウを開かずに、実際の描画関数へ画面いっぱいの文字を流して測る。
毎フレーム内容を変えるので、字形の使い回しだけで速く見えることはない。

### 2.2 入力の往復

```
termit --latency-test
```

`/bin/cat` を PTY で起動し、1 文字書いてからそれがグリッドに現れるまでを測る。
経路は「書き込みスレッド → PTY → 行規律の echo → 読み取りスレッド → VT パーサ → グリッド」で、
利用者が打鍵してから文字が出るまでの、端末側の取り分にあたる。
キーボードを使わないので、画面操作の権限がない環境でも測れる。

### 2.3 実際の動作中の内訳

```
RUST_LOG=info TERMIT_FRAME_LOG=1 tex
```

1 秒ごとに次を出す。

- 読み取りスレッド：読んだ回数とバイト数、再描画の通知を送った回数と抑制した回数
- 主スレッド：通知の受信数、再描画要求数、実際に描いた数、読み取りから表示までの中央値と p90
- 描画：`prepare`、`acquire`、`encode`、`present` の内訳とセル数

## 3. 実測値

### 3.1 描画（`--bench`）

| 画面 | セル数 | prepare 中央 | submit 中央 | 合計 |
|---|---|---|---|---|
| 1280x800（163x47） | 7,661（全面） | 0.64ms | 0.46ms | 1.11ms |
| 1280x800 | 752（1 割） | 0.08ms | 0.31ms | 0.39ms |
| 1920x1200（245x70） | 17,150（全面） | 1.43ms | 0.93ms | 2.37ms |
| 1920x1200 | 1,680（1 割） | 0.17ms | 0.44ms | 0.61ms |

60fps の予算は 16.7ms である。
最も重い「1920x1200 の全面書き換え」でも 2.4ms で、予算の 15% しか使っていない。

**1 文字ごとに `TextArea` を積む方式は、遅さの原因ではなかった。**
実装前に疑っていたが、測ったところ費用は小さい。

### 3.2 入力の往復（`--latency-test`）

```
標本 60 件
中央 0.073ms  p90 0.112ms  p99 0.142ms  最大 0.177ms
```

### 3.3 動作中（1 行 65 バイトを 15ms 間隔で出す）

```
読み 42 回 2688 B, wakeup 送信 42, 抑制 0
wakeup=42 再描画要求=42 実描画=42 | 読み取り→表示 中央 0.8ms p90 1.1ms
prepare=0.45ms acquire=0.05ms encode=0.18ms present=0.02ms 合計=0.70ms
```

読み取り 42 回に対して描画 42 回。取りこぼしも溜め込みもない。

### 3.4 大量出力（15.6MB を `cat`）

```
読み取り 15,475 回 15,800,390 B（約 1 秒で消化）
wakeup=17 再描画要求=19 実描画=19 | 読み取り→表示 中央 16.2ms p90 16.7ms
prepare=0.82ms acquire=9.18ms encode=0.29ms present=0.04ms 合計=10.34ms
```

15.6MB を約 1 秒で消化し、そのあいだ 19 フレームを描いた。
1 回の読み取りごとに描くのではなく、描いていないあいだの更新はまとめている。
`acquire` が 9ms へ伸びるのは、走査の間隔を待っているためで、これは想定どおりである。

## 4. どこに時間が消えているか

打鍵から文字が見えるまでを分解すると次のようになる。

```
打鍵
 └ 端末がキーを PTY へ書く ─────────────┐
 └ シェルが echo して返す              │ 合計 0.073ms（実測）
 └ VT パーサがグリッドへ反映 ───────────┘
 └ 読み取りスレッドが再描画を通知 ───────┐
 └ 主スレッドが描いて present を呼ぶ    │ 合計 0.5〜1.3ms（実測）
 ───────────────────────────────────────┘
 └ present したフレームが表示される ──── 16〜50ms（設定次第、直接は測れない）
```

最後の段だけが二桁 ms である。
ここは端末の作りではなく、**表示装置と合成器の都合**で決まる。

Apple の開発者フォーラムによれば、`CAMetalLayer` の `maximumDrawableCount` が 3 のとき、
CPU から表示までの遅れは **50ms、30ms、16ms のあいだで揺れる**。
2 にすると、描き終えたフレームは次の走査で表示されることがほぼ保証される。

wgpu の `desired_maximum_frame_latency` はこの値に対応する。
termit は既定の 2 を使っていた。**1 へ下げた。**

## 5. 打った手

**`desired_maximum_frame_latency` を 2 から 1 にした。**
投入したフレームが次の走査で出るようになる。この記録で最も効く変更である。

**表示方式を環境に応じて選ぶようにした。**
Mailbox が使えれば選ぶ。ただし手元の Mac では `[Fifo, Immediate]` しか報告されず、Fifo のままだった。

**`window.vsync` を設定に足した。**
`false` にすると Immediate になり、走査を待たずに表示する。
待ちは消えるが、書き換えの途中が見えることがある。
Ghostty は macOS でも Linux でも既定で走査に合わせておらず、TUI では裂けが見えると明言している。
kitty も `sync_to_monitor no` を、遅延を詰めたい利用者向けの設定として挙げている。

**隠れているときに空回りする不具合を直した。**
ウィンドウが他の窓に隠れると `get_current_texture` が `Occluded` を返す。
これまではその場で再描画を要求し直しており、隠れているあいだ要求と失敗を際限なく繰り返していた。
次の出来事まで待つように変えた。

**設定と履歴の置き場所を直した。**
macOS の `dirs::config_dir()` は `~/.config` ではなく `~/Library/Application Support` を返す。
README には `~/.config/termit/config.toml` と書いてあり、実装とずれていた。
XDG の作法（`$XDG_CONFIG_HOME` または `~/.config`）に合わせた。
この記録の調査中は、設定が読まれないまま測っていて、原因を取り違えかけた。

## 6. 打てるが打っていない手

**損傷追跡（damage tracking）**
alacritty も foot も、変わった行だけを描き直す仕組みを持つ。
`alacritty_terminal` は `TermDamage` を提供しており、termit はこれを使っていない。
ただし 3.1 節のとおり全面書き換えでも 2.4ms なので、いま入れても体感は変わらない。
入れる価値が出るのは、4K や 6K で全面を毎フレーム描くようになったときである。

**行単位のまとめ描き**
alacritty は 1 回の描画命令で最大 65,536 個の字形を送る。
termit は 1 セルにつき 1 個の `TextArea` を積んでおり、命令の数では大きく劣る。
それでも 2.4ms で収まっているため、いま作り替える理由はない。

**`presentsWithTransaction`**
Apple のフォーラムでは、遅延を詰める設定として `maximumDrawableCount = 2` と併せて挙げられている。
wgpu からは触れない。触るには Metal の層を直接持つ必要があり、
「VT 解釈もウィンドウ管理も既存のものに委ねる」という方針から外れる。

**キー入力のたびに描いている点**
いま、キーを押すと echo が返る前に一度描いている。
内容は変わっていないので無駄である。ただし費用は 0.5ms 程度で、遅さの原因ではない。

## 7. この調査で分かった、測り方の落とし穴

**設定が読まれていないことに気付かず、3 回測り直した。**
出力を出すためにシェルを差し替える設定を `~/.config/termit/` へ置いたが、
実装は `~/Library/Application Support/tex/` を見ていた。
その結果「11 秒で 1 フレームしか描かない」という数字が出て、深刻な不具合に見えた。
実際には、何も出力していないシェルを正しく待っていただけだった。

**画面キャプチャの権限がない環境では、GUI の状態を目で確かめられない。**
オフスクリーンに 1 フレーム描いて PNG に落とす `--probe` を先に用意してあったため、
描画そのものは確認できた。計測でも同じ方針が効いた。

## 8. 参考にした資料

- [kitty performance](https://sw.kovidgoyal.net/kitty/performance/)：`repaint_delay`、`input_delay`、`sync_to_monitor` の意味と、遅延を詰める設定の組み合わせ
- [foot Performance](https://codeberg.org/dnkl/foot/wiki/Performance)：VT パーサの速さと、変わったセルだけを描く方針
- [Alacritty Rendering Pipeline](https://deepwiki.com/alacritty/alacritty/3.4-rendering-pipeline)：1 命令あたり 65,536 個のまとめ描き、1024x1024 の字形置き場、損傷追跡
- [Alacritty PR #5773](https://github.com/alacritty/alacritty/pull/5773)：損傷追跡の導入と、打鍵の遅延が目に見えて改善したという報告
- [wgpu SurfaceConfiguration](https://docs.rs/wgpu-types/latest/wgpu_types/struct.SurfaceConfiguration.html)：`desired_maximum_frame_latency` の意味と、1 にすると遅延が最小になること
- [wgpu PresentMode](https://docs.rs/wgpu/latest/wgpu/enum.PresentMode.html)：Fifo は約 3 フレームの待ち行列、Mailbox と Immediate は低遅延
- [Apple Developer Forums: How to schedule CAMetalLayer rendering for lowest CPU to Display latency?](https://developer.apple.com/forums/thread/711033)：`maximumDrawableCount` が 3 だと 50/30/16ms で揺れ、2 なら次の走査で出る
- [Ghostty: Let's talk about performance](https://github.com/ghostty-org/ghostty/discussions/4837)：既定で走査に合わせない方針
- [Ghostty Rendering System](https://deepwiki.com/ghostty-org/ghostty/5-rendering-system)：描画専用スレッド、6K でも 1ms 未満
- [glyphon](https://github.com/grovesNL/glyphon)：`prepare` で CPU 側の整形と字形の焼き付けを済ませ、`render` は描画命令だけを出す構造
- [Measuring terminal latency](https://www.lkhrs.com/blog/terminal-latency/)：Typometer による測り方と、macOS での各端末の比較

## 9. 現在の数値

| 項目 | 目標 | 実測 |
|---|---|---|
| 入力の往復 | — | 0.073ms |
| 読み取りから present まで | — | 0.5〜1.3ms |
| 80x24 の全画面書き換え | 16ms 以内 | 1.1ms |
| 常駐メモリ（1 ペイン） | 100MB 以内 | 84.7MB |
| 起動から子プロセスの生成まで | 200ms 以内 | 224ms |
| バイナリ | 単一ファイル | 9.4MB |

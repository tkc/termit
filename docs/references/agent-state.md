# エージェントの状態をどう知るか

作成日：2026-09-15

「どのセッションが私の返事を待っているか」を左ペインで分かるようにした。
先行実装（[herdr](https://github.com/herdrdev/herdr)）を読み、
取るものと取らないものを決めた記録。

## 何が足りなかったか

termit の印は「直近 500ms に出力があったか」だけだった。これだと

- **考えているあいだ出力が止まるエージェント**が、止まって見える。
- **承認待ちで止まっているセッション**と、**終わって暇なセッション**が同じ見た目になる。

後者が実害で、6 本並べていると「どれが私待ちか」を目で探すことになる。

## herdr のやり方

| 手法 | 中身 |
|---|---|
| 状態は 4 つ | `working` / `blocked`（承認・質問待ち）/ `done`（終わったが未読）/ `idle` |
| 判定は**画面の読み取り** | エージェントごとの規則を TOML で持つ（`distribution/agent-detection/*.toml`、24 種） |
| 規則は**版付きで外に置く** | `claude.toml` は `version = "2026.09.11.1"`。相手の UI が変わっても、バイナリを出し直さず規則だけ配れる |
| 最優先の手がかりは **OSC タイトル** | 点字の回る絵（`⠋`）や半円（`◐`）で始まっていれば `working`（優先度 1100） |
| 可能なら画面を読まない | `herdr integration install claude` がフックを入れ、状態やセッション ID をソケットへ報告させる |

規則の形はこうなっている（`claude.toml` 抜粋）。

```toml
[[rules]]
id = "live_blocked_form"
state = "blocked"
region = "after_last_horizontal_rule"   # 最後の水平線より下
contains = ["esc to cancel"]
any = [{ contains = ["enter to confirm"] }, …]
```

`region`（`osc_title` / `bottom_non_empty_lines(12)` / `prompt_box_body` …）×
`priority` × `regex`/`contains`/`any`/`all`/`not` の小さな DSL である。

## termit が取ったもの

**1. 題名を手がかりにする。** termit は OSC 0/2 をすでに解釈している。
題名の 1 文字目が設定の文字に含まれていれば、出力が途切れていても
「動いている」とする（`Session::title_signal`）。画面を読む必要がない。

**2. 返事待ちの語を画面の末尾から探す。** 出力が止まっているセッションだけ、
画面（履歴ではなく今の枠）の末尾 12 行を文字にして、設定の語を探す
（`Session::blocked_in` / `session::screen_tail`）。当たれば印を黄色にする。

**3. 規則をコードの外に置く。** これが herdr から取った一番のものである。

```toml
[agent]
working_title = "⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏◐◑◒◓"
blocked_when  = ["do you want to proceed?", "esc to cancel", …]
blocked_lines = 12
```

**termit の実行ファイルの中に、エージェントの名前も見た目も無い。**
あるのは「題名の先頭文字を照合する」「語を探す」という一般の手続きだけで、
何を探すかは設定にある。相手の UI が変わったら設定を直す。
これなら「端末の責務を逸脱しない」を保ったまま、`blocked` が出せる。

## termit が取らなかったもの

**エージェントごとの規則束。** 24 種ぶんの正規表現を保守するのは、
端末の仕事ではないし、人手も無い。既定値は 4 語だけにした。
足りなければ利用者が足せる。

**画面の領域を指す DSL。** herdr の `region`（プロンプト枠の中、最後の水平線より下）は
精度を上げるが、エージェントの画面構造を知るということでもある。
termit は「末尾 N 行」しか見ない。誤検知は起きうるが、
その代償として termit 側にエージェントの知識が残らない。

**フックによる報告。** 相手の設定ディレクトリに書き込む必要があり、
コンテナ越しでは届かない。termit のプロファイルは docker 経由で動くので、
**PTY の中身と題名だけで判定する**ほうがどちらの起動経路でも同じに効く。

**常駐サーバ。** herdr が `done`（終わったが未読）を持てるのは、
サーバが「見たかどうか」を覚えているからである。termit は窓を閉じれば終わりで、
その区別を持つ土台が無い。`blocked` と `working` の 2 つに絞った。

## 費用

印のために画面を読むので、費用を測れる形にした。

- 読むのは**出力が止まっているセッションだけ**（動いていれば読まない）。
- 読むのは**末尾 12 行**だけ。
- **250ms に 1 回**まで（`AGENT_STATE_INTERVAL`）。

最悪でも 6 セッション × 12 行 × 163 桁 ≒ 12KB を 250ms ごと。
`--bench` の組み立て（全面 7,661 コマで 0.02ms）と比べて無視できる。

## 参考

- [herdrdev/herdr](https://github.com/herdrdev/herdr) — `distribution/agent-detection/claude.toml`、`docs/concepts/`、`docs/integrations/`
- [herdr.dev](https://herdr.dev/)

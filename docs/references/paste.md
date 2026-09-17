# 貼り付け口で何をするか

作成日：2026-09-17

「クラウドの認証情報をエージェントに貼ってしまう」事故を防ぎたい、という求めに対し、
貼り付け口を一番作り込んでいる iTerm2 を読んだ記録。

## iTerm2 の作り

`sources/Pasting/iTermPasteHelper.m` の `sanitizePasteEvent:` が中心にある。
**変換を一本の関数に順番に並べ、旗（`iTermPasteFlags`）で選ぶ**形である。

| 順 | 変換 | 旗 |
|---|---|---|
| 1 | 改行を消す／`\r` にそろえる | `RemovingNewlines` / `SanitizingNewlines` |
| 2 | Unicode の約物を ASCII に寄せる | `ConvertUnicodePunctuation` |
| 3 | 危険な制御コードを落とす | `RemovingUnsafeControlCodes` |
| 4 | タブを空白か `^V` に | `tabTransform` |
| 5 | シェルに解釈される字を退避 | `EscapeSpecialCharacters` |
| 6 | **正規表現で置換** | `UseRegexSubstitution` |
| 7 | base64 に包む | `Base64Encode` |

`PasteEvent` は `originalString`（元）と `string`（変換後）を別々に持つ。

読み取れることが 3 つある。

**端末が貼り付けを書き換えるのは、例外ではなく普通である。** 制御コードを落とすのも
改行をそろえるのも、貼り付け攻撃と事故を防ぐためで、ブラケット貼り付け（`?2004`）が
存在する理由と同じである。認証情報を伏せるのは、この並びに 1 つ足すことにあたる。

**秘密の検出規則は 1 つも積んでいない。** 代わりに `regex` と `substitution` という
**道具だけ**を出し、中身は利用者が Advanced Paste やキー割り当てで与える。

**Advanced Paste は ⌥⌘V である**（⌘⇧V ではない）。

## termit が取ったもの

**正規表現の置換。** 自前の照合器（名前つき／語頭の 2 種類）を考えていたが、やめた。
`regex 1.13.1` は `env_logger` 経由で既に依存木にあり（`cargo tree -i regex`）、
直接使っても組み立ての費用が増えない。Rust の `regex` は後戻りしない実装なので、
利用者の書いた式で端末が固まることがない（PCRE と違い ReDoS が原理的に無い）。
概念が 1 つで済み、gitleaks などの式をそのまま持ち込める。

**⌥⌘V。** 逃げ道（伏せずに貼る）をここに置いた。当初は ⌘⇧V を考えていたが、
このコードには「Shift の同時押しが届かない環境があるため ⌘⇧R の代わりに ⌘I も受ける」
という前例がある。⌥⌘V なら Shift を使わないので、その心配ごと消える。

**一本の変換の並び。** ⌘V は `伏せる → ブラケットに包む → PTY` になった。

## termit が変えたもの

**伏せる範囲は `secret` という名前の捕獲組で指す。** iTerm2 は「式＋置換文字列」の組で、
`$1` などを使って書く。termit は式の中で `(?P<secret>…)` と印を付け、
**その部分だけ**を `[redacted]` に替える。名前や引用符が自動的に残るので、
置換文字列を書き間違えて文脈ごと消す、ということが起きない。

```toml
redact = ['(?i)"(private_key)"\s*:\s*"(?P<secret>[^"]+)"']
```

**既定の式を持つ。** iTerm2 は道具だけを配るが、termit はクラウドの認証情報に絞った
5 つの式を既定で持つ。道具だけ配っても、書く人がいなければ誰も守られない。
ただし式は設定側にあり、実行ファイルの中に「AWS の鍵の形」は無い。

## 取らなかったもの

**⌘C 側とプログラムからの書き込み（OSC 52）。** 利用者の貼り付けだけにした。
守りたいのが「エージェントに食わせない」ことだからで、⌘C を書き換えると
「画面の鍵を選んで写して使う」という正当な用途が黙って壊れる。

**Advanced Paste のような対話窓。** 変換の一覧・試し表示・履歴は、端末の仕事を超える。

## Claude Code の検出（2026-09-17 追記）

「Claude Code には認証情報を検知する仕組みがある」という話を追い、
手元の実物（`~/.local/share/claude/versions/2.1.273`）の中を見た。

**59 個の正規表現の表**を持っており、各項目は
`{id, source, flags, confidence}` の形をしている。
**エントロピーによる検出は無い**（`entropy` の文字列は Node の内部と、
`maskDuplicates` の説明文「long, high-entropy secrets 向け」にしか出てこない）。

確信度は 2 段である。

| 段 | 数 | 中身 |
|---|---|---|
| `high` | 約 50 | 形で断定できるもの。`AKIA`/`ASIA`、`ghp_`、`glpat-`、`xoxb-`、`sk-ant-…AA`、`AIza`、`GOCSPX-`、`dop_v1_`、`SG.`、`npm_`、`shpat_`、`sntryu_` ほか |
| `low` | 7 | 文脈で当てるもの。`sensitive-assign`（名前 = 値）、`cloud-env-var`、`http-auth-scheme`、`loose-jwt` ほか |

**捕獲組の 1 番が「伏せる部分」**である（設定の `extract` も同じ約束）。
PEM の秘密鍵だけは表の外で別に見ている。

### 段を「データの向き」で使い分ける

同じ表から、用途ごとに違う段を引く。ここが一番学ぶところだった。

```js
scan(e)             → high だけ。置換せず「何が見つかったか」を返す
redactTokens(e)     → high だけ
redactContext(e)    → low だけ
redact(e)           → redactTokens(redactContext(e))   // low を先、high を後
redactForDisplay(e) → high だけ
```

**出ていくものは両方使って過剰に伏せ、画面に出すものは high だけにする。**
表示で誤爆すると、利用者が読んでいる文章のほうが壊れるからである。
値の式には `\[REDACTED\]` 自体も含まれていて、二度通しても壊れない。
512 文字以下は結果を覚える（上限 512 件）。

### どこに効いているか

公式の記述では、効いているのは **Anthropic 宛の telemetry と feedback だけ**である。

> Known API key and token patterns are redacted before upload.
> **Source code, file contents, and other conversation content are uploaded as-is.**

つまり**モデルへ送る経路には掛かっていない**。そこを守るのは利用者が宣言する
2 つ（権限の拒否規則と、サンドボックスの `mask`）である。
termit が貼り付け口で伏せるのは、ちょうどこの空いている場所にあたる。

### 取ったもの

- **クラウドの環境変数**（`cloud-env-var`）。ただし**そのままは写さない**。
  あちらは `AWS_|GOOGLE_|AZURE_` で始まる変数の値をすべて伏せるが、
  telemetry 用だから伏せすぎても誰も困らない。こちらはエージェントに読ませる経路なので、
  `AWS_REGION=us-east-1` や `AWS_PROFILE=default` まで消すと質問が成り立たない。
  **名前に SECRET/KEY/TOKEN/PASSWORD/CREDENTIAL があるものだけ**に絞った。
- **PEM の塊**。あちらは検出専用だが、こちらは伏せる。
  終わりの印を必ず求める（「`-----BEGIN PRIVATE KEY-----` という行で始まります」と
  書いてある文章を巻き込まないため）。
- **`GOCSPX-`** と `A3T[A-Z0-9]`、AWS の base32 の字種 `[A-Z2-7]`。

### 取らなかったもの

**`low` の段そのもの。** `password|token|cookie|authorization` まで伏せる
`sensitive-assign` は、貼り付けたコードのほうを壊す。
termit の貼り付けは `redactForDisplay` と同じく「人とエージェントが読んで意味を取る」
経路なので、high 寄りに留めるのが正しい。

## 限界（利用者に伝えるべきこと）

**裸で貼った AWS のシークレットキーは捕まらない。** 40 文字の英数字に目印が無く、
これを捕まえる式はパスワード・ハッシュ・base64・git の SHA を軒並み巻き込む。
gitleaks などの既存ツールも同じ理由で文脈（名前）に頼っている。
捕まるのは「名前とセットのとき」と「`AKIA` などの目印があるとき」である。

**`password` や `token` は既定に入れない。** エージェントに貼るコードの変数名に当たり、
貼った内容のほうが壊れる。守るために貼り付けを壊しては、機能を切られて終わる。

## 参考

- [gnachman/iTerm2](https://github.com/gnachman/iTerm2) — `sources/Pasting/iTermPasteHelper.m`、`sources/Pasting/PasteEvent.h`
- [gitleaks](https://github.com/gitleaks/gitleaks) — クラウドの認証情報の式の書き方

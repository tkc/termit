# tex

エージェント向けの軽量ターミナル。

左ペインにセッションの系統樹を出し、キー一つで会話を分岐させ、必要なら Docker
コンテナへ閉じ込める。それ以外の機能は持たない。

設計の詳細は [仕様書](docs/superpowers/specs/2026-09-08-agent-terminal-design.md) にある。

## ビルド

必要なものは Rust の安定版ツールチェーンだけである。

```
cargo build --release
```

生成物は `target/release/tex` の単一バイナリになる。

## 使い方

| キー | 動作 |
|---|---|
| `Ctrl+Shift+N` | 新規セッション |
| `Ctrl+Shift+F` | 選択中セッションを分岐 |
| `Ctrl+Shift+S` | プロファイルを選んで分岐（`j`/`k` か数字で選び Enter） |
| `Ctrl+Shift+J` / `K` | 左ペインの選択を下 / 上へ |
| `Ctrl+Shift+W` | セッションを終了（停止済みなら一覧から外す） |
| `Ctrl+R` | コマンド履歴を検索 |
| `Ctrl+B` | 左ペインの表示を切り替え |
| `Ctrl+Shift+C` / `V` | コピー / 貼り付け |
| `Ctrl+Shift+=` / `-` | フォントサイズ |
| `Shift+PageUp` / `PageDown` | スクロール |

これ以外のキーはすべて子プロセスへ渡す。
キーの照合には修飾を外したキーを使うので、Ctrl を押した時点で論理キーが
制御文字になる環境でも組み合わせが届く。

## シェル統合

コマンド履歴はシェルの `HISTFILE` ではなく、端末が OSC 133 から組み立てる。
zsh 用の設定断片を出力できる。

```
tex --shell-integration >> ~/.zshrc
```

読み込むと、コマンド、作業ディレクトリ、終了コード、所要時間が
`~/.local/share/tex/history.db` に記録される。
設定しない場合、端末としては動くが履歴は残らない。

## 設定

`~/.config/tex/config.toml` を起動時に一度だけ読む。
ファイルがなければ既定値で動く。

```toml
[window]
font       = "Menlo"      # 等幅フォント。見つからなければ総称の等幅へ落とす
font_size  = 13.0
scrollback = 10000
sidebar_cols = 28

[shell]
program = "/bin/zsh"
args    = ["-l"]

# エージェントの起動コマンド。書かなければシェルが起動する。
# 端末は会話の中身を知らない。エージェント固有の知識はここに閉じる。
[agent]
new  = "claude --session-id {new_id}"
fork = "claude --resume {parent_agent_id} --fork-session --session-id {new_id}"

# ホスト実行を表す予約名。image は書けない。
[profile.host]

# Docker のコンテナ内で起動するプロファイル。
[profile.sandbox]
image   = "tex-agent:latest"   # claude を入れたイメージ
workdir = "/work"
mount   = ["{cwd}:/work"]
network = "bridge"
env     = ["ANTHROPIC_API_KEY"]
args    = ["--dangerously-skip-permissions"]
```

テンプレートで使える変数は `{new_id}`、`{parent_agent_id}`、`{cwd}`、
`{parent_title}` の四つに限る。
`{parent_agent_id}` の値がまだ分かっていないセッションから分岐した場合は、
親と同じコマンドを同じ作業ディレクトリで起動し、会話は引き継がない。
そのセッションは左ペインで名前の後ろに `*` が付く。

### サンドボックス

隔離の根拠はネットワークの遮断ではなく、マウント範囲とプロセス名前空間にある。
コンテナから見えるファイルは `mount` に書いた範囲だけであり、
ホストの他のディレクトリにもホストのプロセスにも届かない。
`network` の既定を `bridge` にしているのは、モデル API への接続が切れると
エージェントが動かないためである。
外部へ出る必要のないプロセスには `network = "none"` を明示する。

## 作らないもの

タブ、任意の分割とタイル管理、合字、画像表示プロトコル、プラグイン機構、
内蔵エディタ、テーマの配布機構、設定画面の GUI、内蔵 SSH クライアント、
セッションの永続化。

## 開発

```
cargo test              # 単体テストと PTY の結合テスト
cargo run -- --probe out.png   # ウィンドウを開かずに 1 フレームを描いて書き出す
```

`--probe` は実際の描画関数を通してオフスクリーンに 1 フレームを描き、PNG にする。
画面キャプチャの権限がない環境でも、割り付けと字形の配置を目で確かめられる。

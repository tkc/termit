# サンドボックスとエージェント

作成日：2026-09-10

エージェントを隔離して走らせる方法を調べ、Apple の `container` を実際に試した記録。
termit が何を担い、何を担わないかの線引きもここに置く。

## 1. 隔離の段階

Anthropic 自身が 6 段階で整理している（[Choose a sandbox environment](https://code.claude.com/docs/en/sandbox-environments)）。

| 手法 | 何が隔離されるか | Docker | 手間 |
|---|---|---|---|
| Bash サンドボックス（`/sandbox`） | Bash とその子だけ | 不要 | macOS はほぼゼロ |
| sandbox runtime | Claude Code のプロセス全体（ファイル操作・MCP・hook も） | 不要 | 小 |
| dev container | 開発環境まるごと | 必要 | 中 |
| 任意のコンテナ | 同上（自前のポリシー） | 必要 | 中〜大 |
| 仮想機械 / microVM | OS まるごと（自前カーネル） | 不要 | 大 |
| Claude Code on the web | Anthropic 管理の VM | 不要 | なし |

実装は OS の機能そのもので、**macOS は Seatbelt（`sandbox-exec`）、Linux は bubblewrap + seccomp**。
Codex CLI も同じ（Seatbelt / Landlock + seccomp）。
ネットワークはいずれも**ホスト側プロキシの許可リスト**で止める。

重要な区別が 2 つある。

- **権限モード**（いつ聞くか）と**隔離境界**（できたとして何に届くか）は別物である。
  Anthropic は「`--dangerously-skip-permissions` を使うならコンテナか VM か
  sandbox runtime の中で」と明記している。
- **Bash サンドボックス単体では無人実行に足りない。**
  MCP サーバと hook はホストで素のまま動く。

ファイル側の隔離は **worktree per task**（タスクごとに git worktree）が定番になっている。
`sbx` はこれを内蔵していて、`--branch auto` で `.sbx/` の下に worktree を作る。

## 2. `sbx`（Docker Sandboxes）

コンテナではなく **microVM 1 個 / セッション 1 個**。

- 独立カーネル、独立した docker daemon、workspace だけ passthrough マウント、
  外向き通信はすべてホスト側プロキシ経由。
- **認証情報はホストに残す。** `sbx secret set` で OS キーチェーンに入れ、
  プロキシが送信時に注入する。環境変数で中に渡さない。
- ネットワークは Open / Balanced / Locked Down の 3 段階。
- `brew install docker/tap/sbx`（macOS 14 以降 / Apple silicon / Docker Desktop 不要）。
- 利用報告では**性能が最大の難点**、コミット署名（ssh-agent）を中に渡せない、
  メモリ既定はホストの 50%。

引数体系が `sbx run <agent>` で docker と違うため、termit の `runner` では扱えない
（4 節）。

## 3. Apple の `container` を試した（2026-09-10、1.4.1）

`brew install container` → `container system start` →
`container system kernel set --recommended`（kata 3.32.0）。

| 試したこと | 結果 |
|---|---|
| 初回実行（イメージ取得込み） | 15.1s |
| 2 回目以降の起動 | **0.74s**（VM 1 個ぶん込み） |
| 対話シェルの往復（pty 経由、プロンプトまで） | **1.02s**。打鍵も出力も正常 |
| `-v {cwd}:/work -w /work` | 読み書きとも可 |
| `-e TERM` | ホストの値が入る（既定は `xterm`） |
| `--network` | ネットワーク**名**を取る。既定 `default` は 192.168.64.0/24、コンテナごとに IP が 1 個 |
| 常駐（コンテナ 0 個） | apiserver 24MB + core-images 22MB ≒ 46MB |
| コンテナ 1 個あたり | ホスト側 `container-runtime-linux` が約 21MB、ゲストは既定 **4 CPU / 1024MB** |

`-v` `-w` `-e` `--network` `-it` `--rm` の綴りが docker と同じなので、
termit が組み立てる引数列は先頭を替えるだけで通る。

### 起動直後の 1 秒、端末の大きさが 0x0 になる

外側の pty には exec の前に 40x120 を入れてある（親が後から入れる競走は避けた）。
それでもゲスト側は：

```
t1: 0 0        ← コンテナ起動直後
t2: 40 120     ← 1 秒後に正しい値が届く
…
（ホスト側で 50x200 に変えて SIGWINCH）
t6: 50 200     ← リサイズは正しく伝わる
```

**欠けているのは起動時の初期値だけ**で、リサイズの経路は動いている。
`tput cols` はこのあいだ terminfo の 80 に落ちる。
起動時に一度だけ大きさを読む全画面 UI は、80 桁で描き始めることになる。

termit 側で塞ごうとすると厄介である。同じ大きさで `TIOCSWINSZ` を呼んでも
SIGWINCH は出ない（大きさが変わったときだけ送られる）ので、
一度違う値にしてから戻す必要があり、画面がちらつく。
**設定で避けられる**ので、そちらを案内する。

```toml
# エージェントの起動を少し遅らせる。1 秒の窓を外すだけでよい。
[agent]
new = "sh -c 'sleep 1.5; exec claude --session-id {new_id}'"
```

上流には初期値の件の報告は無い（[#1747](https://github.com/apple/container/issues/1747) は
SIGWINCH 転送のエラー表示の話）。

## 4. termit が担うもの、担わないもの

termit がやるのは**引数列を組み立てて起動するところまで**である。

**入れた（`runner` / `runner_args`）。**
`runner` は既定 `docker`、`runner_args` はイメージ名の直前に入る逃げ道。
Apple の `container` はこれで通り、`--memory` のような道具固有の指定も書ける。
`network` は書かなければ渡さない（名前が道具ごとに違うため）。
termit のコードに道具の名前は一つも増えていない。

**入れない：`sbx` 用の分岐。**
`sbx run <agent>` は引数の並びが違うので、いまの形では扱えない。
対応するなら「起動コマンドの雛形」への一般化が要る。
それは `image`/`mount`/`network` という語彙を捨てることでもあるので、
必要になってから決める。

**入れない：fork のときに git worktree を作る。**
並列エージェントの定番だが、端末が git を理解し始める。
`[agent] fork` の雛形に `git worktree add` を書けば外側で足りる。

**入れない：認証情報の注入。**
`env` で渡すのはホストと同じ強度しかない。
`sbx` のようなプロキシ注入は端末の仕事ではない。README にその旨を書く。

## 参考

- [Choose a sandbox environment](https://code.claude.com/docs/en/sandbox-environments)
- [Configure the sandboxed Bash tool](https://code.claude.com/docs/en/sandboxing)
- [Docker Sandboxes](https://docs.docker.com/ai/sandboxes/) / [sbx のインストール](https://docs.docker.com/ai/sandboxes/install/)
- [Running AI agents safely in a microVM using docker sandbox](https://andrewlock.net/running-ai-agents-safely-in-a-microvm-using-docker-sandbox/)
- [apple/container](https://github.com/apple/container) — [ドキュメント](https://apple.github.io/container/documentation/)
- [Codex CLI サンドボックスの調査](https://agent-safehouse.dev/docs/agent-investigations/codex)
- [AI エージェント用サンドボックスの比較（E2B / Modal / Daytona ほか）](https://blog.logrocket.com/comparing-ai-agent-sandbox-platforms-e2b-modal-daytona-and-more/)

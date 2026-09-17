# issue の貯め方と消化のしかた

作成日：2026-09-17

思いついたことを昼のあいだ issue に貯め、夕方以降にまとめて片づける。
その形にした理由と、実際の手順。

## なぜ貯めるのか

**手を止める費用のほうが高い。** 端末を使っている最中に「⌘W が効かない」と気づいて
そのまま直しはじめると、元の作業の文脈が失われる。気づきは 10 秒で捨てられる場所に
置き、直すのはまとめてやるほうが、一日の総量が増える。

**夕方に寄せる理由が 2 つある。** 利用の上限は時間で回復するので、日中に使い切っても
夕方には戻っている。そして Claude Code は、**セッションを開いたままにしておけば
回復した時点で自分から作業を拾い直す**（`autoContinueAtUsageLimit`、既定で有効）。
待っているあいだに終了・再起動すると、その予約は消える。

**issue に置く理由。** 作業ツリーから独立している。ブランチを切り替えても、
機械を変えても、一覧はそこにある。手元の TODO ファイルだと、
複数のエージェントが同じ行を書き換えて壊す。これは並行作業での破損の最大要因である。

## 昼：捕まえるだけ

**1 行でよい。** 分類も再現手順も要らない。

```sh
gh issue create --title "⌘W でセッションが閉じない"
```

型（`.github/ISSUE_TEMPLATE/`）はわざと軽くしてある。空の issue も許してある。
**捕まえる手間が重いと、昼のあいだに捕まらず、そのまま消える。**

手がかりがその場にあるなら貼る。あとで再現するより、そのときの 1 行が速い。

```sh
# 描画・読み取りの記録
RUST_LOG=info TERMIT_FRAME_LOG=1 termit
# 押したキーと、その解釈
TERMIT_KEYLOG=/tmp/keys.log termit
```

## 夕方その 1：仕分ける

消化の前に、**全部まとめて 15 分**で仕分ける。1 件ずつ着手しながら考えない。

```sh
gh issue list --state open
```

各件について 3 つだけ決める。

1. **そのまま着手できるか。** できないなら、足りないのは何か。
   多くは「測っていない」である。ここで測る。**原因の見当をコードから立てない** ——
   このリポジトリでは、それで 2 回外している（`docs/performance.md` 7.9、
   Docker 経由のコピーの件）。
2. **termit の仕事か。** 端末の責務を超えるものは、超えると書いて閉じる。
   閉じた理由は残す。同じ提案がまた来る。
3. **順番。** 直したものが次の土台になる並びにする。

仕分けの結果は issue のコメントに書く。夕方の自分と、明日のエージェントが読む。

## 夕方その 2：消化する

**1 issue = 1 ブランチ = 1 PR。** 1 本の PR に 2 件入れない。片方で CI が落ちると
両方止まる。レビューもできない。

```sh
git checkout -b fix-close-on-cmd-w
# 直す。テストを先に落としてから直す。
cargo test && cargo clippy --all-targets -- -D warnings && cargo fmt --check
gh pr create --title "..." --body "... Closes #12"
gh pr checks <branch> --watch
gh pr merge <n> --squash --delete-branch
```

本文に `Closes #12` と書くと、マージで issue が閉じる。手で閉じない。

**並行してやるなら worktree を使う。** 同じ作業ディレクトリで 2 本動かすと、
互いのファイルを踏む。

```sh
git worktree add ../termit-issue-12 -b fix-issue-12
```

**止まったら、止まったと書く。** 分かったことを issue に残して次へ行く。
半端な PR を開いたままにしない。

## 自動に任せる場合

`@claude` に投げる形もある。導入は `claude` の中で `/install-github-app`
（`gh` の認証が済んでいること、リポジトリの管理権限が要る）。秘密鍵として
`ANTHROPIC_API_KEY` か `CLAUDE_CODE_OAUTH_TOKEN`（`claude setup-token` で作る）が
リポジトリに入る。以後、issue やコメントで `@claude …` と書けば、Claude が
読んで PR まで作る。

**このリポジトリには workflow を置いていない。** 秘密鍵が要り、走らせれば
その都度 GitHub Actions の時間とトークンを使う。導入するかは持ち主が決めることなので、
手順だけ書いてある。

入れる場合に効く制限（費用が青天井にならないようにする）。

- `claude_args: --max-turns 5`
- workflow に `timeout-minutes`
- `concurrency` で同時実行を絞る
- **型で文脈を先に渡す。** 往復が減るぶん、そのまま費用が減る

**線引き。** 自動に向くのは、正解が機械で確かめられるもの（テストが落ちている、
lint が落ちている、綴りの直し）。向かないのは、**何を作るべきかの判断が要るもの**。
termit では後者が多い。上の「termit の仕事か」を機械に決めさせない。

## 他所から取ったこと

| 取ったこと | 出どころ |
|---|---|
| issue を、作業ツリーから独立した調整面として使う | 複数エージェント運用の通例 |
| 1 つの計画ファイルを複数で編集しない（= 1 issue 1 ブランチ） | 並行作業での破損の最大要因として報告されている |
| `CLAUDE.md` に規範を置き、簡潔に保つ（毎回読まれる） | [GitHub Actions の推奨](https://code.claude.com/docs/en/github-actions#best-practices) |
| 型で文脈を先に渡して往復を減らす | 同上（費用の節に明記されている） |
| 調べる → 決める → 作る → 見直す → 出す | 主要な運用手順に共通する形 |

**取らなかったもの：重い型。** 再現手順・期待結果・環境を必須にする型は、
報告の質は上がるが、**思いつきを捨てる場所としては重すぎる**。
このリポジトリは一人で回しているので、質は夕方の仕分けで上げるほうが合う。

## 参考

- [Claude Code GitHub Actions](https://code.claude.com/docs/en/github-actions)
- [claude-code-action の例](https://github.com/anthropics/claude-code-action/tree/main/examples)
- [Git worktrees と並行エージェント](https://www.developersdigest.tech/blog/git-worktrees-claude-code-parallel-agents-guide)

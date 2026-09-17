# termit

A macOS terminal for working with coding agents. Rust, one binary, no runtime.

## Scope

termit spawns processes, relays the pty, interprets escape sequences and draws
the result. It does not manage conversations, drive an agent, or run workflows.
Anything that needs the terminal to understand what an agent is *doing* belongs
outside it.

Before adding a feature, read `## Why` and `## Not goals` in the README. If the
change does not fit them, say so rather than quietly widening the scope.

## Rules live in config, not in the binary

Nothing in the code knows what Claude Code or Codex look like. Agent state is
matched against `[agent]`, paste redaction against `[paste]`. When an upstream
tool changes its wording, the fix is a config edit, not a release. Keep it that
way: if you are about to hard-code a vendor's key shape or prompt text, put it
in a default table instead.

## Workflow

- Branch, then PR, then wait for CI, then `gh pr merge --squash --delete-branch`.
  **Never commit to `main`.**
- Commit messages and PR bodies in **English**. Code comments and `docs/` in
  **Japanese**.
- One issue, one branch, one PR.

## Measure before you diagnose

Reasoning from the code has produced the wrong cause more than once here; a
measurement has found it every time. Build the measurement first.

| Tool | What it answers |
|---|---|
| `TERMIT_FRAME_LOG=1` with `RUST_LOG=info` | per-frame timings, surface failures by reason, which rows were drawn |
| `--bench` | cost of building and submitting a frame |
| `--throughput` | how fast bytes from the pty are consumed |
| `--latency-test` | input round trip, no keyboard needed |
| `--probe out.png` | render one frame offscreen (screen capture is blocked here) |
| `--keytest` | what a key press actually arrives as |

State what you measured. If you could not measure something, say that instead
of estimating and presenting it as a result.

## Tests

Tests sit next to the thing they cover and are named in Japanese, as sentences
about behaviour. Prefer a pure function over a test that needs a window or a
pty: `secret.rs` and `input.rs` are testable because the decisions were pulled
out of the event loop.

Before claiming a fix works, run `cargo test`, `cargo clippy --all-targets --
-D warnings` and `cargo fmt --check`, and check that the new test fails when
the fix is reverted.

## Docs

`docs/references/*.md` record what other implementations do and which parts
termit took, with the reason for each rejection. Add to them when you research
something rather than leaving it in a PR body.

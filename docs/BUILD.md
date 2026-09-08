# Building termit

## Requirements

- **Rust**, stable. Nothing else — no Node, no Python, no C build step.
  ```sh
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
  ```
- **macOS.** The renderer, the clipboard bridge (`pbcopy` / `pbpaste`) and the
  keyboard handling assume it. Linux is not supported yet.
- Optional: **Docker**, only if you use a sandbox profile.

Everything else comes from crates.io. The notable dependencies are
`alacritty_terminal` for VT parsing and the grid, `winit` for the window,
`wgpu` + `glyphon` for drawing, `portable-pty` for the pty and `rusqlite` for
the history database.

## Build

```sh
cargo build --release
```

The result is a single binary at `target/release/termit`. There is no bundle
step; the binary runs on its own.

For a debug build (much faster to compile, much slower to run):

```sh
cargo build
```

## Run

```sh
./target/release/termit
```

Install it on your `PATH` if you like:

```sh
cargo install --path .
```

## Tests

```sh
cargo test
```

Around a hundred tests. Most are pure unit tests, but a few spawn real
processes:

- a pty running `/bin/echo`, checking the output reaches the grid
- an interactive `/bin/zsh` loading the shipped shell integration, checking a
  command is recorded
- `/bin/sh` setting mouse and focus modes, checking the flags are parsed
- `/bin/zsh` scrolling past a screenful, checking the clear wipes the scrollback

They need a working `/bin/zsh` and the ability to open a pty. Nothing needs a
GPU: the GPU-dependent parts are exercised by `--probe` and `--bench` instead.

## Development commands

The binary carries its own diagnostics so you do not need extra tooling.

```sh
termit --probe out.png      # draw one frame offscreen and write it to a PNG
termit --keytest            # show what each key press arrives as
termit --bench              # measure the cost of drawing
termit --latency-test       # measure the input round trip, no keyboard needed
termit --shell-integration  # print the zsh snippet
```

`--probe` renders one frame through the real drawing code without opening a
window, so you can check layout and glyph placement on a machine where you
cannot take a screenshot. `TERMIT_PROBE_OVERLAY=find|picker|search|preedit`
draws the overlays too.

Runtime breakdown, once per second:

```sh
RUST_LOG=info TERMIT_FRAME_LOG=1 termit
```

It prints how many times the reader thread read, how many redraws were
requested and actually happened, how long it took from a byte arriving to
`present()` returning, and the per-stage cost of drawing.

To record what your keyboard delivers:

```sh
TERMIT_KEYLOG=/tmp/keys.log termit
```

## Lint and format

CI runs these; run them before pushing.

```sh
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
```

## Layout

| Path | What it is |
|---|---|
| `src/main.rs` | window, event loop, layout, drawing the panes |
| `src/render.rs` | wgpu setup, cell grid text, pixel-positioned text |
| `src/rect.rs` | the rounded-rectangle pipeline |
| `src/pty.rs` | pty, reader / writer / waiter threads, OSC 133 assembly |
| `src/term.rs` | the `Term` wrapper and replies to terminal queries |
| `src/session.rs` | the session tree, fork, docker argv |
| `src/osc.rs` | the OSC scanner and the shell integration snippet |
| `src/search.rs` | screen and scrollback search |
| `src/mouse.rs` | mouse reporting to the program in the terminal |
| `src/input.rs` | key encoding and the binding table |
| `src/history.rs` | the SQLite command history |
| `src/config.rs` | config parsing, command templates, docker argv |
| `src/git.rs` | reading the branch from `.git/HEAD` |
| `src/theme.rs` | colors |
| `src/probe.rs`, `src/bench.rs`, `src/latency.rs`, `src/keytest.rs` | the diagnostics above |

## Where files go

| | |
|---|---|
| Config | `$XDG_CONFIG_HOME/termit/config.toml`, else `~/.config/termit/config.toml` |
| History | `$XDG_DATA_HOME/termit/history.db`, else `~/.local/share/termit/history.db` |
| Session list | `$XDG_DATA_HOME/termit/sessions.toml`, else `~/.local/share/termit/sessions.toml` |

Note that these are **not** the macOS `~/Library/Application Support` paths that
`dirs::config_dir()` would give you. People who use a terminal look for their
config in `~/.config`.

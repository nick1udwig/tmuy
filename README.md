# tmuy

`tmuy` is a terminal multiplexer for agents.

Like `tmux`, but stripped down for agent workflows: one named PTY-backed session per task, no panes, no windows, just durable terminals you can attach to, send input to, tail, wait on, and script.

Session data lives in files in `~/.tmuy`.

Sandboxing via [Bubblewrap](https://github.com/containers/bubblewrap) on Linux.

## Why Not tmux?

`tmux` is optimized for humans managing layouts. `tmuy` is optimized for agents and task-oriented workflows.

- one session = one task
- stable names and hashes, easier for scripts and supervisors to target
- built-in `send`, `tail`, `wait`, `inspect`, and `--json`
- logs and metadata are persisted automatically

## Install

```bash
curl -fsSL https://raw.githubusercontent.com/nick1udwig/tmuy/master/install.sh | sh
```

Or:

```bash
curl -fsSL https://raw.githubusercontent.com/nick1udwig/tmuy/master/install.sh | sh -s -- -b ~/.local/bin
```

Pin a specific release:

```bash
curl -fsSL https://raw.githubusercontent.com/nick1udwig/tmuy/master/install.sh | sh -s -- -v v0.2.0
```

Build from source:

```bash
cargo install --path .
```

## Usage

Start a shell session:

```bash
tmuy new ios
```

Reconnect later:

```bash
tmuy attach ios
tmuy send ios "git status"
tmuy tail -f ios
tmuy wait ios --timeout-secs 300
```

Run a one-off command in its own PTY:

```bash
tmuy new tests -- /bin/sh -lc "cargo test --locked"
tmuy tail tests
```

Run a restricted session on Linux:

```bash
tmuy new review --fs ro:. --net off -- /bin/sh -lc "rg TODO src"
```

## Typical Use Cases

- keep a Codex or Claude Code session alive while you switch devices
- run mobile dev tasks over SSH without losing the terminal state
- supervise long-running builds or test runs
- give an agent a sandboxed terminal with limited filesystem or network access

## Notes

- Linux and macOS support normal sessions
- restricted sandboxing is currently Linux-only via `bubblewrap`
- session data lives under `~/.tmuy` by default

For more detail, see [`docs/spec.md`](docs/spec.md).

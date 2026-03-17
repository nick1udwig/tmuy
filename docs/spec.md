# tmuy v0 Spec

`tmuy` is a one-terminal multiplexer. Each session is exactly one PTY-backed process tree. There are no windows or panes.

## CLI

- `tmuy new|n [name]`
- `tmuy new|n [name] -- <cmd...>`
- `tmuy attach|a <name> [--detach-key "C-b d"]`
- `tmuy kill <name>`
- `tmuy ls|list|l [--dead|--all]`
- `tmuy tail <name> [--raw] [-f|--follow]`
- `tmuy inspect <name>`
- `tmuy send <name> [payload]`
- `tmuy rename <name> <new-name>`
- `tmuy wait <name> [--timeout-secs N]`
- `tmuy signal <name> <INT|TERM|KILL|HUP>`
- Global: `--json`

## Core rules

- `tmuy new [name]` starts the user shell as an interactive PTY session.
- `tmuy new [name] -- <cmd...>` starts a one-off PTY command. The session exits when the process exits.
- `attach` always requires a name.
- Default detach sequence is tmux-style `Ctrl+B d`.
- `attach --detach-key` currently supports space-separated single-byte tokens such as `C-b d` or `C-a d`.
- `Ctrl+C` and `Ctrl+D` are passed to the child process normally.
- Multiple clients may attach to the same live session.
- Detached sessions may receive input via `tmuy send`.
- `kill` is a Ctrl+C-style interrupt. It sends `SIGINT` to the session process group rather than a hard `SIGKILL`.
- `ls` shows live sessions by default, `--dead` shows exited sessions, `--all` shows both.
- Auto-generated names are globally increasing integers: `1`, `2`, `3`, ...
- Explicit live-name collisions are rejected.
- Each session gets a unique 7-character hex hash at creation. The hash never changes, including after rename.
- The log directory is tied to the started name plus the stable hash. Rename only changes the current session name, not the original log path.

## On-disk layout

Base directory:

```text
~/.tmuy/
  state.json
  state.lock
  live/
    <hash>.sock
  YYYY/
    MM/
      DD/
        <started-name>-<hash>/
          meta.json
          pty.log
          events.jsonl
```

Example:

```text
~/.tmuy/2026/03/17/build-0a91b7c/
```

## Metadata

`meta.json` records:

- `id_hash`
- `started_name`
- `current_name`
- `created_at`
- `updated_at`
- `cwd`
- `command`
- `mode`
- `sandbox`
- `status`
- `started_log_dir`
- `meta_path`
- `log_path`
- `events_path`
- `socket_path`
- `service_pid`
- `child_pid`
- `exit_code`
- `failure_reason`
- full inherited `env`
- `detach_key`

## Sandbox shape

Requested interface:

- `--fs full`
- `--fs ro:<path>`
- `--fs rw:<path>`
- `--net on|off`

Behavior:

- Default is full filesystem access plus network on.
- Linux now enforces non-default sandbox specs with `bubblewrap`.
- `--fs full --net off` keeps filesystem access but unshares the network namespace.
- Restricted `ro:/path` and `rw:/path` grants only expose the declared paths plus minimal system directories needed to execute commands.
- For restricted grants, the session `cwd` must be inside one of the granted paths, otherwise startup fails with a recorded `failure_reason`.
- Planned backend: Seatbelt-style macOS runner compatible with the Codex model. No Windows support.

## Attach model

- Sessions are hosted by a detached internal service process.
- `attach` connects to a Unix socket and forwards raw terminal bytes.
- New attach clients receive a bounded replay of recent PTY output before switching to live bytes, so an already-running shell prompt does not appear blank.
- PTY output is broadcast to all attached clients and appended to `pty.log`.
- Resize handling is stubbed for now and remains a post-MVP TODO.

## Post-MVP

- Reliable screen-state replay on attach
- Resize propagation
- Full sandbox enforcement
- Richer `tail/read/search`
- Richer detach-key syntax
- Stronger transcript and end-to-end PTY coverage across Linux/macOS

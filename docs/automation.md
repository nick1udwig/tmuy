# tmuy Automation API

`tmuy` supports automation through its CLI plus `--json`.

This is the supported machine-facing interface for third-party programs in `v0`.

## Public Surface

Use these commands for automation:

- `tmuy --json new <name> --detached -- <cmd...>`
- `tmuy --json ls`
- `tmuy --json inspect <name-or-hash>`
- `tmuy --json rename <name-or-hash> <new-name>`
- `tmuy --json send <name-or-hash> [payload]`
- `tmuy --json wait <name-or-hash> [--timeout-secs N]`
- `tmuy --json signal <name-or-hash> <INT|TERM|KILL|HUP>`
- `tmuy --json kill <name-or-hash>`
- `tmuy events <name-or-hash> --jsonl [--follow]`

For output bytes, use:

- `tmuy tail --raw <name-or-hash>`
- `tmuy tail --raw --follow <name-or-hash>`

For exact input bytes, use:

- `tmuy --json send --no-enter <name-or-hash>` and pipe bytes on stdin

Programs should prefer the stable `id_hash` returned by `new --json` or `inspect --json`.
Session names can change after `rename`, but `id_hash` does not.

For richer integrations, `tmuy rpc serve` exposes a versioned local socket API.
See [`rpc-v1.md`](rpc-v1.md).

## Recommended Flow

1. Create a detached session with `new --json`.
2. Store the returned `id_hash`.
3. Read state with `inspect --json` or `ls --json`.
4. Write input with `send` and use `--no-enter` when exact bytes matter.
5. Read output with `tail --raw --follow`.
6. Follow lifecycle changes with `events --jsonl --follow`.
7. Wait for completion with `wait --json`.

## Non-Public Internals

These are implementation details and should not be treated as stable APIs:

- the hidden `tmuy __serve` subcommand
- the per-session Unix socket wire format
- direct writes to files under `~/.tmuy`

Reading files under `~/.tmuy` can still be useful for debugging, but the supported
automation path is the CLI described above.

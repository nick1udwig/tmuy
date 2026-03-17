# tmuy Test Plan

The test strategy is split into deterministic core tests and real PTY integration tests.

## 1. Deterministic core tests

Goal: fully exercise naming, metadata, path layout, hash stability, list filtering, and sandbox parsing without starting real PTYs.

Approach:

- Test `Store` against a temp `TMUY_HOME`
- Inject fixed cwd, env, names, and commands
- Assert exact `state.json`, `meta.json`, and `events.jsonl` behavior
- Cover:
  - auto numeric naming
  - explicit name validation
  - live-name collision rejection
  - dead-name reuse policy
  - stable 7-char hash across rename
  - started-name log path stability across rename
  - `ls` filters: live, dead, all
  - sandbox flag parsing

## 2. Transcript CLI tests

Goal: validate command UX and machine-readable output.

Approach:

- Spawn the compiled binary in temp homes
- Capture stdout/stderr/exit status
- Assert exact outputs for:
  - `tmuy new --json`
  - `tmuy ls`
  - `tmuy inspect`
  - `tmuy rename`
  - `tmuy wait --timeout-secs`
  - invalid flags and invalid names

Recommended harness:

- `assert_cmd`
- `predicates`
- per-test temp home directories

## 3. Mocked PTY service tests

Goal: fully exercise attach/send/tail logic without a real shell.

Approach:

- Introduce traits around:
  - PTY spawn
  - clock
  - id/hash generation
  - sandbox runner
- Replace the PTY backend with a mock service that:
  - accepts stdin bytes
  - emits scripted stdout bytes
  - exits with scripted codes
- Validate:
  - attach forwarding
  - detach sequence interception
  - `send`
  - `tail --raw` vs cooked
  - event logging
  - wait/exit transitions

## 4. Real end-to-end PTY tests

Goal: verify tmuy against actual PTYs on Linux CI.

Scenarios:

- `new` starts a shell session and shows as live in `ls`
- `attach` can send `echo hi` and receive output
- detach leaves the session alive
- `send` works while detached
- one-off commands exit and appear under `--dead`
- `kill` and `signal TERM` end the child
- `tail -f` follows the log until exit

Notes:

- Keep these tests few and stable
- Gate macOS-specific sandbox tests behind platform checks
- Resize should stay excluded until the TODO is implemented

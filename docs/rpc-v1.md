# tmuy RPC v1

`tmuy rpc serve` exposes a versioned local control API over a Unix socket.

Default socket path:

```text
~/.tmuy/rpc.sock
```

## Transport

- one JSON request per connection
- request is a single newline-terminated JSON object
- responses are newline-delimited JSON objects
- non-streaming operations return one `result` or `error` message
- streaming operations emit `event` or `output` messages and finish with `done`

All requests include `"v": 1`.

## Requests

Supported operations:

- `ping`
- `create`
- `list`
- `inspect`
- `rename`
- `write`
- `signal`
- `wait`
- `subscribe_output`
- `subscribe_events`

## Examples

Create a session:

```json
{"v":1,"op":"create","name":"build","command":["/bin/sh","-lc","cargo test"]}
```

Write exact bytes:

```json
{"v":1,"op":"write","target":"build","data_b64":"Z2l0IHN0YXR1cwo="}
```

Stream lifecycle events:

```json
{"v":1,"op":"subscribe_events","target":"build","follow":true}
```

Stream PTY output:

```json
{"v":1,"op":"subscribe_output","target":"build","follow":true}
```

Output bytes are base64-encoded in `data_b64` so the stream preserves exact PTY data.

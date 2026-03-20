use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read, Seek, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use base64::Engine;
use serde::Deserialize;
use serde_json::json;

use crate::model::{CommandMode, EventRecord, SessionRecord, SessionScope};
use crate::runtime;
use crate::store::{CreateSessionRequest, Store, parse_sandbox, validate_name};

const RPC_VERSION: u8 = 1;
const STREAM_POLL_INTERVAL: Duration = Duration::from_millis(200);

#[derive(Debug, Deserialize)]
struct RpcEnvelope {
    v: u8,
    #[serde(flatten)]
    request: RpcRequest,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum RpcRequest {
    Ping,
    Create {
        name: Option<String>,
        cwd: Option<PathBuf>,
        fs: Option<Vec<String>>,
        net: Option<String>,
        detach_key: Option<String>,
        command: Option<Vec<String>>,
        env: Option<BTreeMap<String, String>>,
    },
    List {
        dead: Option<bool>,
        all: Option<bool>,
    },
    Inspect {
        target: String,
    },
    Rename {
        target: String,
        new_name: String,
    },
    Write {
        target: String,
        data_b64: String,
    },
    Signal {
        target: String,
        signal: String,
    },
    Wait {
        target: String,
        timeout_secs: Option<u64>,
    },
    SubscribeOutput {
        target: String,
        follow: Option<bool>,
    },
    SubscribeEvents {
        target: String,
        follow: Option<bool>,
    },
}

pub fn default_socket_path(store: &Store) -> PathBuf {
    store.base_dir().join("rpc.sock")
}

pub fn run_rpc_server(store: &Store, socket_path: Option<PathBuf>) -> Result<()> {
    let socket_path = socket_path.unwrap_or_else(|| default_socket_path(store));
    if socket_path.exists() {
        let _ = fs::remove_file(&socket_path);
    }
    if let Some(parent) = socket_path.parent() {
        fs::create_dir_all(parent)?;
    }

    let listener = UnixListener::bind(&socket_path)
        .with_context(|| format!("failed to bind rpc socket {}", socket_path.display()))?;

    loop {
        let (mut stream, _) = listener.accept()?;
        let store = store.clone();
        thread::spawn(move || {
            if let Err(err) = handle_connection(&store, &mut stream) {
                let _ = write_error(&mut stream, &err.to_string());
            }
        });
    }
}

fn handle_connection(store: &Store, stream: &mut UnixStream) -> Result<()> {
    handle_connection_with_spawn(store, stream, runtime::spawn_daemon)
}

fn handle_connection_with_spawn(
    store: &Store,
    stream: &mut UnixStream,
    spawn_daemon: impl Fn(&Store, &SessionRecord) -> Result<()>,
) -> Result<()> {
    dispatch_request(store, stream, read_request(stream)?, spawn_daemon)
}

fn dispatch_request(
    store: &Store,
    stream: &mut UnixStream,
    request: RpcRequest,
    spawn_daemon: impl Fn(&Store, &SessionRecord) -> Result<()>,
) -> Result<()> {
    match request {
        RpcRequest::Ping => write_result(stream, json!({ "version": RPC_VERSION })),
        RpcRequest::Create {
            name,
            cwd,
            fs,
            net,
            detach_key,
            command,
            env,
        } => {
            if let Some(name) = name.as_deref() {
                validate_name(name)?;
            }
            let cwd = match cwd {
                Some(cwd) => cwd,
                None => std::env::current_dir()?,
            };
            let sandbox = parse_sandbox(&fs.unwrap_or_default(), net.as_deref(), &cwd)?;
            let env = env.unwrap_or_else(|| std::env::vars().collect());
            let detach_key = detach_key.unwrap_or_else(|| "C-b d".to_string());
            let (mode, command) = match command {
                Some(command) if !command.is_empty() => (CommandMode::OneShot, command),
                _ => (CommandMode::Shell, runtime::default_shell_command()),
            };

            let session = store.create_session(CreateSessionRequest {
                explicit_name: name,
                cwd,
                command,
                mode,
                sandbox,
                detach_key,
                env,
            })?;
            spawn_daemon(store, &session)?;
            write_result(stream, store.session_by_hash(&session.id_hash)?)
        }
        RpcRequest::List { dead, all } => {
            let dead = dead.unwrap_or(false);
            let all = all.unwrap_or(false);
            if dead && all {
                bail!("dead and all cannot both be true");
            }
            let scope = if all {
                SessionScope::All
            } else if dead {
                SessionScope::DeadOnly
            } else {
                SessionScope::LiveOnly
            };
            write_result(stream, store.list_sessions(scope)?)
        }
        RpcRequest::Inspect { target } => {
            write_result(stream, store.resolve_target(&target, SessionScope::All)?)
        }
        RpcRequest::Rename { target, new_name } => {
            validate_name(&new_name)?;
            write_result(stream, store.rename_session(&target, &new_name)?)
        }
        RpcRequest::Write { target, data_b64 } => {
            let data = base64::engine::general_purpose::STANDARD
                .decode(data_b64)
                .context("invalid data_b64 payload")?;
            runtime::send_input(store, &target, &data)?;
            write_result(stream, json!({ "ok": true }))
        }
        RpcRequest::Signal { target, signal } => {
            runtime::signal_session(store, &target, &signal)?;
            write_result(stream, json!({ "ok": true }))
        }
        RpcRequest::Wait {
            target,
            timeout_secs,
        } => {
            let timeout = timeout_secs.map(Duration::from_secs);
            write_result(stream, runtime::wait_for_exit(store, &target, timeout)?)
        }
        RpcRequest::SubscribeOutput { target, follow } => {
            stream_output(store, stream, &target, follow.unwrap_or(true))
        }
        RpcRequest::SubscribeEvents { target, follow } => {
            stream_events(store, stream, &target, follow.unwrap_or(true))
        }
    }
}

fn read_request(stream: &UnixStream) -> Result<RpcRequest> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = String::new();
    if reader.read_line(&mut line)? == 0 {
        bail!("empty rpc request");
    }
    let envelope: RpcEnvelope = serde_json::from_str(&line)?;
    if envelope.v != RPC_VERSION {
        bail!(
            "unsupported rpc version {}; expected {}",
            envelope.v,
            RPC_VERSION
        );
    }
    Ok(envelope.request)
}

fn stream_output(store: &Store, stream: &mut UnixStream, target: &str, follow: bool) -> Result<()> {
    let session = store.resolve_target(target, SessionScope::All)?;
    let hash = session.id_hash.clone();
    let log_path = session.log_path.clone();
    let mut position = 0u64;

    loop {
        if log_path.exists() {
            let mut file = File::open(&log_path)?;
            let len = file.metadata()?.len();
            if len > position {
                let to_read = (len - position) as usize;
                let mut buf = vec![0u8; to_read];
                file.seek(std::io::SeekFrom::Start(position))?;
                file.read_exact(&mut buf)?;
                write_line(
                    stream,
                    &json!({
                        "v": RPC_VERSION,
                        "type": "output",
                        "data_b64": base64::engine::general_purpose::STANDARD.encode(buf),
                    }),
                )?;
                position = len;
            }
        }
        if !follow {
            return write_done(stream);
        }
        let refreshed = store.session_by_hash(&hash)?;
        if !refreshed.status.is_live() && log_path.exists() {
            let len = fs::metadata(&log_path)?.len();
            if len <= position {
                return write_done(stream);
            }
        }
        thread::sleep(STREAM_POLL_INTERVAL);
    }
}

fn stream_events(store: &Store, stream: &mut UnixStream, target: &str, follow: bool) -> Result<()> {
    let session = store.resolve_target(target, SessionScope::All)?;
    let hash = session.id_hash.clone();
    let events_path = session.events_path.clone();
    let mut position = 0u64;
    let mut pending = Vec::new();

    loop {
        if events_path.exists() {
            let mut file = File::open(&events_path)?;
            let len = file.metadata()?.len();
            if len > position {
                let to_read = (len - position) as usize;
                let mut buf = vec![0u8; to_read];
                file.seek(std::io::SeekFrom::Start(position))?;
                file.read_exact(&mut buf)?;
                pending.extend_from_slice(&buf);
                flush_event_messages(stream, &mut pending)?;
                position = len;
            }
        }
        if !follow {
            flush_event_messages(stream, &mut pending)?;
            return write_done(stream);
        }
        let refreshed = store.session_by_hash(&hash)?;
        if !refreshed.status.is_live() && events_path.exists() {
            let len = fs::metadata(&events_path)?.len();
            if len <= position {
                flush_event_messages(stream, &mut pending)?;
                return write_done(stream);
            }
        }
        thread::sleep(STREAM_POLL_INTERVAL);
    }
}

fn flush_event_messages(stream: &mut UnixStream, pending: &mut Vec<u8>) -> Result<()> {
    let mut consumed = 0usize;
    while let Some(offset) = pending[consumed..].iter().position(|byte| *byte == b'\n') {
        let line_end = consumed + offset;
        if line_end > consumed {
            let event: EventRecord = serde_json::from_slice(&pending[consumed..line_end])?;
            write_line(
                stream,
                &json!({
                    "v": RPC_VERSION,
                    "type": "event",
                    "event": event,
                }),
            )?;
        }
        consumed = line_end + 1;
    }
    if consumed > 0 {
        pending.drain(..consumed);
    }
    Ok(())
}

fn write_result(stream: &mut UnixStream, result: impl serde::Serialize) -> Result<()> {
    write_line(
        stream,
        &json!({
            "v": RPC_VERSION,
            "type": "result",
            "result": result,
        }),
    )
}

fn write_error(stream: &mut UnixStream, error: &str) -> Result<()> {
    write_line(
        stream,
        &json!({
            "v": RPC_VERSION,
            "type": "error",
            "error": error,
        }),
    )
}

fn write_done(stream: &mut UnixStream) -> Result<()> {
    write_line(
        stream,
        &json!({
            "v": RPC_VERSION,
            "type": "done",
        }),
    )
}

fn write_line(stream: &mut UnixStream, value: &serde_json::Value) -> Result<()> {
    serde_json::to_writer(&mut *stream, value)?;
    stream.write_all(b"\n")?;
    stream.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::io::Read;
    use std::net::Shutdown;
    use std::os::unix::process::CommandExt;
    use std::path::Path;
    use std::process::Command;

    use nix::unistd::setsid;
    use serde_json::Value;
    use tempfile::{TempDir, tempdir};

    use super::*;
    use crate::model::{CommandMode, SandboxSpec, SessionStatus};
    use crate::store::CreateSessionRequest;

    fn make_store() -> (TempDir, Store) {
        let tmp = tempdir().unwrap();
        let store = Store::from_base_dir(tmp.path().join(".tmuy")).unwrap();
        (tmp, store)
    }

    fn create_session(store: &Store, cwd: &Path, name: &str) -> SessionRecord {
        store
            .create_session(CreateSessionRequest {
                explicit_name: Some(name.to_string()),
                cwd: cwd.to_path_buf(),
                command: vec![
                    "/bin/sh".to_string(),
                    "-lc".to_string(),
                    "printf ok".to_string(),
                ],
                mode: CommandMode::OneShot,
                sandbox: SandboxSpec::default(),
                detach_key: "C-b d".to_string(),
                env: BTreeMap::new(),
            })
            .unwrap()
    }

    fn request_lines_with_spawn(
        store: &Store,
        request: Value,
        spawn_daemon: impl Fn(&Store, &SessionRecord) -> Result<()>,
    ) -> Result<Vec<Value>> {
        let (mut client, mut server) = UnixStream::pair()?;
        serde_json::to_writer(&mut client, &request)?;
        client.write_all(b"\n")?;
        client.shutdown(Shutdown::Write)?;

        if let Err(err) = handle_connection_with_spawn(store, &mut server, spawn_daemon) {
            write_error(&mut server, &err.to_string())?;
        }
        drop(server);

        let mut raw = String::new();
        client.read_to_string(&mut raw)?;
        raw.lines()
            .map(serde_json::from_str::<Value>)
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    #[test]
    fn read_request_parses_versioned_rpc_envelope() {
        let (mut client, server) = UnixStream::pair().unwrap();
        client.write_all(b"{\"v\":1,\"op\":\"ping\"}\n").unwrap();
        let request = read_request(&server).unwrap();
        assert!(matches!(request, RpcRequest::Ping));
    }

    #[test]
    fn read_request_rejects_empty_and_unsupported_versions() {
        let (client, server) = UnixStream::pair().unwrap();
        client.shutdown(Shutdown::Write).unwrap();
        let err = read_request(&server).unwrap_err();
        assert!(err.to_string().contains("empty rpc request"));

        let (mut client, server) = UnixStream::pair().unwrap();
        client.write_all(b"{\"v\":9,\"op\":\"ping\"}\n").unwrap();
        let err = read_request(&server).unwrap_err();
        assert!(err.to_string().contains("unsupported rpc version 9"));
    }

    #[test]
    fn default_socket_path_uses_store_base_dir() {
        let (_tmp, store) = make_store();
        assert_eq!(
            default_socket_path(&store),
            store.base_dir().join("rpc.sock")
        );
    }

    #[test]
    fn rpc_requests_cover_create_list_inspect_rename_wait_and_filter_errors() {
        let (tmp, store) = make_store();
        let created = request_lines_with_spawn(
            &store,
            json!({
                "v": 1,
                "op": "create",
                "name": "alpha",
                "cwd": tmp.path(),
                "env": {
                    "USER_TOKEN": "abc123"
                }
            }),
            |_, _| Ok(()),
        )
        .unwrap();
        let created = created.first().unwrap();
        assert_eq!(created["type"], "result");

        let hash = created["result"]["id_hash"].as_str().unwrap().to_string();
        let created = store.session_by_hash(&hash).unwrap();
        assert_eq!(created.current_name, "alpha");
        assert_eq!(created.mode, CommandMode::Shell);
        assert_eq!(created.command, runtime::default_shell_command());
        assert_eq!(created.detach_key, "C-b d");
        assert_eq!(created.status, SessionStatus::Starting);
        assert_eq!(
            created.env.get("USER_TOKEN").map(String::as_str),
            Some("abc123")
        );

        store.mark_live(&hash, 1, None).unwrap();

        let listed =
            request_lines_with_spawn(&store, json!({"v": 1, "op": "list"}), |_, _| Ok(())).unwrap();
        assert_eq!(listed[0]["type"], "result");
        assert_eq!(listed[0]["result"].as_array().unwrap().len(), 1);

        let inspected = request_lines_with_spawn(
            &store,
            json!({"v": 1, "op": "inspect", "target": hash}),
            |_, _| Ok(()),
        )
        .unwrap();
        assert_eq!(inspected[0]["result"]["current_name"], "alpha");

        let renamed = request_lines_with_spawn(
            &store,
            json!({
                "v": 1,
                "op": "rename",
                "target": hash,
                "new_name": "beta"
            }),
            |_, _| Ok(()),
        )
        .unwrap();
        assert_eq!(renamed[0]["result"]["current_name"], "beta");

        store.mark_exited(&hash, Some(0)).unwrap();

        let dead = request_lines_with_spawn(
            &store,
            json!({"v": 1, "op": "list", "dead": true}),
            |_, _| Ok(()),
        )
        .unwrap();
        assert_eq!(dead[0]["result"].as_array().unwrap().len(), 1);

        let waited = request_lines_with_spawn(
            &store,
            json!({"v": 1, "op": "wait", "target": hash, "timeout_secs": 1}),
            |_, _| Ok(()),
        )
        .unwrap();
        assert_eq!(waited[0]["result"]["exit_code"], 0);

        let err = request_lines_with_spawn(
            &store,
            json!({"v": 1, "op": "list", "dead": true, "all": true}),
            |_, _| Ok(()),
        )
        .unwrap();
        assert_eq!(err[0]["type"], "error");
        assert!(
            err[0]["error"]
                .as_str()
                .unwrap()
                .contains("dead and all cannot both be true")
        );
    }

    #[test]
    fn rpc_requests_cover_write_signal_and_stream_subscriptions() {
        let (tmp, store) = make_store();

        let write_session = create_session(&store, tmp.path(), "writer");
        let listener = UnixListener::bind(&write_session.socket_path).unwrap();
        store
            .mark_live(&write_session.id_hash, std::process::id(), None)
            .unwrap();
        let wrote = request_lines_with_spawn(
            &store,
            json!({
                "v": 1,
                "op": "write",
                "target": write_session.id_hash,
                "data_b64": base64::engine::general_purpose::STANDARD.encode(b"hello\n"),
            }),
            |_, _| Ok(()),
        )
        .unwrap();
        assert_eq!(wrote[0]["result"]["ok"], true);
        let (mut input_stream, _) = listener.accept().unwrap();
        let mut input = Vec::new();
        input_stream.read_to_end(&mut input).unwrap();
        assert_eq!(input, b"Ihello\n");

        let signal_session = create_session(&store, tmp.path(), "signaler");
        let mut child = Command::new("/bin/sh");
        child.args(["-lc", "trap 'exit 0' TERM; while :; do sleep 1; done"]);
        // SAFETY: setsid is called in the child just before exec to isolate its process group.
        unsafe {
            child.pre_exec(|| {
                setsid().map_err(std::io::Error::other)?;
                Ok(())
            });
        }
        let mut child = child.spawn().unwrap();
        store
            .mark_live(
                &signal_session.id_hash,
                std::process::id(),
                Some(child.id()),
            )
            .unwrap();
        let signaled = request_lines_with_spawn(
            &store,
            json!({
                "v": 1,
                "op": "signal",
                "target": signal_session.id_hash,
                "signal": "TERM",
            }),
            |_, _| Ok(()),
        )
        .unwrap();
        assert_eq!(signaled[0]["result"]["ok"], true);
        child.wait().unwrap();
        let events = std::fs::read_to_string(signal_session.events_path).unwrap();
        assert!(events.contains("\"kind\":\"signal\""));

        let output_session = create_session(&store, tmp.path(), "output");
        std::fs::write(&output_session.log_path, b"chunk-1\nchunk-2\n").unwrap();
        store.mark_exited(&output_session.id_hash, Some(0)).unwrap();
        let output_lines = request_lines_with_spawn(
            &store,
            json!({
                "v": 1,
                "op": "subscribe_output",
                "target": output_session.id_hash,
                "follow": false,
            }),
            |_, _| Ok(()),
        )
        .unwrap();
        assert_eq!(output_lines.last().unwrap()["type"], "done");
        let chunks = output_lines
            .iter()
            .filter(|line| line["type"] == "output")
            .map(|line| {
                base64::engine::general_purpose::STANDARD
                    .decode(line["data_b64"].as_str().unwrap())
                    .unwrap()
            })
            .collect::<Vec<_>>();
        assert_eq!(chunks.concat(), b"chunk-1\nchunk-2\n");

        let events_session = create_session(&store, tmp.path(), "events");
        store
            .append_event(
                &events_session,
                EventRecord {
                    ts: chrono::Utc::now(),
                    kind: "renamed".to_string(),
                    detail: json!({"current_name": "events"}),
                },
            )
            .unwrap();
        std::fs::write(
            &events_session.events_path,
            [
                std::fs::read(&events_session.events_path).unwrap(),
                b"\n".to_vec(),
            ]
            .concat(),
        )
        .unwrap();
        store.mark_exited(&events_session.id_hash, Some(0)).unwrap();
        let event_lines = request_lines_with_spawn(
            &store,
            json!({
                "v": 1,
                "op": "subscribe_events",
                "target": events_session.id_hash,
                "follow": false,
            }),
            |_, _| Ok(()),
        )
        .unwrap();
        assert_eq!(event_lines.last().unwrap()["type"], "done");
        let kinds = event_lines
            .iter()
            .filter(|line| line["type"] == "event")
            .filter_map(|line| line["event"]["kind"].as_str())
            .collect::<Vec<_>>();
        assert!(kinds.iter().any(|kind| *kind == "created"));
        assert!(kinds.iter().any(|kind| *kind == "renamed"));

        let err = request_lines_with_spawn(
            &store,
            json!({
                "v": 1,
                "op": "write",
                "target": write_session.id_hash,
                "data_b64": "%%%not-base64%%%",
            }),
            |_, _| Ok(()),
        )
        .unwrap();
        assert_eq!(err[0]["type"], "error");
        assert!(
            err[0]["error"]
                .as_str()
                .unwrap()
                .contains("invalid data_b64 payload")
        );
    }

    #[test]
    fn flush_event_messages_keeps_partial_lines_and_skips_blank_ones() {
        let event = EventRecord {
            ts: chrono::Utc::now(),
            kind: "created".to_string(),
            detail: json!({"name": "demo"}),
        };
        let encoded = serde_json::to_vec(&event).unwrap();
        let mut pending = [encoded.clone(), b"\n\n".to_vec(), encoded[..5].to_vec()].concat();
        let (mut client, mut server) = UnixStream::pair().unwrap();

        flush_event_messages(&mut server, &mut pending).unwrap();
        assert_eq!(pending, encoded[..5].to_vec());
        drop(server);

        let mut raw = String::new();
        client.read_to_string(&mut raw).unwrap();
        let lines = raw
            .lines()
            .map(serde_json::from_str::<Value>)
            .collect::<std::result::Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0]["type"], "event");
        assert_eq!(lines[0]["event"]["kind"], "created");
    }
}

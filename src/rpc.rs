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

use crate::model::{CommandMode, EventRecord, SessionScope};
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
    match read_request(stream)? {
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
            runtime::spawn_daemon(store, &session)?;
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
    use super::*;

    #[test]
    fn read_request_parses_versioned_rpc_envelope() {
        let (mut client, server) = UnixStream::pair().unwrap();
        client.write_all(b"{\"v\":1,\"op\":\"ping\"}\n").unwrap();
        let request = read_request(&server).unwrap();
        assert!(matches!(request, RpcRequest::Ping));
    }
}

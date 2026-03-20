use std::io::{Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use base64::Engine;
use serde_json::{Value, json};
use tempfile::TempDir;

fn tmuy_bin() -> &'static str {
    env!("CARGO_BIN_EXE_tmuy")
}

#[test]
fn rpc_control_requests_round_trip() -> Result<()> {
    let home = TempDir::new()?;
    let server = RpcServer::spawn(home.path())?;

    let created = rpc_result(
        server.socket_path(),
        json!({
            "v": 1,
            "op": "create",
            "name": "alpha",
            "command": ["/bin/sh", "-lc", "trap 'exit 0' TERM; while :; do sleep 1; done"],
        }),
    )?;
    let hash = created["id_hash"]
        .as_str()
        .context("missing id_hash from create result")?
        .to_string();

    let listed = rpc_result(server.socket_path(), json!({"v": 1, "op": "list"}))?;
    let listed = listed.as_array().context("list result was not an array")?;
    assert!(listed.iter().any(|session| session["id_hash"] == hash));

    let renamed = rpc_result(
        server.socket_path(),
        json!({
            "v": 1,
            "op": "rename",
            "target": hash,
            "new_name": "beta",
        }),
    )?;
    assert_eq!(renamed["current_name"], "beta");
    let hash = renamed["id_hash"]
        .as_str()
        .context("missing id_hash from rename result")?
        .to_string();

    let inspected = rpc_result(
        server.socket_path(),
        json!({
            "v": 1,
            "op": "inspect",
            "target": hash,
        }),
    )?;
    assert_eq!(inspected["current_name"], "beta");

    let signaled = rpc_result(
        server.socket_path(),
        json!({
            "v": 1,
            "op": "signal",
            "target": hash,
            "signal": "TERM",
        }),
    )?;
    assert_eq!(signaled["ok"], true);

    let waited = rpc_result(
        server.socket_path(),
        json!({
            "v": 1,
            "op": "wait",
            "target": hash,
            "timeout_secs": 5,
        }),
    )?;
    assert_eq!(waited["current_name"], "beta");
    assert_eq!(waited["exit_code"], 0);

    Ok(())
}

#[test]
fn rpc_stream_requests_follow_output_and_events() -> Result<()> {
    let home = TempDir::new()?;
    let server = RpcServer::spawn(home.path())?;

    let created = rpc_result(
        server.socket_path(),
        json!({
            "v": 1,
            "op": "create",
            "name": "echoer",
            "command": ["/bin/sh", "-lc", "while IFS= read -r line; do printf 'E:%s\\n' \"$line\"; [ \"$line\" = quit ] && exit 7; done"],
        }),
    )?;
    let hash = created["id_hash"]
        .as_str()
        .context("missing id_hash from create result")?
        .to_string();

    let events_stream = rpc_open(
        server.socket_path(),
        json!({
            "v": 1,
            "op": "subscribe_events",
            "target": hash,
            "follow": true,
        }),
    )?;
    let output_stream = rpc_open(
        server.socket_path(),
        json!({
            "v": 1,
            "op": "subscribe_output",
            "target": hash,
            "follow": true,
        }),
    )?;

    let wrote = rpc_result(
        server.socket_path(),
        json!({
            "v": 1,
            "op": "write",
            "target": hash,
            "data_b64": base64::engine::general_purpose::STANDARD.encode(b"hello\nquit\n"),
        }),
    )?;
    assert_eq!(wrote["ok"], true);

    let waited = rpc_result(
        server.socket_path(),
        json!({
            "v": 1,
            "op": "wait",
            "target": hash,
            "timeout_secs": 5,
        }),
    )?;
    assert_eq!(waited["exit_code"], 7);

    let output_lines = read_rpc_lines(output_stream)?;
    let mut output_bytes = Vec::new();
    for line in &output_lines {
        if line["type"] == "output" {
            let chunk = base64::engine::general_purpose::STANDARD
                .decode(line["data_b64"].as_str().context("missing data_b64")?)?;
            output_bytes.extend_from_slice(&chunk);
        }
    }
    let output_text = String::from_utf8_lossy(&output_bytes);
    assert!(output_text.contains("E:hello"));
    assert!(output_text.contains("E:quit"));
    assert_eq!(
        output_lines.last().and_then(|line| line["type"].as_str()),
        Some("done")
    );

    let event_lines = read_rpc_lines(events_stream)?;
    let kinds = event_lines
        .iter()
        .filter(|line| line["type"] == "event")
        .filter_map(|line| line["event"]["kind"].as_str())
        .collect::<Vec<_>>();
    assert!(kinds.iter().any(|kind| *kind == "created"));
    assert!(kinds.iter().any(|kind| *kind == "live"));
    assert!(kinds.iter().any(|kind| *kind == "exited"));
    assert_eq!(
        event_lines.last().and_then(|line| line["type"].as_str()),
        Some("done")
    );

    Ok(())
}

struct RpcServer {
    child: Child,
    socket_path: PathBuf,
}

impl RpcServer {
    fn spawn(home: &Path) -> Result<Self> {
        let socket_path = home.join("rpc.sock");
        let child = Command::new(tmuy_bin())
            .args(["rpc", "serve"])
            .env("TMUY_HOME", home)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("failed to spawn rpc server")?;

        let server = Self { child, socket_path };
        server.wait_until_ready()?;
        Ok(server)
    }

    fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    fn wait_until_ready(&self) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if self.socket_path.exists()
                && rpc_result(self.socket_path(), json!({"v": 1, "op": "ping"})).is_ok()
            {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        anyhow::bail!(
            "timed out waiting for rpc server socket {}",
            self.socket_path.display()
        );
    }
}

impl Drop for RpcServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn rpc_result(socket_path: &Path, request: Value) -> Result<Value> {
    let lines = rpc_call(socket_path, request)?;
    let first = lines.first().context("rpc response was empty")?;
    match first["type"].as_str() {
        Some("result") => Ok(first["result"].clone()),
        Some("error") => anyhow::bail!(
            "{}",
            first["error"].as_str().unwrap_or("rpc request failed")
        ),
        other => anyhow::bail!("unexpected rpc response type: {other:?}"),
    }
}

fn rpc_call(socket_path: &Path, request: Value) -> Result<Vec<Value>> {
    let stream = rpc_open(socket_path, request)?;
    read_rpc_lines(stream)
}

fn rpc_open(socket_path: &Path, request: Value) -> Result<UnixStream> {
    let mut stream = UnixStream::connect(socket_path)
        .with_context(|| format!("failed to connect to {}", socket_path.display()))?;
    serde_json::to_writer(&mut stream, &request)?;
    stream.write_all(b"\n")?;
    stream.shutdown(Shutdown::Write)?;
    Ok(stream)
}

fn read_rpc_lines(mut stream: UnixStream) -> Result<Vec<Value>> {
    let mut raw = String::new();
    stream.read_to_string(&mut raw)?;
    raw.lines()
        .map(serde_json::from_str::<Value>)
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(Into::into)
}

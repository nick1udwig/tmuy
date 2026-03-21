use std::fs::{self, File};
use std::io::{self, Read, Seek, Write};
use std::os::unix::net::UnixStream;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use crossterm::terminal::{enable_raw_mode, size};

use crate::model::{EventRecord, SessionScope};
use crate::store::Store;

use super::protocol::{
    attach_input_loop, detach_sequence, is_peer_closed, parse_signal, signal_process_group,
};
use super::ui::{AlternateScreenGuard, BracketedPasteGuard, RawModeGuard, StatusBarGuard};

pub fn attach(store: &Store, name: &str, detach_key: &str) -> Result<()> {
    let session = store.resolve_target(name, SessionScope::LiveOnly)?;
    let stream = UnixStream::connect(&session.socket_path)
        .with_context(|| format!("failed to connect to {}", session.socket_path.display()))?;
    let mut write_stream = stream;
    let (cols, rows) = size().unwrap_or((80, 24));
    write_stream.write_all(&attach_handshake_payload(rows, cols))?;
    write_stream.flush()?;
    let mut read_stream = write_stream.try_clone()?;
    let seq = detach_sequence(detach_key)?;
    let mut input_stream = write_stream.try_clone()?;

    enable_raw_mode()?;
    let _restore = RawModeGuard;
    let _paste = BracketedPasteGuard::enter()?;
    let _screen = AlternateScreenGuard::enter()?;
    let status_bar = StatusBarGuard::enter(&session, detach_key)?;
    thread::spawn(move || {
        let _ = attach_input_loop(&mut input_stream, &seq);
    });
    let mut stdout = io::stdout();
    let mut buf = [0u8; 4096];
    status_bar.render(&mut stdout)?;
    loop {
        match read_stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                stdout.write_all(&buf[..n])?;
                stdout.flush()?;
                status_bar.render(&mut stdout)?;
            }
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) if is_peer_closed(&err) => break,
            Err(err) => return Err(err.into()),
        }
    }
    Ok(())
}

fn attach_handshake_payload(rows: u16, cols: u16) -> [u8; 5] {
    let usable_rows = rows.saturating_sub(1).max(1);
    let mut payload = [0u8; 5];
    payload[0] = b'A';
    payload[1..3].copy_from_slice(&usable_rows.to_be_bytes());
    payload[3..5].copy_from_slice(&cols.max(1).to_be_bytes());
    payload
}

pub fn send_input(store: &Store, name: &str, bytes: &[u8]) -> Result<()> {
    let session = store.resolve_target(name, SessionScope::LiveOnly)?;
    let mut stream = UnixStream::connect(&session.socket_path)?;
    stream.write_all(b"I")?;
    stream.write_all(bytes)?;
    stream.flush()?;
    Ok(())
}

pub fn tail(store: &Store, name: &str, raw: bool, follow: bool) -> Result<()> {
    let session = store.resolve_target(name, SessionScope::All)?;
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
                if raw {
                    io::stdout().write_all(&buf)?;
                } else {
                    let cooked = String::from_utf8_lossy(&buf);
                    print!("{cooked}");
                }
                io::stdout().flush()?;
                position = len;
            }
        }
        if !follow {
            return Ok(());
        }
        let refreshed = store.session_by_hash(&hash)?;
        if !refreshed.status.is_live() && log_path.exists() {
            let len = fs::metadata(&log_path)?.len();
            if len <= position {
                return Ok(());
            }
        }
        thread::sleep(Duration::from_millis(200));
    }
}

pub fn events(store: &Store, name: &str, jsonl: bool, follow: bool) -> Result<()> {
    let session = store.resolve_target(name, SessionScope::All)?;
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
                let mut stdout = io::stdout();
                flush_event_lines_to(&mut pending, jsonl, &mut stdout)?;
                position = len;
            }
        }
        if !follow {
            let mut stdout = io::stdout();
            flush_event_lines_to(&mut pending, jsonl, &mut stdout)?;
            return Ok(());
        }
        let refreshed = store.session_by_hash(&hash)?;
        if !refreshed.status.is_live() && events_path.exists() {
            let len = fs::metadata(&events_path)?.len();
            if len <= position {
                let mut stdout = io::stdout();
                flush_event_lines_to(&mut pending, jsonl, &mut stdout)?;
                return Ok(());
            }
        }
        thread::sleep(Duration::from_millis(200));
    }
}

fn flush_event_lines_to(pending: &mut Vec<u8>, jsonl: bool, writer: &mut impl Write) -> Result<()> {
    let mut consumed = 0usize;
    while let Some(offset) = pending[consumed..].iter().position(|byte| *byte == b'\n') {
        let line_end = consumed + offset;
        emit_event_line(writer, &pending[consumed..line_end], jsonl)?;
        consumed = line_end + 1;
    }
    if consumed > 0 {
        pending.drain(..consumed);
    }
    Ok(())
}

fn emit_event_line(stdout: &mut impl Write, line: &[u8], jsonl: bool) -> Result<()> {
    if line.is_empty() {
        return Ok(());
    }
    if jsonl {
        stdout.write_all(line)?;
        stdout.write_all(b"\n")?;
        stdout.flush()?;
        return Ok(());
    }

    let event: EventRecord = serde_json::from_slice(line)?;
    writeln!(
        stdout,
        "{}\t{}\t{}",
        event.ts,
        event.kind,
        serde_json::to_string(&event.detail)?
    )?;
    stdout.flush()?;
    Ok(())
}

pub fn signal_session(store: &Store, name: &str, signal_name: &str) -> Result<()> {
    let session = store.resolve_target(name, SessionScope::LiveOnly)?;
    let pid = session
        .child_pid
        .ok_or_else(|| anyhow!("session has no child pid yet: {name}"))?;
    let signal = parse_signal(signal_name)?;
    signal_process_group(pid, signal)?;
    store.append_event(
        &session,
        EventRecord {
            ts: chrono::Utc::now(),
            kind: "signal".to_string(),
            detail: serde_json::json!({
                "signal": signal_name,
            }),
        },
    )?;
    Ok(())
}

pub fn wait_for_exit(
    store: &Store,
    name: &str,
    timeout: Option<Duration>,
) -> Result<crate::model::SessionRecord> {
    let hash = store.resolve_target(name, SessionScope::All)?.id_hash;
    let started = Instant::now();
    loop {
        let session = store.session_by_hash(&hash)?;
        if !session.status.is_live() {
            return Ok(session);
        }
        if let Some(limit) = timeout {
            if started.elapsed() >= limit {
                bail!("timed out waiting for session to exit: {name}");
            }
        }
        thread::sleep(Duration::from_millis(200));
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::io::Read;
    use std::os::unix::{net::UnixListener, process::CommandExt};
    use std::process::Command;
    use std::time::Duration;

    use nix::unistd::setsid;
    use tempfile::tempdir;

    use super::{
        attach_handshake_payload, emit_event_line, flush_event_lines_to, send_input,
        signal_session, wait_for_exit,
    };
    use crate::model::{CommandMode, EventRecord, SandboxSpec, SessionStatus};
    use crate::store::{CreateSessionRequest, Store};

    fn make_store_and_session(
        name: &str,
    ) -> (tempfile::TempDir, Store, crate::model::SessionRecord) {
        let tmp = tempdir().unwrap();
        let store = Store::from_base_dir(tmp.path().join(".tmuy")).unwrap();
        let session = store
            .create_session(CreateSessionRequest {
                explicit_name: Some(name.to_string()),
                cwd: tmp.path().to_path_buf(),
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
            .unwrap();
        (tmp, store, session)
    }

    #[test]
    fn attach_handshake_uses_usable_rows_and_cols() {
        assert_eq!(attach_handshake_payload(8, 40), [b'A', 0, 7, 0, 40]);
        assert_eq!(attach_handshake_payload(1, 0), [b'A', 0, 1, 0, 1]);
    }

    #[test]
    fn flush_event_lines_keeps_partial_line_until_newline() {
        let event = EventRecord {
            ts: chrono::Utc::now(),
            kind: "created".to_string(),
            detail: serde_json::json!({"name": "demo"}),
        };
        let line = serde_json::to_vec(&event).unwrap();
        let mut output = Vec::new();

        let mut pending = line[..line.len() / 2].to_vec();
        flush_event_lines_to(&mut pending, true, &mut output).unwrap();
        assert!(!pending.is_empty());
        assert!(output.is_empty());

        pending.extend_from_slice(&line[line.len() / 2..]);
        pending.push(b'\n');
        flush_event_lines_to(&mut pending, true, &mut output).unwrap();
        assert!(pending.is_empty());
        assert_eq!(output, [line, b"\n".to_vec()].concat());
    }

    #[test]
    fn emit_event_line_formats_human_output_and_skips_blank_lines() {
        let event = EventRecord {
            ts: chrono::Utc::now(),
            kind: "created".to_string(),
            detail: serde_json::json!({"name": "demo"}),
        };
        let line = serde_json::to_vec(&event).unwrap();
        let mut output = Vec::new();
        emit_event_line(&mut output, &line, false).unwrap();
        let text = String::from_utf8(output).unwrap();
        assert!(text.contains("\tcreated\t"));
        assert!(text.contains("\"name\":\"demo\""));

        let mut blank = Vec::new();
        emit_event_line(&mut blank, b"", false).unwrap();
        assert!(blank.is_empty());
    }

    #[test]
    fn send_input_writes_mode_byte_and_payload() {
        let (_tmp, store, session) = make_store_and_session("writer");
        let listener = UnixListener::bind(&session.socket_path).unwrap();
        store
            .mark_live(&session.id_hash, std::process::id(), None)
            .unwrap();

        send_input(&store, &session.id_hash, b"hello\n").unwrap();

        let (mut stream, _) = listener.accept().unwrap();
        let mut data = Vec::new();
        stream.read_to_end(&mut data).unwrap();
        assert_eq!(data, b"Ihello\n");
    }

    #[test]
    fn signal_session_records_event_and_rejects_missing_child_pid() {
        let (_tmp, store, session) = make_store_and_session("missing-pid");
        store
            .mark_live(&session.id_hash, std::process::id(), None)
            .unwrap();
        let err = signal_session(&store, &session.id_hash, "TERM").unwrap_err();
        assert!(err.to_string().contains("session has no child pid yet"));

        let (_tmp, store, session) = make_store_and_session("signaled");
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
            .mark_live(&session.id_hash, std::process::id(), Some(child.id()))
            .unwrap();

        signal_session(&store, &session.id_hash, "TERM").unwrap();
        child.wait().unwrap();
        let events = std::fs::read_to_string(session.events_path).unwrap();
        assert!(events.contains("\"kind\":\"signal\""));
    }

    #[test]
    fn wait_for_exit_returns_dead_sessions_and_times_out_for_live_ones() {
        let (_tmp, store, session) = make_store_and_session("done");
        store.mark_exited(&session.id_hash, Some(7)).unwrap();
        let waited = wait_for_exit(&store, &session.id_hash, Some(Duration::from_secs(1))).unwrap();
        assert_eq!(waited.status, SessionStatus::Exited);
        assert_eq!(waited.exit_code, Some(7));

        let (_tmp, store, session) = make_store_and_session("live");
        store
            .mark_live(&session.id_hash, std::process::id(), None)
            .unwrap();
        let err =
            wait_for_exit(&store, &session.id_hash, Some(Duration::from_millis(1))).unwrap_err();
        assert!(
            err.to_string()
                .contains("timed out waiting for session to exit")
        );
    }
}

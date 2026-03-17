use std::fs::{self, File};
use std::io::{self, Read, Seek, Write};
use std::os::unix::net::UnixStream;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use crossterm::terminal::enable_raw_mode;

use crate::model::{EventRecord, SessionScope};
use crate::store::Store;

use super::protocol::{
    attach_input_loop, detach_sequence, is_peer_closed, parse_signal, signal_process_group,
};
use super::ui::{AlternateScreenGuard, RawModeGuard, StatusBarGuard};

pub fn attach(store: &Store, name: &str, detach_key: &str) -> Result<()> {
    let session = store.resolve_target(name, SessionScope::LiveOnly)?;
    let stream = UnixStream::connect(&session.socket_path)
        .with_context(|| format!("failed to connect to {}", session.socket_path.display()))?;
    let mut write_stream = stream;
    write_stream.write_all(b"A")?;
    write_stream.flush()?;
    let mut read_stream = write_stream.try_clone()?;
    let seq = detach_sequence(detach_key)?;
    let mut input_stream = write_stream.try_clone()?;

    enable_raw_mode()?;
    let _restore = RawModeGuard;
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

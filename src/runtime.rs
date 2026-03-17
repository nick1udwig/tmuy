use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, Write};
use std::net::Shutdown;
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use nix::sys::signal::{Signal, kill as send_signal};
use nix::unistd::{Pid, setsid};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};

use crate::model::{EventRecord, FsGrant, SessionRecord, SessionScope, SessionStatus};
use crate::store::Store;

type BroadcastMap = Arc<Mutex<HashMap<usize, Sender<Vec<u8>>>>>;

pub fn default_shell_command() -> Vec<String> {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".to_string());
    vec![shell, "-i".to_string()]
}

pub fn spawn_daemon(store: &Store, session: &SessionRecord) -> Result<()> {
    let exe = std::env::current_exe().context("failed to locate current executable")?;
    let null = Stdio::null();
    let mut cmd = Command::new(exe);
    cmd.arg("__serve")
        .arg(&session.id_hash)
        .stdin(null)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .current_dir(&session.cwd);
    let base_dir = store.base_dir().to_path_buf();
    cmd.env("TMUY_HOME", base_dir);
    // SAFETY: pre_exec runs in the child just before exec; setsid is async-signal-safe here.
    unsafe {
        cmd.pre_exec(|| {
            setsid().map_err(io::Error::other)?;
            Ok(())
        });
    }
    cmd.spawn()
        .context("failed to spawn tmuy session service")?;

    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(3) {
        match store.session_by_hash(&session.id_hash) {
            Ok(updated)
                if updated.status == SessionStatus::Live
                    || updated.status == SessionStatus::Exited =>
            {
                return Ok(());
            }
            Ok(_) => {}
            Err(_) => {}
        }
        if session.socket_path.exists() {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(50));
    }
    Ok(())
}

pub fn run_server(store: &Store, hash: &str) -> Result<()> {
    let session = store.session_by_hash(hash)?;
    if session.socket_path.exists() {
        let _ = fs::remove_file(&session.socket_path);
    }
    if let Some(parent) = session.socket_path.parent() {
        fs::create_dir_all(parent)?;
    }

    if !sandbox_supported(&session) {
        bail!(
            "sandbox enforcement beyond default full-access is not implemented yet for tmuy; requested {:?}",
            session.sandbox
        );
    }

    let pty_system = native_pty_system();
    let pair = pty_system.openpty(PtySize {
        rows: 24,
        cols: 80,
        pixel_width: 0,
        pixel_height: 0,
    })?;

    let builder = build_command(&session)?;
    let mut child = pair.slave.spawn_command(builder)?;
    let child_pid = child.process_id().map(|pid| pid as u32);
    drop(pair.slave);

    let _ = store.mark_live(hash, std::process::id(), child_pid)?;
    let session = store.session_by_hash(hash)?;
    store.append_event(
        &session,
        EventRecord {
            ts: chrono::Utc::now(),
            kind: "live".to_string(),
            detail: serde_json::json!({
                "service_pid": session.service_pid,
                "child_pid": session.child_pid,
            }),
        },
    )?;

    let listener = UnixListener::bind(&session.socket_path)?;
    listener.set_nonblocking(true)?;

    let mut reader = pair.master.try_clone_reader()?;
    let writer = Arc::new(Mutex::new(pair.master.take_writer()?));
    let running = Arc::new(AtomicBool::new(true));
    let broadcasters: BroadcastMap = Arc::new(Mutex::new(HashMap::new()));
    let next_client_id = Arc::new(AtomicUsize::new(1));

    let read_running = running.clone();
    let read_broadcasters = broadcasters.clone();
    let read_log_path = session.log_path.clone();
    let read_thread = thread::spawn(move || -> Result<()> {
        let mut log_file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(read_log_path)?;
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    let chunk = buf[..n].to_vec();
                    log_file.write_all(&chunk)?;
                    log_file.flush()?;
                    broadcast(&read_broadcasters, &chunk);
                }
                Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                Err(err) => return Err(err.into()),
            }
        }
        read_running.store(false, Ordering::SeqCst);
        Ok(())
    });

    let exit_store = store.clone();
    let exit_hash = hash.to_string();
    let exit_socket_path = session.socket_path.clone();
    let exit_running = running.clone();
    let wait_thread = thread::spawn(move || -> Result<()> {
        let status = child.wait()?;
        exit_running.store(false, Ordering::SeqCst);
        let exit_code = Some(status.exit_code() as i32);
        let updated = exit_store.mark_exited(&exit_hash, exit_code)?;
        exit_store.append_event(
            &updated,
            EventRecord {
                ts: chrono::Utc::now(),
                kind: "exited".to_string(),
                detail: serde_json::json!({
                    "exit_code": exit_code,
                }),
            },
        )?;
        let _ = fs::remove_file(exit_socket_path);
        Ok(())
    });

    while running.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((stream, _)) => {
                let server_state = ServerState {
                    writer: writer.clone(),
                    broadcasters: broadcasters.clone(),
                    next_client_id: next_client_id.clone(),
                    child_pid,
                };
                thread::spawn(move || {
                    let _ = handle_client(stream, server_state);
                });
            }
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(50));
            }
            Err(err) => return Err(err.into()),
        }
    }

    let _ = read_thread.join();
    let _ = wait_thread.join();
    let _ = fs::remove_file(&session.socket_path);
    Ok(())
}

pub fn attach(store: &Store, name: &str, detach_key: &str) -> Result<()> {
    let session = store.resolve_by_name(name, SessionScope::LiveOnly)?;
    let stream = UnixStream::connect(&session.socket_path)
        .with_context(|| format!("failed to connect to {}", session.socket_path.display()))?;
    let mut write_stream = stream;
    write_stream.write_all(b"A")?;
    write_stream.flush()?;
    let mut read_stream = write_stream.try_clone()?;
    let seq = detach_sequence(detach_key)?;
    let mut input_stream = write_stream.try_clone()?;
    thread::spawn(move || {
        let _ = attach_input_loop(&mut input_stream, &seq);
    });

    enable_raw_mode()?;
    let _restore = RawModeGuard;
    let mut stdout = io::stdout();
    let mut buf = [0u8; 4096];
    loop {
        match read_stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                stdout.write_all(&buf[..n])?;
                stdout.flush()?;
            }
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) if is_peer_closed(&err) => break,
            Err(err) => return Err(err.into()),
        }
    }
    Ok(())
}

pub fn send_input(store: &Store, name: &str, bytes: &[u8]) -> Result<()> {
    let session = store.resolve_by_name(name, SessionScope::LiveOnly)?;
    let mut stream = UnixStream::connect(&session.socket_path)?;
    stream.write_all(b"I")?;
    stream.write_all(bytes)?;
    stream.flush()?;
    Ok(())
}

pub fn tail(store: &Store, name: &str, raw: bool, follow: bool) -> Result<()> {
    let session = store
        .resolve_by_name(name, SessionScope::All)
        .or_else(|_| store.resolve_by_name(name, SessionScope::DeadOnly))?;
    let mut position = 0u64;
    loop {
        if session.log_path.exists() {
            let mut file = File::open(&session.log_path)?;
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
        let refreshed = store.resolve_by_name(name, SessionScope::All)?;
        if !refreshed.status.is_live() && refreshed.log_path.exists() {
            let len = fs::metadata(&refreshed.log_path)?.len();
            if len <= position {
                return Ok(());
            }
        }
        thread::sleep(Duration::from_millis(200));
    }
}

pub fn signal_session(store: &Store, name: &str, signal_name: &str) -> Result<()> {
    let session = store.resolve_by_name(name, SessionScope::LiveOnly)?;
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
) -> Result<SessionRecord> {
    let started = Instant::now();
    loop {
        let session = store.resolve_by_name(name, SessionScope::All)?;
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

fn build_command(session: &SessionRecord) -> Result<CommandBuilder> {
    let mut iter = session.command.iter();
    let program = iter
        .next()
        .ok_or_else(|| anyhow!("session has no command configured"))?;
    let mut builder = CommandBuilder::new(program);
    for arg in iter {
        builder.arg(arg);
    }
    builder.cwd(&session.cwd);
    for (key, value) in &session.env {
        builder.env(key, value);
    }
    Ok(builder)
}

fn sandbox_supported(session: &SessionRecord) -> bool {
    matches!(session.sandbox.fs.as_slice(), [FsGrant::Full])
        && matches!(session.sandbox.net, crate::model::NetworkMode::On)
}

fn broadcast(broadcasters: &BroadcastMap, chunk: &[u8]) {
    let mut dead = Vec::new();
    let mut map = broadcasters.lock().expect("broadcast map poisoned");
    for (id, tx) in map.iter() {
        if tx.send(chunk.to_vec()).is_err() {
            dead.push(*id);
        }
    }
    for id in dead {
        map.remove(&id);
    }
}

struct ServerState {
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    broadcasters: BroadcastMap,
    next_client_id: Arc<AtomicUsize>,
    child_pid: Option<u32>,
}

fn handle_client(mut stream: UnixStream, state: ServerState) -> Result<()> {
    let mut mode = [0u8; 1];
    stream.read_exact(&mut mode)?;
    match mode[0] {
        b'A' => handle_attach_client(stream, state),
        b'I' => {
            let mut input = Vec::new();
            stream.read_to_end(&mut input)?;
            let mut writer = state.writer.lock().expect("pty writer poisoned");
            writer.write_all(&input)?;
            writer.flush()?;
            Ok(())
        }
        b'S' => {
            let mut signal_name = String::new();
            stream.read_to_string(&mut signal_name)?;
            if let Some(pid) = state.child_pid {
                let signal = parse_signal(signal_name.trim())?;
                send_signal(Pid::from_raw(pid as i32), signal)?;
            }
            Ok(())
        }
        other => bail!("unknown client mode byte: {other:?}"),
    }
}

fn handle_attach_client(mut stream: UnixStream, state: ServerState) -> Result<()> {
    let client_id = state.next_client_id.fetch_add(1, Ordering::SeqCst);
    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    state
        .broadcasters
        .lock()
        .expect("broadcast map poisoned")
        .insert(client_id, tx);

    let mut write_stream = stream.try_clone()?;
    let writer_thread = thread::spawn(move || -> Result<()> {
        while let Ok(chunk) = rx.recv() {
            if let Err(err) = write_stream.write_all(&chunk) {
                if is_peer_closed(&err) {
                    break;
                }
                return Err(err.into());
            }
            if let Err(err) = write_stream.flush() {
                if is_peer_closed(&err) {
                    break;
                }
                return Err(err.into());
            }
        }
        Ok(())
    });

    let mut buf = [0u8; 4096];
    loop {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                let mut writer = state.writer.lock().expect("pty writer poisoned");
                writer.write_all(&buf[..n])?;
                writer.flush()?;
            }
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) => {
                state
                    .broadcasters
                    .lock()
                    .expect("broadcast map poisoned")
                    .remove(&client_id);
                return Err(err.into());
            }
        }
    }

    state
        .broadcasters
        .lock()
        .expect("broadcast map poisoned")
        .remove(&client_id);
    let _ = writer_thread.join();
    Ok(())
}

fn detach_sequence(detach_key: &str) -> Result<Vec<u8>> {
    match detach_key {
        "C-b d" => Ok(vec![0x02, b'd']),
        other => bail!("unsupported detach key format for now: {other}"),
    }
}

fn parse_signal(raw: &str) -> Result<Signal> {
    match raw.to_ascii_uppercase().as_str() {
        "INT" | "SIGINT" => Ok(Signal::SIGINT),
        "TERM" | "SIGTERM" => Ok(Signal::SIGTERM),
        "KILL" | "SIGKILL" => Ok(Signal::SIGKILL),
        "HUP" | "SIGHUP" => Ok(Signal::SIGHUP),
        other => bail!("unsupported signal: {other}"),
    }
}

fn attach_input_loop(stream: &mut UnixStream, detach_sequence: &[u8]) -> Result<()> {
    let mut stdin = io::stdin();
    let mut pending = Vec::new();
    let mut buf = [0u8; 1];
    loop {
        let n = stdin.read(&mut buf)?;
        if n == 0 {
            let _ = stream.shutdown(Shutdown::Both);
            return Ok(());
        }
        pending.push(buf[0]);
        if pending == detach_sequence {
            let _ = stream.shutdown(Shutdown::Both);
            return Ok(());
        }
        if detach_sequence.starts_with(&pending) {
            continue;
        }
        if let Err(err) = stream.write_all(&pending) {
            if is_peer_closed(&err) {
                return Ok(());
            }
            return Err(err.into());
        }
        if let Err(err) = stream.flush() {
            if is_peer_closed(&err) {
                return Ok(());
            }
            return Err(err.into());
        }
        pending.clear();
    }
}

fn signal_process_group(pid: u32, signal: Signal) -> Result<()> {
    send_signal(Pid::from_raw(-(pid as i32)), signal)?;
    Ok(())
}

fn is_peer_closed(err: &io::Error) -> bool {
    matches!(
        err.kind(),
        io::ErrorKind::BrokenPipe
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::UnexpectedEof
    )
}

struct RawModeGuard;

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
    }
}

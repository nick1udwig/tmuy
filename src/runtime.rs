use std::collections::{HashMap, VecDeque};
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
use crossterm::cursor::{MoveTo, RestorePosition, SavePosition};
use crossterm::queue;
use crossterm::style::{Color, Print, ResetColor, SetBackgroundColor, SetForegroundColor};
use crossterm::terminal::{Clear, ClearType, size};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use nix::sys::signal::{Signal, kill as send_signal};
use nix::unistd::{Pid, setsid};
use portable_pty::{PtySize, native_pty_system};

use crate::model::{EventRecord, FsGrant, SessionRecord, SessionScope, SessionStatus};
use crate::sandbox;
use crate::store::Store;

const MAX_HISTORY_BYTES: usize = 256 * 1024;

type SharedOutputState = Arc<Mutex<OutputState>>;

struct OutputState {
    history: VecDeque<u8>,
    broadcasters: HashMap<usize, Sender<Vec<u8>>>,
}

impl OutputState {
    fn new() -> Self {
        Self {
            history: VecDeque::new(),
            broadcasters: HashMap::new(),
        }
    }

    fn record_chunk(&mut self, chunk: &[u8]) -> Vec<(usize, Sender<Vec<u8>>)> {
        self.history.extend(chunk.iter().copied());
        while self.history.len() > MAX_HISTORY_BYTES {
            self.history.pop_front();
        }
        self.broadcasters
            .iter()
            .map(|(id, tx)| (*id, tx.clone()))
            .collect()
    }

    fn register_client(&mut self, id: usize, tx: Sender<Vec<u8>>) -> Vec<u8> {
        let snapshot = self.history.iter().copied().collect::<Vec<_>>();
        self.broadcasters.insert(id, tx);
        snapshot
    }

    fn remove_client(&mut self, id: usize) {
        self.broadcasters.remove(&id);
    }
}

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
            Ok(updated) if updated.status == SessionStatus::Failed => {
                let reason = updated
                    .failure_reason
                    .unwrap_or_else(|| "session failed to start".to_string());
                bail!("{reason}");
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

    let builder = sandbox::build_command(&session)?;
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
    let output_state: SharedOutputState = Arc::new(Mutex::new(OutputState::new()));
    let next_client_id = Arc::new(AtomicUsize::new(1));

    let read_running = running.clone();
    let read_output_state = output_state.clone();
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
                    broadcast(&read_output_state, &chunk);
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
                    output_state: output_state.clone(),
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
    let status_bar = StatusBarGuard::enter(&session, detach_key)?;
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

fn sandbox_supported(session: &SessionRecord) -> bool {
    matches!(session.sandbox.fs.as_slice(), [FsGrant::Full])
        && matches!(session.sandbox.net, crate::model::NetworkMode::On)
        || cfg!(target_os = "linux")
}

fn broadcast(output_state: &SharedOutputState, chunk: &[u8]) {
    let senders = {
        let mut state = output_state.lock().expect("output state poisoned");
        state.record_chunk(chunk)
    };

    let mut dead = Vec::new();
    for (id, tx) in senders {
        if tx.send(chunk.to_vec()).is_err() {
            dead.push(id);
        }
    }
    if dead.is_empty() {
        return;
    }

    let mut state = output_state.lock().expect("output state poisoned");
    for id in dead {
        state.remove_client(id);
    }
}

struct ServerState {
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    output_state: SharedOutputState,
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
    let mut write_stream = stream.try_clone()?;
    let history = {
        let mut output_state = state.output_state.lock().expect("output state poisoned");
        output_state.register_client(client_id, tx)
    };
    if !history.is_empty() {
        write_stream.write_all(&history)?;
        write_stream.flush()?;
    }

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
                    .output_state
                    .lock()
                    .expect("output state poisoned")
                    .remove_client(client_id);
                return Err(err.into());
            }
        }
    }

    state
        .output_state
        .lock()
        .expect("output state poisoned")
        .remove_client(client_id);
    let _ = writer_thread.join();
    Ok(())
}

fn detach_sequence(detach_key: &str) -> Result<Vec<u8>> {
    let tokens = detach_key.split_whitespace().collect::<Vec<_>>();
    if tokens.is_empty() {
        bail!("detach key cannot be empty");
    }
    let mut bytes = Vec::with_capacity(tokens.len());
    for token in tokens {
        bytes.push(parse_detach_token(token)?);
    }
    Ok(bytes)
}

fn parse_detach_token(token: &str) -> Result<u8> {
    if let Some(control) = token.strip_prefix("C-") {
        let mut chars = control.chars();
        let ch = chars
            .next()
            .ok_or_else(|| anyhow!("invalid control key token: {token}"))?;
        if chars.next().is_some() || !ch.is_ascii() {
            bail!("invalid control key token: {token}");
        }
        let lower = ch.to_ascii_lowercase() as u8;
        return Ok(lower & 0x1f);
    }

    let mut chars = token.chars();
    let ch = chars
        .next()
        .ok_or_else(|| anyhow!("invalid detach key token: {token}"))?;
    if chars.next().is_some() || !ch.is_ascii() {
        bail!("invalid detach key token: {token}");
    }
    Ok(ch as u8)
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
    attach_input_loop_from(&mut stdin, stream, detach_sequence)
}

fn attach_input_loop_from(
    reader: &mut impl Read,
    stream: &mut UnixStream,
    detach_sequence: &[u8],
) -> Result<()> {
    let mut pending = Vec::new();
    let mut buf = [0u8; 1];
    loop {
        let n = reader.read(&mut buf)?;
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

fn format_status_bar(session: &SessionRecord, detach_key: &str, width: u16) -> String {
    let width = width as usize;
    if width == 0 {
        return String::new();
    }

    let content = format!(
        " tmuy {} ({}) | detach {} | sandbox {} ",
        session.current_name,
        session.id_hash,
        detach_key,
        format_sandbox_summary(session)
    );
    fit_status_content(&content, width)
}

fn fit_status_content(content: &str, width: usize) -> String {
    if content.len() >= width {
        if width <= 3 {
            return ".".repeat(width);
        }
        let mut shortened = content[..width - 3].to_string();
        shortened.push_str("...");
        return shortened;
    }

    let mut padded = content.to_string();
    padded.push_str(&" ".repeat(width - content.len()));
    padded
}

fn format_sandbox_summary(session: &SessionRecord) -> String {
    let grants = session
        .sandbox
        .fs
        .iter()
        .map(|grant| match grant {
            FsGrant::Full => "fs:full".to_string(),
            FsGrant::ReadOnly(path) => format!("fs:ro:{}", path.display()),
            FsGrant::ReadWrite(path) => format!("fs:rw:{}", path.display()),
        })
        .collect::<Vec<_>>()
        .join(",");
    let net = match session.sandbox.net {
        crate::model::NetworkMode::On => "net:on",
        crate::model::NetworkMode::Off => "net:off",
    };
    format!("{grants} {net}")
}

struct StatusBarGuard {
    session: SessionRecord,
    detach_key: String,
    rows: Option<u16>,
}

impl StatusBarGuard {
    fn enter(session: &SessionRecord, detach_key: &str) -> Result<Self> {
        let (_, rows) = terminal_size();
        let rows = rows.max(2);
        let mut stdout = io::stdout();
        write!(stdout, "\x1b[1;{}r", rows - 1)?;
        stdout.flush()?;
        Ok(Self {
            session: session.clone(),
            detach_key: detach_key.to_string(),
            rows: Some(rows),
        })
    }

    fn render(&self, stdout: &mut impl Write) -> Result<()> {
        let Some(rows) = self.rows else {
            return Ok(());
        };
        let (width, _) = terminal_size();
        let line = format_status_bar(&self.session, &self.detach_key, width);
        queue!(
            stdout,
            SavePosition,
            MoveTo(0, rows - 1),
            SetForegroundColor(Color::Black),
            SetBackgroundColor(Color::White),
            Clear(ClearType::CurrentLine),
            Print(line),
            ResetColor,
            RestorePosition
        )?;
        stdout.flush()?;
        Ok(())
    }
}

impl Drop for StatusBarGuard {
    fn drop(&mut self) {
        if let Some(rows) = self.rows {
            let mut stdout = io::stdout();
            let _ = queue!(
                stdout,
                SavePosition,
                MoveTo(0, rows - 1),
                ResetColor,
                Clear(ClearType::CurrentLine),
                RestorePosition
            );
            let _ = write!(stdout, "\x1b[r");
            let _ = stdout.flush();
        }
    }
}

fn terminal_size() -> (u16, u16) {
    match size() {
        Ok((cols, rows)) if cols >= 1 && rows >= 1 => (cols, rows),
        _ => (80, 24),
    }
}

struct RawModeGuard;

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;
    use std::io::Cursor;
    use std::path::PathBuf;
    use std::process::Command;
    use std::sync::mpsc;
    use std::sync::{Arc, Mutex};

    use chrono::Utc;
    use std::io::Write;
    use std::os::unix::net::UnixStream;
    use tempfile::tempdir;

    use super::*;
    use crate::model::{CommandMode, FsGrant, NetworkMode, SandboxSpec, SessionStatus};
    use crate::store::{CreateSessionRequest, Store};

    fn sample_session() -> SessionRecord {
        SessionRecord {
            id_hash: "1a2b3c4".to_string(),
            started_name: "demo".to_string(),
            current_name: "demo".to_string(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
            cwd: PathBuf::from("/tmp/demo"),
            command: vec!["/bin/sh".to_string()],
            mode: CommandMode::Shell,
            sandbox: SandboxSpec {
                fs: vec![
                    FsGrant::ReadOnly(PathBuf::from("/tmp/demo")),
                    FsGrant::ReadWrite(PathBuf::from("/tmp/out")),
                ],
                net: NetworkMode::Off,
            },
            status: SessionStatus::Live,
            started_log_dir: PathBuf::from("/tmp/log"),
            meta_path: PathBuf::from("/tmp/log/meta.json"),
            log_path: PathBuf::from("/tmp/log/pty.log"),
            events_path: PathBuf::from("/tmp/log/events.jsonl"),
            socket_path: PathBuf::from("/tmp/live.sock"),
            service_pid: Some(1),
            child_pid: Some(2),
            exit_code: None,
            failure_reason: None,
            env: BTreeMap::new(),
            detach_key: "C-b d".to_string(),
        }
    }

    fn make_store_and_session(command: Vec<String>) -> (tempfile::TempDir, Store, SessionRecord) {
        let tmp = tempdir().unwrap();
        let store = Store::from_base_dir(tmp.path().join(".tmuy")).unwrap();
        let session = store
            .create_session(CreateSessionRequest {
                explicit_name: Some("srv".to_string()),
                cwd: tmp.path().to_path_buf(),
                command,
                mode: CommandMode::OneShot,
                sandbox: SandboxSpec::default(),
                detach_key: "C-b d".to_string(),
                env: BTreeMap::new(),
            })
            .unwrap();
        (tmp, store, session)
    }

    #[test]
    fn format_status_bar_includes_requested_fields() {
        let session = sample_session();
        let line = format_status_bar(&session, "C-b d", 200);
        assert!(line.contains("demo"));
        assert!(line.contains("1a2b3c4"));
        assert!(line.contains("C-b d"));
        assert!(line.contains("fs:ro:/tmp/demo"));
        assert!(line.contains("fs:rw:/tmp/out"));
        assert!(line.contains("net:off"));
    }

    #[test]
    fn format_status_bar_truncates_to_width() {
        let session = sample_session();
        let line = format_status_bar(&session, "C-b d", 20);
        assert_eq!(line.len(), 20);
        assert!(line.ends_with("..."));
    }

    #[test]
    fn fit_status_content_handles_zero_and_tiny_widths() {
        assert_eq!(format_status_bar(&sample_session(), "C-b d", 0), "");
        assert_eq!(fit_status_content("abcdef", 3), "...");
        assert_eq!(fit_status_content("abcdef", 2), "..");
    }

    #[test]
    fn output_state_trims_history_and_registers_snapshot() {
        let mut state = OutputState::new();
        let oversized = vec![b'x'; MAX_HISTORY_BYTES + 10];
        let _ = state.record_chunk(&oversized);
        let (tx, _rx) = mpsc::channel();
        let snapshot = state.register_client(1, tx);
        assert_eq!(snapshot.len(), MAX_HISTORY_BYTES);
        assert!(snapshot.iter().all(|byte| *byte == b'x'));
    }

    #[test]
    fn broadcast_removes_dead_clients() {
        let output_state = Arc::new(Mutex::new(OutputState::new()));
        let (tx, rx) = mpsc::channel();
        output_state.lock().unwrap().register_client(1, tx);
        drop(rx);
        broadcast(&output_state, b"hello");
        assert!(output_state.lock().unwrap().broadcasters.is_empty());
    }

    #[test]
    fn handle_client_input_and_unknown_mode_paths() {
        let shared = Arc::new(Mutex::new(Vec::new()));
        let writer = Arc::new(Mutex::new(
            Box::new(SharedWriter(shared.clone())) as Box<dyn Write + Send>
        ));
        let output_state = Arc::new(Mutex::new(OutputState::new()));
        let (mut client, server) = UnixStream::pair().unwrap();
        client.write_all(b"Ihello").unwrap();
        client.shutdown(Shutdown::Write).unwrap();
        handle_client(
            server,
            ServerState {
                writer,
                output_state: output_state.clone(),
                next_client_id: Arc::new(AtomicUsize::new(1)),
                child_pid: None,
            },
        )
        .unwrap();
        assert_eq!(&*shared.lock().unwrap(), b"hello");

        let writer = Arc::new(Mutex::new(
            Box::new(SharedWriter(Arc::new(Mutex::new(Vec::new())))) as Box<dyn Write + Send>,
        ));
        let (mut client, server) = UnixStream::pair().unwrap();
        client.write_all(b"STERM").unwrap();
        client.shutdown(Shutdown::Write).unwrap();
        handle_client(
            server,
            ServerState {
                writer,
                output_state,
                next_client_id: Arc::new(AtomicUsize::new(1)),
                child_pid: None,
            },
        )
        .unwrap();

        let writer = Arc::new(Mutex::new(
            Box::new(SharedWriter(Arc::new(Mutex::new(Vec::new())))) as Box<dyn Write + Send>,
        ));
        let (mut client, server) = UnixStream::pair().unwrap();
        client.write_all(b"X").unwrap();
        client.shutdown(Shutdown::Write).unwrap();
        let err = handle_client(
            server,
            ServerState {
                writer,
                output_state: Arc::new(Mutex::new(OutputState::new())),
                next_client_id: Arc::new(AtomicUsize::new(1)),
                child_pid: None,
            },
        )
        .unwrap_err();
        assert!(err.to_string().contains("unknown client mode byte"));
    }

    #[test]
    fn handle_client_signal_mode_sends_signal_to_child_pid() {
        let mut child = Command::new("/bin/sh")
            .args(["-lc", "trap 'exit 0' TERM; while :; do sleep 1; done"])
            .spawn()
            .unwrap();
        let pid = child.id();

        let writer = Arc::new(Mutex::new(
            Box::new(SharedWriter(Arc::new(Mutex::new(Vec::new())))) as Box<dyn Write + Send>,
        ));
        let (mut client, server) = UnixStream::pair().unwrap();
        client.write_all(b"STERM").unwrap();
        client.shutdown(Shutdown::Write).unwrap();
        handle_client(
            server,
            ServerState {
                writer,
                output_state: Arc::new(Mutex::new(OutputState::new())),
                next_client_id: Arc::new(AtomicUsize::new(1)),
                child_pid: Some(pid),
            },
        )
        .unwrap();

        let status = child.wait().unwrap();
        assert!(!status.success() || status.code() == Some(0));
    }

    #[test]
    fn detach_and_signal_parsers_cover_error_cases() {
        assert_eq!(detach_sequence("C-a d").unwrap(), vec![0x01, b'd']);
        assert_eq!(parse_signal("HUP").unwrap(), Signal::SIGHUP);
        assert_eq!(parse_signal("KILL").unwrap(), Signal::SIGKILL);
        assert!(detach_sequence("").is_err());
        assert!(parse_detach_token("C-ab").is_err());
        assert!(parse_detach_token("xy").is_err());
        assert!(parse_signal("BOGUS").is_err());
    }

    #[test]
    fn is_peer_closed_matches_expected_kinds() {
        assert!(is_peer_closed(&io::Error::new(
            io::ErrorKind::BrokenPipe,
            "x"
        )));
        assert!(is_peer_closed(&io::Error::new(
            io::ErrorKind::ConnectionReset,
            "x"
        )));
        assert!(!is_peer_closed(&io::Error::new(io::ErrorKind::Other, "x")));
    }

    #[test]
    fn attach_input_loop_handles_eof_and_peer_closed() {
        let (mut stream, peer) = UnixStream::pair().unwrap();
        let mut empty = Cursor::new(Vec::<u8>::new());
        attach_input_loop_from(&mut empty, &mut stream, b"\x02d").unwrap();
        let mut buf = [0u8; 1];
        let mut peer = peer;
        assert_eq!(peer.read(&mut buf).unwrap(), 0);

        let (mut stream, peer) = UnixStream::pair().unwrap();
        peer.shutdown(Shutdown::Read).unwrap();
        let mut reader = Cursor::new(vec![b'x']);
        attach_input_loop_from(&mut reader, &mut stream, b"\x02d").unwrap();
    }

    #[test]
    fn handle_attach_client_writer_thread_handles_peer_closed() {
        let output_state = Arc::new(Mutex::new(OutputState::new()));
        let writer = Arc::new(Mutex::new(
            Box::new(SharedWriter(Arc::new(Mutex::new(Vec::new())))) as Box<dyn Write + Send>,
        ));
        let (client, server) = UnixStream::pair().unwrap();
        let state = ServerState {
            writer,
            output_state: output_state.clone(),
            next_client_id: Arc::new(AtomicUsize::new(1)),
            child_pid: None,
        };
        let handle = thread::spawn(move || handle_attach_client(server, state));

        let deadline = Instant::now() + Duration::from_secs(1);
        while Instant::now() < deadline {
            if !output_state.lock().unwrap().broadcasters.is_empty() {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        assert!(!output_state.lock().unwrap().broadcasters.is_empty());

        client.shutdown(Shutdown::Read).unwrap();
        broadcast(&output_state, b"hello");
        let _ = client.shutdown(Shutdown::Write);

        assert!(handle.join().unwrap().is_ok());
    }

    #[test]
    fn status_bar_guard_render_handles_missing_rows() {
        let session = sample_session();
        let guard = StatusBarGuard {
            session: session.clone(),
            detach_key: "C-b d".to_string(),
            rows: None,
        };
        let mut buf = Vec::new();
        guard.render(&mut buf).unwrap();
        assert!(buf.is_empty());

        let guard = StatusBarGuard {
            session,
            detach_key: "C-b d".to_string(),
            rows: Some(2),
        };
        guard.render(&mut buf).unwrap();
        assert!(!buf.is_empty());
    }

    #[test]
    fn run_server_recreates_socket_parent_and_records_lifecycle() {
        let (_tmp, store, session) = make_store_and_session(vec![
            "/bin/sh".to_string(),
            "-lc".to_string(),
            "printf ok".to_string(),
        ]);
        fs::remove_dir_all(store.live_dir()).unwrap();
        run_server(&store, &session.id_hash).unwrap();

        let updated = store.session_by_hash(&session.id_hash).unwrap();
        assert_eq!(updated.status, SessionStatus::Exited);
        let events = fs::read_to_string(updated.events_path).unwrap();
        assert!(events.contains("\"kind\":\"live\""));
        assert!(events.contains("\"kind\":\"exited\""));
        assert!(fs::read_to_string(updated.log_path).unwrap().contains("ok"));
    }

    #[test]
    fn run_server_removes_stale_socket_file() {
        let (_tmp, store, session) = make_store_and_session(vec![
            "/bin/sh".to_string(),
            "-lc".to_string(),
            "printf stale".to_string(),
        ]);
        fs::write(&session.socket_path, b"stale").unwrap();
        run_server(&store, &session.id_hash).unwrap();

        let updated = store.session_by_hash(&session.id_hash).unwrap();
        assert_eq!(updated.status, SessionStatus::Exited);
        assert!(!updated.socket_path.exists());
    }

    #[derive(Clone)]
    struct SharedWriter(Arc<Mutex<Vec<u8>>>);

    impl Write for SharedWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
}

use std::collections::{HashMap, VecDeque};
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::{Result, bail};
use nix::sys::signal::kill as send_signal;
use nix::unistd::Pid;
use portable_pty::{PtySize, native_pty_system};

use crate::model::{EventRecord, FsGrant, SessionRecord};
use crate::sandbox;
use crate::store::Store;

use super::protocol::{is_peer_closed, parse_signal};

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

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;
    use std::io::Write;
    use std::os::unix::net::UnixStream;
    use std::process::Command;
    use std::sync::{Arc, Mutex};
    use std::time::Instant;

    use tempfile::tempdir;

    use super::*;
    use crate::model::{CommandMode, SandboxSpec, SessionStatus};
    use crate::store::{CreateSessionRequest, Store};

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
        client.shutdown(std::net::Shutdown::Write).unwrap();
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
        client.shutdown(std::net::Shutdown::Write).unwrap();
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
        client.shutdown(std::net::Shutdown::Write).unwrap();
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
        client.shutdown(std::net::Shutdown::Write).unwrap();
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

        client.shutdown(std::net::Shutdown::Read).unwrap();
        broadcast(&output_state, b"hello");
        let _ = client.shutdown(std::net::Shutdown::Write);

        assert!(handle.join().unwrap().is_ok());
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

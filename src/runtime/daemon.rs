use std::io;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use nix::unistd::setsid;

use crate::model::{SessionRecord, SessionStatus};
use crate::store::Store;

pub fn default_shell_command() -> Vec<String> {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".to_string());
    vec![shell, "-i".to_string()]
}

pub fn spawn_daemon(store: &Store, session: &SessionRecord) -> Result<()> {
    let exe = std::env::current_exe().context("failed to locate current executable")?;
    spawn_daemon_with_exe(store, session, &exe)
}

fn spawn_daemon_with_exe(store: &Store, session: &SessionRecord, exe: &Path) -> Result<()> {
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
    let mut child = cmd
        .spawn()
        .context("failed to spawn tmuy session service")?;

    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(3) {
        match store.session_by_hash(&session.id_hash) {
            Ok(updated) => {
                if let Some(result) = ready_result(&updated) {
                    return result;
                }
            }
            Err(_) => {}
        }
        if let Some(status) = child.try_wait()? {
            if let Some(result) =
                wait_for_ready_state(store, &session.id_hash, Duration::from_secs(1))?
            {
                return result;
            }
            bail!(
                "tmuy session service exited before session became ready: {:?}",
                status.code()
            );
        }
        thread::sleep(Duration::from_millis(50));
    }
    if let Some(result) = wait_for_ready_state(store, &session.id_hash, Duration::from_millis(250))?
    {
        return result;
    }
    bail!(
        "timed out waiting for session to become ready: {}",
        session.short_ref()
    )
}

fn wait_for_ready_state(
    store: &Store,
    hash: &str,
    timeout: Duration,
) -> Result<Option<Result<()>>> {
    let deadline = Instant::now() + timeout;
    loop {
        match store.session_by_hash(hash) {
            Ok(updated) => {
                if let Some(result) = ready_result(&updated) {
                    return Ok(Some(result));
                }
            }
            Err(_) => {}
        }
        if Instant::now() >= deadline {
            return Ok(None);
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn ready_result(session: &SessionRecord) -> Option<Result<()>> {
    match session.status {
        SessionStatus::Live | SessionStatus::Exited => Some(Ok(())),
        SessionStatus::Failed => Some(match &session.failure_reason {
            Some(reason) => Err(anyhow::anyhow!(reason.clone())),
            None => Err(anyhow::anyhow!("session failed to start")),
        }),
        SessionStatus::Starting => None,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::PathBuf;
    use std::sync::Mutex;
    use std::time::Duration;

    use tempfile::tempdir;

    use super::*;
    use crate::model::{CommandMode, SandboxSpec};
    use crate::store::{CreateSessionRequest, Store};

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn make_store_and_session(command: Vec<String>) -> (tempfile::TempDir, Store, SessionRecord) {
        let tmp = tempdir().unwrap();
        let store = Store::from_base_dir(tmp.path().join(".tmuy")).unwrap();
        let session = store
            .create_session(CreateSessionRequest {
                explicit_name: Some("demo".to_string()),
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
    fn default_shell_command_honors_shell_env_and_fallback() {
        let _guard = ENV_LOCK.lock().unwrap();
        let saved = std::env::var_os("SHELL");

        unsafe {
            std::env::set_var("SHELL", "/bin/zsh");
        }
        assert_eq!(
            default_shell_command(),
            vec!["/bin/zsh".to_string(), "-i".to_string()]
        );

        unsafe {
            std::env::remove_var("SHELL");
        }
        assert_eq!(
            default_shell_command(),
            vec!["/bin/bash".to_string(), "-i".to_string()]
        );

        match saved {
            Some(value) => unsafe {
                std::env::set_var("SHELL", value);
            },
            None => unsafe {
                std::env::remove_var("SHELL");
            },
        }
    }

    #[test]
    fn ready_helpers_cover_starting_live_exited_and_failed_states() {
        let (_tmp, store, session) = make_store_and_session(vec![
            "/bin/sh".to_string(),
            "-lc".to_string(),
            "printf ok".to_string(),
        ]);

        assert!(ready_result(&session).is_none());
        assert!(
            wait_for_ready_state(&store, &session.id_hash, Duration::from_millis(1))
                .unwrap()
                .is_none()
        );

        let live = store.mark_live(&session.id_hash, 10, Some(11)).unwrap();
        assert!(ready_result(&live).unwrap().is_ok());
        assert!(
            wait_for_ready_state(&store, &session.id_hash, Duration::from_millis(1))
                .unwrap()
                .unwrap()
                .is_ok()
        );

        let exited = store.mark_exited(&session.id_hash, Some(0)).unwrap();
        assert!(ready_result(&exited).unwrap().is_ok());

        let failed = store.mark_failed(&session.id_hash, "boom").unwrap();
        assert_eq!(
            ready_result(&failed).unwrap().unwrap_err().to_string(),
            "boom"
        );

        let mut failed_without_reason = failed;
        failed_without_reason.failure_reason = None;
        assert_eq!(
            ready_result(&failed_without_reason)
                .unwrap()
                .unwrap_err()
                .to_string(),
            "session failed to start"
        );
    }

    #[test]
    fn spawn_daemon_ignores_stale_socket_and_surfaces_start_failure() {
        let tmp = tempdir().unwrap();
        let store = Store::from_base_dir(tmp.path().join(".tmuy")).unwrap();
        let session = store
            .create_session(CreateSessionRequest {
                explicit_name: Some("bad".to_string()),
                cwd: tmp.path().to_path_buf(),
                command: Vec::new(),
                mode: CommandMode::OneShot,
                sandbox: SandboxSpec::default(),
                detach_key: "C-b d".to_string(),
                env: BTreeMap::new(),
            })
            .unwrap();
        std::fs::write(&session.socket_path, b"stale").unwrap();

        let err = spawn_daemon_with_exe(&store, &session, &tmuy_binary()).unwrap_err();
        assert!(!err.to_string().contains("timed out"));

        let updated = store.session_by_hash(&session.id_hash).unwrap();
        assert_eq!(updated.status, SessionStatus::Failed);
        assert!(
            updated
                .failure_reason
                .as_deref()
                .unwrap_or_default()
                .contains("session has no command configured")
        );
    }

    #[test]
    fn spawn_daemon_reports_success_for_ready_sessions() {
        let (_tmp, store, session) = make_store_and_session(vec![
            "/bin/sh".to_string(),
            "-lc".to_string(),
            "printf daemon-ok".to_string(),
        ]);

        spawn_daemon_with_exe(&store, &session, &tmuy_binary()).unwrap();

        let updated = store.session_by_hash(&session.id_hash).unwrap();
        assert!(matches!(
            updated.status,
            SessionStatus::Live | SessionStatus::Exited
        ));
        let log = std::fs::read_to_string(updated.log_path).unwrap();
        assert!(log.contains("daemon-ok"));
    }

    fn tmuy_binary() -> PathBuf {
        let current = std::env::current_exe().unwrap();
        current.parent().unwrap().parent().unwrap().join("tmuy")
    }
}

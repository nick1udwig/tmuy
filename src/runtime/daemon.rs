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

    use tempfile::tempdir;

    use super::*;
    use crate::model::{CommandMode, SandboxSpec};
    use crate::store::{CreateSessionRequest, Store};

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

    fn tmuy_binary() -> PathBuf {
        let current = std::env::current_exe().unwrap();
        current.parent().unwrap().parent().unwrap().join("tmuy")
    }
}

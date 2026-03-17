use std::io;
use std::os::unix::process::CommandExt;
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

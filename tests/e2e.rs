use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::AsFd;
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use nix::pty::openpty;
use tempfile::TempDir;

fn tmuy_bin() -> &'static str {
    env!("CARGO_BIN_EXE_tmuy")
}

struct AttachHarness {
    child: Child,
    master: File,
}

impl AttachHarness {
    fn write_all(&mut self, bytes: &[u8]) -> Result<()> {
        self.master.write_all(bytes)?;
        self.master.flush()?;
        Ok(())
    }

    fn read_until_contains(&mut self, needle: &str, timeout: Duration) -> Result<String> {
        let mut buf = Vec::new();
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            let chunk = read_ready(&mut self.master, Duration::from_millis(200))?;
            if !chunk.is_empty() {
                buf.extend_from_slice(&chunk);
                let text = String::from_utf8_lossy(&buf).to_string();
                if text.contains(needle) {
                    return Ok(text);
                }
            }
        }
        bail!(
            "timed out waiting for output containing {needle:?}; current output: {}",
            String::from_utf8_lossy(&buf)
        )
    }

    fn read_for(&mut self, timeout: Duration) -> Result<String> {
        let mut buf = Vec::new();
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            let chunk = read_ready(&mut self.master, Duration::from_millis(100))?;
            if chunk.is_empty() {
                continue;
            }
            buf.extend_from_slice(&chunk);
        }
        Ok(String::from_utf8_lossy(&buf).to_string())
    }

    fn wait_for_exit(&mut self, timeout: Duration) -> Result<std::process::ExitStatus> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if let Some(status) = self.child.try_wait()? {
                return Ok(status);
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        bail!("timed out waiting for attach process to exit")
    }
}

#[test]
fn attach_detaches_cleanly() -> Result<()> {
    let home = TempDir::new()?;
    let created = run_tmuy(home.path(), &["new", "demo"])?;
    assert_success(&created);

    let mut attach = spawn_attach(home.path(), "demo")?;
    std::thread::sleep(Duration::from_millis(500));
    attach.write_all(b"echo attach-ok\r")?;
    let output = attach.read_until_contains("attach-ok", Duration::from_secs(5))?;
    assert!(output.contains("attach-ok"));

    attach.write_all(&[0x02, b'd'])?;
    let status = attach.wait_for_exit(Duration::from_secs(5))?;
    assert!(status.success(), "attach exit status was {status:?}");
    Ok(())
}

#[test]
fn attach_ctrl_c_exits_without_broken_pipe() -> Result<()> {
    let home = TempDir::new()?;
    let created = run_tmuy(
        home.path(),
        &["new", "job", "--", "/bin/sh", "-lc", "sleep 100"],
    )?;
    assert_success(&created);

    let mut attach = spawn_attach(home.path(), "job")?;
    std::thread::sleep(Duration::from_millis(500));
    attach.write_all(&[0x03])?;
    let status = attach.wait_for_exit(Duration::from_secs(5))?;
    assert!(status.success(), "attach exit status was {status:?}");

    let transcript = attach.read_for(Duration::from_millis(500))?;
    assert!(
        !transcript.contains("os error 32")
            && !transcript.to_ascii_lowercase().contains("broken pipe"),
        "unexpected attach stderr/stdout after Ctrl+C: {transcript:?}"
    );

    let waited = run_tmuy(home.path(), &["wait", "job", "--timeout-secs", "5"])?;
    assert_success(&waited);
    Ok(())
}

#[test]
fn kill_sends_ctrl_c_style_interrupt() -> Result<()> {
    let home = TempDir::new()?;
    let created = run_tmuy(
        home.path(),
        &[
            "new",
            "killer",
            "--",
            "/bin/sh",
            "-lc",
            "trap 'echo trapped; exit 42' INT; while :; do sleep 1; done",
        ],
    )?;
    assert_success(&created);

    std::thread::sleep(Duration::from_millis(500));
    let killed = run_tmuy(home.path(), &["kill", "killer"])?;
    assert_success(&killed);

    let waited = run_tmuy(home.path(), &["wait", "killer", "--timeout-secs", "5"])?;
    assert_success(&waited);

    let tailed = run_tmuy(home.path(), &["tail", "killer"])?;
    assert_success(&tailed);
    let output = String::from_utf8_lossy(&tailed.stdout);
    assert!(
        output.contains("trapped"),
        "expected INT trap output in log, got: {output:?}"
    );
    Ok(())
}

fn run_tmuy(home: &std::path::Path, args: &[&str]) -> Result<Output> {
    let output = Command::new(tmuy_bin())
        .args(args)
        .env("TMUY_HOME", home)
        .output()
        .with_context(|| format!("failed to run tmuy {:?}", args))?;
    Ok(output)
}

fn spawn_attach(home: &std::path::Path, name: &str) -> Result<AttachHarness> {
    let pty = openpty(None, None)?;
    let master = File::from(pty.master);
    let slave = File::from(pty.slave);
    let stdin = Stdio::from(slave.try_clone()?);
    let stdout = Stdio::from(slave.try_clone()?);
    let stderr = Stdio::from(slave);

    let child = Command::new(tmuy_bin())
        .args(["attach", name])
        .env("TMUY_HOME", home)
        .stdin(stdin)
        .stdout(stdout)
        .stderr(stderr)
        .spawn()
        .with_context(|| format!("failed to spawn attach for {name}"))?;

    Ok(AttachHarness { child, master })
}

fn read_ready(master: &mut File, timeout: Duration) -> Result<Vec<u8>> {
    let millis = timeout.as_millis().min(u16::MAX as u128) as u16;
    let mut fds = [PollFd::new(master.as_fd(), PollFlags::POLLIN)];
    let ready = poll(&mut fds, PollTimeout::from(millis))?;
    if ready == 0 {
        return Ok(Vec::new());
    }

    let mut buf = [0u8; 4096];
    match master.read(&mut buf) {
        Ok(0) => Ok(Vec::new()),
        Ok(n) => Ok(buf[..n].to_vec()),
        Err(err) if err.kind() == io::ErrorKind::WouldBlock => Ok(Vec::new()),
        Err(err) if err.kind() == io::ErrorKind::Interrupted => Ok(Vec::new()),
        Err(err) if err.raw_os_error() == Some(5) => Ok(Vec::new()),
        Err(err) => Err(err.into()),
    }
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "command failed: status={:?}\nstdout={}\nstderr={}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

use std::io::{Read, Write};
use std::os::fd::AsFd;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use nix::pty::openpty;
use serde_json::Value;
use tempfile::TempDir;

fn tmuy_bin() -> &'static str {
    env!("CARGO_BIN_EXE_tmuy")
}

#[test]
fn ls_human_prints_no_sessions() -> Result<()> {
    let home = TempDir::new()?;
    let output = run_tmuy(home.path(), &["ls"])?;
    assert_success(&output);
    assert_eq!(String::from_utf8_lossy(&output.stdout), "no sessions\n");
    Ok(())
}

#[test]
fn subcommand_help_includes_user_facing_descriptions() -> Result<()> {
    let home = TempDir::new()?;
    let cases: [(&[&str], &str); 12] = [
        (&["new", "--help"], "Create a new session"),
        (&["attach", "--help"], "Attach to a live session"),
        (&["kill", "--help"], "Send a Ctrl+C-style interrupt"),
        (&["ls", "--help"], "List live sessions"),
        (&["tail", "--help"], "Print or follow terminal output"),
        (&["events", "--help"], "Print or follow session events"),
        (&["inspect", "--help"], "Show full metadata"),
        (&["send", "--help"], "pressing Enter by default"),
        (&["rename", "--help"], "Rename a live session"),
        (&["wait", "--help"], "Wait for a session to exit"),
        (&["signal", "--help"], "Send a specific POSIX signal"),
        (&["rpc", "--help"], "Serve the versioned local RPC API"),
    ];

    for (args, needle) in cases {
        let output = run_tmuy(home.path(), args)?;
        assert_success(&output);
        assert!(
            String::from_utf8_lossy(&output.stdout).contains(needle),
            "help for {:?} did not contain {:?}\n{}",
            args,
            needle,
            String::from_utf8_lossy(&output.stdout)
        );
    }
    Ok(())
}

#[test]
fn inspect_human_prints_session_detail_lines() -> Result<()> {
    let home = TempDir::new()?;
    let created = run_tmuy(
        home.path(),
        &["--json", "new", "demo", "--", "/bin/sh", "-lc", "sleep 30"],
    )?;
    assert_success(&created);
    let created_json: Value = serde_json::from_slice(&created.stdout)?;
    let hash = created_json["id_hash"]
        .as_str()
        .context("missing id_hash")?;

    let output = run_tmuy(home.path(), &["inspect", "demo"])?;
    assert_success(&output);
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("name: demo"));
    assert!(text.contains(&format!("hash: {hash}")));
    assert!(text.contains("detach_key: C-b d"));

    assert_success(&run_tmuy(home.path(), &["kill", "demo"])?);
    assert_success(&run_tmuy(
        home.path(),
        &["wait", "demo", "--timeout-secs", "5"],
    )?);
    Ok(())
}

#[test]
fn ls_human_prints_live_session_rows() -> Result<()> {
    let home = TempDir::new()?;
    let created = run_tmuy(
        home.path(),
        &["--json", "new", "demo", "--", "/bin/sh", "-lc", "sleep 30"],
    )?;
    assert_success(&created);
    let created_json: Value = serde_json::from_slice(&created.stdout)?;
    let hash = created_json["id_hash"]
        .as_str()
        .context("missing id_hash")?;

    let output = run_tmuy(home.path(), &["ls"])?;
    assert_success(&output);
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("demo"));
    assert!(text.contains(hash));
    assert!(text.contains("Live"));

    assert_success(&run_tmuy(home.path(), &["kill", "demo"])?);
    assert_success(&run_tmuy(
        home.path(),
        &["wait", "demo", "--timeout-secs", "5"],
    )?);
    Ok(())
}

#[test]
fn rename_send_from_stdin_and_wait_print_human_outputs() -> Result<()> {
    let home = TempDir::new()?;
    let created = run_tmuy(
        home.path(),
        &[
            "new",
            "alpha",
            "--",
            "/bin/sh",
            "-lc",
            "while IFS= read -r line; do printf 'line:%s\\n' \"$line\"; [ \"$line\" = quit ] && exit 0; done",
        ],
    )?;
    assert_success(&created);

    let renamed = run_tmuy(home.path(), &["rename", "alpha", "beta"])?;
    assert_success(&renamed);
    assert!(String::from_utf8_lossy(&renamed.stdout).contains("renamed alpha -> beta"));

    let sent = run_tmuy_with_stdin(home.path(), &["send", "beta"], b"hello")?;
    assert_success(&sent);
    assert_eq!(String::from_utf8_lossy(&sent.stdout), "sent\n");

    let quit = run_tmuy(home.path(), &["send", "beta", "quit"])?;
    assert_success(&quit);
    let waited = run_tmuy(home.path(), &["wait", "beta", "--timeout-secs", "5"])?;
    assert_success(&waited);
    assert!(String::from_utf8_lossy(&waited.stdout).contains("beta ("));

    let tailed = run_tmuy(home.path(), &["tail", "beta"])?;
    assert_success(&tailed);
    let tailed_text = String::from_utf8_lossy(&tailed.stdout);
    assert!(tailed_text.contains("line:hello"));
    Ok(())
}

#[test]
fn signal_prints_human_output() -> Result<()> {
    let home = TempDir::new()?;
    let created = run_tmuy(
        home.path(),
        &[
            "new",
            "termy",
            "--",
            "/bin/sh",
            "-lc",
            "trap 'echo term-trapped; exit 43' TERM; while :; do sleep 1; done",
        ],
    )?;
    assert_success(&created);

    let signaled = run_tmuy(home.path(), &["signal", "termy", "TERM"])?;
    assert_success(&signaled);
    assert_eq!(String::from_utf8_lossy(&signaled.stdout), "signaled\n");

    let waited = run_tmuy(home.path(), &["wait", "termy", "--timeout-secs", "5"])?;
    assert_success(&waited);

    let tailed = run_tmuy(home.path(), &["tail", "termy"])?;
    assert_success(&tailed);
    let tailed_text = String::from_utf8_lossy(&tailed.stdout);
    assert!(tailed_text.contains("term-trapped"));
    Ok(())
}

#[test]
fn kill_prints_interrupted() -> Result<()> {
    let home = TempDir::new()?;
    let created = run_tmuy(
        home.path(),
        &["new", "killme", "--", "/bin/sh", "-lc", "sleep 30"],
    )?;
    assert_success(&created);

    let killed = run_tmuy(home.path(), &["kill", "killme"])?;
    assert_success(&killed);
    assert_eq!(String::from_utf8_lossy(&killed.stdout), "interrupted\n");
    assert_success(&run_tmuy(
        home.path(),
        &["wait", "killme", "--timeout-secs", "5"],
    )?);
    Ok(())
}

#[test]
fn tail_raw_prints_exact_bytes() -> Result<()> {
    let home = TempDir::new()?;
    let created = run_tmuy(
        home.path(),
        &["new", "raw", "--", "/bin/sh", "-lc", "printf 'raw-hi\\n'"],
    )?;
    assert_success(&created);

    let tailed = run_tmuy(home.path(), &["tail", "--raw", "raw"])?;
    assert_success(&tailed);
    assert_eq!(tailed.stdout, b"raw-hi\r\n");
    Ok(())
}

#[test]
fn tail_follow_on_exited_session_returns_after_drain() -> Result<()> {
    let home = TempDir::new()?;
    let created = run_tmuy(
        home.path(),
        &["new", "done", "--", "/bin/sh", "-lc", "printf 'done\\n'"],
    )?;
    assert_success(&created);

    let tailed = run_tmuy(home.path(), &["tail", "-f", "done"])?;
    assert_success(&tailed);
    assert_eq!(tailed.stdout, b"done\r\n");
    Ok(())
}

#[test]
fn ls_rejects_dead_and_all_together() -> Result<()> {
    let home = TempDir::new()?;
    let output = run_tmuy(home.path(), &["ls", "--dead", "--all"])?;
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("--dead and --all are mutually exclusive")
    );
    Ok(())
}

#[test]
fn attach_json_prints_detached_after_detach() -> Result<()> {
    let home = TempDir::new()?;
    let created = run_tmuy(home.path(), &["new", "demo"])?;
    assert_success(&created);

    let mut attach = spawn_attach(home.path(), &["--json", "attach", "demo"])?;
    std::thread::sleep(Duration::from_millis(300));
    attach.write_all(&[0x02, b'd'])?;
    let status = attach.wait_for_exit(Duration::from_secs(5))?;
    assert!(status.success(), "attach exit status was {status:?}");
    let transcript = attach.read_for(Duration::from_millis(300))?;
    assert!(transcript.contains("\"message\": \"detached\""));

    let sent = run_tmuy(home.path(), &["send", "demo", "exit\n"])?;
    assert_success(&sent);
    let waited = run_tmuy(home.path(), &["wait", "demo", "--timeout-secs", "5"])?;
    assert_success(&waited);
    Ok(())
}

#[test]
fn wait_timeout_reports_error_for_live_session() -> Result<()> {
    let home = TempDir::new()?;
    let created = run_tmuy(
        home.path(),
        &["new", "slow", "--", "/bin/sh", "-lc", "sleep 30"],
    )?;
    assert_success(&created);

    let waited = run_tmuy(home.path(), &["wait", "slow", "--timeout-secs", "0"])?;
    assert!(!waited.status.success());
    assert!(
        String::from_utf8_lossy(&waited.stderr).contains("timed out waiting for session to exit")
    );

    assert_success(&run_tmuy(home.path(), &["kill", "slow"])?);
    assert_success(&run_tmuy(
        home.path(),
        &["wait", "slow", "--timeout-secs", "5"],
    )?);
    Ok(())
}

fn run_tmuy(home: &Path, args: &[&str]) -> Result<Output> {
    let output = Command::new(tmuy_bin())
        .args(args)
        .env("TMUY_HOME", home)
        .output()
        .with_context(|| format!("failed to run tmuy {:?}", args))?;
    Ok(output)
}

fn run_tmuy_with_stdin(home: &Path, args: &[&str], stdin_bytes: &[u8]) -> Result<Output> {
    let mut child = Command::new(tmuy_bin())
        .args(args)
        .env("TMUY_HOME", home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("failed to spawn tmuy {:?}", args))?;
    let mut stdin = child.stdin.take().context("missing child stdin")?;
    stdin.write_all(stdin_bytes)?;
    drop(stdin);
    let output = child.wait_with_output()?;
    Ok(output)
}

struct AttachHarness {
    child: std::process::Child,
    master: std::fs::File,
}

impl AttachHarness {
    fn write_all(&mut self, bytes: &[u8]) -> Result<()> {
        self.master.write_all(bytes)?;
        self.master.flush()?;
        Ok(())
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
        anyhow::bail!("timed out waiting for attach process to exit")
    }
}

fn spawn_attach(home: &Path, args: &[&str]) -> Result<AttachHarness> {
    let pty = openpty(None, None)?;
    let master = std::fs::File::from(pty.master);
    let slave = std::fs::File::from(pty.slave);
    let stdin = Stdio::from(slave.try_clone()?);
    let stdout = Stdio::from(slave.try_clone()?);
    let stderr = Stdio::from(slave);

    let child = Command::new(tmuy_bin())
        .args(args)
        .env("TMUY_HOME", home)
        .stdin(stdin)
        .stdout(stdout)
        .stderr(stderr)
        .spawn()
        .with_context(|| format!("failed to spawn attach for {:?}", args))?;

    Ok(AttachHarness { child, master })
}

fn read_ready(master: &mut std::fs::File, timeout: Duration) -> Result<Vec<u8>> {
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
        Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => Ok(Vec::new()),
        Err(err) if err.kind() == std::io::ErrorKind::Interrupted => Ok(Vec::new()),
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

use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::AsFd;
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use nix::pty::openpty;
use tempfile::TempDir;

fn tmuy_bin() -> &'static str {
    env!("CARGO_BIN_EXE_tmuy")
}

fn linux_bwrap_supported() -> bool {
    static SUPPORTED: OnceLock<bool> = OnceLock::new();
    *SUPPORTED.get_or_init(|| {
        if !cfg!(target_os = "linux") {
            return false;
        }

        Command::new("bwrap")
            .args([
                "--new-session",
                "--die-with-parent",
                "--unshare-all",
                "--share-net",
                "--dev-bind",
                "/",
                "/",
                "--proc",
                "/proc",
                "--chdir",
                "/",
                "--",
                "/bin/sh",
                "-lc",
                "true",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    })
}

struct AttachHarness {
    child: Child,
    master: File,
}

struct OutputHarness {
    child: Child,
    stdout: std::process::ChildStdout,
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

impl OutputHarness {
    fn read_until_contains(&mut self, needle: &str, timeout: Duration) -> Result<String> {
        let mut buf = Vec::new();
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            let chunk = read_pipe_ready(&mut self.stdout, Duration::from_millis(200))?;
            if chunk.is_empty() {
                continue;
            }
            buf.extend_from_slice(&chunk);
            let text = String::from_utf8_lossy(&buf).to_string();
            if text.contains(needle) {
                return Ok(text);
            }
        }
        bail!(
            "timed out waiting for output containing {needle:?}; current output: {}",
            String::from_utf8_lossy(&buf)
        )
    }

    fn wait_for_exit(&mut self, timeout: Duration) -> Result<std::process::ExitStatus> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if let Some(status) = self.child.try_wait()? {
                return Ok(status);
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        bail!("timed out waiting for process to exit")
    }
}

#[test]
fn attach_detaches_cleanly() -> Result<()> {
    let home = TempDir::new()?;
    let created = run_tmuy(home.path(), &["new", "demo"])?;
    assert_success(&created);

    let mut attach = spawn_attach(home.path(), &["attach", "demo"])?;
    std::thread::sleep(Duration::from_millis(500));
    attach.write_all(b"echo attach-ok\r")?;
    let output = attach.read_until_contains("attach-ok", Duration::from_secs(5))?;
    assert!(output.contains("attach-ok"));

    attach.write_all(&[0x02, b'd'])?;
    let status = attach.wait_for_exit(Duration::from_secs(5))?;
    assert!(status.success(), "attach exit status was {status:?}");
    let sent = run_tmuy(home.path(), &["send", "demo", "exit\n"])?;
    assert_success(&sent);
    let waited = run_tmuy(home.path(), &["wait", "demo", "--timeout-secs", "5"])?;
    assert_success(&waited);
    Ok(())
}

#[test]
fn interactive_new_attaches_by_default_and_uses_custom_detach_key() -> Result<()> {
    let home = TempDir::new()?;

    let mut session = spawn_pty_process(home.path(), &["new", "demo", "--detach-key", "C-a d"])?;
    let output = session.read_until_contains("tmuy demo", Duration::from_secs(5))?;
    assert!(output.contains("detach C-a d"));
    assert!(output.contains("\u{1b}[?1049h"));

    session.write_all(&[0x01, b'd'])?;
    let status = session.wait_for_exit(Duration::from_secs(5))?;
    assert!(status.success(), "new exit status was {status:?}");
    let transcript = session.read_for(Duration::from_millis(300))?;
    assert!(transcript.contains("\u{1b}[?1049l"));

    let sent = run_tmuy(home.path(), &["send", "demo", "exit\n"])?;
    assert_success(&sent);
    let waited = run_tmuy(home.path(), &["wait", "demo", "--timeout-secs", "5"])?;
    assert_success(&waited);
    Ok(())
}

#[test]
fn interactive_new_detached_flag_skips_auto_attach() -> Result<()> {
    let home = TempDir::new()?;

    let mut session = spawn_pty_process(home.path(), &["new", "demo", "--detached"])?;
    let status = session.wait_for_exit(Duration::from_secs(5))?;
    assert!(status.success(), "new exit status was {status:?}");
    let transcript = session.read_for(Duration::from_millis(300))?;
    assert!(transcript.contains("created demo"));
    assert!(!transcript.contains("\u{1b}[?1049h"));

    let sent = run_tmuy(home.path(), &["send", "demo", "exit\n"])?;
    assert_success(&sent);
    let waited = run_tmuy(home.path(), &["wait", "demo", "--timeout-secs", "5"])?;
    assert_success(&waited);
    Ok(())
}

#[test]
fn interactive_new_one_shot_stays_detached() -> Result<()> {
    let home = TempDir::new()?;

    let mut session = spawn_pty_process(
        home.path(),
        &["new", "oneshot", "--", "/bin/sh", "-lc", "printf once"],
    )?;
    let status = session.wait_for_exit(Duration::from_secs(5))?;
    assert!(status.success(), "new exit status was {status:?}");
    let transcript = session.read_for(Duration::from_millis(300))?;
    assert!(transcript.contains("created oneshot"));
    assert!(!transcript.contains("\u{1b}[?1049h"));
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

    let mut attach = spawn_attach(home.path(), &["attach", "job"])?;
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
fn attach_uses_session_detach_key_by_default() -> Result<()> {
    let home = TempDir::new()?;
    let created = run_tmuy(home.path(), &["new", "custom", "--detach-key", "C-a d"])?;
    assert_success(&created);
    let inspect = run_tmuy(home.path(), &["--json", "inspect", "custom"])?;
    assert_success(&inspect);
    let inspect_json: serde_json::Value = serde_json::from_slice(&inspect.stdout)?;
    let hash = inspect_json["id_hash"]
        .as_str()
        .context("missing id_hash in inspect --json output")?
        .to_string();

    let mut attach = spawn_attach(home.path(), &["attach", &hash])?;
    let output = attach.read_until_contains("detach C-a d", Duration::from_secs(5))?;
    assert!(output.contains("tmuy custom"));

    attach.write_all(&[0x01, b'd'])?;
    let status = attach.wait_for_exit(Duration::from_secs(5))?;
    assert!(status.success(), "attach exit status was {status:?}");

    let sent = run_tmuy(home.path(), &["send", "custom", "exit\n"])?;
    assert_success(&sent);
    let waited = run_tmuy(home.path(), &["wait", "custom", "--timeout-secs", "5"])?;
    assert_success(&waited);
    Ok(())
}

#[test]
fn attach_custom_detach_key_works() -> Result<()> {
    let home = TempDir::new()?;
    let created = run_tmuy(home.path(), &["new", "custom"])?;
    assert_success(&created);

    let mut attach = spawn_attach(home.path(), &["attach", "custom", "--detach-key", "C-a d"])?;
    std::thread::sleep(Duration::from_millis(500));
    attach.write_all(b"echo custom-detach\r")?;
    let output = attach.read_until_contains("custom-detach", Duration::from_secs(5))?;
    assert!(output.contains("custom-detach"));

    attach.write_all(&[0x01, b'd'])?;
    let status = attach.wait_for_exit(Duration::from_secs(5))?;
    assert!(status.success(), "attach exit status was {status:?}");
    let sent = run_tmuy(home.path(), &["send", "custom", "exit\n"])?;
    assert_success(&sent);
    let waited = run_tmuy(home.path(), &["wait", "custom", "--timeout-secs", "5"])?;
    assert_success(&waited);
    Ok(())
}

#[test]
fn attach_rejects_recursive_attach_and_new_auto_attach() -> Result<()> {
    let home = TempDir::new()?;
    assert_success(&run_tmuy(home.path(), &["new", "outer"])?);
    assert_success(&run_tmuy(home.path(), &["new", "other"])?);

    let mut attach = spawn_attach(home.path(), &["attach", "outer"])?;
    let _ = attach.read_until_contains("tmuy outer", Duration::from_secs(5))?;

    let recursive_attach = format!("{0} attach other; printf 'rc:%s\\n' $? \r", tmuy_bin());
    attach.write_all(recursive_attach.as_bytes())?;
    let output = attach.read_until_contains("rc:1", Duration::from_secs(5))?;
    assert!(output.contains("cannot attach from inside tmuy session"));
    assert!(output.contains("rc:1"));

    let recursive_new = format!("{0} new nested; printf 'new-rc:%s\\n' $? \r", tmuy_bin());
    attach.write_all(recursive_new.as_bytes())?;
    let output = attach.read_until_contains("new-rc:1", Duration::from_secs(5))?;
    assert!(output.contains("cannot attach from inside tmuy session"));

    let detached_new = format!("{0} new nested --detached\r", tmuy_bin());
    attach.write_all(detached_new.as_bytes())?;
    let output = attach.read_until_contains("created nested", Duration::from_secs(5))?;
    assert!(output.contains("created nested"));

    attach.write_all(b"exit\r")?;
    let status = attach.wait_for_exit(Duration::from_secs(5))?;
    assert!(status.success(), "attach exit status was {status:?}");

    let waited = run_tmuy(home.path(), &["wait", "outer", "--timeout-secs", "5"])?;
    assert_success(&waited);
    let sent = run_tmuy(home.path(), &["send", "nested", "exit\n"])?;
    assert_success(&sent);
    let waited = run_tmuy(home.path(), &["wait", "nested", "--timeout-secs", "5"])?;
    assert_success(&waited);
    let sent = run_tmuy(home.path(), &["send", "other", "exit\n"])?;
    assert_success(&sent);
    let waited = run_tmuy(home.path(), &["wait", "other", "--timeout-secs", "5"])?;
    assert_success(&waited);
    Ok(())
}

#[test]
fn attach_replays_existing_output() -> Result<()> {
    let home = TempDir::new()?;
    let created = run_tmuy(
        home.path(),
        &[
            "new",
            "replay",
            "--",
            "/bin/sh",
            "-lc",
            "printf READY; sleep 30",
        ],
    )?;
    assert_success(&created);

    std::thread::sleep(Duration::from_millis(500));
    let mut attach = spawn_attach(home.path(), &["attach", "replay"])?;
    let output = attach.read_until_contains("READY", Duration::from_secs(5))?;
    assert!(output.contains("READY"));

    attach.write_all(&[0x02, b'd'])?;
    let status = attach.wait_for_exit(Duration::from_secs(5))?;
    assert!(status.success(), "attach exit status was {status:?}");

    let interrupted = run_tmuy(home.path(), &["kill", "replay"])?;
    assert_success(&interrupted);
    let waited = run_tmuy(home.path(), &["wait", "replay", "--timeout-secs", "5"])?;
    assert_success(&waited);
    Ok(())
}

#[test]
fn attach_shows_status_bar_immediately_for_quiet_sessions() -> Result<()> {
    let home = TempDir::new()?;
    let created = run_tmuy(
        home.path(),
        &["--json", "new", "quiet", "--", "/bin/sh", "-lc", "sleep 30"],
    )?;
    assert_success(&created);
    let created_json: serde_json::Value = serde_json::from_slice(&created.stdout)?;
    let hash = created_json["id_hash"]
        .as_str()
        .context("missing id_hash in new --json output")?
        .to_string();

    let mut attach = spawn_attach(home.path(), &["attach", "quiet"])?;
    let output = attach.read_until_contains("tmuy quiet", Duration::from_secs(5))?;
    assert!(output.contains(&hash));
    assert!(output.contains("detach C-b d"));
    assert!(output.contains("sandbox fs:full net:on"));

    attach.write_all(&[0x02, b'd'])?;
    let status = attach.wait_for_exit(Duration::from_secs(5))?;
    assert!(status.success(), "attach exit status was {status:?}");

    let interrupted = run_tmuy(home.path(), &["kill", "quiet"])?;
    assert_success(&interrupted);
    let waited = run_tmuy(home.path(), &["wait", "quiet", "--timeout-secs", "5"])?;
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

#[test]
fn sandbox_ro_denies_write() -> Result<()> {
    if !linux_bwrap_supported() {
        eprintln!("skipping sandbox_ro_denies_write: Linux bwrap sandbox is unavailable");
        return Ok(());
    }

    let home = TempDir::new()?;
    let work = TempDir::new()?;
    std::fs::write(work.path().join("file"), "before")?;

    let created = run_tmuy_in_dir(
        home.path(),
        work.path(),
        &[
            "new",
            "ro",
            "--fs",
            "ro:.",
            "--",
            "/bin/sh",
            "-lc",
            "cat file >/dev/null && (touch new >/dev/null 2>&1; rc=$?; [ $rc -ne 0 ]) && printf ro-ok",
        ],
    )?;
    assert_success(&created);

    let tailed = run_tmuy(home.path(), &["tail", "ro"])?;
    assert_success(&tailed);
    assert!(String::from_utf8_lossy(&tailed.stdout).contains("ro-ok"));
    assert!(!work.path().join("new").exists());
    Ok(())
}

#[test]
fn sandbox_rw_allows_write() -> Result<()> {
    if !linux_bwrap_supported() {
        eprintln!("skipping sandbox_rw_allows_write: Linux bwrap sandbox is unavailable");
        return Ok(());
    }

    let home = TempDir::new()?;
    let work = TempDir::new()?;

    let created = run_tmuy_in_dir(
        home.path(),
        work.path(),
        &[
            "new",
            "rw",
            "--fs",
            "rw:.",
            "--",
            "/bin/sh",
            "-lc",
            "touch new && test -f new && printf rw-ok",
        ],
    )?;
    assert_success(&created);

    let tailed = run_tmuy(home.path(), &["tail", "rw"])?;
    assert_success(&tailed);
    assert!(String::from_utf8_lossy(&tailed.stdout).contains("rw-ok"));
    assert!(work.path().join("new").exists());
    Ok(())
}

#[test]
fn sandbox_net_off_unshares_network_namespace() -> Result<()> {
    if !linux_bwrap_supported() {
        eprintln!(
            "skipping sandbox_net_off_unshares_network_namespace: Linux bwrap sandbox is unavailable"
        );
        return Ok(());
    }

    let home = TempDir::new()?;
    let work = TempDir::new()?;
    let host_ns = std::fs::read_link("/proc/self/ns/net")?
        .to_string_lossy()
        .to_string();

    let on = run_tmuy_in_dir(
        home.path(),
        work.path(),
        &[
            "new",
            "neton",
            "--fs",
            "rw:.",
            "--net",
            "on",
            "--",
            "/bin/sh",
            "-lc",
            "readlink /proc/self/ns/net",
        ],
    )?;
    assert_success(&on);
    let on_tail = run_tmuy(home.path(), &["tail", "neton"])?;
    assert_success(&on_tail);
    let on_ns = String::from_utf8_lossy(&on_tail.stdout).trim().to_string();
    assert_eq!(on_ns, host_ns);

    let off = run_tmuy_in_dir(
        home.path(),
        work.path(),
        &[
            "new",
            "netoff",
            "--fs",
            "rw:.",
            "--net",
            "off",
            "--",
            "/bin/sh",
            "-lc",
            "readlink /proc/self/ns/net",
        ],
    )?;
    assert_success(&off);
    let off_tail = run_tmuy(home.path(), &["tail", "netoff"])?;
    assert_success(&off_tail);
    let off_ns = String::from_utf8_lossy(&off_tail.stdout).trim().to_string();
    assert_ne!(off_ns, host_ns);
    Ok(())
}

#[test]
fn sandbox_fails_when_cwd_not_granted() -> Result<()> {
    let home = TempDir::new()?;
    let work = TempDir::new()?;
    std::fs::create_dir(work.path().join("subdir"))?;

    let output = run_tmuy_in_dir(
        home.path(),
        work.path(),
        &[
            "new",
            "bad",
            "--fs",
            "ro:subdir",
            "--",
            "/bin/sh",
            "-lc",
            "printf should-not-run",
        ],
    )?;
    assert!(
        !output.status.success(),
        "sandbox startup should have failed\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let inspect = run_tmuy(home.path(), &["--json", "inspect", "bad"])?;
    assert_success(&inspect);
    let inspected: serde_json::Value = serde_json::from_slice(&inspect.stdout)?;
    assert_eq!(inspected["status"], "Failed");
    assert!(
        inspected["failure_reason"]
            .as_str()
            .unwrap_or_default()
            .contains("not covered")
    );
    Ok(())
}

#[test]
fn send_reaches_detached_session() -> Result<()> {
    let home = TempDir::new()?;
    let created = run_tmuy(
        home.path(),
        &[
            "new",
            "echoer",
            "--",
            "/bin/sh",
            "-lc",
            "while IFS= read -r line; do printf 'E:%s\\n' \"$line\"; [ \"$line\" = quit ] && exit 0; done",
        ],
    )?;
    assert_success(&created);

    let sent = run_tmuy(home.path(), &["send", "echoer", "hello"])?;
    assert_success(&sent);
    std::thread::sleep(Duration::from_millis(300));

    let tail = run_tmuy(home.path(), &["tail", "echoer"])?;
    assert_success(&tail);
    assert!(String::from_utf8_lossy(&tail.stdout).contains("E:hello"));

    let quit = run_tmuy(home.path(), &["send", "echoer", "quit"])?;
    assert_success(&quit);
    let waited = run_tmuy(home.path(), &["wait", "echoer", "--timeout-secs", "5"])?;
    assert_success(&waited);
    Ok(())
}

#[test]
fn send_no_enter_leaves_command_pending_until_later_submit() -> Result<()> {
    let home = TempDir::new()?;
    let created = run_tmuy(
        home.path(),
        &[
            "new",
            "pending",
            "--",
            "/bin/sh",
            "-lc",
            "while IFS= read -r line; do printf 'E:%s\\n' \"$line\"; [ \"$line\" = quit ] && exit 0; done",
        ],
    )?;
    assert_success(&created);

    let sent = run_tmuy(home.path(), &["send", "--no-enter", "pending", "hel"])?;
    assert_success(&sent);
    std::thread::sleep(Duration::from_millis(300));

    let tail = run_tmuy(home.path(), &["tail", "pending"])?;
    assert_success(&tail);
    assert!(!String::from_utf8_lossy(&tail.stdout).contains("E:hel"));

    let sent = run_tmuy(home.path(), &["send", "pending", "lo"])?;
    assert_success(&sent);
    std::thread::sleep(Duration::from_millis(300));

    let tail = run_tmuy(home.path(), &["tail", "pending"])?;
    assert_success(&tail);
    assert!(String::from_utf8_lossy(&tail.stdout).contains("E:hello"));

    let quit = run_tmuy(home.path(), &["send", "pending", "quit"])?;
    assert_success(&quit);
    let waited = run_tmuy(home.path(), &["wait", "pending", "--timeout-secs", "5"])?;
    assert_success(&waited);
    Ok(())
}

#[test]
fn tail_follow_streams_new_output() -> Result<()> {
    let home = TempDir::new()?;
    let created = run_tmuy(
        home.path(),
        &[
            "new",
            "stream",
            "--",
            "/bin/sh",
            "-lc",
            "while IFS= read -r line; do printf 'S:%s\\n' \"$line\"; [ \"$line\" = quit ] && exit 0; done",
        ],
    )?;
    assert_success(&created);

    let mut tail = spawn_output_tmuy(home.path(), &["tail", "-f", "stream"])?;
    let sent = run_tmuy(home.path(), &["send", "stream", "alpha"])?;
    assert_success(&sent);
    let output = tail.read_until_contains("S:alpha", Duration::from_secs(5))?;
    assert!(output.contains("S:alpha"));

    let quit = run_tmuy(home.path(), &["send", "stream", "quit"])?;
    assert_success(&quit);
    let status = tail.wait_for_exit(Duration::from_secs(5))?;
    assert!(status.success(), "tail -f exit status was {status:?}");
    Ok(())
}

#[test]
fn tail_follow_by_name_survives_session_rename() -> Result<()> {
    let home = TempDir::new()?;
    let created = run_tmuy(
        home.path(),
        &[
            "new",
            "stream",
            "--",
            "/bin/sh",
            "-lc",
            "while IFS= read -r line; do printf 'S:%s\\n' \"$line\"; [ \"$line\" = quit ] && exit 0; done",
        ],
    )?;
    assert_success(&created);

    let mut tail = spawn_output_tmuy(home.path(), &["tail", "-f", "stream"])?;
    std::thread::sleep(Duration::from_millis(300));

    let renamed = run_tmuy(home.path(), &["rename", "stream", "moved"])?;
    assert_success(&renamed);

    let sent = run_tmuy(home.path(), &["send", "moved", "alpha"])?;
    assert_success(&sent);
    let output = tail.read_until_contains("S:alpha", Duration::from_secs(5))?;
    assert!(output.contains("S:alpha"));

    let quit = run_tmuy(home.path(), &["send", "moved", "quit"])?;
    assert_success(&quit);
    let status = tail.wait_for_exit(Duration::from_secs(5))?;
    assert!(status.success(), "tail -f exit status was {status:?}");
    Ok(())
}

#[test]
fn wait_by_name_survives_session_rename() -> Result<()> {
    let home = TempDir::new()?;
    let created = run_tmuy(
        home.path(),
        &[
            "new",
            "watch",
            "--",
            "/bin/sh",
            "-lc",
            "while IFS= read -r line; do [ \"$line\" = quit ] && exit 7; done",
        ],
    )?;
    assert_success(&created);

    let mut waited = spawn_output_tmuy(home.path(), &["wait", "watch", "--timeout-secs", "5"])?;
    std::thread::sleep(Duration::from_millis(300));

    let renamed = run_tmuy(home.path(), &["rename", "watch", "moved"])?;
    assert_success(&renamed);

    let quit = run_tmuy(home.path(), &["send", "moved", "quit"])?;
    assert_success(&quit);
    let output = waited.read_until_contains("moved (", Duration::from_secs(5))?;
    assert!(output.contains("exited with Some(7)"));
    let status = waited.wait_for_exit(Duration::from_secs(5))?;
    assert!(status.success(), "wait exit status was {status:?}");
    Ok(())
}

#[test]
fn signal_term_reaches_session_process_group() -> Result<()> {
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

    std::thread::sleep(Duration::from_millis(500));
    let signaled = run_tmuy(home.path(), &["signal", "termy", "TERM"])?;
    assert_success(&signaled);

    let waited = run_tmuy(home.path(), &["wait", "termy", "--timeout-secs", "5"])?;
    assert_success(&waited);
    let tailed = run_tmuy(home.path(), &["tail", "termy"])?;
    assert_success(&tailed);
    assert!(String::from_utf8_lossy(&tailed.stdout).contains("term-trapped"));
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

fn run_tmuy_in_dir(home: &Path, dir: &Path, args: &[&str]) -> Result<Output> {
    let output = Command::new(tmuy_bin())
        .args(args)
        .env("TMUY_HOME", home)
        .current_dir(dir)
        .output()
        .with_context(|| format!("failed to run tmuy {:?} in {}", args, dir.display()))?;
    Ok(output)
}

fn spawn_attach(home: &std::path::Path, args: &[&str]) -> Result<AttachHarness> {
    spawn_pty_process(home, args)
}

fn spawn_pty_process(home: &std::path::Path, args: &[&str]) -> Result<AttachHarness> {
    let pty = openpty(None, None)?;
    let master = File::from(pty.master);
    let slave = File::from(pty.slave);
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

fn spawn_output_tmuy(home: &Path, args: &[&str]) -> Result<OutputHarness> {
    let mut child = Command::new(tmuy_bin())
        .args(args)
        .env("TMUY_HOME", home)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .with_context(|| format!("failed to spawn tmuy {:?}", args))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("child stdout was not piped"))?;
    Ok(OutputHarness { child, stdout })
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

fn read_pipe_ready(stdout: &mut std::process::ChildStdout, timeout: Duration) -> Result<Vec<u8>> {
    let millis = timeout.as_millis().min(u16::MAX as u128) as u16;
    let mut fds = [PollFd::new(stdout.as_fd(), PollFlags::POLLIN)];
    let ready = poll(&mut fds, PollTimeout::from(millis))?;
    if ready == 0 {
        return Ok(Vec::new());
    }

    let mut buf = [0u8; 4096];
    match stdout.read(&mut buf) {
        Ok(0) => Ok(Vec::new()),
        Ok(n) => Ok(buf[..n].to_vec()),
        Err(err) if err.kind() == io::ErrorKind::WouldBlock => Ok(Vec::new()),
        Err(err) if err.kind() == io::ErrorKind::Interrupted => Ok(Vec::new()),
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

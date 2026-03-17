use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};

use anyhow::{Context, Result};
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
fn inspect_human_prints_session_detail_lines() -> Result<()> {
    let home = TempDir::new()?;
    let created = run_tmuy(
        home.path(),
        &["--json", "new", "demo", "--", "/bin/sh", "-lc", "sleep 30"],
    )?;
    assert_success(&created);
    let created_json: Value = serde_json::from_slice(&created.stdout)?;
    let hash = created_json["id_hash"].as_str().context("missing id_hash")?;

    let output = run_tmuy(home.path(), &["inspect", "demo"])?;
    assert_success(&output);
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("name: demo"));
    assert!(text.contains(&format!("hash: {hash}")));
    assert!(text.contains("detach_key: C-b d"));

    assert_success(&run_tmuy(home.path(), &["kill", "demo"])?);
    assert_success(&run_tmuy(home.path(), &["wait", "demo", "--timeout-secs", "5"])?);
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

    let sent = run_tmuy_with_stdin(home.path(), &["send", "beta"], b"hello\n")?;
    assert_success(&sent);
    assert_eq!(String::from_utf8_lossy(&sent.stdout), "sent\n");

    let quit = run_tmuy(home.path(), &["send", "beta", "quit\n"])?;
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
    assert_success(&run_tmuy(home.path(), &["wait", "killme", "--timeout-secs", "5"])?);
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

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "command failed: status={:?}\nstdout={}\nstderr={}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

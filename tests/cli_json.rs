use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::Value;
use tempfile::TempDir;

fn tmuy_bin() -> &'static str {
    env!("CARGO_BIN_EXE_tmuy")
}

#[test]
fn new_inspect_and_rename_keep_stable_hash() -> Result<()> {
    let home = TempDir::new()?;

    let created = run_tmuy(
        home.path(),
        &[
            "--json",
            "new",
            "alpha",
            "--detached",
            "--",
            "/bin/sh",
            "-lc",
            "sleep 30",
        ],
    )?;
    assert_success(&created);
    let created_json: Value = serde_json::from_slice(&created.stdout)?;
    let id_hash = created_json["id_hash"]
        .as_str()
        .context("missing id_hash")?
        .to_string();
    assert_eq!(created_json["current_name"], "alpha");
    assert_eq!(created_json["started_name"], "alpha");

    let renamed = run_tmuy(home.path(), &["--json", "rename", &id_hash, "beta"])?;
    assert_success(&renamed);
    let renamed_json: Value = serde_json::from_slice(&renamed.stdout)?;
    assert_eq!(renamed_json["id_hash"], id_hash);
    assert_eq!(renamed_json["current_name"], "beta");
    assert_eq!(renamed_json["started_name"], "alpha");

    let inspect = run_tmuy(home.path(), &["--json", "inspect", &id_hash])?;
    assert_success(&inspect);
    let inspect_json: Value = serde_json::from_slice(&inspect.stdout)?;
    assert_eq!(inspect_json["id_hash"], id_hash);
    assert_eq!(inspect_json["current_name"], "beta");
    assert_eq!(inspect_json["started_name"], "alpha");

    assert_success(&run_tmuy(home.path(), &["kill", &id_hash])?);
    assert_success(&run_tmuy(
        home.path(),
        &["wait", &id_hash, "--timeout-secs", "5"],
    )?);
    Ok(())
}

#[test]
fn ls_json_respects_live_dead_and_all_filters() -> Result<()> {
    let home = TempDir::new()?;

    let dead = run_tmuy(
        home.path(),
        &["new", "dead", "--", "/bin/sh", "-lc", "printf dead"],
    )?;
    assert_success(&dead);

    let live = run_tmuy(
        home.path(),
        &["new", "live", "--", "/bin/sh", "-lc", "sleep 30"],
    )?;
    assert_success(&live);

    let live_only = run_tmuy(home.path(), &["--json", "ls"])?;
    assert_success(&live_only);
    let live_json: Value = serde_json::from_slice(&live_only.stdout)?;
    let live_list = live_json
        .as_array()
        .context("live ls output was not an array")?;
    assert_eq!(live_list.len(), 1);
    assert_eq!(live_list[0]["current_name"], "live");

    let dead_only = run_tmuy(home.path(), &["--json", "ls", "--dead"])?;
    assert_success(&dead_only);
    let dead_json: Value = serde_json::from_slice(&dead_only.stdout)?;
    let dead_list = dead_json
        .as_array()
        .context("dead ls output was not an array")?;
    assert_eq!(dead_list.len(), 1);
    assert_eq!(dead_list[0]["current_name"], "dead");

    let all = run_tmuy(home.path(), &["--json", "ls", "--all"])?;
    assert_success(&all);
    let all_json: Value = serde_json::from_slice(&all.stdout)?;
    let all_list = all_json
        .as_array()
        .context("all ls output was not an array")?;
    assert_eq!(all_list.len(), 2);

    let interrupted = run_tmuy(home.path(), &["kill", "live"])?;
    assert_success(&interrupted);
    let _ = run_tmuy(home.path(), &["wait", "live", "--timeout-secs", "5"])?;
    Ok(())
}

#[test]
fn failed_startup_is_reflected_in_json_inspect() -> Result<()> {
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
            "printf no",
        ],
    )?;
    assert!(!output.status.success());

    let inspect = run_tmuy(home.path(), &["--json", "inspect", "bad"])?;
    assert_success(&inspect);
    let inspect_json: Value = serde_json::from_slice(&inspect.stdout)?;
    assert_eq!(inspect_json["status"], "Failed");
    assert!(
        inspect_json["failure_reason"]
            .as_str()
            .unwrap_or_default()
            .contains("not covered")
    );
    Ok(())
}

#[test]
fn json_outputs_cover_send_wait_kill_and_signal() -> Result<()> {
    let home = TempDir::new()?;

    let echoer = run_tmuy(
        home.path(),
        &[
            "new",
            "echoer",
            "--",
            "/bin/sh",
            "-lc",
            "while IFS= read -r line; do printf 'E:%s\\n' \"$line\"; [ \"$line\" = quit ] && exit 7; done",
        ],
    )?;
    assert_success(&echoer);
    let echoer_json: Value =
        serde_json::from_slice(&run_tmuy(home.path(), &["--json", "inspect", "echoer"])?.stdout)?;
    let echoer_hash = echoer_json["id_hash"]
        .as_str()
        .context("missing echoer id_hash")?
        .to_string();

    let sent = run_tmuy(home.path(), &["--json", "send", &echoer_hash, "quit"])?;
    assert_success(&sent);
    let sent_json: Value = serde_json::from_slice(&sent.stdout)?;
    assert_eq!(sent_json["ok"], true);
    assert_eq!(sent_json["message"], "sent");

    let waited = run_tmuy(
        home.path(),
        &["--json", "wait", &echoer_hash, "--timeout-secs", "5"],
    )?;
    assert_success(&waited);
    let waited_json: Value = serde_json::from_slice(&waited.stdout)?;
    assert_eq!(waited_json["current_name"], "echoer");
    assert_eq!(waited_json["exit_code"], 7);
    let tailed = run_tmuy(home.path(), &["tail", &echoer_hash])?;
    assert_success(&tailed);
    assert!(String::from_utf8_lossy(&tailed.stdout).contains("E:quit"));

    let killme = run_tmuy(
        home.path(),
        &["new", "killme", "--", "/bin/sh", "-lc", "sleep 30"],
    )?;
    assert_success(&killme);
    let killme_json: Value =
        serde_json::from_slice(&run_tmuy(home.path(), &["--json", "inspect", "killme"])?.stdout)?;
    let killme_hash = killme_json["id_hash"]
        .as_str()
        .context("missing killme id_hash")?
        .to_string();

    let killed = run_tmuy(home.path(), &["--json", "kill", &killme_hash])?;
    assert_success(&killed);
    let killed_json: Value = serde_json::from_slice(&killed.stdout)?;
    assert_eq!(killed_json["ok"], true);
    assert_eq!(killed_json["message"], "interrupted");
    assert_success(&run_tmuy(
        home.path(),
        &["wait", &killme_hash, "--timeout-secs", "5"],
    )?);

    let termy = run_tmuy(
        home.path(),
        &[
            "new",
            "termy",
            "--",
            "/bin/sh",
            "-lc",
            "trap 'exit 0' TERM; while :; do sleep 1; done",
        ],
    )?;
    assert_success(&termy);
    let termy_json: Value =
        serde_json::from_slice(&run_tmuy(home.path(), &["--json", "inspect", "termy"])?.stdout)?;
    let termy_hash = termy_json["id_hash"]
        .as_str()
        .context("missing termy id_hash")?
        .to_string();

    let signaled = run_tmuy(home.path(), &["--json", "signal", &termy_hash, "TERM"])?;
    assert_success(&signaled);
    let signaled_json: Value = serde_json::from_slice(&signaled.stdout)?;
    assert_eq!(signaled_json["ok"], true);
    assert_eq!(signaled_json["message"], "signaled");
    assert_success(&run_tmuy(
        home.path(),
        &["wait", &termy_hash, "--timeout-secs", "5"],
    )?);
    Ok(())
}

#[test]
fn json_automation_flow_prefers_stable_hash_and_raw_bytes() -> Result<()> {
    let home = TempDir::new()?;

    let created = run_tmuy(
        home.path(),
        &[
            "--json",
            "new",
            "worker",
            "--detached",
            "--",
            "/bin/sh",
            "-lc",
            "while IFS= read -r line; do printf 'E:%s\\n' \"$line\"; [ \"$line\" = quit ] && exit 9; done",
        ],
    )?;
    assert_success(&created);
    let created_json: Value = serde_json::from_slice(&created.stdout)?;
    let hash = created_json["id_hash"]
        .as_str()
        .context("missing worker id_hash")?
        .to_string();

    let renamed = run_tmuy(home.path(), &["--json", "rename", &hash, "renamed"])?;
    assert_success(&renamed);
    let renamed_json: Value = serde_json::from_slice(&renamed.stdout)?;
    assert_eq!(renamed_json["id_hash"], hash);
    assert_eq!(renamed_json["current_name"], "renamed");

    let partial = run_tmuy_with_stdin(
        home.path(),
        &["--json", "send", "--no-enter", &hash],
        b"hello",
    )?;
    assert_success(&partial);

    let before_submit = run_tmuy(home.path(), &["tail", "--raw", &hash])?;
    assert_success(&before_submit);
    assert!(
        !before_submit
            .stdout
            .windows(b"E:hello".len())
            .any(|w| w == b"E:hello"),
        "tail unexpectedly contained submitted line before newline:\n{}",
        String::from_utf8_lossy(&before_submit.stdout)
    );

    let submit = run_tmuy_with_stdin(home.path(), &["--json", "send", &hash], b"")?;
    assert_success(&submit);

    wait_for_tail_contains(home.path(), &hash, "E:hello", Duration::from_secs(10))?;

    let quit = run_tmuy(home.path(), &["--json", "send", &hash, "quit"])?;
    assert_success(&quit);

    let waited = run_tmuy(
        home.path(),
        &["--json", "wait", &hash, "--timeout-secs", "15"],
    )?;
    assert_success(&waited);
    let waited_json: Value = serde_json::from_slice(&waited.stdout)?;
    assert_eq!(waited_json["current_name"], "renamed");
    assert_eq!(waited_json["started_name"], "worker");
    assert_eq!(waited_json["id_hash"], hash);
    assert_eq!(waited_json["exit_code"], 9);

    let tailed = run_tmuy(home.path(), &["tail", "--raw", &hash])?;
    assert_success(&tailed);
    let tailed_text = String::from_utf8_lossy(&tailed.stdout);
    assert!(tailed_text.contains("E:hello"));
    assert!(tailed_text.contains("E:quit"));

    Ok(())
}

#[test]
fn events_jsonl_follow_reports_session_lifecycle() -> Result<()> {
    let home = TempDir::new()?;

    let created = run_tmuy(
        home.path(),
        &[
            "--json",
            "new",
            "events",
            "--detached",
            "--",
            "/bin/sh",
            "-lc",
            "trap 'exit 0' TERM; while :; do sleep 1; done",
        ],
    )?;
    assert_success(&created);
    let created_json: Value = serde_json::from_slice(&created.stdout)?;
    let hash = created_json["id_hash"]
        .as_str()
        .context("missing events id_hash")?
        .to_string();

    let events = spawn_tmuy(home.path(), &["events", &hash, "--jsonl", "--follow"])?;

    let signaled = run_tmuy(home.path(), &["--json", "signal", &hash, "TERM"])?;
    assert_success(&signaled);

    let output = events.wait_with_output()?;
    assert_success(&output);

    let kinds = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(serde_json::from_str::<Value>)
        .collect::<std::result::Result<Vec<_>, _>>()?
        .into_iter()
        .filter_map(|line| line["kind"].as_str().map(ToOwned::to_owned))
        .collect::<Vec<_>>();

    assert!(kinds.iter().any(|kind| kind == "created"));
    assert!(kinds.iter().any(|kind| kind == "live"));
    assert!(kinds.iter().any(|kind| kind == "signal"));
    assert!(kinds.iter().any(|kind| kind == "exited"));

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

fn run_tmuy_in_dir(home: &Path, dir: &Path, args: &[&str]) -> Result<Output> {
    let output = Command::new(tmuy_bin())
        .args(args)
        .env("TMUY_HOME", home)
        .current_dir(dir)
        .output()
        .with_context(|| format!("failed to run tmuy {:?} in {}", args, dir.display()))?;
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
        .with_context(|| format!("failed to run tmuy {:?} with piped stdin", args))?;

    child
        .stdin
        .take()
        .context("child stdin missing")?
        .write_all(stdin_bytes)?;
    let output = child.wait_with_output()?;
    Ok(output)
}

fn spawn_tmuy(home: &Path, args: &[&str]) -> Result<std::process::Child> {
    let child = Command::new(tmuy_bin())
        .args(args)
        .env("TMUY_HOME", home)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("failed to spawn tmuy {:?}", args))?;
    Ok(child)
}

fn wait_for_tail_contains(
    home: &Path,
    target: &str,
    needle: &str,
    timeout: Duration,
) -> Result<()> {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        let tailed = run_tmuy(home, &["tail", "--raw", target])?;
        assert_success(&tailed);
        if String::from_utf8_lossy(&tailed.stdout).contains(needle) {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    anyhow::bail!("timed out waiting for tail to contain {needle:?}");
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

#[allow(dead_code)]
fn _sleep_briefly() {
    std::thread::sleep(Duration::from_millis(100));
}

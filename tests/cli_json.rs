use std::path::Path;
use std::process::{Command, Output};
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
            "--",
            "/bin/sh",
            "-lc",
            "printf hi",
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

    let renamed = run_tmuy(home.path(), &["--json", "rename", "alpha", "beta"])?;
    assert_success(&renamed);
    let renamed_json: Value = serde_json::from_slice(&renamed.stdout)?;
    assert_eq!(renamed_json["id_hash"], id_hash);
    assert_eq!(renamed_json["current_name"], "beta");
    assert_eq!(renamed_json["started_name"], "alpha");

    let inspect = run_tmuy(home.path(), &["--json", "inspect", "beta"])?;
    assert_success(&inspect);
    let inspect_json: Value = serde_json::from_slice(&inspect.stdout)?;
    assert_eq!(inspect_json["id_hash"], id_hash);
    assert_eq!(inspect_json["current_name"], "beta");
    assert_eq!(inspect_json["started_name"], "alpha");
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

    let sent = run_tmuy(home.path(), &["--json", "send", "echoer", "quit\n"])?;
    assert_success(&sent);
    let sent_json: Value = serde_json::from_slice(&sent.stdout)?;
    assert_eq!(sent_json["ok"], true);
    assert_eq!(sent_json["message"], "sent");

    let waited = run_tmuy(
        home.path(),
        &["--json", "wait", "echoer", "--timeout-secs", "5"],
    )?;
    assert_success(&waited);
    let waited_json: Value = serde_json::from_slice(&waited.stdout)?;
    assert_eq!(waited_json["current_name"], "echoer");
    assert_eq!(waited_json["exit_code"], 7);

    let killme = run_tmuy(
        home.path(),
        &["new", "killme", "--", "/bin/sh", "-lc", "sleep 30"],
    )?;
    assert_success(&killme);

    let killed = run_tmuy(home.path(), &["--json", "kill", "killme"])?;
    assert_success(&killed);
    let killed_json: Value = serde_json::from_slice(&killed.stdout)?;
    assert_eq!(killed_json["ok"], true);
    assert_eq!(killed_json["message"], "interrupted");
    assert_success(&run_tmuy(
        home.path(),
        &["wait", "killme", "--timeout-secs", "5"],
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

    let signaled = run_tmuy(home.path(), &["--json", "signal", "termy", "TERM"])?;
    assert_success(&signaled);
    let signaled_json: Value = serde_json::from_slice(&signaled.stdout)?;
    assert_eq!(signaled_json["ok"], true);
    assert_eq!(signaled_json["message"], "signaled");
    assert_success(&run_tmuy(
        home.path(),
        &["wait", "termy", "--timeout-secs", "5"],
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

fn run_tmuy_in_dir(home: &Path, dir: &Path, args: &[&str]) -> Result<Output> {
    let output = Command::new(tmuy_bin())
        .args(args)
        .env("TMUY_HOME", home)
        .current_dir(dir)
        .output()
        .with_context(|| format!("failed to run tmuy {:?} in {}", args, dir.display()))?;
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

#[allow(dead_code)]
fn _sleep_briefly() {
    std::thread::sleep(Duration::from_millis(100));
}

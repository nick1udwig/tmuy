use std::path::Path;

use anyhow::{Result, anyhow, bail};
use portable_pty::CommandBuilder;

use crate::model::{FsGrant, NetworkMode, SandboxSpec, SessionRecord};

pub fn build_command(session: &SessionRecord) -> Result<CommandBuilder> {
    if is_default(&session.sandbox) {
        return plain_command(session);
    }

    #[cfg(target_os = "linux")]
    {
        linux_bwrap_command(session)
    }

    #[cfg(not(target_os = "linux"))]
    {
        bail!("non-default sandbox is not implemented on this platform yet");
    }
}

fn plain_command(session: &SessionRecord) -> Result<CommandBuilder> {
    let mut iter = session.command.iter();
    let program = iter
        .next()
        .ok_or_else(|| anyhow!("session has no command configured"))?;
    let mut builder = CommandBuilder::new(program);
    for arg in iter {
        builder.arg(arg);
    }
    builder.cwd(&session.cwd);
    for (key, value) in &session.env {
        builder.env(key, value);
    }
    Ok(builder)
}

fn is_default(spec: &SandboxSpec) -> bool {
    matches!(spec.fs.as_slice(), [FsGrant::Full]) && matches!(spec.net, NetworkMode::On)
}

#[cfg(target_os = "linux")]
fn linux_bwrap_command(session: &SessionRecord) -> Result<CommandBuilder> {
    let mut builder = CommandBuilder::new("bwrap");
    builder.arg("--new-session");
    builder.arg("--die-with-parent");
    builder.arg("--unshare-all");
    if matches!(session.sandbox.net, NetworkMode::On) {
        builder.arg("--share-net");
    }

    match session.sandbox.fs.as_slice() {
        [FsGrant::Full] => {
            builder.arg("--dev-bind");
            builder.arg("/");
            builder.arg("/");
            builder.arg("--proc");
            builder.arg("/proc");
            builder.arg("--chdir");
            builder.arg(path_arg(&session.cwd)?);
        }
        grants => {
            ensure_cwd_allowed(&session.cwd, grants)?;
            builder.arg("--proc");
            builder.arg("/proc");
            builder.arg("--dev");
            builder.arg("/dev");
            builder.arg("--tmpfs");
            builder.arg("/tmp");

            for path in ["/bin", "/usr"] {
                add_ro_bind(&mut builder, path, false);
            }
            for path in ["/sbin", "/lib", "/lib64", "/etc", "/opt", "/run"] {
                add_ro_bind(&mut builder, path, true);
            }

            for grant in grants {
                if let FsGrant::ReadOnly(path) = grant {
                    add_bind_for_grant(&mut builder, path, true)?;
                }
            }
            for grant in grants {
                if let FsGrant::ReadWrite(path) = grant {
                    add_bind_for_grant(&mut builder, path, false)?;
                }
            }

            builder.arg("--chdir");
            builder.arg(path_arg(&session.cwd)?);
        }
    }

    builder.arg("--");
    let mut iter = session.command.iter();
    let program = iter
        .next()
        .ok_or_else(|| anyhow!("session has no command configured"))?;
    builder.arg(program);
    for arg in iter {
        builder.arg(arg);
    }
    builder.cwd(&session.cwd);
    for (key, value) in &session.env {
        builder.env(key, value);
    }
    Ok(builder)
}

#[cfg(target_os = "linux")]
fn ensure_cwd_allowed(cwd: &Path, grants: &[FsGrant]) -> Result<()> {
    if grants.iter().any(|grant| grant_allows_cwd(grant, cwd)) {
        return Ok(());
    }
    bail!(
        "sandbox cwd {} is not covered by any --fs grant; use --fs ro:. or --fs rw:. from that directory, or cd into a granted directory first",
        cwd.display()
    )
}

#[cfg(target_os = "linux")]
fn grant_allows_cwd(grant: &FsGrant, cwd: &Path) -> bool {
    match grant {
        FsGrant::Full => true,
        FsGrant::ReadOnly(path) | FsGrant::ReadWrite(path) => cwd.starts_with(path),
    }
}

#[cfg(target_os = "linux")]
fn add_ro_bind(builder: &mut CommandBuilder, path: &str, optional: bool) {
    if optional {
        builder.arg("--ro-bind-try");
    } else {
        builder.arg("--ro-bind");
    }
    builder.arg(path);
    builder.arg(path);
}

#[cfg(target_os = "linux")]
fn add_bind_for_grant(builder: &mut CommandBuilder, path: &Path, readonly: bool) -> Result<()> {
    let arg = if readonly { "--ro-bind" } else { "--bind" };
    let path = path_arg(path)?;
    builder.arg(arg);
    builder.arg(&path);
    builder.arg(&path);
    Ok(())
}

fn path_arg(path: &Path) -> Result<String> {
    path.to_str()
        .map(ToOwned::to_owned)
        .ok_or_else(|| anyhow!("non-utf8 paths are not supported yet: {}", path.display()))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::ffi::OsString;
    use std::path::Path;
    use std::path::PathBuf;

    use chrono::Utc;
    use super::*;
    use crate::model::{CommandMode, SessionStatus};

    fn sample_session(sandbox: SandboxSpec) -> SessionRecord {
        let mut env = BTreeMap::new();
        env.insert("TEST_ENV".to_string(), "VALUE".to_string());
        SessionRecord {
            id_hash: "1a2b3c4".to_string(),
            started_name: "demo".to_string(),
            current_name: "demo".to_string(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
            cwd: PathBuf::from("/tmp/demo"),
            command: vec!["/bin/sh".to_string(), "-lc".to_string(), "printf ok".to_string()],
            mode: CommandMode::OneShot,
            sandbox,
            status: SessionStatus::Starting,
            started_log_dir: PathBuf::from("/tmp/log"),
            meta_path: PathBuf::from("/tmp/log/meta.json"),
            log_path: PathBuf::from("/tmp/log/pty.log"),
            events_path: PathBuf::from("/tmp/log/events.jsonl"),
            socket_path: PathBuf::from("/tmp/log/live.sock"),
            service_pid: None,
            child_pid: None,
            exit_code: None,
            failure_reason: None,
            env,
            detach_key: "C-b d".to_string(),
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn cwd_must_be_within_grant() {
        let err = ensure_cwd_allowed(
            Path::new("/work/project"),
            &[FsGrant::ReadOnly("/work/project/subdir".into())],
        )
        .unwrap_err();
        assert!(err.to_string().contains("not covered"));
    }

    #[test]
    fn default_sandbox_uses_plain_command() {
        let builder = build_command(&sample_session(SandboxSpec::default())).unwrap();
        let debug = format!("{builder:?}");
        assert!(debug.contains("/bin/sh"));
        assert!(debug.contains("printf ok"));
        assert!(debug.contains("/tmp/demo"));
        assert!(debug.contains("TEST_ENV"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn full_fs_non_default_sandbox_uses_bwrap_full_branch() {
        let session = sample_session(SandboxSpec {
            fs: vec![FsGrant::Full],
            net: NetworkMode::Off,
        });
        let builder = linux_bwrap_command(&session).unwrap();
        let debug = format!("{builder:?}");
        assert!(debug.contains("bwrap"));
        assert!(debug.contains("--dev-bind"));
        assert!(debug.contains("/proc"));
        assert!(!debug.contains("--share-net"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn grant_allows_cwd_accepts_full() {
        assert!(grant_allows_cwd(&FsGrant::Full, Path::new("/anywhere")));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn path_arg_rejects_non_utf8() {
        use std::os::unix::ffi::OsStringExt;

        let path = PathBuf::from(OsString::from_vec(vec![0xff, 0xfe]));
        let err = path_arg(&path).unwrap_err();
        assert!(err.to_string().contains("non-utf8"));
    }
}

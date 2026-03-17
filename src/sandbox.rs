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
    use std::path::Path;

    use super::*;

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
}

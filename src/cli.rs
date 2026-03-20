use std::collections::BTreeMap;
use std::io::IsTerminal;
use std::time::Duration;

use anyhow::{Result, bail};
use clap::{ArgAction, Parser, Subcommand};
use serde::Serialize;

use crate::model::{CommandMode, SessionRecord, SessionScope};
use crate::runtime;
use crate::store::{CreateSessionRequest, Store, parse_sandbox, validate_name};

#[derive(Parser, Debug)]
#[command(
    name = "tmuy",
    version,
    about = "Terminal multiplexer for one-terminal sessions"
)]
struct Cli {
    /// Emit machine-readable JSON instead of human-oriented text output.
    #[arg(long, global = true, action = ArgAction::SetTrue)]
    json: bool,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Create a new session. Starts an interactive shell by default.
    ///
    /// When run from a terminal, shell sessions attach immediately unless
    /// `--detached` is passed. One-off commands stay detached and exit when the
    /// child process exits.
    #[command(alias = "n")]
    New(NewArgs),
    /// Attach to a live session and stream its terminal output.
    #[command(alias = "a")]
    Attach(AttachArgs),
    /// Send a Ctrl+C-style interrupt to a live session.
    Kill(KillArgs),
    /// List live sessions by default, or dead/all sessions with flags.
    #[command(alias = "l", alias = "list")]
    Ls(ListArgs),
    /// Print or follow terminal output from a session log.
    Tail(TailArgs),
    /// Show full metadata for a session, including paths and sandbox settings.
    Inspect(NameArgs),
    /// Send input to a detached or attached live session, pressing Enter by default.
    Send(SendArgs),
    /// Rename a live session without changing its stable hash.
    Rename(RenameArgs),
    /// Wait for a session to exit, optionally with a timeout.
    Wait(WaitArgs),
    /// Send a specific POSIX signal to a live session process group.
    Signal(SignalArgs),
    #[command(hide = true, name = "__serve")]
    Serve(ServeArgs),
}

#[derive(clap::Args, Debug)]
struct NewArgs {
    /// Optional session name. If omitted, tmuy uses the next numeric name.
    name: Option<String>,

    /// Filesystem grants for the child process, for example `full`, `ro:.`, or `rw:/tmp`.
    #[arg(long = "fs")]
    fs: Vec<String>,

    /// Network access mode for the child process: `on` or `off`.
    #[arg(long = "net")]
    net: Option<String>,

    /// Detach key sequence used by attached clients for this session.
    #[arg(long, default_value = "C-b d")]
    detach_key: String,

    /// Create the session but do not attach to it, even in an interactive terminal.
    #[arg(long, action = ArgAction::SetTrue)]
    detached: bool,

    /// Optional one-off command to run instead of starting an interactive shell.
    #[arg(last = true)]
    command: Vec<String>,
}

#[derive(clap::Args, Debug)]
struct AttachArgs {
    /// Live session name or hash to attach to.
    name: String,

    /// Override the session's stored detach key for this attach client.
    #[arg(long)]
    detach_key: Option<String>,
}

#[derive(clap::Args, Debug)]
struct KillArgs {
    /// Live session name or hash to interrupt.
    name: String,
}

#[derive(clap::Args, Debug)]
struct ListArgs {
    /// Show only exited or failed sessions.
    #[arg(long)]
    dead: bool,

    /// Show both live and dead sessions.
    #[arg(long)]
    all: bool,
}

#[derive(clap::Args, Debug)]
struct TailArgs {
    /// Session name or hash to read from.
    name: String,

    /// Emit raw PTY bytes instead of cooked text.
    #[arg(long)]
    raw: bool,

    /// Keep streaming new output until the session exits and the log is drained.
    #[arg(short = 'f', long)]
    follow: bool,
}

#[derive(clap::Args, Debug)]
struct NameArgs {
    /// Session name or hash to inspect.
    name: String,
}

#[derive(clap::Args, Debug)]
struct SendArgs {
    /// Live session name or hash to write to.
    name: String,

    /// Send the bytes exactly as provided without pressing Enter afterwards.
    #[arg(long, action = ArgAction::SetTrue)]
    no_enter: bool,

    /// Literal payload to send. If omitted, tmuy reads bytes from stdin.
    payload: Option<String>,
}

#[derive(clap::Args, Debug)]
struct RenameArgs {
    /// Current live session name or hash.
    name: String,
    /// New live session name.
    new_name: String,
}

#[derive(clap::Args, Debug)]
struct WaitArgs {
    /// Session name or hash to wait on.
    name: String,

    /// Maximum time to wait before returning an error.
    #[arg(long)]
    timeout_secs: Option<u64>,
}

#[derive(clap::Args, Debug)]
struct SignalArgs {
    /// Live session name or hash to signal.
    name: String,
    /// Signal name such as `INT`, `TERM`, or `HUP`.
    signal: String,
}

#[derive(clap::Args, Debug)]
struct ServeArgs {
    hash: String,
}

#[derive(Serialize)]
struct BasicOutput<'a> {
    ok: bool,
    message: &'a str,
}

pub fn run() -> Result<()> {
    let cli = Cli::parse();
    let store = Store::new()?;

    match cli.command {
        Commands::New(args) => cmd_new(&store, cli.json, args),
        Commands::Attach(args) => {
            ensure_attach_allowed()?;
            let session = store.resolve_target(&args.name, SessionScope::LiveOnly)?;
            let detach_key = args.detach_key.as_deref().unwrap_or(&session.detach_key);
            runtime::attach(&store, &args.name, detach_key)?;
            if cli.json {
                print_json(&BasicOutput {
                    ok: true,
                    message: "detached",
                })?;
            }
            Ok(())
        }
        Commands::Kill(args) => {
            runtime::signal_session(&store, &args.name, "INT")?;
            print_maybe_json(
                cli.json,
                &BasicOutput {
                    ok: true,
                    message: "interrupted",
                },
                "interrupted",
            )?;
            Ok(())
        }
        Commands::Ls(args) => cmd_ls(&store, cli.json, args),
        Commands::Tail(args) => runtime::tail(&store, &args.name, args.raw, args.follow),
        Commands::Inspect(args) => {
            let session = store.resolve_target(&args.name, SessionScope::All)?;
            if cli.json {
                print_json(&session)?;
            } else {
                print_session_detail(&session);
            }
            Ok(())
        }
        Commands::Send(args) => {
            let mut bytes = match args.payload {
                Some(payload) => payload.into_bytes(),
                None => {
                    let mut buf = Vec::new();
                    std::io::Read::read_to_end(&mut std::io::stdin(), &mut buf)?;
                    buf
                }
            };
            maybe_append_enter(&mut bytes, args.no_enter);
            runtime::send_input(&store, &args.name, &bytes)?;
            print_maybe_json(
                cli.json,
                &BasicOutput {
                    ok: true,
                    message: "sent",
                },
                "sent",
            )?;
            Ok(())
        }
        Commands::Rename(args) => {
            validate_name(&args.new_name)?;
            let renamed = store.rename_session(&args.name, &args.new_name)?;
            if cli.json {
                print_json(&renamed)?;
            } else {
                println!(
                    "renamed {} -> {} ({})",
                    args.name, renamed.current_name, renamed.id_hash
                );
            }
            Ok(())
        }
        Commands::Wait(args) => {
            let timeout = args.timeout_secs.map(Duration::from_secs);
            let session = runtime::wait_for_exit(&store, &args.name, timeout)?;
            if cli.json {
                print_json(&session)?;
            } else {
                println!(
                    "{} exited with {:?}",
                    session.short_ref(),
                    session.exit_code
                );
            }
            Ok(())
        }
        Commands::Signal(args) => {
            runtime::signal_session(&store, &args.name, &args.signal)?;
            print_maybe_json(
                cli.json,
                &BasicOutput {
                    ok: true,
                    message: "signaled",
                },
                "signaled",
            )?;
            Ok(())
        }
        Commands::Serve(args) => match runtime::run_server(&store, &args.hash) {
            Ok(()) => Ok(()),
            Err(err) => {
                let _ = store.mark_failed(&args.hash, err.to_string());
                Err(err)
            }
        },
    }
}

fn cmd_new(store: &Store, json: bool, args: NewArgs) -> Result<()> {
    if let Some(name) = args.name.as_deref() {
        validate_name(name)?;
    }
    let cwd = std::env::current_dir()?;
    let sandbox = parse_sandbox(&args.fs, args.net.as_deref(), &cwd)?;
    let env = std::env::vars().collect::<BTreeMap<_, _>>();
    let (mode, command) = if args.command.is_empty() {
        (CommandMode::Shell, runtime::default_shell_command())
    } else {
        (CommandMode::OneShot, args.command)
    };
    let auto_attach = should_auto_attach(json, args.detached, &mode);
    if auto_attach {
        ensure_attach_allowed()?;
    }

    let session = store.create_session(CreateSessionRequest {
        explicit_name: args.name,
        cwd,
        command,
        mode,
        sandbox,
        detach_key: args.detach_key,
        env,
    })?;
    runtime::spawn_daemon(store, &session)?;
    let refreshed = store.session_by_hash(&session.id_hash)?;

    if auto_attach {
        runtime::attach(store, &refreshed.current_name, &refreshed.detach_key)?;
    } else if json {
        print_json(&refreshed)?;
    } else {
        println!("created {} ({})", refreshed.current_name, refreshed.id_hash);
    }
    Ok(())
}

fn should_auto_attach(json: bool, detached: bool, mode: &CommandMode) -> bool {
    !json
        && !detached
        && matches!(mode, CommandMode::Shell)
        && std::io::stdin().is_terminal()
        && std::io::stdout().is_terminal()
}

fn ensure_attach_allowed() -> Result<()> {
    let inside_tmuy = std::env::var_os("TMUY_INSIDE").as_deref() == Some("1".as_ref());
    if inside_tmuy && let Some(hash) = std::env::var_os("TMUY_SESSION_HASH") {
        bail!(
            "cannot attach from inside tmuy session {}; detach first or create the new session with --detached",
            hash.to_string_lossy()
        );
    }
    Ok(())
}

fn maybe_append_enter(bytes: &mut Vec<u8>, no_enter: bool) {
    if no_enter {
        return;
    }
    if matches!(bytes.last(), Some(b'\n' | b'\r')) {
        return;
    }
    bytes.push(b'\n');
}

#[cfg(test)]
mod tests {
    use super::maybe_append_enter;

    #[test]
    fn maybe_append_enter_adds_newline_once() {
        let mut bytes = b"echo hello".to_vec();
        maybe_append_enter(&mut bytes, false);
        assert_eq!(bytes, b"echo hello\n");

        maybe_append_enter(&mut bytes, false);
        assert_eq!(bytes, b"echo hello\n");
    }

    #[test]
    fn maybe_append_enter_respects_no_enter_and_carriage_return() {
        let mut bytes = b"raw".to_vec();
        maybe_append_enter(&mut bytes, true);
        assert_eq!(bytes, b"raw");

        let mut bytes = b"line\r".to_vec();
        maybe_append_enter(&mut bytes, false);
        assert_eq!(bytes, b"line\r");
    }
}

fn cmd_ls(store: &Store, json: bool, args: ListArgs) -> Result<()> {
    if args.dead && args.all {
        bail!("--dead and --all are mutually exclusive");
    }
    let scope = if args.all {
        SessionScope::All
    } else if args.dead {
        SessionScope::DeadOnly
    } else {
        SessionScope::LiveOnly
    };
    let sessions = store.list_sessions(scope)?;
    if json {
        print_json(&sessions)?;
    } else if sessions.is_empty() {
        println!("no sessions");
    } else {
        for session in sessions {
            println!(
                "{}\t{}\t{:?}\t{}",
                session.current_name,
                session.id_hash,
                session.status,
                session.started_log_dir.display()
            );
        }
    }
    Ok(())
}

fn print_session_detail(session: &SessionRecord) {
    println!("name: {}", session.current_name);
    println!("started_name: {}", session.started_name);
    println!("hash: {}", session.id_hash);
    println!("status: {:?}", session.status);
    println!("created_at: {}", session.created_at);
    println!("cwd: {}", session.cwd.display());
    println!("command: {}", shell_words(&session.command));
    println!("log_dir: {}", session.started_log_dir.display());
    println!("log_path: {}", session.log_path.display());
    println!("meta_path: {}", session.meta_path.display());
    println!("events_path: {}", session.events_path.display());
    println!("child_pid: {:?}", session.child_pid);
    println!("service_pid: {:?}", session.service_pid);
    println!("exit_code: {:?}", session.exit_code);
    println!("failure_reason: {:?}", session.failure_reason);
    println!("detach_key: {}", session.detach_key);
}

fn shell_words(parts: &[String]) -> String {
    parts
        .iter()
        .map(|part| {
            if part.contains(' ') {
                format!("{part:?}")
            } else {
                part.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn print_json<T: Serialize>(value: &T) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

fn print_maybe_json<T: Serialize>(json: bool, value: &T, text: &str) -> Result<()> {
    if json {
        print_json(value)
    } else {
        println!("{text}");
        Ok(())
    }
}

use std::collections::BTreeMap;
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
    #[arg(long, global = true, action = ArgAction::SetTrue)]
    json: bool,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    #[command(alias = "n")]
    New(NewArgs),
    #[command(alias = "a")]
    Attach(AttachArgs),
    Kill(KillArgs),
    #[command(alias = "l", alias = "list")]
    Ls(ListArgs),
    Tail(TailArgs),
    Inspect(NameArgs),
    Send(SendArgs),
    Rename(RenameArgs),
    Wait(WaitArgs),
    Signal(SignalArgs),
    #[command(hide = true, name = "__serve")]
    Serve(ServeArgs),
}

#[derive(clap::Args, Debug)]
struct NewArgs {
    name: Option<String>,

    #[arg(long = "fs")]
    fs: Vec<String>,

    #[arg(long = "net")]
    net: Option<String>,

    #[arg(long, default_value = "C-b d")]
    detach_key: String,

    #[arg(last = true)]
    command: Vec<String>,
}

#[derive(clap::Args, Debug)]
struct AttachArgs {
    name: String,

    #[arg(long, default_value = "C-b d")]
    detach_key: String,
}

#[derive(clap::Args, Debug)]
struct KillArgs {
    name: String,
}

#[derive(clap::Args, Debug)]
struct ListArgs {
    #[arg(long)]
    dead: bool,

    #[arg(long)]
    all: bool,
}

#[derive(clap::Args, Debug)]
struct TailArgs {
    name: String,

    #[arg(long)]
    raw: bool,

    #[arg(short = 'f', long)]
    follow: bool,
}

#[derive(clap::Args, Debug)]
struct NameArgs {
    name: String,
}

#[derive(clap::Args, Debug)]
struct SendArgs {
    name: String,

    payload: Option<String>,
}

#[derive(clap::Args, Debug)]
struct RenameArgs {
    name: String,
    new_name: String,
}

#[derive(clap::Args, Debug)]
struct WaitArgs {
    name: String,

    #[arg(long)]
    timeout_secs: Option<u64>,
}

#[derive(clap::Args, Debug)]
struct SignalArgs {
    name: String,
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
            runtime::attach(&store, &args.name, &args.detach_key)?;
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
            let session = store.resolve_by_name(&args.name, SessionScope::All)?;
            if cli.json {
                print_json(&session)?;
            } else {
                print_session_detail(&session);
            }
            Ok(())
        }
        Commands::Send(args) => {
            let bytes = match args.payload {
                Some(payload) => payload.into_bytes(),
                None => {
                    let mut buf = Vec::new();
                    std::io::Read::read_to_end(&mut std::io::stdin(), &mut buf)?;
                    buf
                }
            };
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
        Commands::Serve(args) => runtime::run_server(&store, &args.hash),
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

    if json {
        print_json(&refreshed)?;
    } else {
        println!("created {} ({})", refreshed.current_name, refreshed.id_hash);
    }
    Ok(())
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

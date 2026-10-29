use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use ss::client::Client;
use ss::daemon::{RunOutcome, run};
use ss::settings::Settings;
use ss_proto::{DaemonStatus, Request, Response};

/// The shared `ss` daemon and its admin commands. `ss` starts it on demand; these manage it.
#[derive(Debug, Parser)]
#[command(name = "ssd", version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run the daemon in the foreground (what `ss` spawns detached)
    Serve {
        /// Leave the controlling terminal's session first
        #[arg(long)]
        setsid: bool,
    },
    /// Show whether the daemon is running and what it holds
    Status,
    /// Ask the running daemon to exit
    Stop,
    /// Manage watch folders
    Watch {
        #[command(subcommand)]
        action: WatchAction,
    },
}

#[derive(Debug, Subcommand)]
enum WatchAction {
    /// Keep a folder indexed automatically
    Add { path: PathBuf },
    /// Stop watching a folder (its index is kept)
    Rm { path: PathBuf },
    /// List watched folders
    List,
}

#[proxima::main(runtime = "tokio", flavor = "multi_thread")]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    if matches!(cli.command, Command::Serve { setsid: true }) {
        ss_ipc::become_session_leader();
    }
    let settings = match Settings::load() {
        Ok(settings) => settings,
        Err(error) => return fail(&format!("bad SS_* configuration: {error}")),
    };
    let outcome = match cli.command {
        Command::Serve { .. } => serve(settings).await,
        Command::Status => status(&Client::new(settings)).await,
        Command::Stop => stop(&Client::new(settings)).await,
        Command::Watch { action } => watch(&Client::new(settings), action).await,
    };
    outcome.unwrap_or_else(|message| fail(&message))
}

fn fail(message: &str) -> ExitCode {
    eprintln!("ssd: {message}");
    ExitCode::from(2)
}

async fn serve(settings: Settings) -> Result<ExitCode, String> {
    match run(settings, None)
        .await
        .map_err(|error| error.to_string())?
    {
        RunOutcome::AlreadyRunning | RunOutcome::Stopped => Ok(ExitCode::SUCCESS),
    }
}

async fn status(client: &Client) -> Result<ExitCode, String> {
    match client
        .send_if_running(Request::Status)
        .await
        .map_err(|error| error.to_string())?
    {
        None => {
            println!("ssd: not running");
            Ok(ExitCode::from(1))
        }
        Some(Response::Status { status }) => {
            print_status(&status);
            Ok(ExitCode::SUCCESS)
        }
        Some(other) => Err(format!("unexpected reply: {other:?}")),
    }
}

fn print_status(status: &DaemonStatus) {
    println!(
        "ssd: running pid={} uptime={}s",
        status.pid, status.uptime_secs
    );
    println!("roots: {}", status.roots.len());
    status.roots.iter().for_each(|root| println!("  {root}"));
    println!(
        "watches: {} ({} active watchers)",
        status.watches.len(),
        status.active_watchers
    );
    status
        .watches
        .iter()
        .for_each(|watch| println!("  {watch}"));
    println!(
        "queries: {} running, {} waiting, {} rejected busy; index rejected busy: {}",
        status.queries_running, status.queries_waiting, status.queries_busy, status.index_busy
    );
    println!(
        "chunks: {} seen, {} embedded, {} skipped",
        status.chunks_seen, status.chunks_embedded, status.chunks_skipped
    );
}

async fn stop(client: &Client) -> Result<ExitCode, String> {
    match client
        .send_if_running(Request::Stop)
        .await
        .map_err(|error| error.to_string())?
    {
        None => println!("ssd: not running"),
        Some(_) => println!("ssd: stopping"),
    }
    Ok(ExitCode::SUCCESS)
}

async fn watch(client: &Client, action: WatchAction) -> Result<ExitCode, String> {
    let request = match action {
        WatchAction::Add { path } => Request::WatchAdd {
            path: absolute(&path)?,
        },
        WatchAction::Rm { path } => Request::WatchRm {
            path: absolute(&path)?,
        },
        WatchAction::List => Request::WatchList,
    };
    match client
        .send(request)
        .await
        .map_err(|error| error.to_string())?
    {
        Response::Watches { paths } => {
            for path in &paths {
                println!("{path}");
            }
            Ok(ExitCode::SUCCESS)
        }
        Response::Ok => Ok(ExitCode::SUCCESS),
        Response::Error { message, .. } => Err(message),
        other => Err(format!("unexpected reply: {other:?}")),
    }
}

fn absolute(path: &Path) -> Result<String, String> {
    std::fs::canonicalize(path)
        .map(|canonical| canonical.to_string_lossy().into_owned())
        .map_err(|error| format!("{}: {error}", path.display()))
}

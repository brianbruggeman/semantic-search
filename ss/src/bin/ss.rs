use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use ss::client::Client;
use ss::settings::Settings;
use ss_proto::{Hit, Request, Response};

const EXIT_HITS: u8 = 0;
const EXIT_NONE: u8 = 1;
const EXIT_ERROR: u8 = 2;

/// Semantic code search backed by the shared `ssd` daemon, which `ss` starts on demand.
#[derive(Debug, Parser)]
#[command(name = "ss", version, args_conflicts_with_subcommands = true)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
    /// Search terms; quote nothing, they are joined with spaces
    query: Vec<String>,
    /// Directory or file to search under (default: the current directory)
    #[arg(long)]
    path: Option<PathBuf>,
    /// Maximum hits to print
    #[arg(short = 'n', long, default_value_t = 10)]
    limit: u32,
    /// Print hits as JSON lines
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Index a directory (created as a root on first use) and wait for it to finish
    Index { path: PathBuf },
}

#[proxima::main(runtime = "tokio")]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    let client = match Settings::load() {
        Ok(settings) => Client::new(settings),
        Err(error) => return fail(&format!("bad SS_* configuration: {error}")),
    };
    let outcome = match cli.command {
        Some(Command::Index { path }) => run_index(&client, &path).await,
        None => run_search(&client, &cli).await,
    };
    outcome.unwrap_or_else(|message| fail(&message))
}

fn fail(message: &str) -> ExitCode {
    eprintln!("ss: {message}");
    ExitCode::from(EXIT_ERROR)
}

fn absolute(path: &Path) -> Result<String, String> {
    std::fs::canonicalize(path)
        .map(|canonical| canonical.to_string_lossy().into_owned())
        .map_err(|error| format!("{}: {error}", path.display()))
}

async fn run_index(client: &Client, path: &Path) -> Result<ExitCode, String> {
    let request = Request::Index {
        path: absolute(path)?,
    };
    match client
        .send(request)
        .await
        .map_err(|error| error.to_string())?
    {
        Response::Indexed { stats } => {
            println!(
                "indexed {} files ({} unchanged, {} removed): {} chunks, {} embedded, {} skipped",
                stats.files_seen,
                stats.files_unchanged,
                stats.files_removed,
                stats.chunks_seen,
                stats.chunks_embedded,
                stats.chunks_skipped
            );
            Ok(ExitCode::from(EXIT_HITS))
        }
        Response::Error { message, .. } => Err(message),
        other => Err(format!("unexpected reply: {other:?}")),
    }
}

async fn run_search(client: &Client, cli: &Cli) -> Result<ExitCode, String> {
    if cli.query.is_empty() {
        return Err("no search terms given (try `ss walk files respecting gitignore`)".to_owned());
    }
    let here = std::env::current_dir().map_err(|error| error.to_string())?;
    let scope = absolute(cli.path.as_deref().unwrap_or(&here))?;
    let request = Request::Search {
        query: cli.query.join(" "),
        path: scope,
        limit: cli.limit,
    };
    match client
        .send(request)
        .await
        .map_err(|error| error.to_string())?
    {
        Response::Hits { hits } => Ok(print_hits(&hits, &here, cli.json)),
        Response::Error { message, .. } => Err(message),
        other => Err(format!("unexpected reply: {other:?}")),
    }
}

fn print_hits(hits: &[Hit], here: &Path, json: bool) -> ExitCode {
    for hit in hits {
        if json {
            println!("{}", serde_json::to_string(hit).unwrap_or_default());
            continue;
        }
        let shown = Path::new(&hit.path)
            .strip_prefix(here)
            .map_or(hit.path.clone(), |rel| rel.display().to_string());
        println!(
            "{shown}:{}-{}  {:.4}",
            hit.start_line, hit.end_line, hit.score
        );
        hit.preview.iter().for_each(|line| println!("    {line}"));
    }
    ExitCode::from(if hits.is_empty() {
        EXIT_NONE
    } else {
        EXIT_HITS
    })
}

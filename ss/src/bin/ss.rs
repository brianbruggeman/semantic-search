use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand};
use ss::client::Client;
use ss::exact::{compile, scan};
use ss::history::{Source, View, render as render_history};
use ss::render::{
    COMPACT_BUDGET, Compact, EXACT_BUDGET, compact_text, exact_text, facets, findings_json,
    findings_text, graph_json, graph_text, lines_text,
};
use ss::settings::Settings;
use ss_proto::{Hit, Request, Response};

const EXIT_HITS: u8 = 0;
const EXIT_NONE: u8 = 1;
const EXIT_ERROR: u8 = 2;

/// Semantic code search backed by the shared `ssd` daemon, which `ss` starts on demand.
// clap switches are independent flags, not a state machine
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Parser)]
#[command(name = "ss", version, args_conflicts_with_subcommands = true)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
    /// Search terms; bare words are joined into one query, but if any word holds a space each word is its own query
    query: Vec<String>,
    /// One more query to answer in the same call (repeatable); results are grouped per query
    #[arg(short = 'q', long = "query")]
    queries: Vec<String>,
    /// Directory or file to search under (default: the current directory)
    #[arg(long)]
    path: Option<PathBuf>,
    /// Maximum hits per query
    #[arg(short = 'n', long, default_value_t = 10)]
    limit: u32,
    /// Print the structured reply as one JSON document (schema in `ai_docs/graph.md`); with --lines, one hit per line
    #[arg(long)]
    json: bool,
    /// Cap the whole response at about this many tokens (chars / 4), keeping hits in rank order and the top hit always
    #[arg(long, conflicts_with = "lines")]
    budget: Option<usize>,
    /// Compact reply: paths grouped by file, signature and doc for the top hit, budget 250 tokens unless --budget is given
    #[arg(long, conflicts_with_all = ["lines", "json"])]
    compact: bool,
    /// Exact search: the words are one regex, matched line by line over the indexed tree (no index needed)
    #[arg(long, conflicts_with_all = ["lines", "json", "compact"])]
    exact: bool,
    /// Exact search for a literal string (implies --exact)
    #[arg(short = 'F', long = "fixed-strings", conflicts_with_all = ["lines", "json", "compact"])]
    fixed: bool,
    /// Ignore case in an exact search
    #[arg(short = 'i', long)]
    ignore_case: bool,
    /// Matches shown per file in an exact search; more are counted as `+N more`
    #[arg(long, default_value_t = 5)]
    per_file: usize,
    /// Print the plain format: path:start-end, score and the first lines of each chunk
    #[arg(long)]
    lines: bool,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Symbol-level summary of one commit: files by size with the symbols touched, no daemon or index
    Show {
        /// Commit to summarize
        rev: String,
        #[command(flatten)]
        args: HistoryArgs,
    },
    /// Symbol-level summary of a diff: working tree vs index, `--staged`, `<rev>` vs working tree, or `<rev1> <rev2>`
    Diff {
        /// Up to two revisions
        #[arg(num_args = 0..=2)]
        revs: Vec<String>,
        /// Compare the index with HEAD (or with the given revision)
        #[arg(long)]
        staged: bool,
        #[command(flatten)]
        args: HistoryArgs,
    },
    /// Index a directory (created as a root on first use) and wait for it to finish
    Index { path: PathBuf },
    /// Show the neighbourhood of a symbol name or path:line: members, callers, callees, importers, tests
    Graph {
        /// A symbol name, a `qpath` suffix such as `query::rrf_fuse`, or `path:line`
        target: String,
        /// 1 for direct neighbours, 2 to add the next hop
        #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u8).range(1..=2))]
        depth: u8,
        /// Print the reply as JSON
        #[arg(long)]
        json: bool,
        /// Directory or file to search under (default: the current directory)
        #[arg(long)]
        path: Option<PathBuf>,
    },
}

/// Drill-down and budget switches shared by `show` and `diff`.
#[derive(Debug, Args)]
struct HistoryArgs {
    /// Cap the reply at about this many tokens (chars / 4); the summary defaults to 400, drill-downs to 1500
    #[arg(long)]
    budget: Option<usize>,
    /// Print this file's hunks instead of the summary
    #[arg(long, conflicts_with_all = ["symbol", "full"])]
    file: Option<String>,
    /// Print only the hunks inside the symbol with this name or qualified path
    #[arg(long, conflicts_with = "full")]
    symbol: Option<String>,
    /// Print the raw git patch
    #[arg(long)]
    full: bool,
    /// Context lines around hunks in --file and --symbol (default 3)
    #[arg(long)]
    context: Option<u32>,
}

impl HistoryArgs {
    fn view(&self) -> View {
        match (&self.file, &self.symbol, self.full) {
            (Some(path), _, _) => View::File(path.clone()),
            (_, Some(name), _) => View::Symbol(name.clone()),
            (_, _, true) => View::Full,
            _ => View::Summary,
        }
    }
}

#[proxima::main(runtime = "tokio")]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    match &cli.command {
        Some(Command::Show { rev, args }) => return run_history(&Source::Show(rev.clone()), args),
        Some(Command::Diff { revs, staged, args }) => {
            let source = Source::Diff { revs: revs.clone(), staged: *staged };
            return run_history(&source, args);
        }
        _ => {}
    }
    let client = match Settings::load() {
        Ok(settings) => Client::new(settings),
        Err(error) => return fail(&format!("bad SS_* configuration: {error}")),
    };
    let outcome = match cli.command {
        Some(Command::Show { .. } | Command::Diff { .. }) => unreachable_history(),
        Some(Command::Index { path }) => run_index(&client, &path).await,
        Some(Command::Graph {
            target,
            depth,
            json,
            path,
        }) => run_graph(&client, &target, depth, json, path.as_deref()).await,
        None => run_search(&client, &cli).await,
    };
    outcome.unwrap_or_else(|message| fail(&message))
}

fn unreachable_history() -> Result<ExitCode, String> {
    Err("history subcommands are handled before the daemon client".to_owned())
}

fn run_history(source: &Source, args: &HistoryArgs) -> ExitCode {
    let here = match std::env::current_dir() {
        Ok(dir) => dir,
        Err(error) => return fail(&error.to_string()),
    };
    match render_history(&here, source, &args.view(), args.budget, args.context) {
        Ok(text) => {
            print!("{text}");
            exit_for(!text.is_empty())
        }
        Err(error) => fail(&error.to_string()),
    }
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
    if cli.exact || cli.fixed {
        return run_exact(cli).await;
    }
    let queries = facets(&cli.query, &cli.queries);
    if queries.is_empty() {
        return Err("no search terms given (try `ss walk files respecting gitignore`)".to_owned());
    }
    let here = std::env::current_dir().map_err(|error| error.to_string())?;
    let scope = absolute(cli.path.as_deref().unwrap_or(&here))?;
    let request = if cli.lines {
        Request::Search {
            query: queries.join(" "),
            path: scope,
            limit: cli.limit,
        }
    } else {
        Request::Query {
            queries,
            path: scope,
            limit: cli.limit,
        }
    };
    match client
        .send(request)
        .await
        .map_err(|error| error.to_string())?
    {
        Response::Hits { hits } => Ok(print_lines(&hits, &here, cli.json)),
        Response::Findings { findings } => {
            let found = findings
                .facets
                .iter()
                .any(|facet| !facet.records.is_empty());
            if cli.json {
                println!("{}", findings_json(&findings, cli.budget));
            } else if cli.compact {
                let opts = Compact::lean(cli.budget.unwrap_or(COMPACT_BUDGET));
                print!("{}", compact_text(&findings, &here, &opts));
            } else {
                print!("{}", findings_text(&findings, &here, cli.budget));
            }
            Ok(exit_for(found))
        }
        Response::Error { message, .. } => Err(message),
        other => Err(format!("unexpected reply: {other:?}")),
    }
}

async fn run_exact(cli: &Cli) -> Result<ExitCode, String> {
    let pattern = cli.query.join(" ");
    if pattern.is_empty() {
        return Err("no pattern given (try `ss --exact 'fn \\w+_files'`)".to_owned());
    }
    let regex = compile(&pattern, cli.fixed, cli.ignore_case).map_err(|error| error.to_string())?;
    let here = std::env::current_dir().map_err(|error| error.to_string())?;
    let scope = PathBuf::from(absolute(cli.path.as_deref().unwrap_or(&here))?);
    let keep = cli.per_file;
    let files = tokio::task::spawn_blocking(move || scan(&scope, &regex, keep))
        .await
        .map_err(|error| error.to_string())?;
    print!(
        "{}",
        exact_text(&files, &here, cli.budget.unwrap_or(EXACT_BUDGET))
    );
    Ok(exit_for(!files.is_empty()))
}

async fn run_graph(
    client: &Client,
    target: &str,
    depth: u8,
    json: bool,
    path: Option<&Path>,
) -> Result<ExitCode, String> {
    let here = std::env::current_dir().map_err(|error| error.to_string())?;
    let request = Request::Graph {
        target: target.to_owned(),
        path: absolute(path.unwrap_or(&here))?,
        depth,
    };
    match client
        .send(request)
        .await
        .map_err(|error| error.to_string())?
    {
        Response::Graph { graph } => {
            if json {
                println!("{}", graph_json(&graph));
            } else {
                print!("{}", graph_text(&graph, &here));
            }
            Ok(exit_for(!graph.neighborhoods.items.is_empty()))
        }
        Response::Error { message, .. } => Err(message),
        other => Err(format!("unexpected reply: {other:?}")),
    }
}

fn exit_for(found: bool) -> ExitCode {
    ExitCode::from(if found { EXIT_HITS } else { EXIT_NONE })
}

fn print_lines(hits: &[Hit], here: &Path, json: bool) -> ExitCode {
    if json {
        for hit in hits {
            println!("{}", serde_json::to_string(hit).unwrap_or_default());
        }
    } else {
        print!("{}", lines_text(hits, here));
    }
    exit_for(!hits.is_empty())
}

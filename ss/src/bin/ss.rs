use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand};
use ss::client::Client;
use ss::exact::compile;
use ss::exact::plain::{Context, files_of, globs, pattern_source, scan_rows};
use ss::find::{FIND_BUDGET, Shape, classify, find, render_json as find_json, render_text as find_text};
use ss::map::{MAP_BUDGET, map_dir, map_json, map_text};
use ss::history::{Source, View, render as render_history};
use ss::read::{OUTLINE_BUDGET, SECTION_BUDGET, Target, outline_file, section_file};
use ss::render::{
    COMPACT_BUDGET, Compact, Layout, Mode, compact_text, exact_reply, facets, findings_json,
    findings_text, graph_json, graph_text, lines_text,
};
use ss::scip::{REFS_BUDGET, scip_graph, find_defs, load_file, render_calls, render_defs, render_refs};
use ss::settings::Settings;
use ss::store::Store;
use ss_proto::{Hit, Request, Response};

const EXIT_HITS: u8 = 0;
const EXIT_NONE: u8 = 1;
const EXIT_ERROR: u8 = 2;

/// What `ss -h` prints; `ss --help` keeps clap's full text.
const BRIEF_HELP: &str = "\
Usage: ss [OPTIONS] <QUERY>... | ss <COMMAND>
  ss QUERY...        semantic search; --exact regex
  ss find QUERY...   one call: path, ident, regex or phrase
  ss outline FILE    symbols with line ranges
  ss section FILE N  one symbol body (or --line N)
  ss graph SYMBOL    callers, callees, members
  ss def|refs SYM    scip definition, references
  ss scip load FILE  load a scip index
  ss show REV|diff   symbol-level changes
  ss map [DIR]       file purposes
  ss index PATH      index a directory
  ss --help          all options
";

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
    /// Exact search: a pattern (repeatable); with -e every positional word is a path
    #[arg(short = 'e', long = "regexp")]
    patterns: Vec<String>,
    /// Exact search: match whole words only
    #[arg(short = 'w', long)]
    word: bool,
    /// Exact search: print only the paths of files with a match
    #[arg(short = 'l', long = "files-with-matches")]
    files_only: bool,
    /// Exact search: print `path:count` per file
    #[arg(short = 'c', long)]
    count: bool,
    /// Exact search: lines of context after each match
    #[arg(short = 'A', long)]
    after_context: Option<usize>,
    /// Exact search: lines of context before each match
    #[arg(short = 'B', long)]
    before_context: Option<usize>,
    /// Exact search: lines of context around each match
    #[arg(short = 'C', long)]
    context: Option<usize>,
    /// Exact search: only files matching this glob (repeatable)
    #[arg(long, alias = "glob")]
    include: Vec<String>,
    /// Exact search: skip files matching this glob (repeatable)
    #[arg(long)]
    exclude: Vec<String>,
    /// Accepted for grep compatibility and ignored: the search is always recursive
    #[arg(short = 'r', short_alias = 'R', long)]
    recursive: bool,
    /// Accepted for grep compatibility and ignored: extended regex is the default
    #[arg(short = 'E', long)]
    extended: bool,
    /// Exact search: read the pattern as grep basic regex (no `\|` translation)
    #[arg(short = 'G', long)]
    basic: bool,
    /// Accepted for grep compatibility and ignored: the path prefix follows grep's rules
    #[arg(short = 'H', long)]
    with_filename: bool,
    /// Exact search: no path prefix on matches
    #[arg(long)]
    no_filename: bool,
    /// Exact search: print `line:` before each match (grep's -n)
    #[arg(long)]
    line_number: bool,
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
    /// One call that classifies the query (path, identifier, regex, phrase) and returns at most 5 files with a next step
    Find {
        /// The query; words are joined by spaces
        #[arg(required = true)]
        query: Vec<String>,
        /// Cap the reply at about this many tokens (chars / 4); default 500
        #[arg(long)]
        budget: Option<usize>,
        /// Print the reply as JSON
        #[arg(long)]
        json: bool,
        /// Directory to search under (default: the current directory)
        #[arg(long)]
        path: Option<PathBuf>,
    },
    /// Headings (docs) or symbols (source) of a file with `start-end` line ranges, within a token budget; no daemon
    Outline {
        file: PathBuf,
        /// Cap the reply at about this many tokens (chars / 4); default 400
        #[arg(long)]
        budget: Option<usize>,
        /// Show only entries nested fewer than this many levels deep (1 = top level)
        #[arg(long)]
        depth: Option<usize>,
    },
    /// One heading's section or one symbol's body of a file, with line numbers, within a token budget; no daemon
    Section {
        file: PathBuf,
        /// Heading or symbol name: case-insensitive, exact match first, then substring
        #[arg(required_unless_present = "line")]
        name: Option<String>,
        /// Pick the innermost section or symbol containing this line instead of a name
        #[arg(long, conflicts_with = "name")]
        line: Option<usize>,
        /// Cap the reply at about this many tokens (chars / 4); default 1200
        #[arg(long)]
        budget: Option<usize>,
        /// Extra lines printed before and after the range
        #[arg(long, default_value_t = 0)]
        context: usize,
    },
    /// Budgeted map of a directory tree: size and one-line purpose per file plus purpose coverage; no daemon or index
    Map {
        /// Directory to map (default: the current directory)
        dir: Option<PathBuf>,
        /// Cap the reply at about this many tokens (chars / 4); default 600
        #[arg(long)]
        budget: Option<usize>,
        /// Show only directories nested at most this many levels below DIR (0 = files directly in DIR)
        #[arg(long)]
        depth: Option<usize>,
        /// Print the map as JSON (schema in `ai_docs/map.md`)
        #[arg(long)]
        json: bool,
    },
    /// Compiler-resolved definitions and references from a SCIP index
    Scip {
        #[command(subcommand)]
        action: ScipAction,
    },
    /// Definition of a symbol name or `path:line:col` from the SCIP index
    Def {
        target: String,
        #[arg(long)]
        root: Option<PathBuf>,
    },
    /// References of a symbol from the SCIP index, grouped by file with the enclosing symbol
    Refs {
        target: String,
        #[arg(long)]
        root: Option<PathBuf>,
        /// Cap the reply at about this many tokens (chars / 4); default 600
        #[arg(long)]
        budget: Option<usize>,
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

#[derive(Debug, Subcommand)]
enum ScipAction {
    /// Callers and callees of a symbol from compiler-resolved occurrences, tagged by freshness
    Calls {
        target: String,
        #[arg(long)]
        root: Option<PathBuf>,
        #[arg(long)]
        budget: Option<usize>,
    },
    /// Load an index.scip (for example from `rust-analyzer scip`) into the root's store
    Load {
        index: PathBuf,
        /// Root the index was generated for (default: the current directory)
        #[arg(long)]
        root: Option<PathBuf>,
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
    let args: Vec<OsString> = std::env::args_os().collect();
    if args.len() == 2 && args[1] == "-h" {
        print!("{BRIEF_HELP}");
        return ExitCode::from(EXIT_HITS);
    }
    let cli = Cli::parse_from(grep_argv(args));
    match &cli.command {
        Some(Command::Show { rev, args }) => return run_history(&Source::Show(rev.clone()), args),
        Some(Command::Diff { revs, staged, args }) => {
            let source = Source::Diff { revs: revs.clone(), staged: *staged };
            return run_history(&source, args);
        }
        Some(Command::Find { query, budget, json, path }) => return run_find(query, *budget, *json, path.as_deref()).await,
        Some(Command::Outline { file, budget, depth }) => {
            return print_read(outline_file(file, budget.unwrap_or(OUTLINE_BUDGET), *depth));
        }
        Some(Command::Map { dir, budget, depth, json }) => {
            return run_map(dir.as_deref().unwrap_or(Path::new(".")), budget.unwrap_or(MAP_BUDGET), *depth, *json);
        }
        Some(Command::Section { file, name, line, budget, context }) => {
            let target = line.map_or_else(|| Target::Name(name.as_deref().unwrap_or("")), Target::Line);
            let reply = section_file(file, target, budget.unwrap_or(SECTION_BUDGET), *context);
            return print_read(reply);
        }
        _ => {}
    }
    match &cli.command {
        Some(Command::Scip { action: ScipAction::Load { index, root } }) => return run_scip_load(index, root.as_deref()),
        Some(Command::Scip { action: ScipAction::Calls { target, root, budget } }) => return run_scip_query(Query::Calls, target, root.as_deref(), *budget),
        Some(Command::Def { target, root }) => return run_scip_query(Query::Def, target, root.as_deref(), None),
        Some(Command::Refs { target, root, budget }) => return run_scip_query(Query::Refs, target, root.as_deref(), *budget),
        _ => {}
    }
    if let Some(Command::Graph { target, json: false, path, .. }) = &cli.command
        && let Some(text) = scip_graph_text(target, path.as_deref())
    {
        print!("{text}");
        return ExitCode::from(EXIT_HITS);
    }
    let client = match Settings::load() {
        Ok(settings) => Client::new(settings),
        Err(error) => return fail(&format!("bad SS_* configuration: {error}")),
    };
    let outcome = match cli.command {
        Some(Command::Show { .. } | Command::Diff { .. } | Command::Find { .. } | Command::Outline { .. } | Command::Map { .. } | Command::Section { .. } | Command::Scip { .. } | Command::Def { .. } | Command::Refs { .. }) => {
            unreachable_history()
        }
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

#[derive(Clone, Copy)]
enum Query {
    Def,
    Refs,
    Calls,
}

fn run_scip_query(query: Query, target: &str, root: Option<&Path>, budget: Option<usize>) -> ExitCode {
    let outcome = (|| -> Result<String, String> {
        let here = std::env::current_dir().map_err(|error| error.to_string())?;
        let root = PathBuf::from(absolute(root.unwrap_or(&here))?);
        let settings = Settings::load().map_err(|error| error.to_string())?;
        let store = Store::open_reader(&settings.store_config(&root)).map_err(|error| error.to_string())?;
        let defs = find_defs(&store, target).map_err(|error| error.to_string())?;
        let budget = budget.unwrap_or(REFS_BUDGET);
        match query {
            Query::Def => Ok(render_defs(&store, &root, &defs)),
            Query::Refs => render_refs(&store, &root, &defs, budget).map_err(|error| error.to_string()),
            Query::Calls => render_calls(&store, &root, &defs, budget).map_err(|error| error.to_string()),
        }
    })();
    match outcome {
        Ok(text) => {
            print!("{text}");
            exit_for(!text.is_empty())
        }
        Err(message) => fail(&message),
    }
}

/// SCIP-resolved graph when this directory is an indexed root with SCIP rows; `None` falls back to the daemon.
fn scip_graph_text(target: &str, path: Option<&Path>) -> Option<String> {
    let root = PathBuf::from(absolute(path.unwrap_or(Path::new("."))).ok()?);
    let store = Store::open_reader(&Settings::load().ok()?.store_config(&root)).ok()?;
    scip_graph(&store, &root, target)
}

fn run_scip_load(index: &Path, root: Option<&Path>) -> ExitCode {
    let outcome = (|| -> Result<(u64, u64, u64), String> {
        let here = std::env::current_dir().map_err(|error| error.to_string())?;
        let root = PathBuf::from(absolute(root.unwrap_or(&here))?);
        let settings = Settings::load().map_err(|error| error.to_string())?;
        let mut store = Store::open(&settings.store_config(&root)).map_err(|error| error.to_string())?;
        load_file(&mut store, &root, index).map_err(|error| error.to_string())
    })();
    match outcome {
        Ok((symbols, occurrences, files)) => {
            println!("loaded scip: {symbols} symbols, {occurrences} occurrences, {files} files");
            ExitCode::from(EXIT_HITS)
        }
        Err(message) => fail(&message),
    }
}

async fn run_find(words: &[String], budget: Option<usize>, json: bool, path: Option<&Path>) -> ExitCode {
    match find_outcome(&words.join(" "), budget, json, path).await {
        Ok(code) => code,
        Err(message) => fail(&message),
    }
}

async fn find_outcome(query: &str, budget: Option<usize>, json: bool, path: Option<&Path>) -> Result<ExitCode, String> {
    let root = PathBuf::from(absolute(path.unwrap_or(Path::new(".")))?);
    let phrase = classify(&root, query) == Shape::Phrase;
    let semantic = if phrase { semantic_files(&root, query).await } else { None };
    let (scan_root, scan_query, hits) = (root.clone(), query.to_owned(), semantic.clone().unwrap_or_default());
    let mut found = tokio::task::spawn_blocking(move || find(&scan_root, &scan_query, &hits))
        .await
        .map_err(|error| error.to_string())?
        .map_err(|error| error.to_string())?;
    if phrase && semantic.is_none() {
        found.notes.push("semantic skipped: no daemon running".to_owned());
    }
    if let Some(ident) = found.ident.clone() {
        found.graph = scip_graph_text(&ident, Some(&root));
        if found.graph.is_none() {
            found.notes.push("graph skipped: no scip rows".to_owned());
        }
    }
    let text = if json { find_json(&found) } else { find_text(&found, budget.unwrap_or(FIND_BUDGET)) };
    println!("{}", text.trim_end());
    Ok(exit_for(!found.is_empty()))
}

/// The running daemon's hybrid hits as (path, start, end); `None` when no daemon answers (never spawns one).
async fn semantic_files(root: &Path, query: &str) -> Option<Vec<(String, usize, usize)>> {
    let client = Client::new(Settings::load().ok()?);
    let request = Request::Search { query: query.to_owned(), path: root.to_string_lossy().into_owned(), limit: 20 };
    match client.send_if_running(request).await.ok()?? {
        Response::Hits { hits } => Some(hits.into_iter().map(|hit| (hit.path, hit.start_line as usize, hit.end_line as usize)).collect()),
        _ => None,
    }
}

fn unreachable_history() -> Result<ExitCode, String> {
    Err("history and read subcommands are handled before the daemon client".to_owned())
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

fn run_map(dir: &Path, budget: usize, depth: Option<usize>, json: bool) -> ExitCode {
    if !dir.is_dir() {
        return fail(&format!("{}: not a directory", dir.display()));
    }
    let map = map_dir(dir, budget, depth);
    if json {
        println!("{}", map_json(&map));
    } else {
        print!("{}", map_text(&map));
    }
    exit_for(map.total > 0)
}

fn print_read(reply: Result<String, ss::read::ReadError>) -> ExitCode {
    match reply {
        Ok(text) => {
            print!("{text}");
            ExitCode::from(EXIT_HITS)
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

const GREP_SWITCHES: &str = "rRnHhiwFlcEGs";

fn is_switch_cluster(arg: &OsStr) -> bool {
    arg.to_str().is_some_and(|text| {
        text.len() > 1
            && text.starts_with('-')
            && text[1..]
                .chars()
                .all(|letter| GREP_SWITCHES.contains(letter))
    })
}

/// grep's `-n` is a bare switch; in the semantic form it takes a count. In an exact search it becomes `--line-number`.
fn grep_argv(args: Vec<OsString>) -> Vec<OsString> {
    let exact = args.iter().any(|arg| {
        arg == "--exact"
            || arg == "--fixed-strings"
            || (is_switch_cluster(arg) && arg.to_string_lossy().contains('F'))
    });
    if !exact {
        return args;
    }
    let mut seen: Vec<OsString> = Vec::new();
    args.into_iter()
        .flat_map(exact_switches)
        .filter(|arg| {
            let repeat = (arg == "--line-number" || arg == "--no-filename") && seen.contains(arg);
            seen.push(arg.clone());
            !repeat
        })
        .collect()
}

fn exact_switches(arg: OsString) -> Vec<OsString> {
    if !is_switch_cluster(&arg) {
        return vec![arg];
    }
    let text = arg.to_string_lossy();
    let rest: String = text
        .chars()
        .filter(|letter| !"nh".contains(*letter))
        .collect();
    let mut out = Vec::new();
    for (letter, long) in [('n', "--line-number"), ('h', "--no-filename")] {
        if text[1..].contains(letter) {
            out.push(OsString::from(long));
        }
    }
    if rest != "-" {
        out.push(OsString::from(rest));
    }
    out
}

fn exact_patterns(cli: &Cli) -> (Vec<String>, Vec<PathBuf>) {
    let mut words = cli.query.iter();
    let mut patterns = cli.patterns.clone();
    if patterns.is_empty() {
        patterns.extend(words.next().cloned());
    }
    let mut paths: Vec<PathBuf> = words.map(PathBuf::from).collect();
    paths.extend(cli.path.clone());
    (patterns, paths)
}

async fn run_exact(cli: &Cli) -> Result<ExitCode, String> {
    let (patterns, mut paths) = exact_patterns(cli);
    if patterns.is_empty() {
        return Err("no pattern given (try `ss --exact 'fn \\w+_files'`)".to_owned());
    }
    let here = std::env::current_dir().map_err(|error| error.to_string())?;
    if paths.is_empty() {
        paths.push(here.clone());
    }
    let paths = paths
        .iter()
        .map(|path| absolute(path).map(PathBuf::from))
        .collect::<Result<Vec<_>, _>>()?;
    let source = pattern_source(&patterns, cli.fixed, cli.word, !cli.basic);
    let regex = compile(&source, false, cli.ignore_case).map_err(|error| error.to_string())?;
    let filter = globs(&here, &cli.include, &cli.exclude).map_err(|error| error.to_string())?;
    let mode = if cli.files_only {
        Mode::Files
    } else if cli.count {
        Mode::Count
    } else {
        Mode::Lines
    };
    let layout = Layout {
        mode,
        numbered: cli.line_number,
        prefix: !(cli.no_filename || paths.len() == 1 && paths[0].is_file()),
        separators: cli
            .context
            .or(cli.after_context)
            .or(cli.before_context)
            .is_some(),
        per_file: cli.per_file,
    };
    let around = Context {
        before: cli.before_context.or(cli.context).unwrap_or(0),
        after: cli.after_context.or(cli.context).unwrap_or(0),
    };
    let around = if mode == Mode::Lines {
        around
    } else {
        Context::default()
    };
    let files =
        tokio::task::spawn_blocking(move || scan_rows(&files_of(&paths, &filter), &regex, around))
            .await
            .map_err(|error| error.to_string())?;
    print!("{}", exact_reply(&files, &here, layout, cli.budget));
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

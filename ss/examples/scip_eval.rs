//! Checks name-resolved call edges against SCIP occurrences.
//!
//! `scip_eval <edges.tsv> <index.scip> [seed] [samples]` reads the TSV written by `graph_eval edges`, samples
//! Rust call edges uniformly (splitmix64, partial Fisher-Yates) and classifies each by what SCIP resolves at the
//! call identifier: agrees with a name-resolved candidate, resolves elsewhere, resolves outside the workspace, or
//! has no occurrence there.
// harness: aborts on any setup failure
#![allow(clippy::expect_used, clippy::print_stdout)]

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use ss::scip::{ScipRows, convert, parse};

// a call chain's identifier can sit on a later line than the edge's recorded start line
const CHAIN_LINES: u32 = 2;
const DEFAULT_SEED: u64 = 20_261_030;
const DEFAULT_SAMPLES: usize = 60;
const KNOWN_FALSE: [(&str, u32, &str); 8] = [
    ("ss/src/daemon.rs", 107, "load"),
    ("ss/src/embed.rs", 192, "spawn"),
    ("ss-ipc/src/spawn.rs", 26, "spawn"),
    ("ss/src/embed.rs", 192, "name"),
    ("ss/src/telemetry.rs", 33, "name"),
    ("ss-ipc/src/endpoint.rs", 62, "name"),
    ("ss/src/query.rs", 41, "is_empty"),
    ("ss/src/chunk.rs", 130, "push"),
];

struct Edge {
    resolution: String,
    kind: String,
    file: String,
    line: u32,
    dst: String,
    targets: Vec<(String, u32)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
enum Verdict {
    Agrees,
    ResolvesElsewhere,
    ResolvesOutsideWorkspace,
    NoOccurrence,
}

struct Scip {
    at_line: HashMap<(String, u32), Vec<(String, String)>>,
    definitions: HashMap<String, Vec<(String, u32, u32)>>,
}

fn parse_edges(text: &str) -> Vec<Edge> {
    text.lines()
        .filter_map(|line| {
            let cells: Vec<&str> = line.split('\t').collect();
            let (file, number) = cells.get(1)?.rsplit_once(':')?;
            let targets = cells.get(6).map_or_else(Vec::new, |cell| {
                cell.split(' ')
                    .filter_map(|target| {
                        let (path, start) = target.rsplit_once(':')?;
                        Some((path.to_owned(), start.parse().ok()?))
                    })
                    .collect()
            });
            Some(Edge {
                resolution: (*cells.first()?).to_owned(),
                kind: (*cells.get(2)?).to_owned(),
                file: file.to_owned(),
                line: number.parse().ok()?,
                dst: (*cells.get(4)?).to_owned(),
                targets,
            })
        })
        .collect()
}

fn index_scip(rows: &ScipRows) -> Scip {
    let names: HashMap<&str, &str> = rows
        .symbols
        .iter()
        .map(|row| (row.symbol.as_str(), row.display_name.as_str()))
        .collect();
    let mut scip = Scip {
        at_line: HashMap::new(),
        definitions: HashMap::new(),
    };
    for occurrence in &rows.occurrences {
        if occurrence.is_definition() {
            let (start, end) = occurrence
                .enclosing
                .unwrap_or((occurrence.start_line, occurrence.end_line));
            scip.definitions
                .entry(occurrence.symbol.clone())
                .or_default()
                .push((occurrence.file.clone(), start + 1, end + 1));
        } else if !occurrence.symbol.starts_with("local ") {
            let name = names.get(occurrence.symbol.as_str()).map_or_else(
                || derived_name(&occurrence.symbol),
                |name| (*name).to_owned(),
            );
            scip.at_line
                .entry((occurrence.file.clone(), occurrence.start_line + 1))
                .or_default()
                .push((name, occurrence.symbol.clone()));
        }
    }
    scip
}

fn derived_name(symbol: &str) -> String {
    let trimmed = symbol.trim_end_matches(['#', '.', '(', ')', '[', ']', ':', '!']);
    trimmed
        .rsplit(['/', '#', '.', ']'])
        .next()
        .unwrap_or(trimmed)
        .to_owned()
}

fn last_segment(path: &str) -> &str {
    path.rsplit("::").next().unwrap_or(path)
}

fn judge(scip: &Scip, edge: &Edge) -> Verdict {
    let wanted = last_segment(&edge.dst);
    let hits: Vec<&(String, String)> = (edge.line..=edge.line + CHAIN_LINES)
        .filter_map(|line| scip.at_line.get(&(edge.file.clone(), line)))
        .flatten()
        .collect();
    let symbols: Vec<&String> = hits
        .into_iter()
        .filter(|(name, _)| name == wanted)
        .map(|(_, symbol)| symbol)
        .collect();
    if symbols.is_empty() {
        return Verdict::NoOccurrence;
    }
    let spans: Vec<&(String, u32, u32)> = symbols
        .iter()
        .filter_map(|symbol| scip.definitions.get(*symbol))
        .flatten()
        .collect();
    if spans.is_empty() {
        return Verdict::ResolvesOutsideWorkspace;
    }
    let agrees = edge.targets.iter().any(|(file, start)| {
        spans
            .iter()
            .any(|(span_file, from, to)| span_file == file && start >= from && start <= to)
    });
    if agrees {
        Verdict::Agrees
    } else {
        Verdict::ResolvesElsewhere
    }
}

fn sample(count: usize, seed: u64, take: usize) -> Vec<usize> {
    let mut state = seed;
    let mut order: Vec<usize> = (0..count).collect();
    for index in 0..take.min(count) {
        state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut mixed = state;
        mixed = (mixed ^ (mixed >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        mixed = (mixed ^ (mixed >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        mixed ^= mixed >> 31;
        let pick = index + usize::try_from(mixed % ((count - index) as u64)).expect("fits usize");
        order.swap(index, pick);
    }
    order.truncate(take);
    order
}

fn tally(edges: &[&Edge], scip: &Scip) -> Vec<(Verdict, usize)> {
    let mut counts: HashMap<Verdict, usize> = HashMap::new();
    for edge in edges {
        *counts.entry(judge(scip, edge)).or_default() += 1;
    }
    let mut rows: Vec<_> = counts.into_iter().collect();
    rows.sort();
    rows
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let text = std::fs::read_to_string(PathBuf::from(args.get(1).expect("edges.tsv")))
        .expect("read edges");
    let bytes = std::fs::read(args.get(2).expect("index.scip")).expect("read index");
    let seed = args
        .get(3)
        .map_or(DEFAULT_SEED, |value| value.parse().expect("seed"));
    let take = args
        .get(4)
        .map_or(DEFAULT_SAMPLES, |value| value.parse().expect("samples"));
    let scip = index_scip(&convert(&parse(&bytes).expect("parse scip")));
    let all = parse_edges(&text);
    let rust: Vec<&Edge> = all
        .iter()
        .filter(|edge| {
            edge.kind == "calls"
                && Path::new(&edge.file)
                    .extension()
                    .is_some_and(|ext| ext == "rs")
                && !edge.file.contains("tests/data")
                && !edge.file.ends_with("store.rs")
                && !edge.file.ends_with("lib.rs")
        })
        .collect();
    println!(
        "resolved edges {} ; rust call edges judged {}",
        all.len(),
        rust.len()
    );
    for resolution in ["exact", "ambiguous"] {
        let group: Vec<&Edge> = rust
            .iter()
            .copied()
            .filter(|edge| edge.resolution == resolution)
            .collect();
        println!(
            "category {resolution}: N={} {:?}",
            group.len(),
            tally(&group, &scip)
        );
    }
    let picked: Vec<&Edge> = sample(rust.len(), seed, take)
        .into_iter()
        .map(|index| rust[index])
        .collect();
    println!(
        "sample seed {seed} N={}: {:?}",
        picked.len(),
        tally(&picked, &scip)
    );
    for edge in &picked {
        println!(
            "  {:?}\t{}\t{}:{}\t{}",
            judge(&scip, edge),
            edge.resolution,
            edge.file,
            edge.line,
            edge.dst
        );
    }
    let mut rejected: HashMap<&str, usize> = HashMap::new();
    for edge in rust.iter().filter(|edge| edge.resolution == "ambiguous") {
        if matches!(
            judge(&scip, edge),
            Verdict::ResolvesOutsideWorkspace | Verdict::ResolvesElsewhere
        ) {
            *rejected.entry(last_segment(&edge.dst)).or_default() += 1;
        }
    }
    let mut top: Vec<_> = rejected.into_iter().collect();
    top.sort_by_key(|(name, count)| (std::cmp::Reverse(*count), *name));
    println!(
        "rejected ambiguous edges by callee name (top 12): {:?}",
        &top[..top.len().min(12)]
    );
    let known: HashSet<(&str, u32, &str)> = KNOWN_FALSE.into_iter().collect();
    for edge in all
        .iter()
        .filter(|edge| known.contains(&(edge.file.as_str(), edge.line, last_segment(&edge.dst))))
    {
        let nearby: Vec<String> = (edge.line.saturating_sub(2)..=edge.line + 2)
            .filter_map(|line| scip.at_line.get(&(edge.file.clone(), line)))
            .flatten()
            .filter(|(name, _)| name == last_segment(&edge.dst))
            .map(|(_, symbol)| symbol.rsplit(' ').next().unwrap_or(symbol).to_owned())
            .collect();
        println!(
            "known-false\tscip-symbols-near-line={nearby:?}\t{:?}\t{}\t{}:{}\t{}\ttargets={:?}",
            judge(&scip, edge),
            edge.resolution,
            edge.file,
            edge.line,
            edge.dst,
            edge.targets
        );
    }
}

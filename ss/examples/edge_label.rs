//! Writes one SCIP verdict per judged Rust call edge as TSV (file, line, callee, kind, candidates, label).
//!
//! `edge_label <edges.tsv> <index.scip> <out.tsv>` reads the TSV written by `graph_eval edges`, samples
//! Rust call edges uniformly (splitmix64, partial Fisher-Yates) and classifies each by what SCIP resolves at the
//! call identifier: agrees with a name-resolved candidate, resolves elsewhere, resolves outside the workspace, or
//! has no occurrence there.
// harness: aborts on any setup failure
#![allow(clippy::expect_used, clippy::print_stdout)]

use std::collections::HashMap;
use std::path::Path;

use ss::scip::{ScipRows, convert, parse};

// a call chain's identifier can sit on a later line than the edge's recorded start line
const CHAIN_LINES: u32 = 2;

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

fn label(verdict: Verdict) -> &'static str {
    match verdict {
        Verdict::Agrees => "agrees",
        Verdict::ResolvesElsewhere | Verdict::ResolvesOutsideWorkspace => "rejected",
        Verdict::NoOccurrence => "cannot-judge",
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let text = std::fs::read_to_string(args.get(1).expect("edges.tsv")).expect("read edges");
    let bytes = std::fs::read(args.get(2).expect("index.scip")).expect("read index");
    let out = args.get(3).expect("out.tsv");
    let scip = index_scip(&convert(&parse(&bytes).expect("parse scip")));
    let rows: Vec<String> = parse_edges(&text)
        .iter()
        .filter(|edge| {
            edge.kind == "calls"
                && Path::new(&edge.file).extension().is_some_and(|ext| ext == "rs")
                && !edge.file.contains("tests/data")
                && !edge.file.ends_with("store.rs")
                && !edge.file.ends_with("lib.rs")
        })
        .map(|edge| {
            format!(
                "{}\t{}\t{}\t{}\t{}\t{}",
                edge.file,
                edge.line,
                last_segment(&edge.dst),
                edge.resolution,
                edge.targets.len(),
                label(judge(&scip, edge))
            )
        })
        .collect();
    std::fs::write(out, rows.join("\n") + "\n").expect("write labels");
    println!("labeled {} edges", rows.len());
}

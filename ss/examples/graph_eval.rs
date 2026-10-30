//! Measures the structured output against the old format on the frozen corpus and the 44 gold queries.
//!
//! `graph_eval tokens <out.json>`  M1 tokens, M2 answerability and M6 one-call sufficiency (real CPU embedder)
//! `graph_eval edges <out.tsv>`    every resolved call/import edge of the corpus graph, for the M3 precision sample
// harness: aborts on any setup failure and divides counts into ratios for display
#![allow(
    clippy::expect_used,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::too_many_lines
)]

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::json;
use ss::context::{candidates, follow_ups};
use ss::embed::{EmbedPool, EmbedProfile, EmbedderKind, load_fastembed};
use ss::graph::{EdgeKind, resolution_of};
use ss::index::{Shard, index_tree};
use ss::query::{search, search_findings};
use ss::render::{findings_text, lines_text, tokens};
use ss::store::{Store, StoreConfig};

const BUDGET_SMALL: usize = 400;
const BUDGET_WIDE: usize = 800;
const SUFFICIENT: f64 = 0.8;

struct Gold {
    query: String,
    path: String,
    start: u32,
    end: u32,
}

fn data_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data")
}

fn load_gold() -> Vec<Gold> {
    std::fs::read_to_string(data_dir().join("queries.tsv"))
        .expect("queries.tsv")
        .lines()
        .filter(|line| !line.starts_with('#') && !line.trim().is_empty())
        .map(|line| {
            let cells: Vec<&str> = line.split('\t').collect();
            let (start, end) = cells[2].split_once('-').expect("start-end");
            Gold {
                query: cells[0].to_owned(),
                path: cells[1].to_owned(),
                start: start.parse().expect("start"),
                end: end.parse().expect("end"),
            }
        })
        .collect()
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).expect("create destination");
    for entry in std::fs::read_dir(from).expect("read source dir").flatten() {
        let target = to.join(entry.file_name());
        if entry.path().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).expect("copy file");
        }
    }
}

fn percentile(sorted: &[usize], fraction: f64) -> usize {
    let rank = (fraction * sorted.len() as f64).ceil() as usize;
    sorted[rank.max(1) - 1]
}

fn summary(values: &[usize]) -> (usize, usize) {
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    (percentile(&sorted, 0.5), percentile(&sorted, 0.95))
}

fn collapse(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn has_word(text: &str, word: &str) -> bool {
    text.split(|ch: char| !(ch.is_alphanumeric() || ch == '_'))
        .any(|part| part == word)
}

fn line_has_site(line: &str) -> bool {
    line.as_bytes()
        .windows(2)
        .any(|pair| pair[0] == b':' && pair[1].is_ascii_digit())
}

fn present_anywhere(text: &str, name: &str) -> bool {
    has_word(text, name)
}

fn present_with_site(text: &str, name: &str) -> bool {
    text.lines()
        .any(|line| has_word(line, name) && line_has_site(line))
}

async fn open_index(work: &Path) -> (Shard, PathBuf, StoreConfig, EmbedPool) {
    let profile = EmbedProfile::default();
    let cache = work.join("model");
    std::fs::create_dir_all(&cache).expect("model cache");
    let root = work.join("tree");
    copy_dir(&data_dir().join("corpus"), &root);
    let config = StoreConfig {
        dir: work.join("shard"),
        model_id: profile.store_id(EmbedderKind::FastEmbed),
    };
    let pool = EmbedPool::with_lanes(1, 2, move || load_fastembed(&cache, 2, profile));
    let shard = Shard::new(&root, config.clone(), &pool);
    let stats = index_tree(&shard, &root).await.expect("index");
    println!("indexed: {stats:?}");
    (shard, root, config, pool)
}

fn rg_tokens(root: &Path, name: &str) -> usize {
    let output = Command::new("rg")
        .args(["-n", "--no-heading", "-w", name])
        .arg(root)
        .output()
        .expect("rg");
    tokens(&String::from_utf8_lossy(&output.stdout))
}

struct Row {
    query: String,
    old: [usize; 2],
    new: [usize; 2],
    budgeted: [usize; 2],
    has_node: bool,
    name_sig: (bool, bool),
    doc: Option<(bool, bool)>,
    targets: usize,
    anywhere: (usize, usize),
    strict: (usize, usize),
    resp_new: usize,
    resp_old: usize,
    missing_new: Vec<String>,
    categories: [(usize, usize); 3],
    line_chars: [usize; 7],
}

async fn measure(shard: &Shard, root: &Path, config: &StoreConfig, gold: &Gold) -> Row {
    let queries = vec![gold.query.clone()];
    let mut old = [0; 2];
    let mut new = [0; 2];
    let mut budgeted = [0; 2];
    for (slot, limit) in [5_usize, 10].into_iter().enumerate() {
        let hits = search(shard, root, &gold.query, limit)
            .await
            .expect("search");
        old[slot] = tokens(&lines_text(&hits, root));
        let findings = search_findings(shard, root, &queries, limit)
            .await
            .expect("findings");
        new[slot] = tokens(&findings_text(&findings, root, None));
        budgeted[slot] = tokens(&findings_text(&findings, root, Some(BUDGET_SMALL)));
    }
    let hits = search(shard, root, &gold.query, 5).await.expect("search");
    let old_text = lines_text(&hits, root);
    let findings = search_findings(shard, root, &queries, 5)
        .await
        .expect("findings");
    let new_text = findings_text(&findings, root, None);
    let wide_text = findings_text(&findings, root, Some(BUDGET_WIDE));
    let store = Store::open(config).expect("store");
    let node = store
        .primary_node(&gold.path, gold.start, gold.end)
        .expect("primary");
    let mut row = Row {
        query: gold.query.clone(),
        old,
        new,
        budgeted,
        has_node: node.is_some(),
        name_sig: (false, false),
        doc: None,
        targets: 0,
        anywhere: (0, 0),
        strict: (0, 0),
        resp_new: tokens(&wide_text),
        resp_old: tokens(&old_text),
        missing_new: Vec::new(),
        categories: [(0, 0); 3],
        line_chars: line_chars(&new_text),
    };
    let Some(node) = node else { return row };
    let signature = collapse(&node.node.signature);
    let preview_old = collapse(
        &hits
            .iter()
            .flat_map(|hit| hit.preview.clone())
            .collect::<Vec<_>>()
            .join(" "),
    );
    let new_flat = collapse(&new_text);
    row.name_sig = (
        has_word(&new_text, &node.node.name) && new_flat.contains(&signature),
        preview_old.contains(&signature),
    );
    if !node.node.doc.is_empty() {
        row.doc = Some((
            new_flat.contains(&collapse(&node.node.doc)),
            preview_old.contains(&collapse(&node.node.doc)),
        ));
    }
    let (outgoing, incoming, siblings) = follow_ups(&store, root, &node).expect("follow ups");
    let by_category: [HashSet<String>; 3] = [
        outgoing.iter().map(|e| e.name.clone()).collect(),
        incoming
            .iter()
            .map(|e| e.name.rsplit("::").next().unwrap_or(&e.name).to_owned())
            .collect(),
        siblings.iter().map(|m| m.name.clone()).collect(),
    ];
    for (slot, names) in by_category.iter().enumerate() {
        let found = names
            .iter()
            .filter(|name| present_with_site(&wide_text, name))
            .count();
        row.categories[slot] = (names.len(), found);
    }
    let distinct: HashSet<String> = by_category.iter().flatten().cloned().collect();
    row.targets = distinct.len();
    for name in &distinct {
        let new_any = present_anywhere(&wide_text, name);
        let new_site = present_with_site(&wide_text, name);
        let old_any = present_anywhere(&old_text, name);
        let old_site = present_with_site(&old_text, name);
        row.anywhere.0 += usize::from(new_any);
        row.anywhere.1 += usize::from(old_any);
        row.strict.0 += usize::from(new_site);
        row.strict.1 += usize::from(old_site);
        if !new_site {
            row.missing_new.push(name.clone());
        }
    }
    row
}

const LINE_KINDS: [&str; 7] = [
    "header",
    "sig",
    "doc",
    "calls",
    "called by",
    "tests",
    "members",
];

fn line_chars(text: &str) -> [usize; 7] {
    let mut sums = [0; 7];
    for line in text.lines() {
        let trimmed = line.trim_start();
        let slot = LINE_KINDS
            .iter()
            .skip(1)
            .position(|kind| trimmed.starts_with(&format!("{kind}:")))
            .map_or(0, |found| found + 1);
        sums[slot] += line.chars().count() + 1;
    }
    sums
}

fn share(count: usize, targets: usize) -> f64 {
    if targets == 0 {
        1.0
    } else {
        count as f64 / targets as f64
    }
}

async fn run_tokens(out: &Path) {
    let work = tempfile::tempdir().expect("work dir");
    let (shard, root, config, _pool) = open_index(work.path()).await;
    let golds = load_gold();
    let mut rows = Vec::new();
    for gold in &golds {
        rows.push(measure(&shard, &root, &config, gold).await);
    }
    assert_eq!(rows.len(), 44, "N queries");
    report(&rows, &root);
    let dump: Vec<_> = rows
        .iter()
        .map(|row| {
            json!({"query": row.query, "old": row.old, "new": row.new, "budget400": row.budgeted,
            "targets": row.targets, "missing_new": row.missing_new})
        })
        .collect();
    std::fs::write(out, serde_json::to_string_pretty(&dump).expect("json")).expect("write");
}

fn report(rows: &[Row], root: &Path) {
    for (label, slot) in [("-n 5", 0), ("-n 10", 1)] {
        for (arm, pick) in [
            ("old --lines", 0),
            ("new default", 1),
            ("new --budget 400", 2),
        ] {
            let values: Vec<usize> = rows
                .iter()
                .map(|row| [row.old, row.new, row.budgeted][pick][slot])
                .collect();
            let (median, p95) = summary(&values);
            println!(
                "M1 {label} {arm}: N={} median={median} p95={p95} sum={}",
                values.len(),
                values.iter().sum::<usize>()
            );
        }
    }
    let with_node: Vec<&Row> = rows.iter().filter(|row| row.has_node).collect();
    println!(
        "M2 gold chunks with a primary symbol: {} of {}",
        with_node.len(),
        rows.len()
    );
    println!(
        "M2 name+signature present (top-5): new {} of {}, old {} of {}",
        with_node.iter().filter(|r| r.name_sig.0).count(),
        rows.len(),
        with_node.iter().filter(|r| r.name_sig.1).count(),
        rows.len()
    );
    let docs: Vec<&&Row> = with_node.iter().filter(|r| r.doc.is_some()).collect();
    println!(
        "M2 gold symbols with a doc line: {}; doc present new {}, old {}",
        docs.len(),
        docs.iter().filter(|r| r.doc.is_some_and(|d| d.0)).count(),
        docs.iter().filter(|r| r.doc.is_some_and(|d| d.1)).count()
    );
    let all_three_new = with_node
        .iter()
        .filter(|r| r.name_sig.0 && r.doc.is_none_or(|d| d.0))
        .count();
    println!(
        "M2 name AND signature AND doc (doc only where one exists): new {all_three_new} of {}",
        rows.len()
    );
    let total: usize = rows.iter().flat_map(|row| row.line_chars).sum();
    for (slot, kind) in LINE_KINDS.iter().enumerate() {
        let chars: usize = rows.iter().map(|row| row.line_chars[slot]).sum();
        println!(
            "M1 share of new default -n 5 characters, {kind} lines: {chars} of {total} ({:.1}%)",
            100.0 * chars as f64 / total as f64
        );
    }
    sufficiency(rows, root);
}

fn sufficiency(rows: &[Row], root: &Path) {
    let with_targets: Vec<&Row> = rows
        .iter()
        .filter(|row| row.has_node && row.targets > 0)
        .collect();
    println!(
        "M6 queries with a primary symbol and >= 1 follow-up target: {} of {}",
        with_targets.len(),
        rows.len()
    );
    let pass = |pick: &dyn Fn(&Row) -> usize| {
        with_targets
            .iter()
            .filter(|r| share(pick(r), r.targets) >= SUFFICIENT)
            .count()
    };
    println!(
        "M6 >= 80% targets present, name anywhere: new(-n 5 --budget 800) {} old(--lines -n 5) {}",
        pass(&|r| r.anywhere.0),
        pass(&|r| r.anywhere.1)
    );
    println!(
        "M6 >= 80% targets present, name with :line on its line: new {} old {}",
        pass(&|r| r.strict.0),
        pass(&|r| r.strict.1)
    );
    let shown: Vec<&&Row> = with_targets.iter().filter(|r| r.name_sig.0).collect();
    println!(
        "M6 restricted to the {} queries whose gold symbol is in the top 5: >= 80% present (name with :line) new {}, old {}",
        shown.len(),
        shown
            .iter()
            .filter(|r| share(r.strict.0, r.targets) >= SUFFICIENT)
            .count(),
        shown
            .iter()
            .filter(|r| share(r.strict.1, r.targets) >= SUFFICIENT)
            .count()
    );
    let total: usize = with_targets.iter().map(|r| r.targets).sum();
    println!(
        "M6 target totals {total}: present new anywhere {} strict {}, old anywhere {} strict {}",
        with_targets.iter().map(|r| r.anywhere.0).sum::<usize>(),
        with_targets.iter().map(|r| r.strict.0).sum::<usize>(),
        with_targets.iter().map(|r| r.anywhere.1).sum::<usize>(),
        with_targets.iter().map(|r| r.strict.1).sum::<usize>()
    );
    for (slot, label) in ["callees", "callers", "container siblings"]
        .into_iter()
        .enumerate()
    {
        let total: usize = with_targets.iter().map(|r| r.categories[slot].0).sum();
        let found: usize = with_targets.iter().map(|r| r.categories[slot].1).sum();
        let queries = with_targets
            .iter()
            .filter(|r| r.categories[slot].0 > 0)
            .count();
        println!(
            "M6 recall (name with :line, -n 5 --budget 800) {label}: {found} of {total} targets over {queries} queries"
        );
    }
    let names: HashSet<&String> = with_targets
        .iter()
        .flat_map(|r| r.missing_new.iter())
        .collect();
    let mut rg: Vec<usize> = names.iter().map(|name| rg_tokens(root, name)).collect();
    rg.sort_unstable();
    let median_rg = rg.get(rg.len() / 2).copied().unwrap_or(0);
    println!(
        "M6 rg -nw result tokens over {} missing target names: median {median_rg}",
        rg.len()
    );
    let new_tokens: Vec<usize> = with_targets.iter().map(|r| r.resp_new).collect();
    let old_tokens: Vec<usize> = with_targets.iter().map(|r| r.resp_old).collect();
    let chain_new: Vec<usize> = with_targets
        .iter()
        .map(|r| r.resp_new + r.missing_new.len() * median_rg)
        .collect();
    let chain_pure: Vec<usize> = with_targets
        .iter()
        .map(|r| (1 + r.targets) * median_rg)
        .collect();
    println!(
        "M6 median tokens: single new response {} | old response {} | new + rg for missing {} | pure rg chain {}",
        summary(&new_tokens).0,
        summary(&old_tokens).0,
        summary(&chain_new).0,
        summary(&chain_pure).0
    );
    println!(
        "M6 table: query | targets | present new(strict) | present old(any) | ss calls | rg followups after new | rg calls pure"
    );
    for row in &with_targets {
        println!(
            "  {:<60.60} | {:>3} | {:>3} | {:>3} | 1 | {:>3} | {:>3}",
            row.query,
            row.targets,
            row.strict.0,
            row.anywhere.1,
            row.missing_new.len(),
            1 + row.targets
        );
    }
}

async fn run_edges(out: &Path, source: &Path) {
    let work = tempfile::tempdir().expect("work dir");
    let pool = EmbedPool::spawn(2, || Ok(ss::embed::hash_embed));
    let root = work.path().join("tree");
    copy_dir(source, &root);
    let config = StoreConfig {
        dir: work.path().join("shard"),
        model_id: EmbedProfile::default().store_id(EmbedderKind::Hash),
    };
    let shard = Shard::new(&root, config.clone(), &pool);
    index_tree(&shard, &root).await.expect("index");
    let store = Store::open(&config).expect("store");
    let mut lines = Vec::new();
    for edge in store
        .edges_of_kinds(&[EdgeKind::Calls, EdgeKind::Imports])
        .expect("edges")
    {
        let found = candidates(&store, &edge).expect("candidates");
        let resolution = resolution_of(&edge.edge.qual, found.len());
        if resolution == ss::graph::Resolution::Unresolved {
            continue;
        }
        let targets: Vec<String> = found
            .iter()
            .map(|n| format!("{}:{}", n.file, n.node.start_line))
            .collect();
        lines.push(format!(
            "{}\t{}:{}\t{}\t{}\t{}\t{}\t{}",
            resolution.as_str(),
            edge.file,
            edge.edge.line,
            edge.edge.kind.as_str(),
            edge.edge.src,
            edge.edge.dst,
            edge.edge.qual,
            targets.join(" ")
        ));
    }
    std::fs::write(out, lines.join("\n") + "\n").expect("write edges");
    println!(
        "resolved call/import edges: {}; nodes {} edges {} (all kinds, all resolutions)",
        lines.len(),
        store.node_count().expect("nodes"),
        store.edge_count().expect("edges")
    );
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    let out = PathBuf::from(args.get(2).expect("output path"));
    match args.get(1).map(String::as_str) {
        Some("tokens") => run_tokens(&out).await,
        Some("edges") => {
            let source = args
                .get(3)
                .map_or_else(|| data_dir().join("corpus"), PathBuf::from);
            run_edges(&out, &source).await;
        }
        other => panic!("usage: graph_eval tokens|edges <out>, got {other:?}"),
    }
}

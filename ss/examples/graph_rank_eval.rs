//! Compares the plain hybrid ranking with the graph-aware one on the 44 gold queries, split dev / held-out.
//!
//! `graph_rank_eval` cold-indexes the frozen corpus with the real CPU embedder once, then ranks every gold query with
//! `graph_boost` off and on. Dev is gold start line odd, held-out is even. Vector-only is printed for both arms; the
//! boost only touches the fused list, so its numbers must be equal.
// harness: aborts on any setup failure and divides counts into ratios for display
#![allow(clippy::expect_used, clippy::cast_precision_loss)]

use std::path::{Path, PathBuf};

use proxima::pipe::Pipe;
use ss::embed::{EmbedPool, EmbedProfile, EmbedRequest, EmbedderKind, Lane, load_fastembed};
use ss::index::{Shard, index_tree};
use ss::query::search;
use ss::store::{Store, StoreConfig};

const DEPTH: usize = 50;

struct Gold {
    query: String,
    chunk: String,
    start: u32,
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
            let (start, _) = cells[2].split_once('-').expect("start-end");
            Gold {
                query: cells[0].to_owned(),
                chunk: format!("{}:{}", cells[1], cells[2]),
                start: start.parse().expect("start"),
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

#[derive(Clone, Copy, Default, PartialEq)]
struct Scores {
    n: usize,
    hit1: f64,
    hit3: f64,
    hit5: f64,
    mrr: f64,
}

fn scores(ranks: &[usize]) -> Scores {
    let share = |limit: usize| {
        ranks.iter().filter(|rank| **rank <= limit).count() as f64 / ranks.len() as f64
    };
    let mrr = ranks.iter().map(|rank| 1.0 / *rank as f64).sum::<f64>() / ranks.len() as f64;
    Scores {
        n: ranks.len(),
        hit1: share(1),
        hit3: share(3),
        hit5: share(5),
        mrr,
    }
}

fn rank_in(ids: &[String], gold: &str) -> usize {
    ids.iter()
        .position(|id| id == gold)
        .map_or(usize::MAX, |at| at + 1)
}

fn show(label: &str, value: Scores) {
    println!(
        "{label}: N={} hit@1 {:.3} hit@3 {:.3} hit@5 {:.3} MRR {:.3}",
        value.n, value.hit1, value.hit3, value.hit5, value.mrr
    );
}

fn drops(base: Scores, changed: Scores) -> bool {
    changed.hit1 < base.hit1 - 1e-9
        || changed.hit3 < base.hit3 - 1e-9
        || changed.hit5 < base.hit5 - 1e-9
        || changed.mrr < base.mrr - 1e-9
}

fn improves(base: Scores, changed: Scores) -> bool {
    changed.hit3 > base.hit3 + 1e-9 || changed.mrr > base.mrr + 1e-9
}

async fn hybrid_ranks(shard: &Shard, root: &Path, golds: &[Gold]) -> Vec<usize> {
    let mut ranks = Vec::new();
    for gold in golds {
        let hits = search(shard, root, &gold.query, DEPTH)
            .await
            .expect("search");
        let ids: Vec<String> = hits
            .iter()
            .map(|hit| {
                let rel = Path::new(&hit.path).strip_prefix(root).expect("under root");
                format!(
                    "{}:{}-{}",
                    rel.to_string_lossy(),
                    hit.start_line,
                    hit.end_line
                )
            })
            .collect();
        ranks.push(rank_in(&ids, &gold.chunk));
    }
    ranks
}

async fn vector_ranks(store: &Store, pool: &EmbedPool, golds: &[Gold]) -> Vec<usize> {
    let mut ranks = Vec::new();
    for gold in golds {
        let embedding = pool
            .call(EmbedRequest {
                texts: vec![gold.query.clone()],
                lane: Lane::Query,
            })
            .await
            .expect("embed")
            .remove(0);
        let ids: Vec<String> = store
            .vector_search(&embedding, "", DEPTH)
            .expect("vector")
            .iter()
            .filter_map(|row| store.chunk(*row).expect("chunk"))
            .map(|row| format!("{}:{}-{}", row.path, row.start_line, row.end_line))
            .collect();
        ranks.push(rank_in(&ids, &gold.chunk));
    }
    ranks
}

fn split<'a>(golds: &'a [Gold], ranks: &'a [usize], dev: bool) -> Vec<usize> {
    golds
        .iter()
        .zip(ranks)
        .filter(|(gold, _)| (gold.start % 2 == 1) == dev)
        .map(|(_, rank)| *rank)
        .collect()
}

#[tokio::main]
async fn main() {
    let work = tempfile::tempdir().expect("work dir");
    let root = work.path().join("tree");
    copy_dir(&data_dir().join("corpus"), &root);
    let profile = EmbedProfile::default();
    let cache = work.path().join("model");
    std::fs::create_dir_all(&cache).expect("cache");
    let config = StoreConfig {
        dir: work.path().join("shard"),
        model_id: profile.store_id(EmbedderKind::FastEmbed),
    };
    let pool = EmbedPool::with_lanes(1, 2, move || load_fastembed(&cache, 2, profile));
    let shard = Shard::new(&root, config.clone(), &pool);
    index_tree(&shard, &root).await.expect("index");
    let golds = load_gold();
    assert_eq!(golds.len(), 44, "N queries");
    let plain = hybrid_ranks(&shard, &root, &golds).await;
    let boosted = hybrid_ranks(&shard.clone().with_graph_boost(true), &root, &golds).await;
    let store = Store::open(&config).expect("store");
    let vector = vector_ranks(&store, &pool, &golds).await;
    show("vector-only (both arms)", scores(&vector));
    let mut violated = false;
    for (name, dev) in [
        ("dev", Some(true)),
        ("held-out", Some(false)),
        ("all 44", None),
    ] {
        let pick =
            |ranks: &[usize]| dev.map_or_else(|| ranks.to_vec(), |flag| split(&golds, ranks, flag));
        let (base, changed) = (scores(&pick(&plain)), scores(&pick(&boosted)));
        show(&format!("hybrid plain   {name}"), base);
        show(&format!("hybrid boosted {name}"), changed);
        println!(
            "  {name}: improves hit@3 or MRR: {}; any hit@1/3/5/MRR drop: {}",
            improves(base, changed),
            drops(base, changed)
        );
        violated |= drops(base, changed);
    }
    let moved = plain
        .iter()
        .zip(&boosted)
        .filter(|(left, right)| left != right)
        .count();
    println!(
        "queries whose gold rank changed: {moved} of {}",
        golds.len()
    );
    for ((gold, before), after) in golds.iter().zip(&plain).zip(&boosted) {
        if before != after {
            println!("  rank {before} -> {after}: {}", gold.query);
        }
    }
    println!("any drop on dev, held-out or all: {violated}");
}

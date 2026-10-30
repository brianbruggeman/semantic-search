//! Pins the hybrid ranking of every gold query over the frozen corpus, so a change that must not touch ranking can prove it.
// tests assert with expect messages
#![allow(clippy::expect_used)]

use std::path::{Path, PathBuf};

use ss::embed::{EmbedPool, EmbedProfile, EmbedderKind, hash_embed};
use ss::index::{Shard, index_changed};
use ss::query::search;
use ss::store::StoreConfig;

const DEPTH: usize = 10;

fn data_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data")
}

fn gold_queries() -> Vec<String> {
    std::fs::read_to_string(data_dir().join("queries.tsv"))
        .expect("queries.tsv")
        .lines()
        .filter(|line| !line.starts_with('#') && !line.trim().is_empty())
        .filter_map(|line| line.split('\t').next().map(str::to_owned))
        .collect()
}

/// Files are indexed one at a time in path order so row ids, which break score ties, are the same on every run.
fn corpus_files(root: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = ignore::WalkBuilder::new(root)
        .build()
        .flatten()
        .map(ignore::DirEntry::into_path)
        .filter(|path| path.is_file())
        .collect();
    files.sort();
    files
}

async fn ranked_top_chunks() -> Vec<String> {
    let root = data_dir().join("corpus");
    let work = tempfile::tempdir().expect("tempdir");
    let config = StoreConfig {
        dir: work.path().join("shard"),
        model_id: EmbedProfile::default().store_id(EmbedderKind::Hash),
    };
    let pool = EmbedPool::spawn(2, || Ok(hash_embed));
    let shard = Shard::new(&root, config, &pool);
    for file in corpus_files(&root) {
        index_changed(&shard, vec![file]).await.expect("index file");
    }
    let mut rows = Vec::new();
    for query in gold_queries() {
        let hits = search(&shard, &root, &query, DEPTH).await.expect("search");
        let scored: Vec<(f64, String)> = hits
            .iter()
            .map(|hit| {
                let rel = Path::new(&hit.path)
                    .strip_prefix(&root)
                    .expect("under root");
                (
                    hit.score,
                    format!("{}:{}-{}", rel.display(), hit.start_line, hit.end_line),
                )
            })
            .collect();
        let ids: Vec<String> = scored.into_iter().map(|(_, id)| id).collect();
        rows.push(format!("{query}\t{}", ids.join(" ")));
    }
    rows
}

#[proxima::test(runtime = "tokio")]
async fn hybrid_top_ten_of_all_44_gold_queries_match_the_pinned_ranking() {
    let rows = ranked_top_chunks().await;
    let path = data_dir().join("hybrid_ranks_hash.tsv");
    if std::env::var_os("SS_BLESS_RANKS").is_some() {
        std::fs::write(&path, rows.join("\n") + "\n").expect("write snapshot");
    }
    let pinned = std::fs::read_to_string(&path).expect("pinned ranking snapshot");
    let expected: Vec<&str> = pinned.lines().collect();

    assert_eq!(rows.len(), 44, "gold query count");
    assert_eq!(expected.len(), rows.len(), "snapshot row count");
    for (row, pin) in rows.iter().zip(&expected) {
        assert_eq!(row, pin, "ranking changed");
    }
}

//! The in-memory vector cache returns exactly what the SQLite scan returns, follows writes, and never mixes generations.
// tests assert with expect messages
#![allow(clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

use ss::chunk::{Language, chunk_source};
use ss::embed::{EmbedProfile, EmbedderKind, hash_embed};
use ss::graph::FileGraph;
use ss::store::{FileRow, Store, StoreConfig, vector_cache};

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

fn config(dir: &Path) -> StoreConfig {
    StoreConfig {
        dir: dir.join("shard"),
        model_id: EmbedProfile::default().store_id(EmbedderKind::Hash),
    }
}

fn embed(text: &str) -> Vec<f32> {
    hash_embed(&[text.to_owned()])
        .expect("embed")
        .remove(0)
}

fn index_text(store: &mut Store, path: &str, text: &str) {
    let chunks = chunk_source(Language::from_extension("rs"), text);
    let texts: Vec<String> = chunks.iter().map(|chunk| chunk.text.clone()).collect();
    let vectors: Vec<_> = chunks
        .iter()
        .zip(hash_embed(&texts).expect("embed"))
        .map(|(chunk, vector)| (chunk.key, vector))
        .collect();
    let hash = chunks.first().map_or([0; 32], |chunk| chunk.key);
    let row = FileRow { mtime_ns: 1, size: 1, hash };
    store
        .replace_file(path, &row, &chunks, &vectors, &FileGraph::default())
        .expect("replace file");
}

fn index_corpus(store: &mut Store) {
    let root = data_dir().join("corpus");
    let mut files: Vec<PathBuf> = ignore::WalkBuilder::new(&root)
        .build()
        .flatten()
        .map(ignore::DirEntry::into_path)
        .filter(|path| path.is_file())
        .collect();
    files.sort();
    for file in files {
        if let Ok(text) = std::fs::read_to_string(&file) {
            index_text(store, &file.to_string_lossy(), &text);
        }
    }
}

#[proxima::test(runtime = "tokio")]
async fn cache_and_sqlite_scan_agree_on_ids_and_order_for_all_44_gold_queries() {
    let work = tempfile::tempdir().expect("tempdir");
    let mut store = Store::open(&config(work.path())).expect("open");
    index_corpus(&mut store);
    let queries = gold_queries();
    assert_eq!(queries.len(), 44, "gold query count");

    for query in &queries {
        let embedding = embed(query);
        let cached = store.vector_search(&embedding, "", 50).expect("cached");
        let scanned = store.scan_sqlite(&embedding, "", 50).expect("scan");
        assert!(!cached.is_empty(), "hits for {query}");
        assert_eq!(cached, scanned, "ordering for {query}");
    }
    assert_eq!(
        vector_cache::cached_generation(&config(work.path()).dir),
        Some(store.generation().expect("generation"))
    );
}

#[proxima::test(runtime = "tokio")]
async fn a_write_bumps_the_generation_and_the_next_query_sees_it() {
    let work = tempfile::tempdir().expect("tempdir");
    let mut store = Store::open(&config(work.path())).expect("open");
    index_corpus(&mut store);
    let probe = embed("zebra quasar nebula");
    let before = store.generation().expect("generation");
    let stale = store.vector_search(&probe, "added.rs", 5).expect("search");
    assert!(stale.is_empty(), "nothing under added.rs yet");

    let body = "pub fn zebra_quasar_nebula() {\n    zebra();\n    quasar();\n    nebula();\n}\n";
    index_text(&mut store, "added.rs", body);

    assert!(store.generation().expect("generation") > before, "gen bumped");
    let fresh = store.vector_search(&probe, "added.rs", 5).expect("search");
    assert!(!fresh.is_empty(), "next query sees the new chunk");
    assert_eq!(fresh, store.scan_sqlite(&probe, "added.rs", 5).expect("scan"));
}

#[proxima::test(runtime = "tokio")]
async fn queries_during_a_reindex_see_one_generation_and_return_hits() {
    let work = tempfile::tempdir().expect("tempdir");
    let settings = config(work.path());
    let mut writer = Store::open(&settings).expect("open");
    index_corpus(&mut writer);
    let done = Arc::new(AtomicBool::new(false));
    let readers: Vec<_> = (0..4)
        .map(|_| {
            let (settings, done) = (settings.clone(), Arc::clone(&done));
            thread::spawn(move || {
                let store = Store::open_reader(&settings).expect("reader open");
                let probe = embed("worker pool threads");
                let mut searches = 0_usize;
                while !done.load(Ordering::Relaxed) || searches == 0 {
                    let snapshot = store.read_snapshot().expect("snapshot");
                    let cached = store.vector_search(&probe, "", 20).expect("cached");
                    let scanned = store.scan_sqlite(&probe, "", 20).expect("scan");
                    drop(snapshot);
                    assert!(!cached.is_empty(), "hits during reindex");
                    assert_eq!(cached, scanned, "one search, one generation");
                    searches += 1;
                }
                searches
            })
        })
        .collect();
    for round in 0..30 {
        let text = format!("fn worker_pool_{round}() {{ threads(); }}\nfn other_{round}() {{}}\n");
        index_text(&mut writer, "churn.rs", &text);
    }
    done.store(true, Ordering::Relaxed);
    for reader in readers {
        assert!(reader.join().expect("reader") > 0, "reader ran searches");
    }
}

#[proxima::test(runtime = "tokio")]
async fn a_shard_over_the_limit_falls_back_to_the_sqlite_scan() {
    let work = tempfile::tempdir().expect("tempdir");
    let mut store = Store::open(&config(work.path())).expect("open");
    index_corpus(&mut store);
    vector_cache::set_limit_mb(0);

    let embedding = embed("worker pool threads");
    let hits = store.vector_search(&embedding, "", 20).expect("search");

    assert!(!hits.is_empty(), "fallback still returns hits");
    assert_eq!(hits, store.scan_sqlite(&embedding, "", 20).expect("scan"));
    assert_eq!(
        vector_cache::cached_generation(&config(work.path()).dir),
        None,
        "nothing cached over the limit"
    );
}

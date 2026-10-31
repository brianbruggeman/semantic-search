//! The segment list returns exactly what the SQLite scan returns, follows writes in one swap, and keeps old views.
// tests assert with expect messages
#![allow(clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;

use ss::chunk::{Language, chunk_source};
use ss::embed::{EmbedProfile, EmbedderKind, hash_embed};
use ss::graph::FileGraph;
use ss::store::{FileRow, Store, StoreConfig, vector_cache};

const BODY: &str =
    "pub fn zebra_quasar_nebula() {\n    zebra();\n    quasar();\n    nebula();\n}\n";

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
    hash_embed(&[text.to_owned()]).expect("embed").remove(0)
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
    let row = FileRow {
        mtime_ns: 1,
        size: 1,
        hash,
    };
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

fn loaded_store(work: &Path) -> Store {
    let mut store = Store::open(&config(work)).expect("open");
    index_corpus(&mut store);
    store.load_segments().expect("load");
    store
}

fn assert_all_gold_agree(store: &Store, stage: &str) {
    let queries = gold_queries();
    assert_eq!(queries.len(), 44, "gold query count");
    for query in &queries {
        let embedding = embed(query);
        let listed = store.vector_search(&embedding, "", 50).expect("listed");
        let scanned = store.scan_sqlite(&embedding, "", 50).expect("scan");
        assert!(!listed.is_empty(), "hits for {query} {stage}");
        assert_eq!(listed, scanned, "ordering for {query} {stage}");
    }
}

#[proxima::test(runtime = "tokio")]
async fn segments_and_sqlite_scan_agree_for_all_44_gold_queries_across_a_replace_and_a_remove() {
    let work = tempfile::tempdir().expect("tempdir");
    let mut store = loaded_store(work.path());
    assert_all_gold_agree(&store, "after load");
    let mut paths = store.file_paths().expect("paths");
    paths.sort();
    let victim = paths.remove(0);

    index_text(&mut store, &victim, BODY);
    assert_all_gold_agree(&store, "after replace");

    store.remove_file(&victim).expect("remove");
    assert_all_gold_agree(&store, "after remove");
}

#[proxima::test(runtime = "tokio")]
async fn a_replaced_file_tombstones_its_old_chunks_and_serves_the_new_one() {
    let work = tempfile::tempdir().expect("tempdir");
    let mut store = loaded_store(work.path());
    let probe = embed("zebra quasar nebula");
    assert_eq!(
        store.vector_search(&probe, "added.rs", 5).expect("search"),
        Vec::<i64>::new()
    );

    index_text(&mut store, "added.rs", BODY);
    let present = store.vector_search(&probe, "added.rs", 5).expect("search");
    assert_ne!(present, Vec::<i64>::new());
    index_text(&mut store, "added.rs", "pub fn only_this() {}\n");

    let after = store.vector_search(&probe, "added.rs", 5).expect("search");
    assert_eq!(after.len(), 1, "old chunk tombstoned, one new chunk");
    assert_eq!(
        after,
        store.scan_sqlite(&probe, "added.rs", 5).expect("scan")
    );
    assert!(
        vector_cache::counters().tombstones > 0,
        "tombstones counted"
    );
}

#[proxima::test(runtime = "tokio")]
async fn compaction_leaves_results_unchanged_and_drops_tombstoned_rows() {
    let work = tempfile::tempdir().expect("tempdir");
    let mut store = loaded_store(work.path());
    for number in 0..8 {
        index_text(
            &mut store,
            &format!("w{number}.rs"),
            &format!("pub fn writer_{number}() {{}}\n"),
        );
    }
    let probe = embed("worker pool threads");
    let before = store.vector_search(&probe, "", 50).expect("before");
    let compactions = vector_cache::counters().compactions;

    vector_cache::compact(&config(work.path()).dir);

    assert_eq!(store.vector_search(&probe, "", 50).expect("after"), before);
    assert!(
        vector_cache::counters().compactions > compactions,
        "compaction ran"
    );
    assert_all_gold_agree(&store, "after compaction");
}

#[proxima::test(runtime = "tokio")]
async fn a_reader_holding_an_old_pointer_sees_a_consistent_old_view_across_a_swap() {
    let work = tempfile::tempdir().expect("tempdir");
    let mut store = loaded_store(work.path());
    let probe = embed("zebra quasar nebula");
    let dir = config(work.path()).dir;
    let held = vector_cache::current(&dir).expect("list");
    let old_hits = held.top(&probe, "", 20);

    index_text(&mut store, "added.rs", BODY);
    vector_cache::compact(&dir);

    assert_eq!(held.top(&probe, "", 20), old_hits, "held view unchanged");
    assert_ne!(
        store.vector_search(&probe, "", 20).expect("new"),
        old_hits,
        "new view differs"
    );
}

#[proxima::test(runtime = "tokio")]
async fn thirty_two_concurrent_searches_during_200_file_writes_all_return_hits() {
    let work = tempfile::tempdir().expect("tempdir");
    let mut writer = loaded_store(work.path());
    let settings = config(work.path());
    let gate = Arc::new(Barrier::new(33));
    let done = Arc::new(AtomicBool::new(false));
    let searchers: Vec<_> = (0..32)
        .map(|_| {
            let (settings, gate, done) = (settings.clone(), Arc::clone(&gate), Arc::clone(&done));
            thread::spawn(move || {
                let store = Store::open_reader(&settings).expect("reader open");
                let probe = embed("worker pool threads");
                gate.wait();
                let mut least = usize::MAX;
                while !done.load(Ordering::Relaxed) || least == usize::MAX {
                    least = least.min(store.vector_search(&probe, "", 20).expect("search").len());
                }
                least
            })
        })
        .collect();
    gate.wait();
    for number in 0..200 {
        index_text(
            &mut writer,
            &format!("sim{}.rs", number % 40),
            &format!("pub fn simulated_{number}() {{}}\n"),
        );
    }
    done.store(true, Ordering::Relaxed);

    for searcher in searchers {
        assert!(
            searcher.join().expect("thread") > 0,
            "every search returned hits"
        );
    }
}

#[proxima::test(runtime = "tokio")]
async fn racing_writers_on_one_file_publish_in_commit_order() {
    let work = tempfile::tempdir().expect("tempdir");
    let store = loaded_store(work.path());
    let settings = config(work.path());
    let writers: Vec<_> = (0..4)
        .map(|writer| {
            let settings = settings.clone();
            thread::spawn(move || {
                let mut store = Store::open(&settings).expect("writer open");
                for number in 0..50 {
                    index_text(
                        &mut store,
                        "contested.rs",
                        &format!("pub fn racer_{writer}_{number}() {{}}\n"),
                    );
                }
            })
        })
        .collect();
    for writer in writers {
        writer.join().expect("writer");
    }

    let probe = embed("racer");
    let listed = store
        .vector_search(&probe, "contested.rs", 50)
        .expect("listed");
    assert_eq!(listed.len(), 1, "only the last committed version is live");
    assert_eq!(
        listed,
        store.scan_sqlite(&probe, "contested.rs", 50).expect("scan")
    );
}

#[proxima::test(runtime = "tokio")]
async fn publishing_past_the_memory_cap_drops_the_list_and_falls_back_to_sqlite() {
    let work = tempfile::tempdir().expect("tempdir");
    let mut store = loaded_store(work.path());
    let dir = config(work.path()).dir;
    let before = vector_cache::counters().over_cap;
    assert!(vector_cache::current(&dir).is_some(), "list loaded");

    vector_cache::set_limit_mb(0);
    index_text(&mut store, "added.rs", BODY);
    vector_cache::set_limit_mb(512);

    assert!(
        vector_cache::current(&dir).is_none(),
        "list dropped over the cap"
    );
    assert!(
        vector_cache::counters().over_cap > before,
        "over_cap counted"
    );
    let probe = embed("zebra quasar nebula");
    assert_eq!(
        store.vector_search(&probe, "", 20).expect("fallback"),
        store.scan_sqlite(&probe, "", 20).expect("scan")
    );
}

#[proxima::test(runtime = "tokio")]
async fn a_write_is_searchable_immediately_without_taking_the_sqlite_scan_path() {
    let work = tempfile::tempdir().expect("tempdir");
    let mut store = loaded_store(work.path());
    let probe = embed("zebra quasar nebula");
    let scans_before = vector_cache::counters().scans;

    index_text(&mut store, "fresh.rs", BODY);
    let found = store.vector_search(&probe, "fresh.rs", 5).expect("search");

    assert!(!found.is_empty(), "new content visible with no wait");
    assert_eq!(
        vector_cache::counters().scans,
        scans_before,
        "no scan_sqlite fallback"
    );
}

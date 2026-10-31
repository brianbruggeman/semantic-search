//! `Store::open_reader` opens an existing shard read-only: no write-side setup, no wait on the writer.
// tests assert with expect messages
#![allow(clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rusqlite::Connection;
use ss::chunk::{Language, chunk_source};
use ss::embed::{EmbedProfile, EmbedderKind, hash_embed};
use ss::error::IndexError;
use ss::graph::FileGraph;
use ss::store::{FileRow, Store, StoreConfig};

const WRITER_STALL_LIMIT: Duration = Duration::from_secs(2);

fn config(dir: &Path) -> StoreConfig {
    StoreConfig {
        dir: dir.join("shard"),
        model_id: EmbedProfile::default().store_id(EmbedderKind::Hash),
    }
}

fn embed(text: &str) -> Vec<f32> {
    hash_embed(&[text.to_owned()]).expect("embed").remove(0)
}

fn index_corpus(store: &mut Store) {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/corpus");
    let files: Vec<PathBuf> = ignore::WalkBuilder::new(&root)
        .build()
        .flatten()
        .map(ignore::DirEntry::into_path)
        .filter(|path| path.is_file())
        .collect();
    for file in files {
        let Ok(text) = std::fs::read_to_string(&file) else { continue };
        let chunks = chunk_source(Language::from_extension("rs"), &text);
        let texts: Vec<String> = chunks.iter().map(|chunk| chunk.text.clone()).collect();
        let vectors: Vec<_> = chunks
            .iter()
            .zip(hash_embed(&texts).expect("embed"))
            .map(|(chunk, vector)| (chunk.key, vector))
            .collect();
        let hash = chunks.first().map_or([0; 32], |chunk| chunk.key);
        let row = FileRow { mtime_ns: 1, size: 1, hash };
        store
            .replace_file(&file.to_string_lossy(), &row, &chunks, &vectors, &FileGraph::default())
            .expect("replace file");
    }
}

#[proxima::test(runtime = "tokio")]
async fn a_reader_on_a_never_indexed_directory_reports_not_indexed_and_creates_nothing() {
    let work = tempfile::tempdir().expect("tempdir");
    let settings = config(work.path());

    let outcome = Store::open_reader(&settings);

    assert!(matches!(outcome, Err(IndexError::NotIndexed(_))), "{:?}", outcome.err());
    assert!(!settings.dir.exists(), "a read must not create the shard");
}

#[proxima::test(runtime = "tokio")]
async fn a_reader_on_an_empty_database_without_the_meta_row_reports_not_indexed() {
    let work = tempfile::tempdir().expect("tempdir");
    let settings = config(work.path());
    std::fs::create_dir_all(&settings.dir).expect("shard dir");
    Connection::open(settings.dir.join("index.db")).expect("bare db");

    let outcome = Store::open_reader(&settings);

    assert!(matches!(outcome, Err(IndexError::NotIndexed(_))), "{:?}", outcome.err());
}

#[proxima::test(runtime = "tokio")]
async fn a_reader_with_a_different_model_identity_is_rejected_not_queried() {
    let work = tempfile::tempdir().expect("tempdir");
    let settings = config(work.path());
    drop(Store::open(&settings).expect("open"));
    let other = StoreConfig {
        dir: settings.dir.clone(),
        model_id: "bge-small-en-v1.5+coreml_all+len128".to_owned(),
    };

    let outcome = Store::open_reader(&other);

    assert!(matches!(outcome, Err(IndexError::IdentityMismatch { .. })), "{:?}", outcome.err());
}

#[proxima::test(runtime = "tokio")]
async fn a_reader_opened_during_a_long_write_transaction_returns_hits_without_waiting_for_it() {
    let work = tempfile::tempdir().expect("tempdir");
    let settings = config(work.path());
    let mut writer = Store::open(&settings).expect("open");
    index_corpus(&mut writer);
    let holder = Connection::open(settings.dir.join("index.db")).expect("holder");
    holder
        .execute_batch("BEGIN IMMEDIATE; INSERT OR REPLACE INTO meta(k, v) VALUES ('held', '1');")
        .expect("hold the write lock");
    let probe = embed("worker pool threads");

    let started = Instant::now();
    let reader = Store::open_reader(&settings).expect("reader opens under a write lock");
    let snapshot = reader.read_snapshot().expect("snapshot");
    let hits = reader.vector_search(&probe, "", 20).expect("search");
    drop(snapshot);
    let elapsed = started.elapsed();

    assert!(!hits.is_empty(), "N>0 hits while the write lock is held");
    assert!(elapsed < WRITER_STALL_LIMIT, "reader waited {elapsed:?} on the writer");
    holder.execute_batch("ROLLBACK;").expect("release");
}

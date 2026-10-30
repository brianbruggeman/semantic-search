use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use futures::stream::{self, StreamExt};
use proxima::pipe::Pipe;
use ss_proto::IndexStats;

use crate::embed::EmbedPool;
use crate::error::IndexError;
use crate::pipeline::{FilePipeline, file_pipeline};
use crate::store::{Store, StoreConfig};
use crate::walk::Walk;

pub const FILE_PARALLELISM: usize = 8;

/// Everything needed to index one root: its shard config, embed pool and per-file pipeline.
#[derive(Clone)]
pub struct Shard {
    pub root: PathBuf,
    pub config: Arc<StoreConfig>,
    pool: EmbedPool,
    pipeline: Arc<FilePipeline>,
}

impl Shard {
    #[must_use]
    pub fn new(root: &Path, config: StoreConfig, pool: &EmbedPool) -> Self {
        let config = Arc::new(config);
        let pipeline = Arc::new(file_pipeline(root, &config, pool));
        Self {
            root: root.to_path_buf(),
            config,
            pool: pool.clone(),
            pipeline,
        }
    }

    #[must_use]
    pub fn pool(&self) -> &EmbedPool {
        &self.pool
    }
}

#[proxima::instrument]
pub async fn index_tree(shard: &Shard, scope: &Path) -> Result<IndexStats, IndexError> {
    let config = Arc::clone(&shard.config);
    let root = shard.root.clone();
    tokio::task::spawn_blocking(move || {
        Store::open(&config)?
            .set_root(&root)
            .map_err(IndexError::from)
    })
    .await
    .map_err(|error| IndexError::Join(error.to_string()))??;
    let files = Walk::new(scope).call(()).await?;
    let present: HashSet<String> = files.iter().map(|path| rel_of(&shard.root, path)).collect();
    let mut stats = run_pipeline(shard, files).await?;
    stats.files_removed += remove_vanished(shard, scope, &present).await?;
    Ok(stats)
}

/// Applies watcher hints: files flow through the pipeline, new directories are walked, and paths that no
/// longer exist (a file or a whole directory) are dropped from the store.
#[proxima::instrument]
pub async fn index_changed(shard: &Shard, paths: Vec<PathBuf>) -> Result<IndexStats, IndexError> {
    let (present, gone): (Vec<PathBuf>, Vec<PathBuf>) =
        paths.into_iter().partition(|path| path.exists());
    let (dirs, files): (Vec<PathBuf>, Vec<PathBuf>) =
        present.into_iter().partition(|path| path.is_dir());
    let mut stats = run_pipeline(shard, files).await?;
    for dir in dirs {
        stats.absorb(index_tree(shard, &dir).await?);
    }
    let config = Arc::clone(&shard.config);
    let rels: Vec<String> = gone.iter().map(|path| rel_of(&shard.root, path)).collect();
    stats.files_removed += tokio::task::spawn_blocking(move || remove_gone(&config, &rels))
        .await
        .map_err(|error| IndexError::Join(error.to_string()))??;
    Ok(stats)
}

fn remove_gone(config: &StoreConfig, rels: &[String]) -> Result<u64, IndexError> {
    let store = Store::open(config)?;
    let mut removed = 0;
    for path in store.file_paths()? {
        let vanished = rels
            .iter()
            .any(|rel| path == *rel || path.starts_with(&format!("{rel}/")));
        if vanished {
            store.remove_file(&path)?;
            removed += 1;
        }
    }
    store.gc_vectors()?;
    Ok(removed)
}

fn rel_of(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .components()
        .map(|part| part.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

async fn run_pipeline(shard: &Shard, files: Vec<PathBuf>) -> Result<IndexStats, IndexError> {
    let pipeline = Arc::clone(&shard.pipeline);
    let mut reports = stream::iter(files)
        .map(|path| {
            let pipeline = Arc::clone(&pipeline);
            async move { pipeline.call(path).await }
        })
        .buffer_unordered(FILE_PARALLELISM);
    let mut stats = IndexStats::default();
    while let Some(report) = reports.next().await {
        stats.absorb(report?.stats);
    }
    Ok(stats)
}

async fn remove_vanished(
    shard: &Shard,
    scope: &Path,
    present: &HashSet<String>,
) -> Result<u64, IndexError> {
    let config = Arc::clone(&shard.config);
    let prefix = rel_of(&shard.root, scope);
    let present = present.clone();
    tokio::task::spawn_blocking(move || -> Result<u64, IndexError> {
        let store = Store::open(&config)?;
        let mut removed = 0;
        for path in store.file_paths()? {
            let in_scope =
                prefix.is_empty() || path == prefix || path.starts_with(&format!("{prefix}/"));
            if in_scope && !present.contains(&path) {
                store.remove_file(&path)?;
                removed += 1;
            }
        }
        store.gc_vectors()?;
        Ok(removed)
    })
    .await
    .map_err(|error| IndexError::Join(error.to_string()))?
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embed::EmbedderKind;
    use crate::fixtures::{hash_pool, own_src_corpus};

    #[proxima::test(runtime = "tokio")]
    async fn first_index_of_own_src_embeds_every_chunk() {
        let corpus = own_src_corpus(&hash_pool(), EmbedderKind::Hash);

        let stats = index_tree(&corpus.shard, &corpus.root())
            .await
            .expect("index");

        assert!(stats.chunks_seen > 0, "no chunks seen");
        assert_eq!(
            stats.chunks_seen,
            stats.chunks_embedded + stats.chunks_skipped
        );
        assert_eq!(stats.chunks_seen, corpus.chunk_count());
        println!(
            "first index: files={} chunks={} embedded={}",
            stats.files_indexed, stats.chunks_seen, stats.chunks_embedded
        );
    }

    #[proxima::test(runtime = "tokio")]
    async fn reindexing_an_unchanged_tree_embeds_exactly_zero_chunks() {
        let corpus = own_src_corpus(&hash_pool(), EmbedderKind::Hash);
        let first = index_tree(&corpus.shard, &corpus.root())
            .await
            .expect("first index");

        let second = index_tree(&corpus.shard, &corpus.root())
            .await
            .expect("second index");

        assert!(first.chunks_embedded > 0);
        assert_eq!((second.chunks_embedded, second.files_indexed), (0, 0));
        assert_eq!(second.files_unchanged, first.files_indexed);
    }

    #[proxima::test(runtime = "tokio")]
    async fn touching_a_file_without_changing_it_embeds_zero_via_content_hash() {
        let corpus = own_src_corpus(&hash_pool(), EmbedderKind::Hash);
        index_tree(&corpus.shard, &corpus.root())
            .await
            .expect("first index");
        let target = corpus.root().join("src/walk.rs");
        let text = std::fs::read(&target).expect("read");
        std::fs::write(&target, &text).expect("rewrite identical bytes");

        let stats = index_tree(&corpus.shard, &corpus.root())
            .await
            .expect("second index");

        assert_eq!((stats.chunks_embedded, stats.files_indexed), (0, 0));
    }

    #[proxima::test(runtime = "tokio")]
    async fn editing_one_function_embeds_exactly_one_chunk() {
        let corpus = own_src_corpus(&hash_pool(), EmbedderKind::Hash);
        index_tree(&corpus.shard, &corpus.root())
            .await
            .expect("first index");
        let target = corpus.root().join("src/walk.rs");
        let edited = std::fs::read_to_string(&target)
            .expect("read")
            .replace("files.sort();", "files.sort_unstable();");
        std::fs::write(&target, edited).expect("edit");

        let stats = index_tree(&corpus.shard, &corpus.root())
            .await
            .expect("second index");

        assert_eq!((stats.files_indexed, stats.chunks_embedded), (1, 1));
        assert!(
            stats.chunks_skipped > 0,
            "other chunks of the edited file must be hash-skipped"
        );
    }

    #[proxima::test(runtime = "tokio")]
    async fn deleting_a_file_removes_its_chunks() {
        let corpus = own_src_corpus(&hash_pool(), EmbedderKind::Hash);
        index_tree(&corpus.shard, &corpus.root())
            .await
            .expect("first index");
        let before = corpus.chunk_count();
        std::fs::remove_file(corpus.root().join("src/walk.rs")).expect("delete");

        let stats = index_tree(&corpus.shard, &corpus.root())
            .await
            .expect("second index");

        let after = corpus.chunk_count();
        assert_eq!(stats.files_removed, 1);
        assert!(after < before && after > 0, "before={before} after={after}");
        println!("delete: chunks before={before} after={after}");
    }

    #[proxima::test(runtime = "tokio")]
    async fn gitignored_files_are_not_indexed() {
        let corpus = own_src_corpus(&hash_pool(), EmbedderKind::Hash);
        std::fs::write(corpus.root().join(".gitignore"), "src/walk.rs\n").expect("gitignore");

        let stats = index_tree(&corpus.shard, &corpus.root())
            .await
            .expect("index");

        let files = Store::open(&corpus.shard.config)
            .expect("open")
            .file_paths()
            .expect("paths");
        assert!(
            !files.is_empty() && !files.iter().any(|path| path == "src/walk.rs"),
            "files={files:?} stats={stats:?}"
        );
    }

    #[proxima::test(runtime = "tokio")]
    async fn changed_hints_index_a_new_file_and_drop_a_deleted_directory() {
        let corpus = own_src_corpus(&hash_pool(), EmbedderKind::Hash);
        index_tree(&corpus.shard, &corpus.root())
            .await
            .expect("first index");
        let before = corpus.chunk_count();
        let fresh = corpus.root().join("src/fresh_hint.rs");
        std::fs::write(&fresh, "pub fn fresh_hint() -> u32 {\n    let a = 1;\n    let b = 2;\n    let c = 3;\n    a + b + c\n}\n").expect("write");

        let added = index_changed(&corpus.shard, vec![fresh])
            .await
            .expect("hint for new file");
        let with_fresh = corpus.chunk_count();
        std::fs::remove_dir_all(corpus.root().join("src")).expect("delete tree");
        let removed = index_changed(&corpus.shard, vec![corpus.root().join("src")])
            .await
            .expect("hint for deleted dir");

        assert_eq!((added.files_indexed, added.chunks_embedded), (1, 1));
        assert!(
            with_fresh > before,
            "before={before} with_fresh={with_fresh}"
        );
        assert!(
            removed.files_removed > 1,
            "removed={}",
            removed.files_removed
        );
        assert_eq!(corpus.chunk_count(), 0);
    }

    fn graph_counts(corpus: &crate::fixtures::Corpus, path: &str) -> (u64, u64) {
        Store::open(&corpus.shard.config)
            .expect("open")
            .file_graph_counts(path)
            .expect("counts")
    }

    #[proxima::test(runtime = "tokio")]
    async fn indexing_own_src_stores_nodes_and_edges() {
        let corpus = own_src_corpus(&hash_pool(), EmbedderKind::Hash);

        index_tree(&corpus.shard, &corpus.root())
            .await
            .expect("index");

        let store = Store::open(&corpus.shard.config).expect("open");
        let (nodes, edges) = (
            store.node_count().expect("nodes"),
            store.edge_count().expect("edges"),
        );
        println!("own src graph: nodes={nodes} edges={edges}");
        assert!(nodes > 100 && edges > 500, "nodes={nodes} edges={edges}");
        assert!(graph_counts(&corpus, "src/query.rs").0 > 5);
    }

    #[proxima::test(runtime = "tokio")]
    async fn every_supported_language_fixture_stores_nodes_and_edges() {
        let corpus = own_src_corpus(&hash_pool(), EmbedderKind::Hash);
        let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/graph");
        crate::fixtures::copy_dir(&fixtures, &corpus.root().join("fixtures"));

        index_tree(&corpus.shard, &corpus.root())
            .await
            .expect("index");

        for name in [
            "inventory.py",
            "scheduler.ts",
            "tally.js",
            "cache.go",
            "cache_test.go",
        ] {
            let (nodes, edges) = graph_counts(&corpus, &format!("fixtures/{name}"));
            println!("{name}: nodes={nodes} edges={edges}");
            assert!(
                nodes > 1 && edges > 1,
                "{name}: nodes={nodes} edges={edges}"
            );
        }
    }

    #[proxima::test(runtime = "tokio")]
    async fn editing_one_function_rewrites_only_that_files_graph_rows() {
        let corpus = own_src_corpus(&hash_pool(), EmbedderKind::Hash);
        index_tree(&corpus.shard, &corpus.root())
            .await
            .expect("first index");
        let store = Store::open(&corpus.shard.config).expect("open");
        let walk_before = (
            graph_counts(&corpus, "src/walk.rs"),
            store.node_ids("src/walk.rs").expect("ids"),
        );
        let query_before = (
            graph_counts(&corpus, "src/query.rs"),
            store.node_ids("src/query.rs").expect("ids"),
        );
        let target = corpus.root().join("src/walk.rs");
        let edited = format!(
            "{}\npub fn added_by_edit() -> u32 {{\n    list_files_stub()\n}}\n",
            std::fs::read_to_string(&target).expect("read")
        );
        std::fs::write(&target, edited).expect("edit");

        index_tree(&corpus.shard, &corpus.root())
            .await
            .expect("second index");

        let walk_after = (
            graph_counts(&corpus, "src/walk.rs"),
            store.node_ids("src/walk.rs").expect("ids"),
        );
        let query_after = (
            graph_counts(&corpus, "src/query.rs"),
            store.node_ids("src/query.rs").expect("ids"),
        );
        assert_eq!(walk_after.0.0, walk_before.0.0 + 1, "one new node");
        assert_eq!(walk_after.0.1, walk_before.0.1 + 2, "contains + calls edge");
        assert_ne!(
            walk_after.1, walk_before.1,
            "edited file rows are rewritten"
        );
        assert_eq!(
            query_after, query_before,
            "untouched file rows are the same rows"
        );
    }

    #[proxima::test(runtime = "tokio")]
    async fn deleting_a_file_removes_its_graph_rows_and_leaves_calls_to_it_unresolved() {
        let corpus = own_src_corpus(&hash_pool(), EmbedderKind::Hash);
        index_tree(&corpus.shard, &corpus.root())
            .await
            .expect("first index");
        let store = Store::open(&corpus.shard.config).expect("open");
        let (nodes_before, edges_before) = (
            store.node_count().expect("nodes"),
            store.edge_count().expect("edges"),
        );
        let (walk_nodes, walk_edges) = graph_counts(&corpus, "src/walk.rs");
        std::fs::remove_file(corpus.root().join("src/walk.rs")).expect("delete");

        index_tree(&corpus.shard, &corpus.root())
            .await
            .expect("second index");

        assert!(walk_nodes > 0 && walk_edges > 0);
        assert_eq!(graph_counts(&corpus, "src/walk.rs"), (0, 0));
        assert_eq!(
            store.node_count().expect("nodes"),
            nodes_before - walk_nodes
        );
        assert_eq!(
            store.edge_count().expect("edges"),
            edges_before - walk_edges
        );
    }
}

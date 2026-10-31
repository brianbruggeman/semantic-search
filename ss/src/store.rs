use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread::sleep;
use std::time::Duration;

use proxima::telemetry::debug;
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};

use crate::chunk::{ChunkKey, RawChunk};
use crate::error::IndexError;
use crate::graph::{FileGraph, GraphEdge, GraphNode};
use vector_cache::VectorSnapshot;

mod graph_read;
pub mod vector_cache;

pub use graph_read::{EdgeRow, NodeRow};

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS meta(k TEXT PRIMARY KEY, v TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS files(path TEXT PRIMARY KEY, mtime_ns INTEGER NOT NULL, size INTEGER NOT NULL, hash BLOB NOT NULL);
CREATE TABLE IF NOT EXISTS vectors(key BLOB PRIMARY KEY, vec BLOB NOT NULL) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS chunks(id INTEGER PRIMARY KEY, path TEXT NOT NULL, start_line INTEGER NOT NULL, end_line INTEGER NOT NULL, key BLOB NOT NULL, text TEXT NOT NULL);
CREATE INDEX IF NOT EXISTS chunks_path ON chunks(path);
CREATE INDEX IF NOT EXISTS chunks_key ON chunks(key);
CREATE TABLE IF NOT EXISTS nodes(id INTEGER PRIMARY KEY, file TEXT NOT NULL, kind TEXT NOT NULL, name TEXT NOT NULL, qpath TEXT NOT NULL, sig TEXT NOT NULL, doc TEXT NOT NULL, start_line INTEGER NOT NULL, end_line INTEGER NOT NULL, chunk_key BLOB);
CREATE INDEX IF NOT EXISTS nodes_file ON nodes(file);
CREATE INDEX IF NOT EXISTS nodes_name ON nodes(name);
CREATE INDEX IF NOT EXISTS nodes_chunk ON nodes(chunk_key);
CREATE TABLE IF NOT EXISTS edges(id INTEGER PRIMARY KEY, file TEXT NOT NULL, kind TEXT NOT NULL, src TEXT NOT NULL, dst TEXT NOT NULL, qual TEXT NOT NULL, line INTEGER NOT NULL);
CREATE INDEX IF NOT EXISTS edges_file ON edges(file, src);
CREATE INDEX IF NOT EXISTS edges_dst ON edges(dst);
CREATE VIRTUAL TABLE IF NOT EXISTS chunks_fts USING fts5(text, path, content='chunks', content_rowid='id');
CREATE TRIGGER IF NOT EXISTS chunks_ai AFTER INSERT ON chunks BEGIN
  INSERT INTO chunks_fts(rowid, text, path) VALUES (new.id, new.text, new.path);
END;
CREATE TRIGGER IF NOT EXISTS chunks_ad AFTER DELETE ON chunks BEGIN
  INSERT INTO chunks_fts(chunks_fts, rowid, text, path) VALUES ('delete', old.id, old.text, old.path);
END;
";

/// Bumped whenever the graph tables change shape or meaning; part of the pinned store identity.
pub const GRAPH_VERSION: u32 = 1;
const BUILD_ATTEMPTS: usize = 3;
const WAL_ATTEMPTS: u32 = 200;
const WAL_RETRY_PAUSE: Duration = Duration::from_millis(5);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreConfig {
    pub dir: PathBuf,
    pub model_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileRow {
    pub mtime_ns: i64,
    pub size: i64,
    pub hash: ChunkKey,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkRow {
    pub key: ChunkKey,
    pub path: String,
    pub start_line: u32,
    pub end_line: u32,
    pub text: String,
}

pub struct Store {
    conn: Connection,
    dir: PathBuf,
}

impl Store {
    /// Opens (creating if needed) the shard at `config.dir`; a model change wipes derived rows.
    #[proxima::instrument]
    pub fn open(config: &StoreConfig) -> Result<Self, IndexError> {
        std::fs::create_dir_all(&config.dir)?;
        let conn = Connection::open(config.dir.join("index.db"))?;
        conn.execute_batch("PRAGMA busy_timeout=10000; PRAGMA synchronous=NORMAL;")?;
        enter_wal(&conn)?;
        conn.execute_batch(SCHEMA)?;
        let store = Self {
            conn,
            dir: config.dir.clone(),
        };
        store.pin_model(&config.model_id)?;
        Ok(store)
    }

    fn pin_model(&self, model_id: &str) -> rusqlite::Result<()> {
        let pinned = format!("{model_id}+graph{GRAPH_VERSION}");
        let current: Option<String> = self
            .conn
            .query_row("SELECT v FROM meta WHERE k='model'", [], |row| row.get(0))
            .optional()?;
        if current.as_deref() == Some(pinned.as_str()) {
            return Ok(());
        }
        self.conn
            .execute_batch(
            "DELETE FROM vectors; DELETE FROM chunks; DELETE FROM files; DELETE FROM nodes; DELETE FROM edges;",
        )?;
        bump_generation(&self.conn)?;
        self.conn.execute(
            "INSERT OR REPLACE INTO meta(k, v) VALUES ('model', ?1)",
            [pinned],
        )?;
        Ok(())
    }

    /// Pins one WAL snapshot for every read until the guard drops, so a search's scan, text match and chunk fetch
    /// agree even while a re-index replaces rows; without it each statement saw a different database.
    pub fn read_snapshot(&self) -> rusqlite::Result<Transaction<'_>> {
        self.conn.unchecked_transaction()
    }

    pub fn set_root(&self, root: &Path) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO meta(k, v) VALUES ('root', ?1)",
            [root.to_string_lossy().as_ref()],
        )?;
        Ok(())
    }

    pub fn file_row(&self, path: &str) -> rusqlite::Result<Option<FileRow>> {
        self.conn
            .query_row(
                "SELECT mtime_ns, size, hash FROM files WHERE path=?1",
                [path],
                |row| {
                    let blob: Vec<u8> = row.get(2)?;
                    Ok(FileRow {
                        mtime_ns: row.get(0)?,
                        size: row.get(1)?,
                        hash: blob.try_into().unwrap_or([0; 32]),
                    })
                },
            )
            .optional()
    }

    pub fn touch_file(&self, path: &str, mtime_ns: i64) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE files SET mtime_ns=?2 WHERE path=?1",
            params![path, mtime_ns],
        )?;
        Ok(())
    }

    pub fn existing_keys(&self, keys: &[ChunkKey]) -> rusqlite::Result<HashSet<ChunkKey>> {
        let mut statement = self
            .conn
            .prepare_cached("SELECT 1 FROM vectors WHERE key=?1")?;
        let mut found = HashSet::new();
        for key in keys {
            if statement.exists([key.as_slice()])? {
                found.insert(*key);
            }
        }
        Ok(found)
    }

    pub fn replace_file(
        &mut self,
        path: &str,
        row: &FileRow,
        chunks: &[RawChunk],
        vectors: &[(ChunkKey, Vec<f32>)],
        graph: &FileGraph,
    ) -> rusqlite::Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        for (key, vector) in vectors {
            tx.execute(
                "INSERT OR IGNORE INTO vectors(key, vec) VALUES (?1, ?2)",
                params![key.as_slice(), encode_vector(vector)],
            )?;
        }
        tx.execute("DELETE FROM chunks WHERE path=?1", [path])?;
        for chunk in chunks {
            tx.execute(
                "INSERT INTO chunks(path, start_line, end_line, key, text) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![path, chunk.start_line, chunk.end_line, chunk.key.as_slice(), chunk.text],
            )?;
        }
        replace_graph(&tx, path, graph)?;
        tx.execute(
            "INSERT OR REPLACE INTO files(path, mtime_ns, size, hash) VALUES (?1, ?2, ?3, ?4)",
            params![path, row.mtime_ns, row.size, row.hash.as_slice()],
        )?;
        bump_generation(&tx)?;
        tx.commit()
    }

    pub fn remove_file(&self, path: &str) -> rusqlite::Result<usize> {
        let tx = self.conn.unchecked_transaction()?;
        let removed = tx.execute("DELETE FROM chunks WHERE path=?1", [path])?;
        tx.execute("DELETE FROM files WHERE path=?1", [path])?;
        tx.execute("DELETE FROM nodes WHERE file=?1", [path])?;
        tx.execute("DELETE FROM edges WHERE file=?1", [path])?;
        bump_generation(&tx)?;
        tx.commit()?;
        Ok(removed)
    }

    pub fn file_paths(&self) -> rusqlite::Result<Vec<String>> {
        let mut statement = self.conn.prepare("SELECT path FROM files")?;
        statement.query_map([], |row| row.get(0))?.collect()
    }

    pub fn gc_vectors(&self) -> rusqlite::Result<usize> {
        self.conn.execute(
            "DELETE FROM vectors WHERE key NOT IN (SELECT key FROM chunks)",
            [],
        )
    }

    pub fn chunk_count(&self) -> rusqlite::Result<u64> {
        self.count("chunks")
    }

    pub fn node_count(&self) -> rusqlite::Result<u64> {
        self.count("nodes")
    }

    pub fn edge_count(&self) -> rusqlite::Result<u64> {
        self.count("edges")
    }

    /// Graph rows owned by one file: (nodes, edges).
    pub fn file_graph_counts(&self, path: &str) -> rusqlite::Result<(u64, u64)> {
        let count = |table: &str| -> rusqlite::Result<u64> {
            let total: i64 = self.conn.query_row(
                &format!("SELECT COUNT(*) FROM {table} WHERE file=?1"),
                [path],
                |row| row.get(0),
            )?;
            Ok(u64::try_from(total).unwrap_or(0))
        };
        Ok((count("nodes")?, count("edges")?))
    }

    /// Row ids of one file's nodes; unchanged ids after a re-index prove the rows were not rewritten.
    pub fn node_ids(&self, path: &str) -> rusqlite::Result<Vec<i64>> {
        let mut statement = self
            .conn
            .prepare("SELECT id FROM nodes WHERE file=?1 ORDER BY id")?;
        statement.query_map([path], |row| row.get(0))?.collect()
    }

    pub fn vector_count(&self) -> rusqlite::Result<u64> {
        self.count("vectors")
    }

    fn count(&self, table: &str) -> rusqlite::Result<u64> {
        let total: i64 =
            self.conn
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                    row.get(0)
                })?;
        Ok(u64::try_from(total).unwrap_or(0))
    }

    /// Write counter bumped by every transaction that changes chunks or vectors; read inside the caller's snapshot.
    pub fn generation(&self) -> rusqlite::Result<i64> {
        read_generation(&self.conn)
    }

    /// Exact cosine scan (vectors are stored unit-length) over chunks under `prefix`. Scores the in-memory
    /// [`VectorSnapshot`] for the current generation, building it once; a shard over the cache limit scans SQLite.
    #[proxima::instrument]
    pub fn vector_search(
        &self,
        query: &[f32],
        prefix: &str,
        limit: usize,
    ) -> rusqlite::Result<Vec<i64>> {
        let _own_snapshot = self
            .conn
            .is_autocommit()
            .then(|| self.read_snapshot())
            .transpose()?;
        let generation = self.generation()?;
        match self.snapshot_for(generation) {
            Some(snapshot) => Ok(snapshot.top(query, prefix, limit)),
            None => self.scan_sqlite(query, prefix, limit),
        }
    }

    /// A snapshot that may serve `generation`, or `None` to scan SQLite. A newer generation than the cache
    /// wakes the shard's one builder thread; the query waits on it unless bounded-stale serving applies.
    fn snapshot_for(&self, generation: i64) -> Option<Arc<VectorSnapshot>> {
        for _attempt in 0..BUILD_ATTEMPTS {
            let found = vector_cache::servable(&self.dir, generation);
            if found.as_ref().is_some_and(|snap| snap.generation() >= generation) {
                return found;
            }
            let build = ensure_build(&self.dir);
            if found.is_some() {
                return found;
            }
            vector_cache::wait_on(&build);
            if build.declined() {
                return None;
            }
        }
        None
    }

    /// The uncached scan: reads every blob out of SQLite per call.
    pub fn scan_sqlite(
        &self,
        query: &[f32],
        prefix: &str,
        limit: usize,
    ) -> rusqlite::Result<Vec<i64>> {
        let mut statement = self.conn.prepare_cached(
            "SELECT c.id, v.vec FROM chunks c JOIN vectors v ON v.key = c.key WHERE substr(c.path, 1, ?1) = ?2",
        )?;
        let width = i64::try_from(prefix.chars().count()).unwrap_or(0);
        let mut scored: Vec<(i64, f32)> = statement
            .query_map(params![width, prefix], |row| {
                let blob: Vec<u8> = row.get(1)?;
                Ok((row.get(0)?, dot(query, &blob)))
            })?
            .collect::<rusqlite::Result<_>>()?;
        debug!("vector scan", scanned = scored.len());
        scored.sort_unstable_by(|left, right| right.1.total_cmp(&left.1));
        scored.truncate(limit);
        Ok(scored.into_iter().map(|(id, _)| id).collect())
    }

    /// BM25 over chunk text and path; ids best-first.
    #[proxima::instrument]
    pub fn text_search(
        &self,
        terms: &[String],
        prefix: &str,
        limit: usize,
    ) -> rusqlite::Result<Vec<i64>> {
        if terms.is_empty() {
            return Ok(Vec::new());
        }
        let expression = terms
            .iter()
            .map(|term| format!("\"{term}\""))
            .collect::<Vec<_>>()
            .join(" OR ");
        let mut statement = self.conn.prepare_cached(
            "SELECT chunks_fts.rowid FROM chunks_fts JOIN chunks c ON c.id = chunks_fts.rowid \
             WHERE chunks_fts MATCH ?1 AND substr(c.path, 1, ?2) = ?3 ORDER BY bm25(chunks_fts) LIMIT ?4",
        )?;
        let width = i64::try_from(prefix.chars().count()).unwrap_or(0);
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        statement
            .query_map(params![expression, width, prefix, limit], |row| row.get(0))?
            .collect()
    }

    pub fn chunk(&self, id: i64) -> rusqlite::Result<Option<ChunkRow>> {
        self.conn
            .query_row(
                "SELECT path, start_line, end_line, text, key FROM chunks WHERE id=?1",
                [id],
                |row| {
                    let key: Vec<u8> = row.get(4)?;
                    Ok(ChunkRow {
                        key: key.try_into().unwrap_or([0; 32]),
                        path: row.get(0)?,
                        start_line: row.get(1)?,
                        end_line: row.get(2)?,
                        text: row.get(3)?,
                    })
                },
            )
            .optional()
    }
}

/// Switching a fresh database to WAL can fail with `busy` while another connection does the same, and sqlite does not
/// run the busy handler for that case, so the switch is retried until the other connection finishes.
fn enter_wal(conn: &Connection) -> rusqlite::Result<()> {
    for _ in 0..WAL_ATTEMPTS {
        match conn.execute_batch("PRAGMA journal_mode=WAL;") {
            Err(rusqlite::Error::SqliteFailure(failure, _))
                if failure.code == rusqlite::ErrorCode::DatabaseBusy =>
            {
                sleep(WAL_RETRY_PAUSE);
            }
            other => return other,
        }
    }
    conn.execute_batch("PRAGMA journal_mode=WAL;")
}

fn replace_graph(tx: &Transaction<'_>, path: &str, graph: &FileGraph) -> rusqlite::Result<()> {
    tx.execute("DELETE FROM nodes WHERE file=?1", [path])?;
    tx.execute("DELETE FROM edges WHERE file=?1", [path])?;
    for node in &graph.nodes {
        insert_node(tx, path, node)?;
    }
    for edge in &graph.edges {
        insert_edge(tx, path, edge)?;
    }
    Ok(())
}

fn insert_node(tx: &Transaction<'_>, path: &str, node: &GraphNode) -> rusqlite::Result<()> {
    tx.execute(
        "INSERT INTO nodes(file, kind, name, qpath, sig, doc, start_line, end_line, chunk_key) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            path,
            node.kind.as_str(),
            node.name,
            node.qpath,
            node.signature,
            node.doc,
            node.start_line,
            node.end_line,
            node.chunk_key.as_ref().map(<[u8; 32]>::as_slice)
        ],
    )?;
    Ok(())
}

fn insert_edge(tx: &Transaction<'_>, path: &str, edge: &GraphEdge) -> rusqlite::Result<()> {
    tx.execute(
        "INSERT INTO edges(file, kind, src, dst, qual, line) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            path,
            edge.kind.as_str(),
            edge.src,
            edge.dst,
            edge.qual,
            edge.line
        ],
    )?;
    Ok(())
}

fn read_generation(conn: &Connection) -> rusqlite::Result<i64> {
    let text: Option<String> = conn
        .query_row("SELECT v FROM meta WHERE k='gen'", [], |row| row.get(0))
        .optional()?;
    Ok(text.and_then(|value| value.parse().ok()).unwrap_or(0))
}

/// Joins the shard's in-flight build or starts one on a dedicated thread, so no query worker ever builds.
fn ensure_build(dir: &Path) -> Arc<vector_cache::Build> {
    let (build, creator) = vector_cache::single_flight(dir);
    if creator {
        let (owned, running) = (dir.to_path_buf(), Arc::clone(&build));
        let spawned = std::thread::Builder::new()
            .name("ss-vector-build".to_owned())
            .spawn(move || {
                let declined = !matches!(load_snapshot(&owned), Ok(Some(_)));
                vector_cache::retire(&owned);
                running.finish(declined);
            });
        if spawned.is_err() {
            vector_cache::retire(dir);
            build.finish(true);
        }
    }
    build
}

/// Loads every vector at one read snapshot and publishes it; `None` when over the limit or dimensions disagree.
fn load_snapshot(dir: &Path) -> rusqlite::Result<Option<Arc<VectorSnapshot>>> {
    let _rebuild = vector_cache::RebuildGuard::begin();
    let conn = Connection::open(dir.join("index.db"))?;
    conn.execute_batch("PRAGMA busy_timeout=10000;")?;
    let read = conn.unchecked_transaction()?;
    let generation = read_generation(&read)?;
    let (rows, bytes): (i64, i64) = read.query_row(
        "SELECT COUNT(*), COALESCE(SUM(length(v.vec)), 0) FROM chunks c JOIN vectors v ON v.key = c.key",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let rows = usize::try_from(rows).unwrap_or(0);
    let bytes = usize::try_from(bytes).unwrap_or(0);
    if rows == 0 || bytes > vector_cache::limit_bytes() {
        return Ok(None);
    }
    let mut snapshot = VectorSnapshot::new(generation, bytes / rows / 4);
    let mut statement =
        read.prepare("SELECT c.id, c.path, v.vec FROM chunks c JOIN vectors v ON v.key = c.key")?;
    let mut cursor = statement.query([])?;
    while let Some(row) = cursor.next()? {
        let (id, path): (i64, String) = (row.get(0)?, row.get(1)?);
        if !snapshot.push(id, path, row.get_ref(2)?.as_blob()?) {
            return Ok(None);
        }
    }
    let snapshot = Arc::new(snapshot);
    vector_cache::publish(dir, &snapshot);
    Ok(Some(snapshot))
}

fn bump_generation(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO meta(k, v) VALUES ('gen', '1') ON CONFLICT(k) DO UPDATE SET v = CAST(v AS INTEGER) + 1",
        [],
    )?;
    Ok(())
}

fn encode_vector(vector: &[f32]) -> Vec<u8> {
    vector
        .iter()
        .flat_map(|value| value.to_le_bytes())
        .collect()
}

fn dot(query: &[f32], blob: &[u8]) -> f32 {
    blob.as_chunks::<4>()
        .0
        .iter()
        .zip(query)
        .map(|(bytes, weight)| f32::from_le_bytes(*bytes) * weight)
        .sum()
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Barrier};
    use std::thread;

    use super::*;
    use crate::chunk::chunk_key;
    use crate::embed::{EmbedProfile, EmbedderKind, Provider};

    fn store_with_one_vector(dir: &Path, model_id: &str) -> Store {
        let mut store = Store::open(&StoreConfig {
            dir: dir.to_path_buf(),
            model_id: model_id.to_owned(),
        })
        .expect("open store");
        let text = "pub fn list_files(root: &Path) -> Vec<PathBuf> {}";
        let key = chunk_key(text);
        let chunk = RawChunk {
            start_line: 1,
            end_line: 1,
            text: text.to_owned(),
            key,
        };
        let row = FileRow {
            mtime_ns: 1,
            size: 1,
            hash: key,
        };
        store
            .replace_file(
                "src/walk.rs",
                &row,
                &[chunk],
                &[(key, vec![1.0, 0.0])],
                &FileGraph::default(),
            )
            .expect("replace file");
        store
    }

    fn reopen(dir: &Path, model_id: &str) -> Store {
        Store::open(&StoreConfig {
            dir: dir.to_path_buf(),
            model_id: model_id.to_owned(),
        })
        .expect("reopen store")
    }

    #[test]
    fn reopening_under_the_same_identity_keeps_the_vectors() {
        let dir = tempfile::tempdir().expect("tempdir");
        let id = EmbedProfile::default().store_id(EmbedderKind::FastEmbed);
        drop(store_with_one_vector(dir.path(), &id));

        let store = reopen(dir.path(), &id);

        assert_eq!(store.vector_count().expect("count"), 1);
    }

    #[test]
    fn a_provider_or_max_length_change_wipes_vectors_so_they_are_never_mixed() {
        let base = EmbedProfile::default();
        let coreml = EmbedProfile {
            provider: Provider::CoreMlGpu,
            ..base
        };
        let shorter = EmbedProfile {
            max_length: 128,
            ..base
        };
        for changed in [coreml, shorter] {
            let dir = tempfile::tempdir().expect("tempdir");
            drop(store_with_one_vector(
                dir.path(),
                &base.store_id(EmbedderKind::FastEmbed),
            ));

            let store = reopen(dir.path(), &changed.store_id(EmbedderKind::FastEmbed));

            assert_eq!(
                (
                    store.vector_count().expect("vectors"),
                    store.chunk_count().expect("chunks")
                ),
                (0, 0),
                "{changed:?} reused vectors built under the default profile"
            );
        }
    }

    #[test]
    fn an_index_pinned_before_the_graph_existed_is_rebuilt_not_mixed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let id = EmbedProfile::default().store_id(EmbedderKind::FastEmbed);
        let mut store = store_with_one_vector(dir.path(), &id);
        let node = crate::graph::GraphNode {
            kind: crate::graph::NodeKind::File,
            name: "walk.rs".to_owned(),
            qpath: "src/walk.rs".to_owned(),
            signature: String::new(),
            doc: String::new(),
            start_line: 1,
            end_line: 1,
            chunk_key: None,
        };
        let graph = FileGraph {
            nodes: vec![node],
            edges: Vec::new(),
        };
        let chunk = RawChunk {
            start_line: 1,
            end_line: 1,
            text: "x".to_owned(),
            key: chunk_key("x"),
        };
        let row = FileRow {
            mtime_ns: 1,
            size: 1,
            hash: chunk.key,
        };
        store
            .replace_file("src/walk.rs", &row, &[chunk], &[], &graph)
            .expect("replace");
        assert_eq!(store.node_count().expect("nodes"), 1);
        drop(store);
        let conn = rusqlite::Connection::open(dir.path().join("index.db")).expect("raw open");
        conn.execute("UPDATE meta SET v=?1 WHERE k='model'", [&id])
            .expect("pin the pre-graph identity");
        drop(conn);

        let reopened = reopen(dir.path(), &id);

        assert_eq!(
            (
                reopened.node_count().expect("nodes"),
                reopened.chunk_count().expect("chunks"),
                reopened.vector_count().expect("vectors")
            ),
            (0, 0, 0)
        );
    }

    #[test]
    fn many_connections_opening_a_fresh_shard_at_once_all_succeed() {
        for round in 0..20 {
            let dir = tempfile::tempdir().expect("tempdir");
            let gate = Arc::new(Barrier::new(16));
            let opens: Vec<_> = (0..16)
                .map(|_| {
                    let (gate, dir) = (Arc::clone(&gate), dir.path().to_path_buf());
                    thread::spawn(move || {
                        gate.wait();
                        Store::open(&StoreConfig {
                            dir,
                            model_id: "feature-hash-256".to_owned(),
                        })
                        .map(drop)
                    })
                })
                .collect();

            for open in opens {
                let outcome = open.join().expect("open thread");
                assert!(outcome.is_ok(), "round {round}: {outcome:?}");
            }
        }
    }

    #[test]
    fn many_writers_replacing_different_files_at_once_all_commit() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = StoreConfig {
            dir: dir.path().to_path_buf(),
            model_id: "feature-hash-256".to_owned(),
        };
        drop(Store::open(&config).expect("create shard"));
        let gate = Arc::new(Barrier::new(16));
        let writers: Vec<_> = (0..16)
            .map(|worker| {
                let (gate, config) = (Arc::clone(&gate), config.clone());
                thread::spawn(move || {
                    let mut store = Store::open(&config).expect("open");
                    gate.wait();
                    (0..20).try_for_each(|round| {
                        let text = format!("pub fn worker_{worker}_round_{round}() {{}}");
                        let key = chunk_key(&text);
                        let chunk = RawChunk {
                            start_line: 1,
                            end_line: 1,
                            text,
                            key,
                        };
                        let row = FileRow {
                            mtime_ns: 1,
                            size: 1,
                            hash: key,
                        };
                        store.replace_file(
                            &format!("src/w{worker}.rs"),
                            &row,
                            &[chunk],
                            &[(key, vec![1.0, 0.0])],
                            &FileGraph::default(),
                        )
                    })
                })
            })
            .collect();

        for writer in writers {
            let outcome = writer.join().expect("writer thread");
            assert!(outcome.is_ok(), "{outcome:?}");
        }
    }

    #[test]
    fn store_identity_names_model_provider_and_max_length() {
        let profile = EmbedProfile {
            provider: Provider::CoreMlAll,
            max_length: 128,
        };

        assert_eq!(
            profile.store_id(EmbedderKind::FastEmbed),
            "bge-small-en-v1.5+coreml_all+len128"
        );
        assert_eq!(profile.store_id(EmbedderKind::Hash), "feature-hash-256");
    }
}

#[cfg(test)]
mod single_flight_tests {
    use std::sync::{Arc, Barrier};
    use std::thread;
    use std::time::Duration;

    use super::vector_cache::{self, IndexRun};
    use super::*;
    use crate::chunk::chunk_key;

    fn open(dir: &Path) -> Store {
        Store::open(&StoreConfig {
            dir: dir.to_path_buf(),
            model_id: "feature-hash-256".to_owned(),
        })
        .expect("open store")
    }

    fn write_file(store: &mut Store, number: usize) {
        let text = format!("pub fn reindexed_{number}() {{}}");
        let key = chunk_key(&text);
        let chunk = RawChunk {
            start_line: 1,
            end_line: 1,
            text,
            key,
        };
        let row = FileRow {
            mtime_ns: 1,
            size: 1,
            hash: key,
        };
        store
            .replace_file(
                &format!("src/f{number}.rs"),
                &row,
                &[chunk],
                &[(key, vec![1.0, 0.0])],
                &FileGraph::default(),
            )
            .expect("replace file");
    }

    #[test]
    fn quiescent_write_then_query_sees_the_write() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut store = open(dir.path());
        write_file(&mut store, 0);
        assert_eq!(store.vector_search(&[1.0, 0.0], "", 10).expect("first").len(), 1);

        write_file(&mut store, 1);
        let hits = store.vector_search(&[1.0, 0.0], "", 10).expect("second");

        assert_eq!(hits.len(), 2, "a write with no index run active must be visible");
    }

    #[test]
    fn a_snapshot_older_than_the_stale_bound_is_not_served() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut store = open(dir.path());
        write_file(&mut store, 0);
        store.vector_search(&[1.0, 0.0], "", 10).expect("publish");
        let _run = IndexRun::begin();
        vector_cache::set_stale_ms(30);
        write_file(&mut store, 1);
        thread::sleep(Duration::from_millis(80));

        let hits = store.vector_search(&[1.0, 0.0], "", 10).expect("search");

        vector_cache::set_stale_ms(2000);
        assert_eq!(hits.len(), 2, "an expired snapshot must wait for the rebuild");
    }

    #[test]
    fn concurrent_searches_during_a_reindex_start_few_rebuilds() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut writer = open(dir.path());
        write_file(&mut writer, 0);
        writer.vector_search(&[1.0, 0.0], "", 10).expect("warm");
        let started_before = vector_cache::counters().started;
        let _run = IndexRun::begin();
        let gate = Arc::new(Barrier::new(33));
        let searchers: Vec<_> = (0..32)
            .map(|_| {
                let (gate, path) = (Arc::clone(&gate), dir.path().to_path_buf());
                thread::spawn(move || {
                    let store = open(&path);
                    gate.wait();
                    (0..5)
                        .map(|_| store.vector_search(&[1.0, 0.0], "", 10).expect("search").len())
                        .min()
                })
            })
            .collect();
        gate.wait();
        (1..=200).for_each(|number| write_file(&mut writer, number));

        for searcher in searchers {
            assert!(searcher.join().expect("thread").is_some_and(|least| least > 0));
        }
        let started = vector_cache::counters().started - started_before;
        assert!(started <= 4, "rebuilds_started was {started}");
    }

    #[test]
    fn a_query_right_after_an_index_run_finds_a_prebuilt_snapshot() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut store = open(dir.path());
        write_file(&mut store, 0);
        store.vector_search(&[1.0, 0.0], "", 10).expect("publish");
        let run = IndexRun::begin();
        (1..=20).for_each(|number| write_file(&mut store, number));
        drop(run);
        while vector_cache::cached_generation(dir.path()) != Some(store.generation().expect("gen")) {
            thread::sleep(Duration::from_millis(5));
        }
        let before = vector_cache::counters();

        let hits = store.vector_search(&[1.0, 0.0], "", 50).expect("search");

        let after = vector_cache::counters();
        assert_eq!(hits.len(), 21);
        assert_eq!((after.started, after.waited_on_build), (before.started, before.waited_on_build));
    }
}

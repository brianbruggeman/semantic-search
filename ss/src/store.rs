use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::thread::sleep;
use std::time::Duration;

use proxima::telemetry::debug;
use rusqlite::{
    Connection, ErrorCode, OpenFlags, OptionalExtension, Transaction, TransactionBehavior, params,
};

use crate::chunk::{ChunkKey, RawChunk};
use crate::error::IndexError;
use crate::graph::{FileGraph, GraphEdge, GraphNode};
use vector_cache::Segment;

mod graph_read;
mod scip;
pub mod vector_cache;

pub use graph_read::{EdgeRow, NodeRow};

pub const SCHEMA: &str = "
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
CREATE TABLE IF NOT EXISTS scip_symbols(id INTEGER PRIMARY KEY, symbol TEXT NOT NULL UNIQUE, display_name TEXT NOT NULL, kind TEXT NOT NULL, signature_doc TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS scip_occurrences(id INTEGER PRIMARY KEY, file TEXT NOT NULL, start_line INTEGER NOT NULL, start_col INTEGER NOT NULL, end_line INTEGER NOT NULL, end_col INTEGER NOT NULL, symbol_id INTEGER NOT NULL, role INTEGER NOT NULL, enclosing_start INTEGER, enclosing_end INTEGER);
CREATE INDEX IF NOT EXISTS scip_occ_file ON scip_occurrences(file, start_line);
CREATE INDEX IF NOT EXISTS scip_occ_symbol ON scip_occurrences(symbol_id);
CREATE TABLE IF NOT EXISTS scip_files(file TEXT PRIMARY KEY, content_hash BLOB NOT NULL);
CREATE VIRTUAL TABLE IF NOT EXISTS chunks_fts USING fts5(text, path, content='chunks', content_rowid='id');
CREATE TRIGGER IF NOT EXISTS chunks_ai AFTER INSERT ON chunks BEGIN
  INSERT INTO chunks_fts(rowid, text, path) VALUES (new.id, new.text, new.path);
END;
CREATE TRIGGER IF NOT EXISTS chunks_ad AFTER DELETE ON chunks BEGIN
  INSERT INTO chunks_fts(chunks_fts, rowid, text, path) VALUES ('delete', old.id, old.text, old.path);
END;
";

/// Bumped whenever the graph tables change shape or meaning; part of the pinned store identity.
pub const GRAPH_VERSION: u32 = 2;
const BUSY_TIMEOUT: Duration = Duration::from_millis(10_000);
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

    /// Opens an existing shard for reading only: no WAL switch, schema batch or model pin, so a query does no
    /// write-side setup. One `meta` read verifies the shard is indexed under `config.model_id`.
    #[proxima::instrument]
    pub fn open_reader(config: &StoreConfig) -> Result<Self, IndexError> {
        let missing = || IndexError::NotIndexed(config.dir.display().to_string());
        let flags = OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_PRIVATE_CACHE;
        let conn =
            Connection::open_with_flags(config.dir.join("index.db"), flags).map_err(|error| {
                if is_code(&error, ErrorCode::CannotOpen) {
                    missing()
                } else {
                    error.into()
                }
            })?;
        conn.busy_timeout(BUSY_TIMEOUT)?;
        let pinned = pinned_identity(&config.model_id);
        let current: Option<String> = conn
            .query_row("SELECT v FROM meta WHERE k='model'", [], |row| row.get(0))
            .optional()
            .or_else(|error| {
                if is_no_table(&error) {
                    Ok(None)
                } else {
                    Err(error)
                }
            })?;
        match current {
            None => Err(missing()),
            Some(found) if found != pinned => Err(IndexError::IdentityMismatch {
                found,
                expected: pinned,
            }),
            Some(_) => Ok(Self {
                conn,
                dir: config.dir.clone(),
            }),
        }
    }

    fn pin_model(&self, model_id: &str) -> rusqlite::Result<()> {
        let pinned = pinned_identity(model_id);
        let current: Option<String> = self
            .conn
            .query_row("SELECT v FROM meta WHERE k='model'", [], |row| row.get(0))
            .optional()?;
        if current.as_deref() == Some(pinned.as_str()) {
            return Ok(());
        }
        self.conn
            .execute_batch(
            "DELETE FROM vectors; DELETE FROM chunks; DELETE FROM files; DELETE FROM nodes; DELETE FROM edges; DELETE FROM scip_symbols; DELETE FROM scip_occurrences; DELETE FROM scip_files;",
        )?;
        vector_cache::evict(&self.dir);
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
        let order = vector_cache::write_order(&self.dir);
        let _ordered = order.lock();
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        for (key, vector) in vectors {
            tx.execute(
                "INSERT OR IGNORE INTO vectors(key, vec) VALUES (?1, ?2)",
                params![key.as_slice(), encode_vector(vector)],
            )?;
        }
        let replaced = chunk_ids(&tx, path)?;
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
        let active = vector_cache::active(&self.dir);
        let added = if active {
            file_segment(&tx, path)?
        } else {
            None
        };
        tx.commit()?;
        if active {
            self.publish(&replaced, added);
        }
        Ok(())
    }

    fn publish(&self, replaced: &[i64], added: Option<Segment>) {
        match added {
            Some(segment) => vector_cache::publish(&self.dir, replaced, segment),
            None => vector_cache::evict(&self.dir),
        }
    }

    pub fn remove_file(&self, path: &str) -> rusqlite::Result<usize> {
        let order = vector_cache::write_order(&self.dir);
        let _ordered = order.lock();
        let tx = self.conn.unchecked_transaction()?;
        let replaced = chunk_ids(&tx, path)?;
        let removed = tx.execute("DELETE FROM chunks WHERE path=?1", [path])?;
        tx.execute("DELETE FROM files WHERE path=?1", [path])?;
        tx.execute("DELETE FROM nodes WHERE file=?1", [path])?;
        tx.execute("DELETE FROM edges WHERE file=?1", [path])?;
        tx.execute("DELETE FROM scip_occurrences WHERE file=?1", [path])?;
        tx.execute("DELETE FROM scip_files WHERE file=?1", [path])?;
        tx.commit()?;
        if vector_cache::active(&self.dir) {
            self.publish(&replaced, Some(Segment::new(0)));
        }
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

    /// Builds the shard's segment list from SQLite in one pass (call off the query path). Until it finishes,
    /// searches scan SQLite; writes during the load are replayed on top of it.
    pub fn load_segments(&self) -> rusqlite::Result<()> {
        if !vector_cache::begin_load(&self.dir) {
            return Ok(());
        }
        let (segment, outcome) = match read_segment(&self.conn) {
            Ok(segment) => (segment, Ok(())),
            Err(error) => (None, Err(error)),
        };
        vector_cache::finish_load(&self.dir, segment);
        outcome
    }

    /// Exact cosine scan (vectors are stored unit-length) over chunks under `prefix`. Scores the shard's captured
    /// segment list when loaded, else scans SQLite. One search uses one list pointer.
    #[proxima::instrument]
    pub fn vector_search(
        &self,
        query: &[f32],
        prefix: &str,
        limit: usize,
    ) -> rusqlite::Result<Vec<i64>> {
        if let Some(list) = vector_cache::current(&self.dir) {
            return Ok(list.top(query, prefix, limit));
        }
        vector_cache::count_scan();
        self.scan_sqlite(query, prefix, limit)
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

fn pinned_identity(model_id: &str) -> String {
    format!("{model_id}+graph{GRAPH_VERSION}")
}

fn is_code(error: &rusqlite::Error, code: ErrorCode) -> bool {
    matches!(error, rusqlite::Error::SqliteFailure(failure, _) if failure.code == code)
}

fn is_no_table(error: &rusqlite::Error) -> bool {
    matches!(error, rusqlite::Error::SqliteFailure(failure, Some(message))
        if failure.code == ErrorCode::Unknown && message.starts_with("no such table"))
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

/// Loads every live vector in one pass at one read snapshot; `None` when over the limit or widths disagree.
fn read_segment(conn: &Connection) -> rusqlite::Result<Option<Segment>> {
    let read = conn.unchecked_transaction()?;
    let (rows, bytes): (i64, i64) = read.query_row(
        "SELECT COUNT(*), COALESCE(SUM(length(v.vec)), 0) FROM chunks c JOIN vectors v ON v.key = c.key",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let rows = usize::try_from(rows).unwrap_or(0);
    let bytes = usize::try_from(bytes).unwrap_or(0);
    if rows == 0 {
        return Ok(Some(Segment::new(0)));
    }
    if bytes > vector_cache::limit_bytes() {
        return Ok(None);
    }
    let mut statement = read.prepare(
        "SELECT c.id, c.path, v.vec FROM chunks c JOIN vectors v ON v.key = c.key ORDER BY c.id",
    )?;
    fill_segment(&mut statement, params![], bytes / rows / 4)
}

fn fill_segment(
    statement: &mut rusqlite::Statement<'_>,
    args: impl rusqlite::Params,
    dim: usize,
) -> rusqlite::Result<Option<Segment>> {
    let mut segment = Segment::new(dim);
    let mut cursor = statement.query(args)?;
    while let Some(row) = cursor.next()? {
        let (id, path): (i64, String) = (row.get(0)?, row.get(1)?);
        if !segment.push(id, path, row.get_ref(2)?.as_blob()?) {
            return Ok(None);
        }
    }
    Ok(Some(segment))
}

fn chunk_ids(conn: &Connection, path: &str) -> rusqlite::Result<Vec<i64>> {
    let mut statement = conn.prepare_cached("SELECT id FROM chunks WHERE path=?1")?;
    statement.query_map([path], |row| row.get(0))?.collect()
}

fn file_segment(conn: &Connection, path: &str) -> rusqlite::Result<Option<Segment>> {
    let mut statement = conn.prepare_cached(
        "SELECT c.id, c.path, v.vec FROM chunks c JOIN vectors v ON v.key = c.key WHERE c.path=?1 ORDER BY c.id",
    )?;
    let dim = conn
        .query_row("SELECT COALESCE(MAX(length(v.vec)), 0) FROM chunks c JOIN vectors v ON v.key = c.key WHERE c.path=?1", [path], |row| row.get::<_, i64>(0))?;
    fill_segment(
        &mut statement,
        [path],
        usize::try_from(dim).unwrap_or(0) / 4,
    )
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

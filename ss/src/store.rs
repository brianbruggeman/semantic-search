use std::collections::HashSet;
use std::path::{Path, PathBuf};

use proxima::telemetry::debug;
use rusqlite::{Connection, OptionalExtension, Transaction, params};

use crate::chunk::{ChunkKey, RawChunk};
use crate::error::IndexError;
use crate::graph::{FileGraph, GraphEdge, GraphNode};

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
    pub path: String,
    pub start_line: u32,
    pub end_line: u32,
    pub text: String,
}

pub struct Store {
    conn: Connection,
}

impl Store {
    /// Opens (creating if needed) the shard at `config.dir`; a model change wipes derived rows.
    #[proxima::instrument]
    pub fn open(config: &StoreConfig) -> Result<Self, IndexError> {
        std::fs::create_dir_all(&config.dir)?;
        let conn = Connection::open(config.dir.join("index.db"))?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA busy_timeout=10000;",
        )?;
        conn.execute_batch(SCHEMA)?;
        let store = Self { conn };
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
        let tx = self.conn.transaction()?;
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
        tx.commit()
    }

    pub fn remove_file(&self, path: &str) -> rusqlite::Result<usize> {
        let removed = self
            .conn
            .execute("DELETE FROM chunks WHERE path=?1", [path])?;
        self.conn
            .execute("DELETE FROM files WHERE path=?1", [path])?;
        self.conn
            .execute("DELETE FROM nodes WHERE file=?1", [path])?;
        self.conn
            .execute("DELETE FROM edges WHERE file=?1", [path])?;
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

    /// Exact cosine scan (vectors are stored unit-length) over chunks under `prefix`.
    #[proxima::instrument]
    pub fn vector_search(
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
                "SELECT path, start_line, end_line, text FROM chunks WHERE id=?1",
                [id],
                |row| {
                    Ok(ChunkRow {
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

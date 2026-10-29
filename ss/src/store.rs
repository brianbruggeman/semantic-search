use std::collections::HashSet;
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OptionalExtension, params};

use crate::chunk::{ChunkKey, RawChunk};
use crate::error::IndexError;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS meta(k TEXT PRIMARY KEY, v TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS files(path TEXT PRIMARY KEY, mtime_ns INTEGER NOT NULL, size INTEGER NOT NULL, hash BLOB NOT NULL);
CREATE TABLE IF NOT EXISTS vectors(key BLOB PRIMARY KEY, vec BLOB NOT NULL) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS chunks(id INTEGER PRIMARY KEY, path TEXT NOT NULL, start_line INTEGER NOT NULL, end_line INTEGER NOT NULL, key BLOB NOT NULL, text TEXT NOT NULL);
CREATE INDEX IF NOT EXISTS chunks_path ON chunks(path);
CREATE INDEX IF NOT EXISTS chunks_key ON chunks(key);
CREATE VIRTUAL TABLE IF NOT EXISTS chunks_fts USING fts5(text, path, content='chunks', content_rowid='id');
CREATE TRIGGER IF NOT EXISTS chunks_ai AFTER INSERT ON chunks BEGIN
  INSERT INTO chunks_fts(rowid, text, path) VALUES (new.id, new.text, new.path);
END;
CREATE TRIGGER IF NOT EXISTS chunks_ad AFTER DELETE ON chunks BEGIN
  INSERT INTO chunks_fts(chunks_fts, rowid, text, path) VALUES ('delete', old.id, old.text, old.path);
END;
";

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
        let current: Option<String> = self
            .conn
            .query_row("SELECT v FROM meta WHERE k='model'", [], |row| row.get(0))
            .optional()?;
        if current.as_deref() == Some(model_id) {
            return Ok(());
        }
        self.conn
            .execute_batch("DELETE FROM vectors; DELETE FROM chunks; DELETE FROM files;")?;
        self.conn.execute(
            "INSERT OR REPLACE INTO meta(k, v) VALUES ('model', ?1)",
            [model_id],
        )?;
        Ok(())
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
        scored.sort_unstable_by(|left, right| right.1.total_cmp(&left.1));
        scored.truncate(limit);
        Ok(scored.into_iter().map(|(id, _)| id).collect())
    }

    /// BM25 over chunk text and path; ids best-first.
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

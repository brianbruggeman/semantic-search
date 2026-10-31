//! Generation-keyed in-memory copy of a shard's vectors, so a query scores a contiguous matrix instead of
//! re-reading every blob out of SQLite. Readers clone an `Arc` and never wait on a rebuild; a rebuild publishes a
//! newer `Arc` and readers holding the old one finish on it, so one search sees exactly one generation.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock};

use proxima_primitives::sync::blocking::Mutex;

const DEFAULT_LIMIT_MB: usize = 512;

static LIMIT_MB: AtomicUsize = AtomicUsize::new(DEFAULT_LIMIT_MB);
// a blocking mutex outside async, held only for an Arc clone or insert, never across a scan or a rebuild
static SHARDS: LazyLock<Mutex<HashMap<PathBuf, Arc<VectorSnapshot>>>> =
    LazyLock::new(Mutex::default);

/// Sets the per-process cache ceiling in megabytes; a shard whose vectors exceed it keeps using the SQLite scan.
pub fn set_limit_mb(megabytes: usize) {
    LIMIT_MB.store(megabytes, Ordering::Relaxed);
}

pub(super) fn limit_bytes() -> usize {
    LIMIT_MB.load(Ordering::Relaxed).saturating_mul(1 << 20)
}

/// Every chunk's vector for one database generation, row-major, in the order SQLite returned them.
pub struct VectorSnapshot {
    generation: i64,
    dim: usize,
    ids: Vec<i64>,
    paths: Vec<String>,
    matrix: Vec<f32>,
}

impl VectorSnapshot {
    pub(super) fn new(generation: i64, dim: usize) -> Self {
        Self {
            generation,
            dim,
            ids: Vec::new(),
            paths: Vec::new(),
            matrix: Vec::new(),
        }
    }

    pub(super) fn push(&mut self, id: i64, path: String, blob: &[u8]) -> bool {
        if blob.len() != self.dim * 4 {
            return false;
        }
        self.ids.push(id);
        self.paths.push(path);
        self.matrix.extend(
            blob.as_chunks::<4>()
                .0
                .iter()
                .map(|bytes| f32::from_le_bytes(*bytes)),
        );
        true
    }

    #[must_use]
    pub fn generation(&self) -> i64 {
        self.generation
    }

    /// Ids of the `limit` best chunks under `prefix`, best first; same arithmetic order as the SQLite scan.
    #[must_use]
    pub fn top(&self, query: &[f32], prefix: &str, limit: usize) -> Vec<i64> {
        let mut scored: Vec<(i64, f32)> = self
            .ids
            .iter()
            .zip(&self.paths)
            .zip(self.matrix.chunks_exact(self.dim.max(1)))
            .filter(|((_, path), _)| path.starts_with(prefix))
            .map(|((id, _), row)| (*id, row.iter().zip(query).map(|(left, right)| left * right).sum()))
            .collect();
        scored.sort_unstable_by(|left, right| right.1.total_cmp(&left.1));
        scored.truncate(limit);
        scored.into_iter().map(|(id, _)| id).collect()
    }
}

/// The cached snapshot for `dir` if it is exactly `generation`.
pub(super) fn lookup(dir: &Path, generation: i64) -> Option<Arc<VectorSnapshot>> {
    SHARDS
        .lock()
        .get(dir)
        .filter(|snapshot| snapshot.generation == generation)
        .cloned()
}

/// Generation of the snapshot cached for `dir`, if any.
#[must_use]
pub fn cached_generation(dir: &Path) -> Option<i64> {
    SHARDS.lock().get(dir).map(|snapshot| snapshot.generation)
}

/// Drops the cached snapshot for `dir`; the next search rebuilds it.
pub fn evict(dir: &Path) {
    SHARDS.lock().remove(dir);
}

/// Publishes `snapshot` unless a newer generation is already cached (a slow rebuild never regresses the cache).
pub(super) fn publish(dir: &Path, snapshot: &Arc<VectorSnapshot>) {
    let mut shards = SHARDS.lock();
    let newer = shards
        .get(dir)
        .is_none_or(|current| current.generation < snapshot.generation);
    if newer {
        shards.insert(dir.to_path_buf(), Arc::clone(snapshot));
    }
}

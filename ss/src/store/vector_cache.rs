//! Generation-keyed in-memory copy of a shard's vectors, so a query scores a contiguous matrix instead of
//! re-reading every blob out of SQLite. Readers clone an `Arc` and never wait on a rebuild; a rebuild publishes a
//! newer `Arc` and readers holding the old one finish on it, so one search sees exactly one generation.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use proxima_primitives::sync::blocking::Mutex;

const DEFAULT_LIMIT_MB: usize = 512;

static LIMIT_MB: AtomicUsize = AtomicUsize::new(DEFAULT_LIMIT_MB);
static HITS: AtomicU64 = AtomicU64::new(0);
static STARTED: AtomicU64 = AtomicU64::new(0);
static PUBLISHED: AtomicU64 = AtomicU64::new(0);
static STALE_MS: AtomicU64 = AtomicU64::new(2000);
static ACTIVE_RUNS: AtomicUsize = AtomicUsize::new(0);
static SERVED_STALE: AtomicU64 = AtomicU64::new(0);
static WAITED: AtomicU64 = AtomicU64::new(0);
static IN_FLIGHT: AtomicU64 = AtomicU64::new(0);
static MAX_IN_FLIGHT: AtomicU64 = AtomicU64::new(0);

/// Cache activity totals; `in_flight` is rebuilds running now and `max_in_flight` its high-water mark.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counters {
    pub hits: u64,
    pub started: u64,
    pub published: u64,
    pub in_flight: u64,
    pub max_in_flight: u64,
    pub served_stale: u64,
    pub waited_on_build: u64,
}

#[must_use]
pub fn counters() -> Counters {
    Counters {
        hits: HITS.load(Ordering::Relaxed),
        started: STARTED.load(Ordering::Relaxed),
        published: PUBLISHED.load(Ordering::Relaxed),
        in_flight: IN_FLIGHT.load(Ordering::Relaxed),
        max_in_flight: MAX_IN_FLIGHT.load(Ordering::Relaxed),
        served_stale: SERVED_STALE.load(Ordering::Relaxed),
        waited_on_build: WAITED.load(Ordering::Relaxed),
    }
}

/// Marks a rebuild running until dropped, so every exit path decrements the gauge.
pub(super) struct RebuildGuard;

impl RebuildGuard {
    pub(super) fn begin() -> Self {
        STARTED.fetch_add(1, Ordering::Relaxed);
        let now = IN_FLIGHT.fetch_add(1, Ordering::Relaxed) + 1;
        MAX_IN_FLIGHT.fetch_max(now, Ordering::Relaxed);
        Self
    }
}

impl Drop for RebuildGuard {
    fn drop(&mut self) {
        IN_FLIGHT.fetch_sub(1, Ordering::Relaxed);
    }
}

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
    published: Instant,
    dim: usize,
    ids: Vec<i64>,
    paths: Vec<String>,
    matrix: Vec<f32>,
}

impl VectorSnapshot {
    pub(super) fn new(generation: i64, dim: usize) -> Self {
        Self {
            generation,
            published: Instant::now(),
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
            .map(|((id, _), row)| {
                (
                    *id,
                    row.iter()
                        .zip(query)
                        .map(|(left, right)| left * right)
                        .sum(),
                )
            })
            .collect();
        scored.sort_unstable_by(|left, right| right.1.total_cmp(&left.1));
        scored.truncate(limit);
        scored.into_iter().map(|(id, _)| id).collect()
    }
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
        PUBLISHED.fetch_add(1, Ordering::Relaxed);
    }
}

/// Sets how long an older snapshot may serve searches while an index run is active.
pub fn set_stale_ms(millis: u64) {
    STALE_MS.store(millis, Ordering::Relaxed);
}

/// Marks an index run active until dropped; searches may serve a bounded-stale snapshot only while one is.
pub struct IndexRun;

impl IndexRun {
    #[must_use]
    pub fn begin() -> Self {
        ACTIVE_RUNS.fetch_add(1, Ordering::Relaxed);
        Self
    }
}

impl Drop for IndexRun {
    fn drop(&mut self) {
        ACTIVE_RUNS.fetch_sub(1, Ordering::Relaxed);
    }
}

/// One in-flight build for a shard; waiters register a sender and the builder wakes them all when it finishes.
pub(super) struct Build {
    waiters: Mutex<Vec<Sender<()>>>,
    done: AtomicBool,
    declined: AtomicBool,
}

impl Build {
    pub(super) fn finish(&self, declined: bool) {
        self.declined.store(declined, Ordering::Release);
        let waiters = {
            let mut guard = self.waiters.lock();
            self.done.store(true, Ordering::Release);
            std::mem::take(&mut *guard)
        };
        for waiter in waiters {
            let _ = waiter.send(());
        }
    }

    pub(super) fn declined(&self) -> bool {
        self.declined.load(Ordering::Acquire)
    }

    fn wait(&self) {
        let (sender, receiver) = channel();
        {
            let mut guard = self.waiters.lock();
            if self.done.load(Ordering::Acquire) {
                return;
            }
            guard.push(sender);
        }
        let _ = receiver.recv();
    }
}

static BUILDS: LazyLock<Mutex<HashMap<PathBuf, Arc<Build>>>> = LazyLock::new(Mutex::default);

/// The shard's in-flight build and whether the caller just created it (and so must start the builder thread).
pub(super) fn single_flight(dir: &Path) -> (Arc<Build>, bool) {
    let mut builds = BUILDS.lock();
    if let Some(running) = builds.get(dir) {
        return (Arc::clone(running), false);
    }
    let build = Arc::new(Build {
        waiters: Mutex::default(),
        done: AtomicBool::new(false),
        declined: AtomicBool::new(false),
    });
    builds.insert(dir.to_path_buf(), Arc::clone(&build));
    (build, true)
}

pub(super) fn retire(dir: &Path) {
    BUILDS.lock().remove(dir);
}

/// Blocks until the in-flight build finishes; counted so status shows how often a query paid for a build.
pub(super) fn wait_on(build: &Build) {
    WAITED.fetch_add(1, Ordering::Relaxed);
    build.wait();
}

/// The newest published snapshot when it may serve a search that wants `generation`: at least that new, or
/// (while an index run is active) younger than the stale bound.
pub(super) fn servable(dir: &Path, generation: i64) -> Option<Arc<VectorSnapshot>> {
    let newest = SHARDS.lock().get(dir).cloned()?;
    if newest.generation >= generation {
        HITS.fetch_add(1, Ordering::Relaxed);
        return Some(newest);
    }
    let bound = Duration::from_millis(STALE_MS.load(Ordering::Relaxed));
    let running = ACTIVE_RUNS.load(Ordering::Relaxed) > 0;
    (running && newest.published.elapsed() <= bound).then(|| {
        SERVED_STALE.fetch_add(1, Ordering::Relaxed);
        newest
    })
}

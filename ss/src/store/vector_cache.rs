//! Content-addressed immutable vector segments. A query clones the current `Arc<List>` (the pointer is the
//! version) and scans it; a write publishes one new segment plus tombstones in a single swap, so a search never
//! sees half of a file. Compaction merges small segments off the query path.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock};

use proxima_primitives::sync::blocking::Mutex;

const DEFAULT_LIMIT_MB: usize = 512;
const DEFAULT_MAX_SEGMENTS: usize = 32;
const DEAD_PERCENT: usize = 25;

static LIMIT_MB: AtomicUsize = AtomicUsize::new(DEFAULT_LIMIT_MB);
static MAX_SEGMENTS: AtomicUsize = AtomicUsize::new(DEFAULT_MAX_SEGMENTS);
static HITS: AtomicU64 = AtomicU64::new(0);
static COMPACTIONS: AtomicU64 = AtomicU64::new(0);
static OVER_CAP: AtomicU64 = AtomicU64::new(0);

/// Cache activity; `segments` and `tombstones` are the live totals across shards.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counters {
    pub hits: u64,
    pub segments: u64,
    pub tombstones: u64,
    pub compactions: u64,
    pub over_cap: u64,
}

#[must_use]
pub fn counters() -> Counters {
    let shards = SHARDS.lock();
    let lists = shards.values().filter_map(|shard| shard.list.as_ref());
    let (segments, tombstones) = lists.fold((0, 0), |(seg, dead), list| {
        (
            seg + list.segments.len() as u64,
            dead + list.dead_rows() as u64,
        )
    });
    Counters {
        hits: HITS.load(Ordering::Relaxed),
        segments,
        tombstones,
        compactions: COMPACTIONS.load(Ordering::Relaxed),
        over_cap: OVER_CAP.load(Ordering::Relaxed),
    }
}

/// Sets the per-process cache ceiling in megabytes; a shard whose vectors exceed it keeps using the SQLite scan.
pub fn set_limit_mb(megabytes: usize) {
    LIMIT_MB.store(megabytes, Ordering::Relaxed);
}

/// Sets the segment count above which compaction runs.
pub fn set_max_segments(count: usize) {
    MAX_SEGMENTS.store(count.max(2), Ordering::Relaxed);
}

pub(super) fn limit_bytes() -> usize {
    LIMIT_MB.load(Ordering::Relaxed).saturating_mul(1 << 20)
}

/// One immutable block of rows: ids ascending, the path of each row, and a contiguous row-major matrix.
pub struct Segment {
    dim: usize,
    ids: Vec<i64>,
    paths: Vec<String>,
    matrix: Vec<f32>,
}

impl Segment {
    #[must_use]
    pub fn new(dim: usize) -> Self {
        Self {
            dim,
            ids: Vec::new(),
            paths: Vec::new(),
            matrix: Vec::new(),
        }
    }

    /// Appends a row from its little-endian blob; false when the width disagrees with the segment.
    pub fn push(&mut self, id: i64, path: String, blob: &[u8]) -> bool {
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
    pub fn len(&self) -> usize {
        self.ids.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }

    fn bytes(&self) -> usize {
        self.matrix.len() * 4
    }

    fn contains(&self, id: i64) -> bool {
        self.ids.binary_search(&id).is_ok()
    }

    fn score_into(
        &self,
        dead: &HashSet<i64>,
        query: &[f32],
        prefix: &str,
        out: &mut Vec<(i64, f32)>,
    ) {
        let rows = self.matrix.chunks_exact(self.dim.max(1));
        for ((id, path), row) in self.ids.iter().zip(&self.paths).zip(rows) {
            if path.starts_with(prefix) && !dead.contains(id) {
                out.push((
                    *id,
                    row.iter()
                        .zip(query)
                        .map(|(left, right)| left * right)
                        .sum(),
                ));
            }
        }
    }
}

/// An immutable view: segments plus, per segment, the ids tombstoned in it. Cloning the `Arc` is the whole capture.
pub struct List {
    segments: Vec<Arc<Segment>>,
    dead: Vec<Arc<HashSet<i64>>>,
}

impl List {
    fn empty() -> Self {
        Self {
            segments: Vec::new(),
            dead: Vec::new(),
        }
    }

    fn dead_rows(&self) -> usize {
        self.dead.iter().map(|set| set.len()).sum()
    }

    fn bytes(&self) -> usize {
        self.segments.iter().map(|segment| segment.bytes()).sum()
    }

    fn rows(&self) -> usize {
        self.segments.iter().map(|segment| segment.len()).sum()
    }

    /// Ids of the `limit` best live chunks under `prefix`, best first; ties keep the unstable-sort order of the scan, as the SQLite scan does.
    #[must_use]
    pub fn top(&self, query: &[f32], prefix: &str, limit: usize) -> Vec<i64> {
        let mut scored = Vec::with_capacity(self.rows());
        for (segment, dead) in self.segments.iter().zip(&self.dead) {
            segment.score_into(dead, query, prefix, &mut scored);
        }
        scored.sort_unstable_by(|left, right| right.1.total_cmp(&left.1));
        scored.truncate(limit);
        scored.into_iter().map(|(id, _)| id).collect()
    }

    fn with_write(&self, replaced: &[i64], added: Option<Segment>) -> Self {
        let mut segments = self.segments.clone();
        let mut dead = self.dead.clone();
        for (segment, set) in segments.iter().zip(dead.iter_mut()) {
            let hit: Vec<i64> = replaced
                .iter()
                .copied()
                .filter(|id| segment.contains(*id))
                .collect();
            if !hit.is_empty() {
                let mut grown = (**set).clone();
                grown.extend(hit);
                *set = Arc::new(grown);
            }
        }
        if let Some(segment) = added.filter(|segment| !segment.is_empty()) {
            segments.push(Arc::new(segment));
            dead.push(Arc::new(HashSet::new()));
        }
        Self { segments, dead }
    }

    fn needs_compaction(&self) -> bool {
        let rows = self.rows().max(1);
        self.segments.len() > MAX_SEGMENTS.load(Ordering::Relaxed)
            || self.dead_rows() * 100 > rows * DEAD_PERCENT
    }
}

struct Shard {
    list: Option<Arc<List>>,
    loading: bool,
    pending: Vec<(Vec<i64>, Option<Arc<Segment>>)>,
    compacting: Arc<AtomicBool>,
}

// a blocking mutex outside async, held only for an Arc clone or swap, never across a scan or a build
static SHARDS: LazyLock<Mutex<HashMap<PathBuf, Shard>>> = LazyLock::new(Mutex::default);

impl Default for Shard {
    fn default() -> Self {
        Self {
            list: None,
            loading: false,
            pending: Vec::new(),
            compacting: Arc::new(AtomicBool::new(false)),
        }
    }
}

/// The current list for `dir`, if one is loaded: one lock, one `Arc` clone.
#[must_use]
pub fn current(dir: &Path) -> Option<Arc<List>> {
    let found = SHARDS.lock().get(dir)?.list.clone();
    if found.is_some() {
        HITS.fetch_add(1, Ordering::Relaxed);
    }
    found
}

/// Drops the list for `dir`; searches scan SQLite until it is loaded again.
pub fn evict(dir: &Path) {
    SHARDS.lock().remove(dir);
}

/// Marks `dir` as loading so writes during the load are recorded; false when a load is already running.
pub(super) fn begin_load(dir: &Path) -> bool {
    let mut shards = SHARDS.lock();
    let shard = shards.entry(dir.to_path_buf()).or_default();
    if shard.loading || shard.list.is_some() {
        return false;
    }
    shard.loading = true;
    true
}

/// Installs the loaded segments, replaying writes published while the load ran (newer rows win over the snapshot).
pub(super) fn finish_load(dir: &Path, loaded: Option<Segment>) {
    let mut shards = SHARDS.lock();
    let Some(shard) = shards.get_mut(dir) else {
        return;
    };
    shard.loading = false;
    let pending = std::mem::take(&mut shard.pending);
    let Some(loaded) = loaded else { return };
    let mut list = List::empty().with_write(&[], Some(loaded));
    for (replaced, added) in pending {
        let ids: Vec<i64> = added
            .iter()
            .flat_map(|segment| segment.ids.iter().copied())
            .chain(replaced)
            .collect();
        list = list.with_write(&ids, None);
        if let Some(segment) = added {
            list.segments.push(segment);
            list.dead.push(Arc::new(HashSet::new()));
        }
    }
    shard.list = Some(Arc::new(list));
}

/// Publishes one file's write: its replaced or removed ids and its new segment, in one swap.
pub fn publish(dir: &Path, replaced: &[i64], added: Segment) {
    let added = Arc::new(added);
    let compact = {
        let mut shards = SHARDS.lock();
        let Some(shard) = shards.get_mut(dir) else {
            return;
        };
        if let Some(list) = shard.list.clone() {
            {
                let mut next = list.with_write(replaced, None);
                if !added.is_empty() {
                    next.segments.push(Arc::clone(&added));
                    next.dead.push(Arc::new(HashSet::new()));
                }
                if next.bytes() > limit_bytes() {
                    shard.list = None;
                    OVER_CAP.fetch_add(1, Ordering::Relaxed);
                    return;
                }
                let due = next.needs_compaction();
                shard.list = Some(Arc::new(next));
                due.then(|| Arc::clone(&shard.compacting))
            }
        } else {
            if shard.loading {
                shard.pending.push((replaced.to_vec(), Some(added)));
            }
            None
        }
    };
    if let Some(flag) = compact {
        spawn_compaction(dir, &flag);
    }
}

fn spawn_compaction(dir: &Path, flag: &Arc<AtomicBool>) {
    if flag.swap(true, Ordering::AcqRel) {
        return;
    }
    let (owned, running) = (dir.to_path_buf(), Arc::clone(flag));
    let spawned = std::thread::Builder::new()
        .name("ss-segment-compact".to_owned())
        .spawn(move || {
            compact(&owned);
            running.store(false, Ordering::Release);
        });
    if spawned.is_err() {
        flag.store(false, Ordering::Release);
    }
}

/// Merges the live rows of every segment in the captured list into one segment and swaps it in.
pub fn compact(dir: &Path) {
    let Some(captured) = current_quiet(dir) else {
        return;
    };
    let mut merged = Segment::new(captured.segments.first().map_or(0, |segment| segment.dim));
    let mut rows: Vec<(i64, &str, &[f32])> = Vec::with_capacity(captured.rows());
    for (segment, dead) in captured.segments.iter().zip(&captured.dead) {
        let width = segment.dim.max(1);
        for ((id, path), row) in segment
            .ids
            .iter()
            .zip(&segment.paths)
            .zip(segment.matrix.chunks_exact(width))
        {
            if !dead.contains(id) {
                rows.push((*id, path, row));
            }
        }
    }
    rows.sort_unstable_by_key(|row| row.0);
    for (id, path, row) in rows {
        merged.ids.push(id);
        merged.paths.push(path.to_owned());
        merged.matrix.extend_from_slice(row);
    }
    install_merged(dir, &captured, merged);
}

fn current_quiet(dir: &Path) -> Option<Arc<List>> {
    SHARDS.lock().get(dir)?.list.clone()
}

fn install_merged(dir: &Path, captured: &List, merged: Segment) {
    let mut shards = SHARDS.lock();
    let Some(shard) = shards.get_mut(dir) else {
        return;
    };
    let Some(current) = shard.list.clone() else {
        return;
    };
    let mut newly_dead = HashSet::new();
    let mut segments = Vec::new();
    let mut dead = Vec::new();
    for (segment, set) in current.segments.iter().zip(&current.dead) {
        let seen = captured
            .segments
            .iter()
            .position(|old| Arc::ptr_eq(old, segment));
        if let Some(index) = seen {
            newly_dead.extend(set.difference(&captured.dead[index]).copied());
        } else {
            segments.push(Arc::clone(segment));
            dead.push(Arc::clone(set));
        }
    }
    segments.insert(0, Arc::new(merged));
    dead.insert(0, Arc::new(newly_dead));
    shard.list = Some(Arc::new(List { segments, dead }));
    COMPACTIONS.fetch_add(1, Ordering::Relaxed);
}

/// True when writes to `dir` must be published (a list is loaded or loading).
pub(super) fn active(dir: &Path) -> bool {
    SHARDS
        .lock()
        .get(dir)
        .is_some_and(|shard| shard.loading || shard.list.is_some())
}

/// Sequential dot product; the SQLite scan multiplies and sums in the same order, so both agree bit for bit.
#[must_use]
pub fn dot(left: &[f32], right: &[f32]) -> f32 {
    left.iter().zip(right).map(|(a, b)| a * b).sum()
}

// serializes one shard's writers from before their transaction until their publish, so publish order is commit order
static ORDER: LazyLock<Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>> = LazyLock::new(Mutex::default);

/// The shard's write-order lock; a writer holds `.lock()` on it across commit and publish.
#[must_use]
pub fn write_order(dir: &Path) -> Arc<Mutex<()>> {
    Arc::clone(ORDER.lock().entry(dir.to_path_buf()).or_default())
}

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
static SCANS: AtomicU64 = AtomicU64::new(0);

/// Cache activity; `segments` and `tombstones` are the live totals across shards.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counters {
    pub hits: u64,
    pub segments: u64,
    pub tombstones: u64,
    pub compactions: u64,
    pub over_cap: u64,
    /// Searches that fell back to the SQLite scan because no list was loaded.
    pub scans: u64,
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
        scans: SCANS.load(Ordering::Relaxed),
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

pub(super) fn count_scan() {
    SCANS.fetch_add(1, Ordering::Relaxed);
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
            let _lease = CompactionLease(running);
            let _ = compact(&owned);
        });
    if spawned.is_err() {
        flag.store(false, Ordering::Release);
    }
}

struct CompactionLease(Arc<AtomicBool>);

impl Drop for CompactionLease {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct IdleCompaction {
    pub compacted_roots: usize,
    pub pending_roots: usize,
}

/// Merges only loaded lists with multiple segments or tombstones, respecting active compactions.
pub fn compact_idle() -> IdleCompaction {
    let candidates: Vec<PathBuf> = SHARDS
        .lock()
        .iter()
        .filter_map(|(dir, shard)| {
            let list = shard.list.as_ref()?;
            idle_candidate(list).then(|| dir.clone())
        })
        .collect();
    let mut outcome = IdleCompaction::default();
    for dir in candidates {
        match compact_idle_for(&dir) {
            IdleAttempt::Compacted => outcome.compacted_roots += 1,
            IdleAttempt::Pending => outcome.pending_roots += 1,
            IdleAttempt::Clean => {}
        }
    }
    outcome
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IdleAttempt {
    Compacted,
    Pending,
    Clean,
}

fn idle_candidate(list: &List) -> bool {
    list.segments.len() > 1 || list.dead_rows() > 0
}

fn compact_idle_for(dir: &Path) -> IdleAttempt {
    let flag = {
        let shards = SHARDS.lock();
        let Some(shard) = shards.get(dir) else { return IdleAttempt::Clean };
        if !shard.list.as_ref().is_some_and(|list| idle_candidate(list)) {
            return IdleAttempt::Clean;
        }
        Arc::clone(&shard.compacting)
    };
    if flag
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return IdleAttempt::Pending;
    }
    let _lease = CompactionLease(flag);
    if compact(dir) {
        IdleAttempt::Compacted
    } else {
        IdleAttempt::Pending
    }
}

/// Merges the live rows of every segment in the captured list into one segment and swaps it in.
#[must_use]
pub fn compact(dir: &Path) -> bool {
    let Some(captured) = current_quiet(dir) else {
        return false;
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
    install_merged(dir, &captured, merged)
}

fn current_quiet(dir: &Path) -> Option<Arc<List>> {
    SHARDS.lock().get(dir)?.list.clone()
}

fn install_merged(dir: &Path, captured: &List, merged: Segment) -> bool {
    let mut shards = SHARDS.lock();
    let Some(shard) = shards.get_mut(dir) else {
        return false;
    };
    let Some(current) = shard.list.clone() else {
        return false;
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
    true
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

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use super::*;

    fn authored_segment(rows: &[(i64, &str, [f32; 2])]) -> Segment {
        let mut segment = Segment::new(2);
        for (id, path, vector) in rows {
            let mut bytes = Vec::new();
            for value in vector {
                bytes.extend_from_slice(&value.to_le_bytes());
            }
            assert!(segment.push(*id, (*path).to_owned(), &bytes));
        }
        segment
    }

    fn authored_live_rows_match(list: &List, expected: &[(i64, &str, [f32; 2])], missing_id: i64) -> bool {
        let mut actual = Vec::new();
        for (segment, dead) in list.segments.iter().zip(&list.dead) {
            for ((id, path), row) in segment
                .ids
                .iter()
                .zip(&segment.paths)
                .zip(segment.matrix.as_chunks::<2>().0)
            {
                if !dead.contains(id) {
                    actual.push((*id, path.as_str(), row.to_vec()));
                }
            }
        }
        actual.sort_unstable_by_key(|row| row.0);
        let expected_rows: Vec<_> = expected
            .iter()
            .map(|(id, path, vector)| (*id, *path, vector.to_vec()))
            .collect();
        actual == expected_rows && !actual.iter().any(|row| row.0 == missing_id)
    }

    #[test]
    fn idle_compaction_oracle_authored_rows_and_negative_controls() {
        let home = tempfile::tempdir().expect("authored fixture directory");
        let authored_path = home.path().join("authored.rs");
        std::fs::write(&authored_path, "pub fn authored() {}\n")
            .expect("write authored fixture");
        assert!(authored_path.is_file());
        assert!(!home.path().join("missing.rs").exists());
        let empty = tempfile::tempdir().expect("empty fixture directory");
        assert_eq!(
            std::fs::read_dir(empty.path())
                .expect("enumerate empty fixture")
                .count(),
            0
        );

        let (sender, receiver) = mpsc::channel::<()>();
        sender.send(()).expect("live owner accepts work");
        assert!(receiver.recv().is_ok());
        drop(receiver);
        assert!(sender.send(()).is_err());

        let authored = authored_segment(&[(7, "authored.rs", [1.0, 0.0])]);
        let list = List::empty().with_write(&[], Some(authored));
        assert!(authored_live_rows_match(&list, &[(7, "authored.rs", [1.0, 0.0])], 8));
        assert!(!authored_live_rows_match(&list, &[(8, "missing.rs", [1.0, 0.0])], 7));
        assert!(!authored_live_rows_match(&list, &[(7, "authored.rs", [0.0, 1.0])], 8));
        assert_eq!(list.top(&[1.0, 0.0], "authored.rs", 1), vec![7]);
        assert_eq!(list.top(&[1.0, 0.0], "missing.rs", 1).len(), 0);
        assert!(!idle_candidate(&list));
        let dead = list.with_write(&[7], None);
        assert!(authored_live_rows_match(&dead, &[], 7));
        assert!(idle_candidate(&dead));
        assert_eq!(dead.top(&[1.0, 0.0], "authored.rs", 1).len(), 0);
    }

    #[test]
    fn idle_compaction_vectors_preserves_exact_live_payload_and_held_reader() {
        let home = tempfile::tempdir().expect("cache directory");
        let dir = home.path();
        let held = Arc::new(List::empty().with_write(
            &[],
            Some(authored_segment(&[
                (1, "old.rs", [1.0, 0.0]),
                (2, "live.rs", [0.0, 1.0]),
                (4, "four.rs", [0.25, 0.5]),
                (5, "five.rs", [0.75, 0.25]),
            ])),
        ));
        let updated = held.with_write(&[1], Some(authored_segment(&[(3, "new.rs", [0.5, 0.25])])));
        SHARDS.lock().insert(
            dir.to_path_buf(),
            Shard {
                list: Some(Arc::new(updated)),
                ..Shard::default()
            },
        );
        assert_eq!(compact_idle_for(dir), IdleAttempt::Compacted);
        let current = current(dir).expect("merged reader view");
        assert_eq!(current.segments.len(), 1);
        assert_eq!(current.dead_rows(), 0);
        assert_eq!(current.segments[0].ids, vec![2, 3, 4, 5]);
        assert!(authored_live_rows_match(
            &current,
            &[
                (2, "live.rs", [0.0, 1.0]),
                (3, "new.rs", [0.5, 0.25]),
                (4, "four.rs", [0.25, 0.5]),
                (5, "five.rs", [0.75, 0.25]),
            ],
            1,
        ));
        assert!(!authored_live_rows_match(&current, &[(1, "old.rs", [1.0, 0.0])], 1));
        assert_eq!(
            current.segments[0].matrix,
            vec![0.0, 1.0, 0.5, 0.25, 0.25, 0.5, 0.75, 0.25]
        );
        assert_eq!(held.segments[0].ids, vec![1, 2, 4, 5]);
        assert_eq!(
            held.segments[0].matrix,
            vec![1.0, 0.0, 0.0, 1.0, 0.25, 0.5, 0.75, 0.25]
        );
        evict(dir);
    }

    #[test]
    fn idle_compaction_vectors_skips_already_compact_live_list() {
        let home = tempfile::tempdir().expect("cache directory");
        let dir = home.path();
        assert!(begin_load(dir));
        finish_load(dir, Some(authored_segment(&[(9, "live.rs", [0.25, 0.75])])));
        let before = current(dir).expect("compact list");
        let compactions_before = counters().compactions;
        assert_eq!(compact_idle_for(dir), IdleAttempt::Clean);
        let after = current(dir).expect("same compact list");
        assert!(Arc::ptr_eq(&before, &after));
        assert_eq!(counters().compactions, compactions_before);
        evict(dir);
    }
}

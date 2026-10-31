//! Checks kill criteria K1-K4 of the segment list (`ai_docs/design.md`); exits 0 only if all hold.
//!
//! `cargo run --release --example segment_budget`
// timing harness aborts on setup failure and divides counts for display
#![allow(clippy::expect_used, clippy::cast_precision_loss)]

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Instant;

use ss::chunk::{Language, chunk_source};
use ss::embed::{EmbedProfile, EmbedderKind, hash_embed};
use ss::graph::FileGraph;
use ss::store::{FileRow, Store, StoreConfig, vector_cache};

const COPIES: usize = 78;
const ITERATIONS: usize = 1000;
const WRITERS: usize = 4;
const K1_US: f64 = 5.0;
const K2_MS: f64 = 10.0;
const K3_MS: f64 = 2.0;

fn thread_cpu_ns() -> u64 {
    let mut spec = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: clock_gettime fills the timespec it is handed
    unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &raw mut spec) };
    u64::try_from(spec.tv_sec).unwrap_or(0) * 1_000_000_000
        + u64::try_from(spec.tv_nsec).unwrap_or(0)
}

fn process_cpu_ms() -> f64 {
    // SAFETY: getrusage fills the zeroed struct it is handed
    let usage = unsafe {
        let mut usage: libc::rusage = std::mem::zeroed();
        libc::getrusage(libc::RUSAGE_SELF, &raw mut usage);
        usage
    };
    let micros = |value: libc::timeval| (value.tv_sec as f64) * 1e6 + f64::from(value.tv_usec);
    (micros(usage.ru_utime) + micros(usage.ru_stime)) / 1000.0
}

fn p99(mut samples: Vec<f64>) -> f64 {
    samples.sort_by(f64::total_cmp);
    samples[(samples.len() * 99 / 100).min(samples.len() - 1)]
}

fn index_text(store: &mut Store, path: &str, text: &str) {
    let chunks = chunk_source(Language::from_extension("rs"), text);
    let texts: Vec<String> = chunks.iter().map(|chunk| chunk.text.clone()).collect();
    let vectors: Vec<_> = chunks
        .iter()
        .zip(hash_embed(&texts).expect("embed"))
        .map(|(chunk, vector)| (chunk.key, vector))
        .collect();
    let hash = chunks.first().map_or([0; 32], |chunk| chunk.key);
    let row = FileRow {
        mtime_ns: 1,
        size: 1,
        hash,
    };
    store
        .replace_file(path, &row, &chunks, &vectors, &FileGraph::default())
        .expect("replace file");
}

fn corpus_files() -> Vec<(String, String)> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/corpus");
    let mut files: Vec<PathBuf> = ignore::WalkBuilder::new(root)
        .build()
        .flatten()
        .map(ignore::DirEntry::into_path)
        .filter(|path| path.is_file())
        .collect();
    files.sort();
    files
        .into_iter()
        .filter_map(|file| {
            std::fs::read_to_string(&file).ok().map(|text| {
                (
                    file.file_name()
                        .map_or_else(String::new, |name| name.to_string_lossy().into_owned()),
                    text,
                )
            })
        })
        .collect()
}

fn build_index(config: &StoreConfig) -> Store {
    let mut store = Store::open(config).expect("open");
    let files = corpus_files();
    for copy in 0..COPIES {
        for (name, text) in &files {
            index_text(
                &mut store,
                &format!("copy{copy}/{name}"),
                &format!("{text}\n// tag {copy}\n"),
            );
        }
    }
    store.load_segments().expect("load");
    store
}

fn k1(dir: &Path) -> f64 {
    let samples = (0..ITERATIONS).map(|_| {
        let start = thread_cpu_ns();
        let list = vector_cache::current(dir);
        let spent = thread_cpu_ns() - start;
        drop(list);
        spent as f64 / 1000.0
    });
    p99(samples.collect())
}

fn search_p99(store: &Store, stop: &AtomicBool) -> f64 {
    let probe = hash_embed(&["worker pool threads".to_owned()])
        .expect("embed")
        .remove(0);
    let mut samples = Vec::with_capacity(ITERATIONS);
    while samples.len() < ITERATIONS && !stop.load(Ordering::Relaxed) {
        let start = thread_cpu_ns();
        let hits = store.vector_search(&probe, "", 20).expect("search");
        samples.push((thread_cpu_ns() - start) as f64 / 1e6);
        assert!(!hits.is_empty(), "hits");
    }
    p99(samples)
}

fn spawn_writers(
    config: &StoreConfig,
    stop: &Arc<AtomicBool>,
) -> Vec<thread::JoinHandle<Vec<f64>>> {
    (0..WRITERS)
        .map(|writer| {
            let (config, stop) = (config.clone(), Arc::clone(stop));
            thread::spawn(move || {
                let mut store = Store::open(&config).expect("writer open");
                let mut publishes = Vec::new();
                let mut number = 0_usize;
                while !stop.load(Ordering::Relaxed) {
                    let (start, text) = (
                        thread_cpu_ns(),
                        format!("pub fn writer_{writer}_{number}() {{ let value = {number}; }}\n"),
                    );
                    index_text(&mut store, &format!("w{writer}/f{}.rs", number % 8), &text);
                    publishes.push((thread_cpu_ns() - start) as f64 / 1e6);
                    number += 1;
                }
                publishes
            })
        })
        .collect()
}

fn k2(config: &StoreConfig, reader: &Store) -> (f64, f64) {
    let stop = Arc::new(AtomicBool::new(false));
    let writers = spawn_writers(config, &stop);
    let search = search_p99(reader, &stop);
    stop.store(true, Ordering::Relaxed);
    let publishes: Vec<f64> = writers
        .into_iter()
        .flat_map(|handle| handle.join().expect("writer"))
        .collect();
    (search, p99(publishes))
}

fn k4(config: &StoreConfig, dir: &Path, reader: &Store) -> (f64, u64) {
    let mut writer = Store::open(config).expect("open");
    (0..32).for_each(|number| {
        index_text(
            &mut writer,
            &format!("seg{number}.rs"),
            &format!("pub fn seg_{number}() {{}}\n"),
        );
    });
    let before = vector_cache::counters().compactions;
    let owned = dir.to_path_buf();
    let compactor = thread::spawn(move || {
        let start = Instant::now();
        vector_cache::compact(&owned);
        start.elapsed().as_secs_f64() * 1000.0
    });
    let stop = AtomicBool::new(false);
    let search = search_p99(reader, &stop);
    let wall = compactor.join().expect("compactor");
    println!("compaction wall {wall:.1} ms (labeled wall, noisy)");
    (search, vector_cache::counters().compactions - before)
}

fn k3(dir: &Path) -> f64 {
    let blob: Vec<u8> = (0..256_u16)
        .flat_map(|value| (f32::from(value) / 256.0).to_le_bytes())
        .collect();
    let samples = (0..ITERATIONS).map(|number| {
        let base = 1_000_000_000 + i64::try_from(number).unwrap_or(0) * 8;
        let mut segment = vector_cache::Segment::new(256);
        (0..6).for_each(|row| assert!(segment.push(base + row, "k3.rs".to_owned(), &blob)));
        let replaced: Vec<i64> = (0..6).map(|row| base - 8 + row).collect();
        let start = thread_cpu_ns();
        vector_cache::publish(dir, &replaced, segment);
        (thread_cpu_ns() - start) as f64 / 1e6
    });
    p99(samples.collect())
}

fn uptime() -> String {
    Command::new("uptime")
        .output()
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_owned())
        .unwrap_or_default()
}

fn main() -> ExitCode {
    let work = tempfile::tempdir().expect("tempdir");
    let config = StoreConfig {
        dir: work.path().join("shard"),
        model_id: EmbedProfile::default().store_id(EmbedderKind::Hash),
    };
    let store = build_index(&config);
    println!(
        "chunks {} uptime {}",
        store.chunk_count().expect("count"),
        uptime()
    );
    let wall = Instant::now();
    let cpu_before = process_cpu_ms();
    let capture_us = k1(&config.dir);
    let (search_ms, write_ms) = k2(&config, &store);
    println!("end-to-end replace_file p99 (sqlite+publish, info only) {write_ms:.3} ms");
    let (during_ms, compactions) = k4(&config, &config.dir, &store);
    let publish_ms = k3(&config.dir);
    println!(
        "process cpu {:.0} ms, wall {:.0} ms (labeled wall)",
        process_cpu_ms() - cpu_before,
        wall.elapsed().as_secs_f64() * 1000.0
    );
    let rows = [
        ("K1 capture p99 us", capture_us, K1_US, capture_us <= K1_US),
        ("K2 search p99 ms", search_ms, K2_MS, search_ms <= K2_MS),
        ("K3 publish p99 ms", publish_ms, K3_MS, publish_ms <= K3_MS),
        (
            "K4 search p99 ms during compaction",
            during_ms,
            K2_MS,
            during_ms <= K2_MS && compactions >= 1,
        ),
    ];
    for (name, value, limit, ok) in &rows {
        println!(
            "{name}: {value:.3} (limit {limit}) {}",
            if *ok { "OK" } else { "FAIL" }
        );
    }
    if rows.iter().all(|row| row.3) {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

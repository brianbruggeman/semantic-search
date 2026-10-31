//! CPU-time split of one `Store::open`: connection, pragmas, WAL switch, schema batch, model pin.
//!
//! `cargo run --release --example open_timing -- <copies> <opens> <threads>`
//!
//! Single-thread run does `opens` opens; the threaded run does `opens / 10` per thread. CPU is the calling
//! thread's CPU clock (so other threads do not pollute it); wall is labeled separately.
// timing harness aborts on setup failure and divides counts for display
#![allow(clippy::expect_used, clippy::cast_precision_loss, clippy::cast_possible_truncation, clippy::cast_sign_loss)]

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use rusqlite::{Connection, OptionalExtension};
use ss::chunk::{Language, chunk_source};
use ss::embed::hash_embed;
use ss::graph::FileGraph;
use ss::store::{FileRow, SCHEMA, Store, StoreConfig};

const MODEL: &str = "feature-hash-256";
const STEPS: [&str; 6] = ["conn_open", "pragma_busy_sync", "wal", "schema", "pin_model", "open_total"];

type Row = [(f64, f64); 6];

fn thread_cpu() -> Duration {
    let mut spec = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: clock_gettime fills the timespec it is handed
    unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &raw mut spec) };
    Duration::new(u64::try_from(spec.tv_sec).unwrap_or(0), u32::try_from(spec.tv_nsec).unwrap_or(0))
}

fn timed<T>(work: impl FnOnce() -> T) -> (T, (f64, f64)) {
    let (cpu_before, wall_before) = (thread_cpu(), Instant::now());
    let out = work();
    let cpu = thread_cpu().saturating_sub(cpu_before).as_secs_f64() * 1000.0;
    (out, (cpu, wall_before.elapsed().as_secs_f64() * 1000.0))
}

fn config(dir: &Path) -> StoreConfig {
    StoreConfig { dir: dir.to_path_buf(), model_id: MODEL.to_owned() }
}

fn build(dir: &Path, copies: usize) {
    let mut store = Store::open(&config(dir)).expect("open");
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    for entry in std::fs::read_dir(&source).expect("src").flatten() {
        let Ok(text) = std::fs::read_to_string(entry.path()) else { continue };
        for copy in 0..copies {
            let tagged = text.lines().fold(String::new(), |mut acc, line| {
                let _ = writeln!(acc, "// c{copy}\n{line}");
                acc
            });
            let chunks = chunk_source(Some(Language::Rust), &tagged);
            let texts: Vec<String> = chunks.iter().map(|chunk| chunk.text.clone()).collect();
            let vectors: Vec<_> = chunks
                .iter()
                .zip(hash_embed(&texts).expect("embed"))
                .map(|(chunk, vector)| (chunk.key, vector))
                .collect();
            let path = format!("copy_{copy}/{}", entry.file_name().to_string_lossy());
            let hash = chunks.first().map_or([0; 32], |chunk| chunk.key);
            let row = FileRow { mtime_ns: 1, size: 1, hash };
            store.replace_file(&path, &row, &chunks, &vectors, &FileGraph::default()).expect("replace");
        }
    }
}

fn one_open(dir: &Path) -> Row {
    let mut row = [(0.0, 0.0); 6];
    let (conn, timing) = timed(|| Connection::open(dir.join("index.db")).expect("conn"));
    row[0] = timing;
    row[1] = timed(|| conn.execute_batch("PRAGMA busy_timeout=10000; PRAGMA synchronous=NORMAL;").expect("pragma")).1;
    row[2] = timed(|| conn.execute_batch("PRAGMA journal_mode=WAL;").expect("wal")).1;
    row[3] = timed(|| conn.execute_batch(SCHEMA).expect("schema")).1;
    row[4] = timed(|| {
        conn.query_row("SELECT v FROM meta WHERE k='model'", [], |found| found.get::<_, String>(0)).optional().expect("pin")
    })
    .1;
    drop(conn);
    let (store, timing) = timed(|| Store::open(&config(dir)).expect("open"));
    row[5] = timing;
    drop(store);
    row
}

fn run(dir: &Path, opens: usize, threads: usize) -> Vec<Row> {
    let handles: Vec<_> = (0..threads)
        .map(|_| {
            let dir: PathBuf = dir.to_path_buf();
            thread::spawn(move || (0..opens).map(|_| one_open(&dir)).collect::<Vec<_>>())
        })
        .collect();
    handles.into_iter().flat_map(|handle| handle.join().expect("join")).collect()
}

fn pct(mut values: Vec<f64>, fraction: f64) -> f64 {
    values.sort_by(f64::total_cmp);
    values[((values.len() - 1) as f64 * fraction).round() as usize]
}

fn report(label: &str, samples: &[Row]) {
    println!("== {label}: n={}", samples.len());
    for (index, step) in STEPS.iter().enumerate() {
        let cpu: Vec<f64> = samples.iter().map(|row| row[index].0).collect();
        let wall: Vec<f64> = samples.iter().map(|row| row[index].1).collect();
        println!(
            "{step:18} cpu_ms p50={:.3} p99={:.3} | wall_ms p50={:.3} p99={:.3}",
            pct(cpu.clone(), 0.5),
            pct(cpu, 0.99),
            pct(wall.clone(), 0.5),
            pct(wall, 0.99)
        );
    }
}

fn main() {
    let args: Vec<usize> = std::env::args().skip(1).take(3).filter_map(|text| text.parse().ok()).collect();
    let (copies, opens, threads) = (
        args.first().copied().unwrap_or(20),
        args.get(1).copied().unwrap_or(1000),
        args.get(2).copied().unwrap_or(32),
    );
    let scratch = std::env::var_os("TMPDIR").map_or_else(std::env::temp_dir, Into::into);
    let dir = scratch.join(format!("ss-open-timing-{copies}"));
    if !dir.join("index.db").exists() {
        build(&dir, copies);
    }
    let chunks = Store::open(&config(&dir)).expect("open").chunk_count().expect("count");
    println!("chunks={chunks}");
    report("single thread", &run(&dir, opens, 1));
    report(&format!("{threads} threads x {}", opens / 10), &run(&dir, opens / 10, threads));
}

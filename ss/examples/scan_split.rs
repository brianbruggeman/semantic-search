//! CPU-time split of one vector scan: sqlite read+blob copy, dot scoring, top-k selection.
//!
//! `cargo run --release --example scan_split -- <copies>`
// timing harness aborts on setup failure and divides counts for display
#![allow(clippy::expect_used, clippy::cast_precision_loss)]

use std::fmt::Write as _;
use std::path::Path;
use std::time::Duration;

use rusqlite::{Connection, params};
use ss::chunk::{Language, chunk_source};
use ss::embed::hash_embed;
use ss::graph::FileGraph;
use ss::store::{FileRow, Store, StoreConfig};

const REPEATS: usize = 20;

fn cpu() -> Duration {
    // SAFETY: getrusage fills the zeroed struct it is handed
    let usage = unsafe {
        let mut usage: libc::rusage = std::mem::zeroed();
        libc::getrusage(libc::RUSAGE_SELF, &raw mut usage);
        usage
    };
    let micros = |value: libc::timeval| {
        u64::try_from(value.tv_sec).unwrap_or(0) * 1_000_000
            + u64::try_from(value.tv_usec).unwrap_or(0)
    };
    Duration::from_micros(micros(usage.ru_utime) + micros(usage.ru_stime))
}

fn measure<T>(work: impl FnOnce() -> T) -> (T, f64) {
    let before = cpu();
    let out = work();
    let spent = cpu().saturating_sub(before);
    (out, spent.as_secs_f64() * 1000.0)
}

fn median(mut samples: Vec<f64>) -> f64 {
    samples.sort_by(f64::total_cmp);
    samples[samples.len() / 2]
}

fn build(dir: &Path, copies: usize) -> Store {
    let mut store = Store::open(&StoreConfig {
        dir: dir.to_path_buf(),
        model_id: "feature-hash-256".to_owned(),
    })
    .expect("open");
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    for entry in std::fs::read_dir(&source).expect("src").flatten() {
        let Ok(text) = std::fs::read_to_string(entry.path()) else {
            continue;
        };
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
            let row = FileRow {
                mtime_ns: 1,
                size: 1,
                hash,
            };
            store
                .replace_file(&path, &row, &chunks, &vectors, &FileGraph::default())
                .expect("replace");
        }
    }
    store
}

fn read_rows(conn: &Connection) -> Vec<(i64, Vec<u8>)> {
    let mut statement = conn
        .prepare("SELECT c.id, v.vec FROM chunks c JOIN vectors v ON v.key = c.key WHERE substr(c.path, 1, ?1) = ?2")
        .expect("prepare");
    statement
        .query_map(params![0_i64, ""], |row| Ok((row.get(0)?, row.get(1)?)))
        .expect("query")
        .collect::<Result<Vec<_>, _>>()
        .expect("rows")
}

fn score(rows: &[(i64, Vec<u8>)], query: &[f32]) -> Vec<(i64, f32)> {
    rows.iter()
        .map(|(id, blob)| {
            let dot = blob
                .as_chunks::<4>()
                .0
                .iter()
                .zip(query)
                .map(|(bytes, weight)| f32::from_le_bytes(*bytes) * weight)
                .sum();
            (*id, dot)
        })
        .collect()
}

fn main() {
    let copies: usize = std::env::args()
        .nth(1)
        .and_then(|text| text.parse().ok())
        .unwrap_or(20);
    let scratch = std::env::var_os("TMPDIR").map_or_else(std::env::temp_dir, Into::into);
    let dir = scratch.join(format!("ss-scan-split-{}", std::process::id()));
    let store = build(&dir, copies);
    println!("chunks={}", store.chunk_count().expect("count"));
    let query = hash_embed(&["embed pool worker threads".to_owned()])
        .expect("embed")
        .remove(0);
    let conn = Connection::open(dir.join("index.db")).expect("conn");
    let (mut total, mut read, mut dots, mut select) = (vec![], vec![], vec![], vec![]);
    for _ in 0..REPEATS {
        total.push(measure(|| store.vector_search(&query, "", 50).expect("search")).1);
        let (rows, ms_read) = measure(|| read_rows(&conn));
        let (mut scored, ms_dot) = measure(|| score(&rows, &query));
        let ((), ms_select) = measure(|| {
            scored.sort_unstable_by(|left, right| right.1.total_cmp(&left.1));
            scored.truncate(50);
        });
        read.push(ms_read);
        dots.push(ms_dot);
        select.push(ms_select);
    }
    println!(
        "median cpu ms over {REPEATS}: total={:.2} read_copy={:.2} dot={:.2} topk={:.2}",
        median(total),
        median(read),
        median(dots),
        median(select)
    );
    std::fs::remove_dir_all(&dir).expect("cleanup");
}

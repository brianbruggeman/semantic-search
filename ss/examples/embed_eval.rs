//! Retrieval and vector-fidelity eval for embedding arms (provider x `max_length`) on the repo's own source.
//!
//! `embed_eval chunks`                       lists every corpus chunk with its token count
//! `embed_eval run <arm> <run> <out_dir>`    cold-indexes the corpus under one arm, writes `<arm>.<run>.run.json`
//!                                           (timings, per-query ranks) and `<arm>.<run>.vecs.json` (chunk vectors)
//! `embed_eval report <out_dir>`             compares every arm to the `cpu256` baseline and prints the table
//!
//! Arms: `cpu256`, `gpu256`, `all256`, `cpu128`, `gpu128`. The corpus is `tests/data/corpus` (a frozen copy of the
//! repo's `src` trees) and the gold list is `tests/data/queries.tsv`: `query<TAB>path<TAB>start-end<TAB>kind`.
// harness: aborts on any setup failure and divides counts into ratios for display
#![allow(
    clippy::expect_used,
    clippy::cast_precision_loss,
    clippy::cast_possible_wrap,
    clippy::too_many_lines
)]

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use proxima::pipe::Pipe;
use serde::{Deserialize, Serialize};
use ss::chunk::{Language, chunk_source};
use ss::embed::{
    DEFAULT_MODEL, EmbedPool, EmbedProfile, EmbedRequest, EmbedderKind, Lane, Provider,
    load_fastembed, parse_model, tokens,
};
use ss::index::{Shard, index_tree};
use ss::query::search;
use ss::store::{Store, StoreConfig};
use tokenizers::Tokenizer;

const BASELINE: &str = "cpu256";
const ARMS: [&str; 5] = ["cpu256", "gpu256", "all256", "cpu128", "gpu128"];
const LONG_TOKENS: usize = 128;
const DEPTH: usize = 50;

fn profile_of(arm: &str) -> EmbedProfile {
    let (spec, label) = arm.split_once('@').unwrap_or((arm, DEFAULT_MODEL));
    let model = parse_model(label).expect("known model label");
    let provider = match &spec[..3] {
        "gpu" => Provider::CoreMlGpu,
        "all" => Provider::CoreMlAll,
        _ => Provider::Cpu,
    };
    EmbedProfile {
        model,
        provider,
        max_length: spec[3..].parse().expect("arm max_length"),
    }
}

#[derive(Debug, Clone)]
struct Query {
    text: String,
    gold: String,
    kind: String,
}

fn corpus_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/corpus")
}

fn load_queries() -> Vec<Query> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/queries.tsv");
    std::fs::read_to_string(path)
        .expect("queries.tsv")
        .lines()
        .filter(|line| !line.trim().is_empty() && !line.starts_with('#'))
        .map(|line| {
            let cells: Vec<&str> = line.split('\t').collect();
            assert_eq!(cells.len(), 4, "bad query row: {line}");
            Query {
                text: cells[0].to_owned(),
                gold: format!("{}:{}", cells[1], cells[2]),
                kind: cells[3].to_owned(),
            }
        })
        .collect()
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).expect("create destination");
    for entry in std::fs::read_dir(from).expect("read source dir").flatten() {
        let target = to.join(entry.file_name());
        if entry.path().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).expect("copy file");
        }
    }
}

fn chunk_id(path: &str, start: u32, end: u32) -> String {
    format!("{path}:{start}-{end}")
}

fn snapshot_dir() -> PathBuf {
    let hub = dirs::home_dir()
        .map(|home| home.join(".cache/huggingface/hub/models--BAAI--bge-small-en-v1.5/snapshots"))
        .expect("home dir");
    std::fs::read_dir(hub)
        .expect("model snapshots")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| path.join("tokenizer.json").is_file())
        .expect("a snapshot with tokenizer.json")
}

fn corpus_chunks() -> Vec<(String, u32, u32, String)> {
    let root = corpus_dir();
    let mut found = Vec::new();
    for entry in ignore::WalkBuilder::new(&root).build().flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|ext| ext != "rs") {
            continue;
        }
        let rel = path
            .strip_prefix(&root)
            .expect("under root")
            .to_string_lossy()
            .into_owned();
        let text = std::fs::read_to_string(path).expect("read source");
        for chunk in chunk_source(Language::from_extension("rs"), &text) {
            found.push((rel.clone(), chunk.start_line, chunk.end_line, chunk.text));
        }
    }
    found.sort_by(|left, right| (&left.0, left.1).cmp(&(&right.0, right.1)));
    found
}

fn token_counts() -> HashMap<String, usize> {
    let tokenizer = Tokenizer::from_file(snapshot_dir().join("tokenizer.json")).expect("tokenizer");
    corpus_chunks()
        .into_iter()
        .map(|(path, start, end, text)| {
            let count = tokenizer.encode(text, true).expect("encode").len();
            (chunk_id(&path, start, end), count)
        })
        .collect()
}

fn list_chunks() {
    let counts = token_counts();
    for (path, start, end, text) in corpus_chunks() {
        let id = chunk_id(&path, start, end);
        let first = text.lines().next().unwrap_or_default();
        println!("{id}\t{}\t{first}", counts[&id]);
    }
}

fn uptime_load() -> String {
    let output = Command::new("uptime").output().expect("uptime");
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

#[derive(Serialize, Deserialize)]
struct QueryRun {
    text: String,
    gold: String,
    kind: String,
    hybrid_rank: usize,
    vector_rank: usize,
    fts_rank: usize,
    hybrid_top5: Vec<String>,
    vector_top5: Vec<String>,
}

#[derive(Serialize, Deserialize)]
struct RunRecord {
    arm: String,
    run: usize,
    uptime_before: String,
    model_load_s: f64,
    cold_index_s: f64,
    chunks: u64,
    embedded: u64,
    per_chunk_ms: f64,
    #[serde(default)]
    query_embed_ms: f64,
    #[serde(default)]
    dim: usize,
    queries: Vec<QueryRun>,
}

#[derive(Serialize, Deserialize)]
struct VectorRecord {
    id: String,
    vector: Vec<f32>,
}

fn rank_of(ids: &[String], gold: &str) -> usize {
    ids.iter()
        .position(|id| id == gold)
        .map_or(DEPTH + 1, |position| position + 1)
}

fn ids_of(store: &Store, rows: &[i64]) -> Vec<String> {
    rows.iter()
        .filter_map(|row| store.chunk(*row).expect("chunk row"))
        .map(|row| chunk_id(&row.path, row.start_line, row.end_line))
        .collect()
}

async fn run_arm(arm: &str, run: usize, out: &Path) {
    let profile = profile_of(arm);
    let queries = load_queries();
    let uptime_before = uptime_load();
    let cache = tempfile::tempdir().expect("model cache");
    let cache_path = std::env::var_os("SS_EVAL_MODEL_CACHE")
        .map_or_else(|| cache.path().to_path_buf(), PathBuf::from);

    let load_started = Instant::now();
    let model = load_fastembed(&cache_path, 2, profile).expect("model load");
    let model_load_s = load_started.elapsed().as_secs_f64();
    drop(model);

    let work = tempfile::tempdir_in(std::env::temp_dir()).expect("work dir");
    let root = work.path().join("tree");
    copy_dir(&corpus_dir(), &root);
    let config = StoreConfig {
        dir: work.path().join("shard"),
        model_id: profile.store_id(EmbedderKind::FastEmbed),
    };
    let pool = EmbedPool::with_lanes(1, 2, move || load_fastembed(&cache_path, 2, profile));
    let shard = Shard::new(&root, config.clone(), &pool);

    let started = Instant::now();
    let stats = index_tree(&shard, &root).await.expect("index");
    let cold_index_s = started.elapsed().as_secs_f64();

    let store = Store::open(&config).expect("open store");
    let known: HashSet<String> = corpus_chunks()
        .into_iter()
        .map(|(path, start, end, _)| chunk_id(&path, start, end))
        .collect();
    let mut query_runs = Vec::new();
    let mut query_embed_total = 0.0_f64;
    for query in &queries {
        let query_started = Instant::now();
        assert!(
            known.contains(&query.gold),
            "gold chunk missing: {}",
            query.gold
        );
        let embedding = pool
            .call(EmbedRequest {
                texts: vec![query.text.clone()],
                lane: Lane::Query,
            })
            .await
            .expect("embed query")
            .remove(0);
        query_embed_total += query_started.elapsed().as_secs_f64() * 1000.0;
        let by_vector = ids_of(
            &store,
            &store.vector_search(&embedding, "", DEPTH).expect("vector"),
        );
        let terms = tokens(&query.text);
        let by_text = ids_of(&store, &store.text_search(&terms, "", DEPTH).expect("fts"));
        let hits = search(&shard, &root, &query.text, DEPTH)
            .await
            .expect("search");
        let hybrid: Vec<String> = hits
            .iter()
            .map(|hit| {
                let rel = Path::new(&hit.path)
                    .strip_prefix(&root)
                    .expect("under root");
                chunk_id(&rel.to_string_lossy(), hit.start_line, hit.end_line)
            })
            .collect();
        query_runs.push(QueryRun {
            text: query.text.clone(),
            gold: query.gold.clone(),
            kind: query.kind.clone(),
            hybrid_rank: rank_of(&hybrid, &query.gold),
            vector_rank: rank_of(&by_vector, &query.gold),
            fts_rank: rank_of(&by_text, &query.gold),
            hybrid_top5: hybrid.iter().take(5).cloned().collect(),
            vector_top5: by_vector.iter().take(5).cloned().collect(),
        });
    }

    let vectors = read_vectors(&config);
    let record = RunRecord {
        arm: arm.to_owned(),
        run,
        uptime_before,
        model_load_s,
        cold_index_s,
        chunks: stats.chunks_seen,
        embedded: stats.chunks_embedded,
        per_chunk_ms: cold_index_s * 1000.0 / stats.chunks_embedded.max(1) as f64,
        query_embed_ms: query_embed_total / queries.len().max(1) as f64,
        dim: profile.dim(),
        queries: query_runs,
    };
    std::fs::create_dir_all(out).expect("out dir");
    std::fs::write(
        out.join(format!("{arm}.{run}.run.json")),
        serde_json::to_string(&record).expect("encode run"),
    )
    .expect("write run");
    std::fs::write(
        out.join(format!("{arm}.{run}.vecs.json")),
        serde_json::to_string(&vectors).expect("encode vectors"),
    )
    .expect("write vectors");
    println!(
        "arm={arm} run={run} chunks={} embedded={} model_load_s={model_load_s:.2} cold_index_s={cold_index_s:.2} per_chunk_ms={:.1} queries={} | {}",
        record.chunks,
        record.embedded,
        record.per_chunk_ms,
        record.queries.len(),
        record.uptime_before
    );
}

fn read_vectors(config: &StoreConfig) -> Vec<VectorRecord> {
    let conn = rusqlite::Connection::open(config.dir.join("index.db")).expect("open db");
    let mut statement = conn
        .prepare("SELECT c.path, c.start_line, c.end_line, v.vec FROM chunks c JOIN vectors v ON v.key = c.key ORDER BY c.path, c.start_line")
        .expect("prepare");
    statement
        .query_map([], |row| {
            let blob: Vec<u8> = row.get(3)?;
            let vector = blob
                .as_chunks::<4>()
                .0
                .iter()
                .map(|bytes| f32::from_le_bytes(*bytes))
                .collect();
            Ok(VectorRecord {
                id: chunk_id(&row.get::<_, String>(0)?, row.get(1)?, row.get(2)?),
                vector,
            })
        })
        .expect("query vectors")
        .collect::<Result<_, _>>()
        .expect("rows")
}

fn load_record(path: &Path) -> RunRecord {
    serde_json::from_str(&std::fs::read_to_string(path).expect("read run")).expect("parse run")
}

fn load_vectors(path: &Path) -> BTreeMap<String, Vec<f32>> {
    let records: Vec<VectorRecord> =
        serde_json::from_str(&std::fs::read_to_string(path).expect("read vecs"))
            .expect("parse vecs");
    records
        .into_iter()
        .map(|record| (record.id, record.vector))
        .collect()
}

fn cosine(left: &[f32], right: &[f32]) -> f64 {
    let dot: f32 = left.iter().zip(right).map(|(a, b)| a * b).sum();
    let norms =
        (left.iter().map(|a| a * a).sum::<f32>() * right.iter().map(|b| b * b).sum::<f32>()).sqrt();
    f64::from(dot / norms)
}

struct Quality {
    hit1: f64,
    hit3: f64,
    hit5: f64,
    mrr: f64,
}

fn quality(ranks: &[usize]) -> Quality {
    let total = ranks.len() as f64;
    let share = |limit: usize| ranks.iter().filter(|&&rank| rank <= limit).count() as f64 / total;
    Quality {
        hit1: share(1),
        hit3: share(3),
        hit5: share(5),
        mrr: ranks
            .iter()
            .map(|&rank| {
                if rank <= DEPTH {
                    1.0 / rank as f64
                } else {
                    0.0
                }
            })
            .sum::<f64>()
            / total,
    }
}

fn overlap(base: &[String], other: &[String]) -> f64 {
    let kept: HashSet<&String> = base.iter().collect();
    other.iter().filter(|id| kept.contains(id)).count() as f64 / 5.0
}

fn mean(values: &[f64]) -> f64 {
    values.iter().sum::<f64>() / values.len().max(1) as f64
}

fn report(out: &Path) {
    let counts = token_counts();
    let base_vectors = load_vectors(&out.join(format!("{BASELINE}.1.vecs.json")));
    let base_run = load_record(&out.join(format!("{BASELINE}.1.run.json")));
    assert!(!base_run.queries.is_empty(), "N==0 queries");
    println!(
        "N queries = {} ({} long-gold: gold chunk > {LONG_TOKENS} tokens), N chunks = {}",
        base_run.queries.len(),
        base_run
            .queries
            .iter()
            .filter(|q| counts[&q.gold] > LONG_TOKENS)
            .count(),
        base_vectors.len()
    );
    println!(
        "arm.run | load_avg_before | model_load_s | cold_index_s | per_chunk_ms | cos_mean | cos_min | top5ov_hyb | top5ov_vec"
    );
    for arm in ARMS {
        for run in 1..=3 {
            let name = format!("{arm}.{run}");
            if !out.join(format!("{name}.run.json")).is_file() {
                continue;
            }
            let record = load_record(&out.join(format!("{name}.run.json")));
            let vectors = load_vectors(&out.join(format!("{name}.vecs.json")));
            assert_eq!(
                vectors.len(),
                base_vectors.len(),
                "{name}: chunk set differs"
            );
            let cosines: Vec<f64> = base_vectors
                .iter()
                .map(|(id, base)| cosine(base, &vectors[id]))
                .collect();
            let hybrid: Vec<f64> = base_run
                .queries
                .iter()
                .zip(&record.queries)
                .map(|(b, o)| overlap(&b.hybrid_top5, &o.hybrid_top5))
                .collect();
            let vector: Vec<f64> = base_run
                .queries
                .iter()
                .zip(&record.queries)
                .map(|(b, o)| overlap(&b.vector_top5, &o.vector_top5))
                .collect();
            let load = record
                .uptime_before
                .split("load averages:")
                .nth(1)
                .unwrap_or("?")
                .trim()
                .to_owned();
            println!(
                "{name} | {load} | {:.2} | {:.2} | {:.1} | {:.6} | {:.6} | {:.3} | {:.3}",
                record.model_load_s,
                record.cold_index_s,
                record.per_chunk_ms,
                mean(&cosines),
                cosines.iter().copied().fold(f64::INFINITY, f64::min),
                mean(&hybrid),
                mean(&vector),
            );
        }
    }
    println!("\narm.run | scope | pipeline | hit@1 | hit@3 | hit@5 | MRR (rank>{DEPTH} counts 0)");
    for arm in ARMS {
        for run in 1..=3 {
            if !out.join(format!("{arm}.{run}.run.json")).is_file() {
                continue;
            }
            let record = load_record(&out.join(format!("{arm}.{run}.run.json")));
            for (scope, long_only) in [("all", false), ("long", true)] {
                let rows: Vec<&QueryRun> = record
                    .queries
                    .iter()
                    .filter(|q| !long_only || counts[&q.gold] > LONG_TOKENS)
                    .collect();
                for (label, pick) in [
                    (
                        "hybrid",
                        (|q: &QueryRun| q.hybrid_rank) as fn(&QueryRun) -> usize,
                    ),
                    ("vector", |q: &QueryRun| q.vector_rank),
                    ("fts", |q: &QueryRun| q.fts_rank),
                ] {
                    let ranks: Vec<usize> = rows.iter().map(|q| pick(q)).collect();
                    let measured = quality(&ranks);
                    println!(
                        "{arm}.{run} | {scope}(n={}) | {label} | {:.3} | {:.3} | {:.3} | {:.3}",
                        ranks.len(),
                        measured.hit1,
                        measured.hit3,
                        measured.hit5,
                        measured.mrr
                    );
                }
            }
        }
    }
    println!("\nper-query gold ranks, baseline run 1 (hybrid/vector/fts), tokens, kind");
    for query in &base_run.queries {
        println!(
            "{}/{}/{} | tok={} | {} | {} | {}",
            query.hybrid_rank,
            query.vector_rank,
            query.fts_rank,
            counts[&query.gold],
            query.kind,
            query.gold,
            query.text
        );
    }
}

#[derive(Clone, Copy)]
enum Half {
    Dev,
    Held,
    All,
}

fn start_line(gold: &str) -> u32 {
    let range = gold.rsplit(':').next().unwrap_or_default();
    range
        .split('-')
        .next()
        .and_then(|text| text.parse().ok())
        .expect("gold start line")
}

fn in_half(query: &QueryRun, half: Half) -> bool {
    match half {
        Half::Dev => !start_line(&query.gold).is_multiple_of(2),
        Half::Held => start_line(&query.gold).is_multiple_of(2),
        Half::All => true,
    }
}

fn quality_of(record: &RunRecord, half: Half, vector: bool) -> Quality {
    let ranks: Vec<usize> = record
        .queries
        .iter()
        .filter(|query| in_half(query, half))
        .map(|query| {
            if vector {
                query.vector_rank
            } else {
                query.hybrid_rank
            }
        })
        .collect();
    assert!(!ranks.is_empty(), "N==0 queries in half");
    quality(&ranks)
}

fn row(quality: &Quality) -> String {
    format!(
        "{:.3} {:.3} {:.3} {:.3}",
        quality.hit1, quality.hit3, quality.hit5, quality.mrr
    )
}

fn no_drop(base: &Quality, cand: &Quality) -> bool {
    cand.hit1 >= base.hit1
        && cand.hit3 >= base.hit3
        && cand.hit5 >= base.hit5
        && cand.mrr >= base.mrr
}

fn passes(base: &RunRecord, cand: &RunRecord, half: Half) -> bool {
    let base_hybrid = quality_of(base, half, false);
    let cand_hybrid = quality_of(cand, half, false);
    let improves = cand_hybrid.hit3 > base_hybrid.hit3 || cand_hybrid.mrr > base_hybrid.mrr;
    improves
        && no_drop(&base_hybrid, &cand_hybrid)
        && no_drop(&quality_of(base, half, true), &quality_of(cand, half, true))
}

fn model_report(out: &Path, arms: &[String]) {
    let records: Vec<RunRecord> = arms
        .iter()
        .map(|arm| load_record(&out.join(format!("{arm}.1.run.json"))))
        .collect();
    assert!(records.len() >= 2, "need a baseline and a candidate");
    for record in &records {
        println!(
            "{} | dim {} | load {:.2}s | cold {:.2}s | per_chunk {:.1}ms | query {:.1}ms | {}",
            record.arm,
            record.dim,
            record.model_load_s,
            record.cold_index_s,
            record.per_chunk_ms,
            record.query_embed_ms,
            record.uptime_before
        );
        for (name, half) in [
            ("dev", Half::Dev),
            ("held-out", Half::Held),
            ("all", Half::All),
        ] {
            println!(
                "  {name:8} hybrid {} | vector {}",
                row(&quality_of(record, half, false)),
                row(&quality_of(record, half, true))
            );
        }
    }
    for cand in &records[1..] {
        println!(
            "RULE {}: dev {} held-out {}",
            cand.arm,
            passes(&records[0], cand, Half::Dev),
            passes(&records[0], cand, Half::Held)
        );
        print_deltas(&records[0], cand);
    }
}

fn print_deltas(base: &RunRecord, cand: &RunRecord) {
    let mut deltas: Vec<(i64, &QueryRun, &QueryRun)> = base
        .queries
        .iter()
        .zip(&cand.queries)
        .map(|(before, after)| {
            (
                after.hybrid_rank as i64 - before.hybrid_rank as i64,
                before,
                after,
            )
        })
        .collect();
    deltas.sort_by_key(|delta| std::cmp::Reverse(delta.0.abs()));
    for (_, before, after) in deltas.iter().take(10) {
        println!(
            "  hybrid {}->{} vector {}->{} | {}",
            before.hybrid_rank,
            after.hybrid_rank,
            before.vector_rank,
            after.vector_rank,
            before.text
        );
    }
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("chunks") => list_chunks(),
        Some("run") => {
            let arm = args.get(2).expect("arm");
            let spec = arm.split('@').next().unwrap_or_default();
            assert!(ARMS.contains(&spec), "unknown arm {arm}");
            let run = args
                .get(3)
                .and_then(|text| text.parse().ok())
                .expect("run number");
            run_arm(arm, run, Path::new(args.get(4).expect("out dir"))).await;
        }
        Some("models") => model_report(Path::new(args.get(2).expect("out dir")), &args[3..]),
        Some("report") => report(Path::new(args.get(2).expect("out dir"))),
        _ => eprintln!("usage: embed_eval chunks | run <arm> <run> <out> | report <out>"),
    }
}

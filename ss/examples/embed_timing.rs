//! Timing harness for the real fastembed model: load vs tokenization vs per-batch inference.
//!
//! `cargo run --release --example embed_timing -- <threads> <batch> <max_length> <priority> <order>`
//! where `priority` is `min` (what `EmbedPool` workers set) or `normal`, and `order` is `file` or `sorted`.
//! Prints one `key=value` line per measurement so runs can be compared with the same command.
// timing harnesses abort on any setup failure and divide counts into milliseconds for display
#![allow(clippy::expect_used, clippy::cast_precision_loss)]

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use fastembed::{
    InitOptionsUserDefined, Pooling, QuantizationMode, TextEmbedding, TokenizerFiles,
    UserDefinedEmbeddingModel,
};
use ss::chunk::{Language, chunk_source};
use thread_priority::{ThreadPriority, set_current_thread_priority};
use tokenizers::{PaddingParams, PaddingStrategy, Tokenizer, TruncationParams};

const CORPUS_CHUNKS: usize = 256;
const QUERY_REPEATS: usize = 20;

struct Config {
    threads: usize,
    batch: usize,
    max_length: usize,
    low_priority: bool,
    sorted: bool,
    model: String,
}

fn arg<T: std::str::FromStr>(position: usize, default: T) -> T {
    std::env::args()
        .nth(position)
        .and_then(|text| text.parse().ok())
        .unwrap_or(default)
}

fn config() -> Config {
    Config {
        threads: arg(1, 2),
        batch: arg(2, 32),
        max_length: arg(3, 256),
        low_priority: arg(4, "min".to_owned()) == "min",
        sorted: arg(5, "file".to_owned()) == "sorted",
        model: arg(6, "model.onnx".to_owned()),
    }
}

fn millis(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
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

fn source_files(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for dir in ["ss/src", "ss-ipc/src", "ss-proto/src"] {
        let mut names: Vec<PathBuf> = std::fs::read_dir(root.join(dir))
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "rs"))
            .collect();
        names.sort();
        found.extend(names);
    }
    found
}

fn corpus() -> Vec<String> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    source_files(&root)
        .iter()
        .filter_map(|path| std::fs::read_to_string(path).ok())
        .flat_map(|text| chunk_source(Language::from_extension("rs"), &text))
        .map(|chunk| chunk.text)
        .take(CORPUS_CHUNKS)
        .collect()
}

fn read_files(dir: &Path, model_name: &str) -> (Vec<u8>, TokenizerFiles) {
    let read = |name: &str| std::fs::read(dir.join(name)).expect("model file");
    let files = TokenizerFiles {
        tokenizer_file: read("tokenizer.json"),
        config_file: read("config.json"),
        special_tokens_map_file: read("special_tokens_map.json"),
        tokenizer_config_file: read("tokenizer_config.json"),
    };
    (read(&format!("onnx/{model_name}")), files)
}

fn build_engine(dir: &Path, config: &Config) -> TextEmbedding {
    let (onnx, files) = read_files(dir, &config.model);
    let model = UserDefinedEmbeddingModel::new(onnx, files)
        .with_pooling(Pooling::Cls)
        .with_quantization(QuantizationMode::None);
    TextEmbedding::try_new_from_user_defined(
        model,
        InitOptionsUserDefined::new()
            .with_max_length(config.max_length)
            .with_intra_threads(config.threads),
    )
    .expect("engine")
}

fn standalone_tokenizer(dir: &Path, max_length: usize) -> Tokenizer {
    let mut tokenizer = Tokenizer::from_file(dir.join("tokenizer.json")).expect("tokenizer");
    tokenizer
        .with_truncation(Some(TruncationParams {
            max_length,
            ..TruncationParams::default()
        }))
        .expect("truncation");
    tokenizer.with_padding(Some(PaddingParams {
        strategy: PaddingStrategy::BatchLongest,
        ..PaddingParams::default()
    }));
    tokenizer
}

fn timed_tokenize(tokenizer: &Tokenizer, batches: &[Vec<String>]) -> (Duration, usize) {
    let started = Instant::now();
    let mut padded_tokens = 0;
    for batch in batches {
        let encoded = tokenizer
            .encode_batch(batch.clone(), true)
            .expect("encode batch");
        padded_tokens += encoded.iter().map(tokenizers::Encoding::len).sum::<usize>();
    }
    (started.elapsed(), padded_tokens)
}

fn timed_embed(engine: &mut TextEmbedding, batches: &[Vec<String>]) -> Vec<f64> {
    batches
        .iter()
        .map(|batch| {
            let started = Instant::now();
            engine.embed(batch, Some(batch.len())).expect("embed batch");
            millis(started.elapsed())
        })
        .collect()
}

fn median(values: &mut [f64]) -> f64 {
    values.sort_by(f64::total_cmp);
    values[values.len() / 2]
}

fn main() {
    let config = config();
    let mut texts = corpus();
    if config.sorted {
        texts.sort_by_key(String::len);
    }
    let dir = snapshot_dir();
    if config.low_priority {
        let _ = set_current_thread_priority(ThreadPriority::Min);
    }
    let read_started = Instant::now();
    let _ = read_files(&dir, &config.model);
    let read_ms = millis(read_started.elapsed());
    let load_started = Instant::now();
    let mut engine = build_engine(&dir, &config);
    let load_ms = millis(load_started.elapsed());
    let tokenizer_started = Instant::now();
    let tokenizer = standalone_tokenizer(&dir, config.max_length);
    let tokenizer_load_ms = millis(tokenizer_started.elapsed());

    let batches: Vec<Vec<String>> = texts.chunks(config.batch).map(<[String]>::to_vec).collect();
    let (tokenize, padded_tokens) = timed_tokenize(&tokenizer, &batches);
    let mut per_batch = timed_embed(&mut engine, &batches);
    let first_batch_ms = per_batch[0];
    let total_ms: f64 = per_batch.iter().sum();
    let steady_ms = total_ms - first_batch_ms;
    let steady_chunks = texts.len() - batches[0].len();

    let query = vec!["walk files respecting gitignore".to_owned()];
    let mut single: Vec<f64> = (0..QUERY_REPEATS)
        .map(|_| timed_embed(&mut engine, std::slice::from_ref(&query))[0])
        .collect();

    println!(
        "threads={} batch={} max_length={} priority={} order={} model={} chunks={} padded_tokens={padded_tokens}",
        config.threads,
        config.batch,
        config.max_length,
        if config.low_priority { "min" } else { "normal" },
        if config.sorted { "sorted" } else { "file" },
        config.model,
        texts.len(),
    );
    println!(
        "load: read_files_ms={read_ms:.1} try_new_ms={load_ms:.1} standalone_tokenizer_load_ms={tokenizer_load_ms:.1}"
    );
    println!(
        "tokenize: total_ms={:.1} per_chunk_ms={:.3}",
        millis(tokenize),
        millis(tokenize) / texts.len() as f64
    );
    println!(
        "embed: total_ms={total_ms:.1} per_chunk_ms={:.2} first_batch_ms={first_batch_ms:.1} steady_per_chunk_ms={:.2} median_batch_ms={:.1}",
        total_ms / texts.len() as f64,
        steady_ms / steady_chunks as f64,
        median(&mut per_batch),
    );
    println!(
        "single_query: median_ms={:.2} min_ms={:.2}",
        median(&mut single),
        single.first().copied().unwrap_or_default()
    );
}

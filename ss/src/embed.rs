use std::path::PathBuf;
use std::sync::Arc;
use std::thread;

use fastembed::{
    InitOptionsUserDefined, Pooling, QuantizationMode, TextEmbedding, TextInitOptions,
    TokenizerFiles, UserDefinedEmbeddingModel,
};
use proxima::pipe::Pipe;
use thread_priority::{ThreadPriority, set_current_thread_priority};
use tokio::sync::oneshot;

use crate::error::EmbedError;

pub type EmbedOutput = Result<Vec<Vec<f32>>, EmbedError>;

pub const HASH_DIM: usize = 256;
pub const FASTEMBED_DIM: usize = 384;
pub const EMBED_MAX_TOKENS: usize = 256;
/// Texts per model call. The tokenizer pads a call to its longest text and cost grows with padded tokens, so calls
/// stay small and are filled shortest-first (256 chunks, 2 threads: 147 ms/chunk at 16 unsorted, 100 at 8 sorted).
pub const EMBED_BATCH: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EmbedderKind {
    #[default]
    FastEmbed,
    Hash,
}

impl EmbedderKind {
    #[must_use]
    pub fn model_id(self) -> &'static str {
        match self {
            Self::FastEmbed => "bge-small-en-v1.5",
            Self::Hash => "feature-hash-256",
        }
    }

    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "fastembed" => Some(Self::FastEmbed),
            "hash" => Some(Self::Hash),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lane {
    Query,
    Index,
}

#[derive(Debug, Clone)]
pub struct EmbedRequest {
    pub texts: Vec<String>,
    pub lane: Lane,
}

struct Job {
    texts: Vec<String>,
    reply: oneshot::Sender<Result<Vec<Vec<f32>>, EmbedError>>,
}

/// Embedding worker threads, each owning its own model. `Pipe<In = EmbedRequest, Out = Vec<Vec<f32>>>`: the
/// embedder seam is a pipe, not a trait.
///
/// The two lanes never share a worker: one thread serves [`Lane::Query`] at normal priority, `workers` low-priority
/// threads serve [`Lane::Index`]. A search embeds its text in milliseconds, an index batch holds a worker for
/// seconds; with shared workers every search queued behind whichever batch was running (measured 2.1-2.4 s
/// instead of 16 ms, `examples/query_load.rs`).
#[derive(Clone)]
pub struct EmbedPool {
    query: flume::Sender<Job>,
    index: flume::Sender<Job>,
}

impl EmbedPool {
    /// One query thread and `workers` index threads; see [`EmbedPool::with_lanes`].
    pub fn spawn<Factory, Model>(workers: usize, factory: Factory) -> Self
    where
        Factory: Fn() -> Result<Model, EmbedError> + Send + Sync + 'static,
        Model: FnMut(&[String]) -> EmbedOutput + 'static,
    {
        Self::with_lanes(1, workers, factory)
    }

    /// Spawns `query_workers` query threads and `index_workers` index threads (each at least one); `factory`
    /// runs lazily on a thread's first job so idle daemons hold no model.
    pub fn with_lanes<Factory, Model>(
        query_workers: usize,
        index_workers: usize,
        factory: Factory,
    ) -> Self
    where
        Factory: Fn() -> Result<Model, EmbedError> + Send + Sync + 'static,
        Model: FnMut(&[String]) -> EmbedOutput + 'static,
    {
        let (query, query_rx) = flume::unbounded::<Job>();
        let (index, index_rx) = flume::unbounded::<Job>();
        let factory = Arc::new(factory);
        for number in 0..query_workers.max(1) {
            start_worker(Lane::Query, number, query_rx.clone(), &factory);
        }
        for number in 0..index_workers.max(1) {
            start_worker(Lane::Index, number, index_rx.clone(), &factory);
        }
        Self { query, index }
    }
}

fn start_worker<Factory, Model>(
    lane: Lane,
    number: usize,
    jobs: flume::Receiver<Job>,
    factory: &Arc<Factory>,
) where
    Factory: Fn() -> Result<Model, EmbedError> + Send + Sync + 'static,
    Model: FnMut(&[String]) -> EmbedOutput + 'static,
{
    let factory = Arc::clone(factory);
    let name = format!("ss-embed-{lane:?}-{number}").to_lowercase();
    let _ = thread::Builder::new()
        .name(name)
        .spawn(move || run_worker(lane, &jobs, factory.as_ref()));
}

fn run_worker<Factory, Model>(lane: Lane, jobs: &flume::Receiver<Job>, factory: &Factory)
where
    Factory: Fn() -> Result<Model, EmbedError>,
    Model: FnMut(&[String]) -> EmbedOutput,
{
    if lane == Lane::Index {
        let _ = set_current_thread_priority(ThreadPriority::Min);
    }
    let mut model: Option<Model> = None;
    while let Ok(job) = jobs.recv() {
        let outcome =
            ensure_model(&mut model, factory).and_then(|ready| embed_checked(ready, &job.texts));
        let _ = job.reply.send(outcome);
    }
}

fn ensure_model<'a, Factory, Model>(
    slot: &'a mut Option<Model>,
    factory: &Factory,
) -> Result<&'a mut Model, EmbedError>
where
    Factory: Fn() -> Result<Model, EmbedError>,
{
    if slot.is_none() {
        *slot = Some(factory()?);
    }
    slot.as_mut().ok_or(EmbedError::PoolClosed)
}

#[proxima::instrument]
fn embed_checked<Model>(model: &mut Model, texts: &[String]) -> EmbedOutput
where
    Model: FnMut(&[String]) -> EmbedOutput,
{
    let mut vectors = model(texts)?;
    if vectors.len() != texts.len() {
        return Err(EmbedError::CountMismatch {
            want: texts.len(),
            got: vectors.len(),
        });
    }
    for vector in &mut vectors {
        normalize(vector);
    }
    Ok(vectors)
}

impl Pipe for EmbedPool {
    type In = EmbedRequest;
    type Out = Vec<Vec<f32>>;
    type Err = EmbedError;

    fn call(&self, input: Self::In) -> impl Future<Output = Result<Self::Out, Self::Err>> {
        let lane = match input.lane {
            Lane::Query => &self.query,
            Lane::Index => &self.index,
        };
        let (reply, answer) = oneshot::channel();
        let sent = lane.send(Job {
            texts: input.texts,
            reply,
        });
        async move {
            sent.map_err(|_| EmbedError::PoolClosed)?;
            answer.await.map_err(|_| EmbedError::PoolClosed)?
        }
    }
}

pub fn normalize(vector: &mut [f32]) {
    let norm = vector.iter().map(|value| value * value).sum::<f32>().sqrt();
    if norm > f32::EPSILON {
        for value in vector.iter_mut() {
            *value /= norm;
        }
    }
}

/// Offline signed feature-hash embedding of identifier-ish tokens; no model download.
pub fn hash_embed(texts: &[String]) -> EmbedOutput {
    Ok(texts.iter().map(|text| hash_one(text)).collect())
}

fn hash_one(text: &str) -> Vec<f32> {
    let mut vector = vec![0.0_f32; HASH_DIM];
    for token in tokens(text) {
        let digest = blake3::hash(token.as_bytes());
        let bytes = digest.as_bytes();
        let slot = usize::from(u16::from_le_bytes([bytes[0], bytes[1]])) % HASH_DIM;
        vector[slot] += if bytes[2] & 1 == 0 { 1.0 } else { -1.0 };
    }
    vector
}

/// Lowercased alphanumeric runs, also split at camelCase humps.
#[must_use]
pub fn tokens(text: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut current = String::new();
    let mut previous_lower = false;
    for ch in text.chars() {
        if !ch.is_alphanumeric() {
            flush(&mut current, &mut found);
            previous_lower = false;
            continue;
        }
        if ch.is_uppercase() && previous_lower {
            flush(&mut current, &mut found);
        }
        previous_lower = ch.is_lowercase();
        current.extend(ch.to_lowercase());
    }
    flush(&mut current, &mut found);
    found
}

fn flush(current: &mut String, found: &mut Vec<String>) {
    if !current.is_empty() {
        found.push(std::mem::take(current));
    }
}

/// Builds the fastembed model: `SS_MODEL_DIR`, then a local Hugging Face snapshot, then a download.
pub fn load_fastembed(
    cache_dir: &std::path::Path,
    intra_threads: usize,
) -> Result<impl FnMut(&[String]) -> EmbedOutput + use<>, EmbedError> {
    let mut engine = match local_snapshot() {
        Some(snapshot) => from_snapshot(&snapshot, intra_threads)?,
        None => TextEmbedding::try_new(
            TextInitOptions::new(fastembed::EmbeddingModel::BGESmallENV15)
                .with_cache_dir(cache_dir.to_path_buf())
                .with_max_length(EMBED_MAX_TOKENS)
                .with_intra_threads(intra_threads)
                .with_show_download_progress(false),
        )
        .map_err(|error| EmbedError::Model(error.to_string()))?,
    };
    Ok(move |texts: &[String]| embed_by_length(&mut engine, texts))
}

/// Indices of `texts` grouped `size` at a time, shortest text first, so each group pads to nearly its own length.
fn length_ordered_groups(texts: &[String], size: usize) -> Vec<Vec<usize>> {
    let mut order: Vec<usize> = (0..texts.len()).collect();
    order.sort_by_key(|&position| texts[position].len());
    order.chunks(size.max(1)).map(<[usize]>::to_vec).collect()
}

fn embed_by_length(engine: &mut TextEmbedding, texts: &[String]) -> EmbedOutput {
    let mut vectors = vec![Vec::new(); texts.len()];
    for group in length_ordered_groups(texts, EMBED_BATCH) {
        let batch: Vec<&str> = group
            .iter()
            .map(|&position| texts[position].as_str())
            .collect();
        let embedded = engine
            .embed(&batch, Some(EMBED_BATCH))
            .map_err(|error| EmbedError::Model(error.to_string()))?;
        if embedded.len() != group.len() {
            return Err(EmbedError::CountMismatch {
                want: group.len(),
                got: embedded.len(),
            });
        }
        for (&position, vector) in group.iter().zip(embedded) {
            vectors[position] = vector;
        }
    }
    Ok(vectors)
}

fn local_snapshot() -> Option<PathBuf> {
    let explicit = std::env::var_os("SS_MODEL_DIR").map(PathBuf::from);
    let hub = std::env::var_os("HF_HOME")
        .map(|home| PathBuf::from(home).join("hub"))
        .or_else(|| dirs::home_dir().map(|home| home.join(".cache/huggingface/hub")))?;
    let snapshots = hub.join("models--BAAI--bge-small-en-v1.5/snapshots");
    let discovered = std::fs::read_dir(snapshots)
        .ok()?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| {
            path.join("onnx/model.onnx").is_file() && path.join("tokenizer.json").is_file()
        });
    explicit.or(discovered)
}

fn from_snapshot(dir: &std::path::Path, intra_threads: usize) -> Result<TextEmbedding, EmbedError> {
    let read = |name: &str| {
        std::fs::read(dir.join(name)).map_err(|error| EmbedError::Model(format!("{name}: {error}")))
    };
    let files = TokenizerFiles {
        tokenizer_file: read("tokenizer.json")?,
        config_file: read("config.json")?,
        special_tokens_map_file: read("special_tokens_map.json")?,
        tokenizer_config_file: read("tokenizer_config.json")?,
    };
    let model = UserDefinedEmbeddingModel::new(read("onnx/model.onnx")?, files)
        .with_pooling(Pooling::Cls)
        .with_quantization(QuantizationMode::None);
    TextEmbedding::try_new_from_user_defined(
        model,
        InitOptionsUserDefined::new()
            .with_max_length(EMBED_MAX_TOKENS)
            .with_intra_threads(intra_threads),
    )
    .map_err(|error| EmbedError::Model(error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_split_snake_and_camel_case() {
        assert_eq!(
            tokens("walkFiles respecting_gitignore"),
            vec!["walk", "files", "respecting", "gitignore"]
        );
    }

    #[test]
    fn hash_vectors_are_unit_length_and_share_signal_for_shared_tokens() {
        let mut near = hash_embed(&[
            "walk files gitignore".to_owned(),
            "walk files ignore".to_owned(),
        ])
        .expect("hash embed");
        for vector in &mut near {
            normalize(vector);
        }

        let similarity: f32 = near[0]
            .iter()
            .zip(&near[1])
            .map(|(left, right)| left * right)
            .sum();
        assert!(similarity > 0.3, "similarity {similarity}");
    }

    #[test]
    fn groups_fill_shortest_first_and_cover_every_text_once() {
        let texts: Vec<String> = ["cccc", "a", "bbb", "dddddd", "ee"]
            .iter()
            .map(|text| (*text).to_owned())
            .collect();

        let groups = length_ordered_groups(&texts, 2);

        assert_eq!(groups, vec![vec![1, 4], vec![2, 0], vec![3]]);
    }

    #[proxima::test(runtime = "tokio")]
    async fn pool_returns_normalized_vectors_for_both_lanes() {
        let pool = EmbedPool::spawn(2, || Ok(hash_embed));
        for lane in [Lane::Query, Lane::Index] {
            let vectors = pool
                .call(EmbedRequest {
                    texts: vec!["fn walk() {}".to_owned()],
                    lane,
                })
                .await
                .expect("embed");

            let length: f32 = vectors[0]
                .iter()
                .map(|value| value * value)
                .sum::<f32>()
                .sqrt();
            assert_eq!((vectors.len(), vectors[0].len()), (1, HASH_DIM));
            assert!((length - 1.0).abs() < 1e-4, "length {length}");
        }
    }

    #[proxima::test(runtime = "tokio")]
    async fn a_query_is_embedded_while_every_index_worker_is_busy() {
        let (entered_tx, entered_rx) = flume::unbounded::<()>();
        let (release_tx, release_rx) = flume::unbounded::<()>();
        let pool = EmbedPool::spawn(1, move || {
            let (entered, release) = (entered_tx.clone(), release_rx.clone());
            Ok(move |texts: &[String]| {
                if texts.iter().any(|text| text.contains("slow index batch")) {
                    let _ = entered.send(());
                    let _ = release.recv();
                }
                hash_embed(texts)
            })
        });
        let busy = pool.clone();
        let index_batch = tokio::spawn(async move {
            busy.call(EmbedRequest {
                texts: vec!["slow index batch".to_owned()],
                lane: Lane::Index,
            })
            .await
        });
        entered_rx
            .recv_async()
            .await
            .expect("index worker entered the batch");

        let answered = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            pool.call(EmbedRequest {
                texts: vec!["walk files".to_owned()],
                lane: Lane::Query,
            }),
        )
        .await;
        release_tx.send(()).expect("release the index batch");

        assert!(
            answered.is_ok_and(|vectors| vectors.is_ok()),
            "the query waited behind the index batch"
        );
        assert!(index_batch.await.expect("index task").is_ok());
    }

    #[proxima::test(runtime = "tokio")]
    async fn model_count_mismatch_is_an_error_not_a_silent_truncation() {
        let pool = EmbedPool::spawn(1, || Ok(|_: &[String]| Ok(Vec::new())));

        let outcome = pool
            .call(EmbedRequest {
                texts: vec!["x".to_owned()],
                lane: Lane::Index,
            })
            .await;

        assert_eq!(outcome, Err(EmbedError::CountMismatch { want: 1, got: 0 }));
    }

    #[proxima::test(runtime = "tokio")]
    async fn real_fastembed_model_ranks_the_walker_above_unrelated_code() {
        let cache = tempfile::tempdir().expect("model cache dir");
        let cache_path = cache.path().to_path_buf();
        let pool = EmbedPool::spawn(1, move || load_fastembed(&cache_path, 2));
        let walker = "/// files honoring .gitignore\npub fn list_files(root: &Path) -> Vec<PathBuf> {\n    WalkBuilder::new(root).require_git(false).build().collect()\n}".to_owned();
        let unrelated = "fn parse_http_header(line: &str) -> Option<(String, String)> {\n    let (name, value) = line.split_once(':')?;\n    Some((name.trim().to_owned(), value.trim().to_owned()))\n}".to_owned();
        let query = "walk files respecting gitignore".to_owned();

        let vectors = pool
            .call(EmbedRequest {
                texts: vec![walker.clone(), unrelated, query],
                lane: Lane::Index,
            })
            .await
            .expect("real embeddings");

        let cosine =
            |left: &[f32], right: &[f32]| left.iter().zip(right).map(|(a, b)| a * b).sum::<f32>();
        let (near, far) = (
            cosine(&vectors[2], &vectors[0]),
            cosine(&vectors[2], &vectors[1]),
        );
        println!(
            "real model: dim={} near={near:.3} far={far:.3}",
            vectors[0].len()
        );
        let alone = pool
            .call(EmbedRequest {
                texts: vec![walker],
                lane: Lane::Index,
            })
            .await
            .expect("single embedding");
        let same = cosine(&alone[0], &vectors[0]);
        assert_eq!(vectors[0].len(), FASTEMBED_DIM);
        assert!(near > far, "near={near} far={far}");
        assert!(same > 0.999, "grouping changed the vector: cosine {same}");
    }
}

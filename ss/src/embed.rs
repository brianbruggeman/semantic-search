use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;

use fastembed::{
    ExecutionProviderDispatch, InitOptionsUserDefined, Pooling, QuantizationMode, TextEmbedding,
    TextInitOptions, TokenizerFiles, UserDefinedEmbeddingModel,
};
#[cfg(target_os = "macos")]
use ort::ep::CoreML;
#[cfg(target_os = "macos")]
use ort::ep::coreml::{ComputeUnits, ModelFormat};
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

/// Where the ONNX model runs. Vectors differ slightly between providers, so the choice is part of the store identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Provider {
    #[default]
    Cpu,
    CoreMlGpu,
    CoreMlAll,
}

impl Provider {
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Cpu => "cpu",
            Self::CoreMlGpu => "coreml",
            Self::CoreMlAll => "coreml_all",
        }
    }

    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "cpu" => Some(Self::Cpu),
            "coreml" => Some(Self::CoreMlGpu),
            "coreml_all" => Some(Self::CoreMlAll),
            _ => None,
        }
    }
}

/// The model-side knobs that change the vectors: execution provider and token truncation length.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EmbedProfile {
    pub provider: Provider,
    pub max_length: usize,
}

impl Default for EmbedProfile {
    fn default() -> Self {
        Self {
            provider: Provider::Cpu,
            max_length: EMBED_MAX_TOKENS,
        }
    }
}

impl EmbedProfile {
    /// The identity a store pins: a vector index built under one id is wiped, not mixed, when opened under another.
    /// The hash embedder has no provider or truncation, so its id is just the model.
    #[must_use]
    pub fn store_id(self, kind: EmbedderKind) -> String {
        match kind {
            EmbedderKind::Hash => kind.model_id().to_owned(),
            EmbedderKind::FastEmbed => format!(
                "{}+{}+len{}",
                kind.model_id(),
                self.provider.name(),
                self.max_length
            ),
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

const WARM_TEXT: &str = "warm up";

/// Warm-up bookkeeping shared by a pool's workers: how many have loaded their model and run one embed.
struct Warmth {
    enabled: bool,
    query_total: usize,
    query_ready: AtomicUsize,
    index_ready: AtomicUsize,
}

/// What a warming worker reports to: the shared counters and the channel a waiter drains.
struct WarmReport {
    warmth: Arc<Warmth>,
    done: flume::Sender<Result<(), EmbedError>>,
}

impl WarmReport {
    fn finish(&self, lane: Lane, outcome: Result<(), EmbedError>) {
        if outcome.is_ok() {
            let counter = match lane {
                Lane::Query => &self.warmth.query_ready,
                Lane::Index => &self.warmth.index_ready,
            };
            counter.fetch_add(1, Ordering::Release);
        }
        let _ = self.done.send(outcome);
    }
}

/// Embedding worker threads, each owning its own model. `Pipe<In = EmbedRequest, Out = Vec<Vec<f32>>>`: the
/// embedder seam is a pipe, not a trait.
///
/// The two lanes never share a worker: `query_workers` threads serve [`Lane::Query`] at normal priority,
/// `index_workers` low-priority threads serve [`Lane::Index`]. A search embeds its text in milliseconds, an index
/// batch holds a worker for seconds; with shared workers every search queued behind whichever batch was running
/// (measured 2.1-2.4 s instead of 16 ms, `examples/query_load.rs`).
///
/// Models load lazily on a worker's first job unless the pool is built with [`EmbedPool::with_lanes_warm`], which
/// loads every worker's model on its own thread at once and runs one short embed through it.
#[derive(Clone)]
pub struct EmbedPool {
    query: flume::Sender<Job>,
    index: flume::Sender<Job>,
    warmth: Arc<Warmth>,
    query_warmed: flume::Receiver<Result<(), EmbedError>>,
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
        Self::with_warm_start(query_workers, index_workers, false, factory)
    }

    /// Like [`EmbedPool::with_lanes`] but every worker calls `factory` and embeds one short text immediately, in
    /// parallel on its own thread. [`EmbedPool::warm_query`] resolves once the query workers are done.
    pub fn with_lanes_warm<Factory, Model>(
        query_workers: usize,
        index_workers: usize,
        factory: Factory,
    ) -> Self
    where
        Factory: Fn() -> Result<Model, EmbedError> + Send + Sync + 'static,
        Model: FnMut(&[String]) -> EmbedOutput + 'static,
    {
        Self::with_warm_start(query_workers, index_workers, true, factory)
    }

    /// [`EmbedPool::with_lanes`] when `warm` is false, [`EmbedPool::with_lanes_warm`] when true; for callers that
    /// pick from a setting.
    pub fn with_warm_start<Factory, Model>(
        query_workers: usize,
        index_workers: usize,
        warm: bool,
        factory: Factory,
    ) -> Self
    where
        Factory: Fn() -> Result<Model, EmbedError> + Send + Sync + 'static,
        Model: FnMut(&[String]) -> EmbedOutput + 'static,
    {
        let (query, query_rx) = flume::unbounded::<Job>();
        let (index, index_rx) = flume::unbounded::<Job>();
        let (query_done, query_warmed) = flume::unbounded();
        let (index_done, _index_warmed) = flume::unbounded();
        let warmth = Arc::new(Warmth {
            enabled: warm,
            query_total: query_workers.max(1),
            query_ready: AtomicUsize::new(0),
            index_ready: AtomicUsize::new(0),
        });
        let factory = Arc::new(factory);
        let report = |done: &flume::Sender<_>| {
            warm.then(|| WarmReport {
                warmth: Arc::clone(&warmth),
                done: done.clone(),
            })
        };
        for number in 0..warmth.query_total {
            start_worker(
                Lane::Query,
                number,
                query_rx.clone(),
                &factory,
                report(&query_done),
            );
        }
        for number in 0..index_workers.max(1) {
            start_worker(
                Lane::Index,
                number,
                index_rx.clone(),
                &factory,
                report(&index_done),
            );
        }
        Self {
            query,
            index,
            warmth,
            query_warmed,
        }
    }

    /// True once every query worker has loaded its model and run its warm-up embed; always false for a lazy pool.
    #[must_use]
    pub fn is_warm(&self) -> bool {
        self.warmth.enabled
            && self.warmth.query_ready.load(Ordering::Acquire) >= self.warmth.query_total
    }

    /// Index workers that finished warming; they may trail [`EmbedPool::is_warm`].
    #[must_use]
    pub fn index_warmed(&self) -> usize {
        self.warmth.index_ready.load(Ordering::Acquire)
    }

    /// Resolves when every query worker has reported: `Ok` once all are warm, the first load error otherwise, and
    /// `PoolClosed` if a worker thread died before reporting. A lazy pool resolves at once.
    pub async fn warm_query(&self) -> Result<(), EmbedError> {
        if !self.warmth.enabled {
            return Ok(());
        }
        for _ in 0..self.warmth.query_total {
            self.query_warmed
                .recv_async()
                .await
                .map_err(|_| EmbedError::PoolClosed)??;
        }
        Ok(())
    }
}

fn start_worker<Factory, Model>(
    lane: Lane,
    number: usize,
    jobs: flume::Receiver<Job>,
    factory: &Arc<Factory>,
    report: Option<WarmReport>,
) where
    Factory: Fn() -> Result<Model, EmbedError> + Send + Sync + 'static,
    Model: FnMut(&[String]) -> EmbedOutput + 'static,
{
    let factory = Arc::clone(factory);
    let name = format!("ss-embed-{lane:?}-{number}").to_lowercase();
    let _ = thread::Builder::new()
        .name(name)
        .spawn(move || run_worker(lane, &jobs, factory.as_ref(), report.as_ref()));
}

fn run_worker<Factory, Model>(
    lane: Lane,
    jobs: &flume::Receiver<Job>,
    factory: &Factory,
    report: Option<&WarmReport>,
) where
    Factory: Fn() -> Result<Model, EmbedError>,
    Model: FnMut(&[String]) -> EmbedOutput,
{
    if lane == Lane::Index {
        let _ = set_current_thread_priority(ThreadPriority::Min);
    }
    let mut model: Option<Model> = None;
    if let Some(report) = report {
        report.finish(lane, warm_up(&mut model, factory));
    }
    while let Ok(job) = jobs.recv() {
        let outcome =
            ensure_model(&mut model, factory).and_then(|ready| embed_checked(ready, &job.texts));
        let _ = job.reply.send(outcome);
    }
}

fn warm_up<Factory, Model>(slot: &mut Option<Model>, factory: &Factory) -> Result<(), EmbedError>
where
    Factory: Fn() -> Result<Model, EmbedError>,
    Model: FnMut(&[String]) -> EmbedOutput,
{
    let ready = ensure_model(slot, factory)?;
    embed_checked(ready, &[WARM_TEXT.to_owned()]).map(|_| ())
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
    profile: EmbedProfile,
) -> Result<impl FnMut(&[String]) -> EmbedOutput + use<>, EmbedError> {
    let providers = execution_providers(profile.provider);
    let mut engine = match local_snapshot() {
        Some(snapshot) => from_snapshot(&snapshot, intra_threads, profile.max_length, providers)?,
        None => TextEmbedding::try_new(
            TextInitOptions::new(fastembed::EmbeddingModel::BGESmallENV15)
                .with_cache_dir(cache_dir.to_path_buf())
                .with_max_length(profile.max_length)
                .with_intra_threads(intra_threads)
                .with_execution_providers(providers)
                .with_show_download_progress(false),
        )
        .map_err(|error| EmbedError::Model(error.to_string()))?,
    };
    Ok(move |texts: &[String]| embed_by_length(&mut engine, texts))
}

#[cfg(target_os = "macos")]
fn execution_providers(provider: Provider) -> Vec<ExecutionProviderDispatch> {
    let units = match provider {
        Provider::Cpu => return Vec::new(),
        Provider::CoreMlGpu => ComputeUnits::CPUAndGPU,
        Provider::CoreMlAll => ComputeUnits::All,
    };
    vec![
        CoreML::default()
            .with_compute_units(units)
            .with_model_format(ModelFormat::MLProgram)
            .build(),
    ]
}

/// CoreML exists only on macos; elsewhere the model runs on the CPU provider.
#[cfg(not(target_os = "macos"))]
fn execution_providers(_provider: Provider) -> Vec<ExecutionProviderDispatch> {
    Vec::new()
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

fn from_snapshot(
    dir: &std::path::Path,
    intra_threads: usize,
    max_length: usize,
    providers: Vec<ExecutionProviderDispatch>,
) -> Result<TextEmbedding, EmbedError> {
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
            .with_max_length(max_length)
            .with_intra_threads(intra_threads)
            .with_execution_providers(providers),
    )
    .map_err(|error| EmbedError::Model(error.to_string()))
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;

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

    fn counting_factory(
        loads: &Arc<AtomicUsize>,
        delay: std::time::Duration,
    ) -> impl Fn() -> Result<fn(&[String]) -> EmbedOutput, EmbedError> + Send + Sync + 'static {
        let loads = Arc::clone(loads);
        move || {
            thread::sleep(delay);
            loads.fetch_add(1, Ordering::SeqCst);
            Ok(hash_embed as fn(&[String]) -> EmbedOutput)
        }
    }

    fn request(lane: Lane) -> EmbedRequest {
        EmbedRequest {
            texts: vec!["walk files".to_owned()],
            lane,
        }
    }

    #[proxima::test(runtime = "tokio")]
    async fn readiness_waits_for_the_query_workers_to_finish_loading() {
        let loads = Arc::new(AtomicUsize::new(0));
        let delay = std::time::Duration::from_millis(150);
        let pool = EmbedPool::with_lanes_warm(2, 1, counting_factory(&loads, delay));

        let warm_before = pool.is_warm();
        pool.warm_query().await.expect("query workers warm");

        assert!(!warm_before, "reported warm before any model loaded");
        assert!(pool.is_warm());
        assert!(
            loads.load(Ordering::SeqCst) >= 2,
            "both query models loaded before readiness"
        );
    }

    #[proxima::test(runtime = "tokio")]
    async fn searches_after_readiness_never_call_the_model_factory_again() {
        let loads = Arc::new(AtomicUsize::new(0));
        let pool =
            EmbedPool::with_lanes_warm(2, 2, counting_factory(&loads, std::time::Duration::ZERO));
        pool.warm_query().await.expect("query workers warm");
        while pool.index_warmed() < 2 {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }

        for _ in 0..6 {
            pool.call(request(Lane::Query)).await.expect("query embed");
            pool.call(request(Lane::Index)).await.expect("index embed");
        }

        assert_eq!(
            loads.load(Ordering::SeqCst),
            4,
            "one load per worker: 2 query + 2 index"
        );
    }

    #[proxima::test(runtime = "tokio")]
    async fn a_model_load_failure_surfaces_as_an_error_not_a_hang() {
        let pool = EmbedPool::with_lanes_warm(1, 1, || {
            Err::<fn(&[String]) -> EmbedOutput, _>(EmbedError::Model("no weights".to_owned()))
        });

        let outcome =
            tokio::time::timeout(std::time::Duration::from_secs(5), pool.warm_query()).await;

        assert!(
            matches!(outcome, Ok(Err(EmbedError::Model(_)))),
            "{outcome:?}"
        );
        assert!(!pool.is_warm());
    }

    #[proxima::test(runtime = "tokio")]
    async fn a_lazy_pool_is_not_warm_and_loads_on_first_use() {
        let loads = Arc::new(AtomicUsize::new(0));
        let pool = EmbedPool::with_lanes(1, 1, counting_factory(&loads, std::time::Duration::ZERO));

        pool.warm_query().await.expect("lazy pool resolves at once");
        let loaded_before = loads.load(Ordering::SeqCst);
        pool.call(request(Lane::Query)).await.expect("query embed");

        assert_eq!(
            (loaded_before, pool.is_warm(), loads.load(Ordering::SeqCst)),
            (0, false, 1)
        );
    }

    fn cpu_micros() -> u64 {
        let mut usage = std::mem::MaybeUninit::<libc::rusage>::zeroed();
        // SAFETY: getrusage fills the zeroed rusage it is given; RUSAGE_SELF is always valid.
        let usage = unsafe {
            libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr());
            usage.assume_init()
        };
        let micros = |time: libc::timeval| time.tv_sec.unsigned_abs() * 1_000_000 + u64::from(time.tv_usec.unsigned_abs());
        micros(usage.ru_utime) + micros(usage.ru_stime)
    }

    #[proxima::test(runtime = "tokio")]
    async fn real_model_first_query_after_warm_up_costs_within_3x_the_tenth_in_cpu_time() {
        let cache = tempfile::tempdir().expect("model cache dir");
        let cache_path = cache.path().to_path_buf();
        let pool = EmbedPool::with_lanes_warm(1, 1, move || {
            load_fastembed(&cache_path, 2, EmbedProfile::default())
        });
        pool.warm_query().await.expect("query worker warm");
        let mut spent = Vec::new();
        for _ in 0..10 {
            let before = cpu_micros();
            pool.call(request(Lane::Query)).await.expect("query embed");
            spent.push(cpu_micros() - before);
        }

        println!("cpu us first={} tenth={}", spent[0], spent[9]);
        assert!(
            spent[0] <= 3 * spent[9].max(1_000),
            "first {} tenth {}",
            spent[0],
            spent[9]
        );
    }

    #[proxima::test(runtime = "tokio")]
    async fn real_fastembed_model_ranks_the_walker_above_unrelated_code() {
        let cache = tempfile::tempdir().expect("model cache dir");
        let cache_path = cache.path().to_path_buf();
        let pool = EmbedPool::spawn(1, move || {
            load_fastembed(&cache_path, 2, EmbedProfile::default())
        });
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

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::Duration;

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
use crate::activity::{Activity, ActivityLease};

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

pub const DEFAULT_MODEL: &str = "bge-small-en-v1.5";

/// The fastembed models `SS_EMBED_MODEL` can select, by label. The label is what the store identity records.
pub const MODELS: [(&str, fastembed::EmbeddingModel); 8] = [
    (
        "bge-small-en-v1.5",
        fastembed::EmbeddingModel::BGESmallENV15,
    ),
    ("bge-base-en-v1.5", fastembed::EmbeddingModel::BGEBaseENV15),
    (
        "jina-embeddings-v2-base-code",
        fastembed::EmbeddingModel::JinaEmbeddingsV2BaseCode,
    ),
    (
        "jina-embeddings-v2-base-en",
        fastembed::EmbeddingModel::JinaEmbeddingsV2BaseEN,
    ),
    (
        "nomic-embed-text-v1.5",
        fastembed::EmbeddingModel::NomicEmbedTextV15,
    ),
    ("gte-base-en-v1.5", fastembed::EmbeddingModel::GTEBaseENV15),
    (
        "snowflake-arctic-embed-s",
        fastembed::EmbeddingModel::SnowflakeArcticEmbedS,
    ),
    (
        "snowflake-arctic-embed-m",
        fastembed::EmbeddingModel::SnowflakeArcticEmbedM,
    ),
];

/// The model-side knobs that change the vectors: model, execution provider and token truncation length.
/// `model` is a label from [`MODELS`]; the vector dimension comes from the model, see [`EmbedProfile::dim`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EmbedProfile {
    pub model: &'static str,
    pub provider: Provider,
    pub max_length: usize,
}

impl Default for EmbedProfile {
    fn default() -> Self {
        Self {
            model: DEFAULT_MODEL,
            provider: Provider::Cpu,
            max_length: EMBED_MAX_TOKENS,
        }
    }
}

/// The label of a known model; `None` for a name outside [`MODELS`].
#[must_use]
pub fn parse_model(name: &str) -> Option<&'static str> {
    MODELS
        .iter()
        .map(|(label, _)| *label)
        .find(|label| label.eq_ignore_ascii_case(name.trim()))
}

impl EmbedProfile {
    /// The fastembed model this profile loads; an unknown label falls back to the default model.
    #[must_use]
    pub fn embedding_model(self) -> fastembed::EmbeddingModel {
        MODELS
            .iter()
            .find(|(label, _)| *label == self.model)
            .map_or_else(fastembed::EmbeddingModel::default, |(_, model)| {
                model.clone()
            })
    }

    /// Vector length, read from the model's own info so no dimension is hard-coded.
    #[must_use]
    pub fn dim(self) -> usize {
        TextEmbedding::get_model_info(&self.embedding_model())
            .map_or(FASTEMBED_DIM, |info| info.dim)
    }

    /// The identity a store pins: a vector index built under one id is wiped, not mixed, when opened under another.
    /// The hash embedder has no provider or truncation, so its id is just the model.
    #[must_use]
    pub fn store_id(self, kind: EmbedderKind) -> String {
        match kind {
            EmbedderKind::Hash => kind.model_id().to_owned(),
            EmbedderKind::FastEmbed => {
                let provider = match self.provider {
                    Provider::Cpu => "cpu-explicit",
                    provider => provider.name(),
                };
                format!(
                    "{}+{}+len{}",
                    self.model,
                    provider,
                    self.max_length
                )
            }
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
    _activity: ActivityLease,
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
    fn set_ready(&self, lane: Lane, ready: bool) {
        let counter = match lane {
            Lane::Query => &self.warmth.query_ready,
            Lane::Index => &self.warmth.index_ready,
        };
        if ready {
            counter.fetch_add(1, Ordering::Release);
        } else {
            counter.fetch_sub(1, Ordering::Release);
        }
    }

    fn finish(&self, lane: Lane, outcome: Result<(), EmbedError>) {
        if outcome.is_ok() {
            self.set_ready(lane, true);
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
    activity: Arc<Activity>,
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
        Self::with_idle_timeout(query_workers, index_workers, warm, None, factory)
    }

    /// Releases models after an idle interval; `None` retains them and zero releases when the queue empties.
    pub fn with_idle_timeout<Factory, Model>(
        query_workers: usize,
        index_workers: usize,
        warm: bool,
        idle_timeout: Option<Duration>,
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
        let activity = Arc::new(Activity::default());
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
                idle_timeout,
                &activity,
            );
        }
        for number in 0..index_workers.max(1) {
            start_worker(
                Lane::Index,
                number,
                index_rx.clone(),
                &factory,
                report(&index_done),
                idle_timeout,
                &activity,
            );
        }
        Self {
            query,
            index,
            warmth,
            query_warmed,
            activity,
        }
    }

    /// True while every eagerly warmed query worker has a ready model; false after expiry or for a lazy pool.
    #[must_use]
    pub fn is_warm(&self) -> bool {
        self.warmth.enabled
            && self.warmth.query_ready.load(Ordering::Acquire) >= self.warmth.query_total
    }

    /// Eagerly warmed index workers whose models have not expired.
    #[must_use]
    pub fn index_warmed(&self) -> usize {
        self.warmth.index_ready.load(Ordering::Acquire)
    }

    #[must_use]
    pub fn is_idle(&self) -> bool {
        self.activity.quiet_epoch().is_some()
    }

    pub(crate) fn activity(&self) -> Arc<Activity> {
        Arc::clone(&self.activity)
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
    idle_timeout: Option<Duration>,
    activity: &Arc<Activity>,
) where
    Factory: Fn() -> Result<Model, EmbedError> + Send + Sync + 'static,
    Model: FnMut(&[String]) -> EmbedOutput + 'static,
{
    let factory = Arc::clone(factory);
    let activity = Arc::clone(activity);
    let name = format!("ss-embed-{lane:?}-{number}").to_lowercase();
    let _ = thread::Builder::new()
        .name(name)
        .spawn(move || run_worker(lane, &jobs, factory.as_ref(), report.as_ref(), idle_timeout, &activity));
}

fn run_worker<Factory, Model>(
    lane: Lane,
    jobs: &flume::Receiver<Job>,
    factory: &Factory,
    report: Option<&WarmReport>,
    idle_timeout: Option<Duration>,
    activity: &Arc<Activity>,
) where
    Factory: Fn() -> Result<Model, EmbedError>,
    Model: FnMut(&[String]) -> EmbedOutput,
{
    if lane == Lane::Index {
        let _ = set_current_thread_priority(ThreadPriority::Min);
    }
    let mut model: Option<Model> = None;
    let mut ready = false;
    if let Some(report) = report {
        let warming = activity.begin();
        let outcome = warm_up(&mut model, factory);
        ready = outcome.is_ok();
        drop(warming);
        report.finish(lane, outcome);
    }
    loop {
        let incoming = match (model.is_some(), idle_timeout) {
            (true, Some(timeout)) => jobs.recv_timeout(timeout),
            _ => jobs
                .recv()
                .map_err(|_| flume::RecvTimeoutError::Disconnected),
        };
        let job = match incoming {
            Ok(job) => job,
            Err(flume::RecvTimeoutError::Timeout) => {
                if ready {
                    if let Some(report) = report {
                        report.set_ready(lane, false);
                    }
                    ready = false;
                }
                model = None;
                proxima::telemetry::debug!(lane = ?lane, "embedding model released because worker is idle");
                continue;
            }
            Err(flume::RecvTimeoutError::Disconnected) => break,
        };
        let Job {
            texts,
            reply,
            _activity: lease,
        } = job;
        if reply.is_closed() {
            continue;
        }
        if model.is_none() {
            proxima::telemetry::debug!(lane = ?lane, "embedding model loading because work arrived");
        }
        let outcome = ensure_model(&mut model, factory).and_then(|ready| embed_checked(ready, &texts));
        if !ready && outcome.is_ok() {
            if let Some(report) = report {
                report.set_ready(lane, true);
            }
            ready = true;
        }
        // End the lease before publishing, so a caller holding the answer is never counted as active work
        drop(lease);
        let _ = reply.send(outcome);
    }
    if ready && let Some(report) = report {
        report.set_ready(lane, false);
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
            _activity: self.activity.begin(),
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

/// Builds the fastembed model: for the default model `SS_MODEL_DIR`, then a local Hugging Face snapshot; otherwise (or
/// failing both) a download into `cache_dir`.
pub fn load_fastembed(
    cache_dir: &std::path::Path,
    intra_threads: usize,
    profile: EmbedProfile,
) -> Result<impl FnMut(&[String]) -> EmbedOutput + use<>, EmbedError> {
    let providers = execution_providers(profile.provider);
    let snapshot = (profile.model == DEFAULT_MODEL)
        .then(local_snapshot)
        .flatten();
    let mut engine = match snapshot {
        Some(snapshot) => from_snapshot(&snapshot, intra_threads, profile.max_length, providers)?,
        None => TextEmbedding::try_new(
            TextInitOptions::new(profile.embedding_model())
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

    use conflaguration::Settings as _;

    use super::*;
    use crate::daemon::configured_pool;
    use crate::settings::Settings;

    #[test]
    fn every_listed_model_reports_its_dimension_from_fastembed() {
        let dims: Vec<(&str, usize)> = MODELS
            .iter()
            .map(|(label, _)| {
                (
                    *label,
                    EmbedProfile {
                        model: label,
                        ..EmbedProfile::default()
                    }
                    .dim(),
                )
            })
            .collect();

        assert_eq!(dims[0], ("bge-small-en-v1.5", FASTEMBED_DIM));
        assert_eq!(dims[2], ("jina-embeddings-v2-base-code", 768));
        assert!(dims.iter().all(|(_, dim)| *dim >= 384), "{dims:?}");
    }

    #[test]
    fn model_names_parse_case_insensitively_and_unknown_names_are_none() {
        assert_eq!(
            parse_model("Jina-Embeddings-V2-Base-Code"),
            Some("jina-embeddings-v2-base-code")
        );
        assert_eq!(parse_model("nope"), None);
    }

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
    async fn cost_cancel_abandoned_job_never_enters_the_model() {
        let (entered, observing) = flume::unbounded::<String>();
        let (release, held) = flume::bounded::<()>(0);
        let pool = EmbedPool::spawn(1, move || {
            let entered = entered.clone();
            let held = held.clone();
            Ok(move |texts: &[String]| {
                let label = texts.first().cloned().expect("one text per fixture job");
                entered.send(label.clone()).expect("record model call");
                if label == "hold" {
                    held.recv().expect("release active model call");
                }
                hash_embed(texts)
            })
        });
        let active_pool = pool.clone();
        let active = tokio::spawn(async move {
            active_pool
                .call(EmbedRequest {
                    texts: vec!["hold".to_owned()],
                    lane: Lane::Index,
                })
                .await
        });
        assert_eq!(observing.recv_async().await.expect("active call entered"), "hold");

        let abandoned = pool.call(EmbedRequest {
            texts: vec!["abandoned".to_owned()],
            lane: Lane::Index,
        });
        drop(abandoned);
        release.send(()).expect("release active model call");
        active.await.expect("active task").expect("active embedding");
        pool.call(EmbedRequest {
            texts: vec!["next".to_owned()],
            lane: Lane::Index,
        })
        .await
        .expect("later live embedding");

        assert_eq!(observing.recv_async().await.expect("later model call"), "next");
        assert!(observing.try_recv().is_err(), "abandoned call reached model");
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

    fn lifetime_model() -> (impl FnMut(&[String]) -> EmbedOutput, flume::Receiver<()>) {
        let (lifetime, ended) = flume::bounded::<()>(0);
        let model = move |texts: &[String]| {
            let _keep_alive = &lifetime;
            Ok(vec![vec![1.0, 0.0]; texts.len()])
        };
        (model, ended)
    }

    async fn assert_model_released(vectors: Vec<Vec<f32>>, ended: flume::Receiver<()>) {
        assert_eq!(vectors, vec![vec![1.0, 0.0]]);
        let outcome = tokio::time::timeout(Duration::from_secs(5), ended.recv_async())
            .await
            .expect("model must drop without another request");
        assert!(outcome.is_err(), "the model-owned sender must disconnect");
    }

    async fn assert_idle_lane(lane: Lane) {
        let loads = Arc::new(AtomicUsize::new(0));
        let observed_loads = Arc::clone(&loads);
        let (lifetimes, observations) = flume::unbounded();
        let pool = EmbedPool::with_idle_timeout(1, 1, false, Some(Duration::ZERO), move || {
            let (model, ended) = lifetime_model();
            loads.fetch_add(1, Ordering::SeqCst);
            lifetimes
                .send(ended)
                .expect("lifetime observer remains connected");
            Ok(model)
        });

        for _ in 0..2 {
            let vectors = pool.call(request(lane)).await.expect("embed after expiry");
            let ended = observations.recv_async().await.expect("model lifetime");
            assert_model_released(vectors, ended).await;
        }

        assert_eq!(observed_loads.load(Ordering::SeqCst), 2);
    }

    #[proxima::test(runtime = "tokio")]
    async fn idle_model_query_releases_and_reloads() {
        assert_idle_lane(Lane::Query).await;
    }

    #[proxima::test(runtime = "tokio")]
    async fn idle_model_index_releases_and_reloads() {
        assert_idle_lane(Lane::Index).await;
    }

    async fn assert_positive_idle_lane(lane: Lane) {
        let loads = Arc::new(AtomicUsize::new(0));
        let observed_loads = Arc::clone(&loads);
        let (lifetimes, observations) = flume::unbounded();
        let (entered, running) = flume::unbounded();
        let (release, continue_work) = flume::unbounded();
        let pool =
            EmbedPool::with_idle_timeout(1, 1, false, Some(Duration::from_secs(1)), move || {
                let (mut model, ended) = lifetime_model();
                let entered = entered.clone();
                let continue_work = continue_work.clone();
                loads.fetch_add(1, Ordering::SeqCst);
                lifetimes
                    .send(ended)
                    .expect("positive interval lifetime observer");
                Ok(move |texts: &[String]| {
                    if texts.first().is_some_and(|text| text == "hold") {
                        entered.send(()).expect("busy model observer");
                        continue_work.recv().expect("release busy model");
                    }
                    model(texts)
                })
            });
        assert_eq!(
            pool.call(request(lane)).await.expect("first model use"),
            vec![vec![1.0, 0.0]]
        );
        let ended = observations
            .recv_async()
            .await
            .expect("first model lifetime");
        let busy_pool = pool.clone();
        let busy = tokio::spawn(async move {
            busy_pool
                .call(EmbedRequest {
                    texts: vec!["hold".to_owned()],
                    lane,
                })
                .await
        });
        running
            .recv_async()
            .await
            .expect("second job entered model");
        let during_work =
            tokio::time::timeout(Duration::from_millis(1200), ended.recv_async()).await;
        release.send(()).expect("finish second job");
        let vectors = busy
            .await
            .expect("second job task")
            .expect("second model use");
        assert!(
            during_work.is_err(),
            "active work must retain its model past the idle interval"
        );
        assert_eq!(observed_loads.load(Ordering::SeqCst), 1);
        assert!(
            tokio::time::timeout(Duration::from_millis(400), ended.recv_async())
                .await
                .is_err(),
            "idle interval must restart after the second job completes"
        );
        assert_model_released(vectors, ended).await;

        let vectors = pool
            .call(request(lane))
            .await
            .expect("reload after positive interval");
        let ended = observations
            .recv_async()
            .await
            .expect("reloaded model lifetime");
        assert_model_released(vectors, ended).await;
        assert_eq!(observed_loads.load(Ordering::SeqCst), 2);
    }

    #[proxima::test(runtime = "tokio")]
    async fn idle_model_query_positive_interval_retains_busy_work_and_resets() {
        assert_positive_idle_lane(Lane::Query).await;
    }

    #[proxima::test(runtime = "tokio")]
    async fn idle_model_index_positive_interval_retains_busy_work_and_resets() {
        assert_positive_idle_lane(Lane::Index).await;
    }

    #[proxima::test(runtime = "tokio")]
    async fn idle_oracle_direct_ownership_uses_the_same_release_assertions() {
        let (mut model, ended) = lifetime_model();
        let vectors = model(&request(Lane::Query).texts).expect("constant vector oracle");
        drop(model);
        assert_model_released(vectors, ended).await;
    }

    #[proxima::test(runtime = "tokio")]
    async fn idle_readiness_clears_after_warmed_model_expires() {
        let (lifetimes, observations) = flume::unbounded();
        let pool =
            EmbedPool::with_idle_timeout(1, 1, true, Some(Duration::from_secs(1)), move || {
                let (model, ended) = lifetime_model();
                lifetimes
                    .send(ended)
                    .expect("lifetime observer remains connected");
                Ok(model)
            });
        pool.warm_query().await.expect("warm completion report");
        assert!(pool.is_warm(), "query warm-up sets readiness");
        for _ in 0..2 {
            let ended = observations.recv_async().await.expect("both models warmed");
            assert_model_released(vec![vec![1.0, 0.0]], ended).await;
        }
        assert!(!pool.is_warm());
        assert_eq!(pool.index_warmed(), 0);

        let vectors = pool.call(request(Lane::Query)).await.expect("query reload");
        assert!(
            pool.is_warm(),
            "a successful query reload restores readiness"
        );
        let ended = observations
            .recv_async()
            .await
            .expect("query model reloaded");
        assert_model_released(vectors, ended).await;
        assert!(!pool.is_warm());
        let vectors = pool.call(request(Lane::Index)).await.expect("index reload");
        assert_eq!(pool.index_warmed(), 1, "index reload restores readiness");
        let ended = observations
            .recv_async()
            .await
            .expect("index model reloaded");
        assert_model_released(vectors, ended).await;
        assert_eq!(pool.index_warmed(), 0);
    }

    #[proxima::test(runtime = "tokio")]
    async fn idle_disabled_existing_constructor_reuses_its_model() {
        let loads = Arc::new(AtomicUsize::new(0));
        let observed_loads = Arc::clone(&loads);
        let pool = EmbedPool::with_lanes(1, 1, move || {
            loads.fetch_add(1, Ordering::SeqCst);
            Ok(|texts: &[String]| Ok(vec![vec![1.0, 0.0]; texts.len()]))
        });

        for _ in 0..2 {
            assert_eq!(
                pool.call(request(Lane::Query)).await.expect("reused model"),
                vec![vec![1.0, 0.0]]
            );
        }
        assert_eq!(observed_loads.load(Ordering::SeqCst), 1);
    }

    #[proxima::test(runtime = "tokio")]
    async fn idle_settings_defaults_and_zero_expiry_reach_the_daemon_pool() {
        let defaults = temp_env::with_vars(
            [
                ("SS_WARM_START", None::<&str>),
                ("SS_EMBED_IDLE_SECS", None),
            ],
            || Settings::from_env().expect("default idle settings"),
        );
        assert!(!defaults.warm_start);
        assert_eq!(defaults.embed_idle_secs, 30);
        let settings = temp_env::with_vars(
            [
                ("SS_WARM_START", Some("true")),
                ("SS_EMBED_IDLE_SECS", Some("0")),
                ("SS_EMBEDDER", Some("hash")),
                ("SS_EMBED_WORKERS", Some("1")),
                ("SS_EMBED_QUERY_WORKERS", Some("1")),
            ],
            || Settings::from_env().expect("configured idle settings"),
        );
        assert_eq!(settings.embed_idle_secs, 0);
        let (lifetimes, observations) = flume::unbounded();
        let pool = configured_pool(&settings, move || {
            let (model, ended) = lifetime_model();
            lifetimes.send(ended).expect("configured lifetime observer");
            Ok(model)
        });
        pool.warm_query().await.expect("warm daemon pool");
        for _ in 0..2 {
            let ended = observations
                .recv_async()
                .await
                .expect("configured warm model");
            assert_model_released(vec![vec![1.0, 0.0]], ended).await;
        }
        for lane in [Lane::Query, Lane::Index] {
            let vectors = pool
                .call(request(lane))
                .await
                .expect("configured model reload");
            let ended = observations
                .recv_async()
                .await
                .expect("configured lane model");
            assert_model_released(vectors, ended).await;
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

    #[cfg(unix)]
    fn cpu_micros() -> u64 {
        let mut usage = std::mem::MaybeUninit::<libc::rusage>::zeroed();
        // SAFETY: getrusage fills the zeroed rusage it is given; RUSAGE_SELF is always valid.
        let usage = unsafe {
            libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr());
            usage.assume_init()
        };
        let micros = |time: libc::timeval| {
            time.tv_sec.unsigned_abs() * 1_000_000 + u64::from(time.tv_usec.unsigned_abs())
        };
        micros(usage.ru_utime) + micros(usage.ru_stime)
    }

    #[cfg(unix)]
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
        assert_eq!(vectors[0].len(), EmbedProfile::default().dim());
        assert!(near > far, "near={near} far={far}");
        assert!(same > 0.999, "grouping changed the vector: cosine {same}");
    }
}

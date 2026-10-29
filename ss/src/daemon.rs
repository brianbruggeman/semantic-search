use std::collections::HashMap;
use std::convert::Infallible;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use proxima::pipe::Pipe;
use proxima::telemetry::{info, warn};
use ss_ipc::{InstanceLock, ListenerExt as _, Stream, listen};
use ss_proto::{
    DaemonStatus, ErrorKind, Frame, Hit, IndexStats, LineDecoder, Request, Response, encode_frame,
};
use thiserror::Error;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{Mutex, Notify, mpsc};

use crate::admit::{Admit, AdmitError, AdmitGauge};
use crate::embed::{EmbedPool, EmbedderKind, hash_embed, load_fastembed};
use crate::error::IndexError;
use crate::index::{Shard, index_changed, index_tree};
use crate::query::search;
use crate::registry::{Registry, WatchChange};
use crate::settings::Settings;
use crate::watch::{Batch, WatchHub};

const READ_CHUNK: usize = 8192;
const DRAIN_GRACE: Duration = Duration::from_secs(2);

#[derive(Debug, Error)]
pub enum ServeError {
    #[error("{0} is not indexed; run `ss index <path>` first")]
    NotIndexed(String),
    #[error("bad request: {0}")]
    BadRequest(String),
    #[error(transparent)]
    Index(#[from] IndexError),
    #[error("state failure: {0}")]
    State(#[from] std::io::Error),
}

impl ServeError {
    fn kind(&self) -> ErrorKind {
        match self {
            Self::NotIndexed(_) => ErrorKind::NotIndexed,
            Self::BadRequest(_) => ErrorKind::BadRequest,
            Self::Index(_) | Self::State(_) => ErrorKind::Internal,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum RunOutcome {
    AlreadyRunning,
    Stopped,
}

#[derive(Debug, Default)]
struct Totals {
    seen: AtomicU64,
    embedded: AtomicU64,
    skipped: AtomicU64,
}

struct Entry {
    shard: Shard,
    writer: Mutex<()>,
}

/// State shared by every connection: one set of indexes, one registry, one embed pool.
pub struct Core {
    settings: Settings,
    hub: WatchHub,
    pool: EmbedPool,
    registry: Mutex<Registry>,
    shards: Mutex<HashMap<PathBuf, Arc<Entry>>>,
    totals: Totals,
    started: Instant,
    shutdown: Notify,
    clients: AtomicUsize,
    last_activity_ms: AtomicU64,
}

impl Core {
    fn new(settings: Settings, pool: EmbedPool, registry: Registry) -> Self {
        Self {
            settings,
            hub: WatchHub::default(),
            pool,
            registry: Mutex::new(registry),
            shards: Mutex::default(),
            totals: Totals::default(),
            started: Instant::now(),
            shutdown: Notify::new(),
            clients: AtomicUsize::new(0),
            last_activity_ms: AtomicU64::new(0),
        }
    }

    fn touch(&self) {
        let elapsed = u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX);
        self.last_activity_ms.store(elapsed, Ordering::Relaxed);
    }

    fn idle_for(&self) -> Duration {
        let now = u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX);
        Duration::from_millis(now.saturating_sub(self.last_activity_ms.load(Ordering::Relaxed)))
    }

    async fn entry(&self, root: &Path) -> Arc<Entry> {
        let mut shards = self.shards.lock().await;
        let settings = &self.settings;
        let pool = &self.pool;
        Arc::clone(shards.entry(root.to_path_buf()).or_insert_with(|| {
            Arc::new(Entry {
                shard: Shard::new(root, settings.store_config(root), pool),
                writer: Mutex::new(()),
            })
        }))
    }

    fn record(&self, stats: &IndexStats) {
        crate::telemetry::record_index_default(stats);
        info!(
            "index run complete",
            chunks_seen = stats.chunks_seen,
            chunks_embedded = stats.chunks_embedded,
            chunks_skipped = stats.chunks_skipped,
            files_indexed = stats.files_indexed
        );
        self.totals
            .seen
            .fetch_add(stats.chunks_seen, Ordering::Relaxed);
        self.totals
            .embedded
            .fetch_add(stats.chunks_embedded, Ordering::Relaxed);
        self.totals
            .skipped
            .fetch_add(stats.chunks_skipped, Ordering::Relaxed);
    }

    async fn index_path(&self, path: &Path) -> Result<IndexStats, ServeError> {
        let canonical = canonical_dir(path)?;
        let root = self.root_for_index(&canonical).await?;
        let entry = self.entry(&root).await;
        let _serialized = entry.writer.lock().await;
        let stats = index_tree(&entry.shard, &canonical).await?;
        self.record(&stats);
        Ok(stats)
    }

    async fn root_for_index(&self, path: &Path) -> Result<PathBuf, ServeError> {
        let mut registry = self.registry.lock().await;
        if let Some(root) = registry.resolve(path) {
            return Ok(root.to_path_buf());
        }
        let absorbed = registry.add_root(path);
        registry.save(&self.settings.state_path())?;
        drop(registry);
        self.forget_shards(&absorbed).await;
        Ok(path.to_path_buf())
    }

    async fn forget_shards(&self, roots: &[PathBuf]) {
        let mut shards = self.shards.lock().await;
        for root in roots {
            shards.remove(root);
            let _ = std::fs::remove_dir_all(self.settings.store_config(root).dir);
        }
    }

    async fn search_path(
        &self,
        query: &str,
        path: &str,
        limit: usize,
    ) -> Result<Vec<Hit>, ServeError> {
        let scope = std::fs::canonicalize(path).unwrap_or_else(|_| PathBuf::from(path));
        let root = self
            .registry
            .lock()
            .await
            .resolve(&scope)
            .map(Path::to_path_buf);
        let root = root.ok_or_else(|| ServeError::NotIndexed(scope.display().to_string()))?;
        let entry = self.entry(&root).await;
        Ok(search(&entry.shard, &scope, query, limit).await?)
    }

    async fn status(&self, search: &AdmitGauge, index: &AdmitGauge) -> DaemonStatus {
        let registry = self.registry.lock().await;
        let paths = |list: &[PathBuf]| list.iter().map(|path| path.display().to_string()).collect();
        DaemonStatus {
            pid: std::process::id(),
            uptime_secs: self.started.elapsed().as_secs(),
            roots: paths(&registry.roots),
            watches: paths(&registry.watches),
            active_watchers: self.hub.count().await as u64,
            queries_running: search.running() as u64,
            queries_waiting: search.waiting() as u64,
            queries_busy: search.rejected(),
            index_busy: index.rejected(),
            chunks_seen: self.totals.seen.load(Ordering::Relaxed),
            chunks_embedded: self.totals.embedded.load(Ordering::Relaxed),
            chunks_skipped: self.totals.skipped.load(Ordering::Relaxed),
        }
    }
}

impl Core {
    pub(crate) fn settings(&self) -> &Settings {
        &self.settings
    }

    pub(crate) async fn root_of(&self, path: &Path) -> Option<PathBuf> {
        self.registry
            .lock()
            .await
            .resolve(path)
            .map(Path::to_path_buf)
    }

    /// Indexes one debounced batch of watcher hints under the root's single-writer lock.
    pub(crate) async fn apply_batch(
        &self,
        watch: &Path,
        batch: Batch,
    ) -> Result<IndexStats, ServeError> {
        let root = self
            .root_of(watch)
            .await
            .unwrap_or_else(|| watch.to_path_buf());
        let entry = self.entry(&root).await;
        let _serialized = entry.writer.lock().await;
        let stats = if batch.rescan {
            index_tree(&entry.shard, watch).await?
        } else {
            index_changed(&entry.shard, batch.paths.into_iter().collect()).await?
        };
        self.record(&stats);
        Ok(stats)
    }

    /// Registers `path` as a watched root and starts its watcher; repeats and nested paths change nothing.
    async fn watch_add(self: &Arc<Self>, path: &Path) -> Result<Vec<String>, ServeError> {
        let canonical = canonical_dir(path)?;
        let (change, absorbed_roots) = {
            let mut registry = self.registry.lock().await;
            let absorbed_roots = if registry.resolve(&canonical).is_some() {
                Vec::new()
            } else {
                registry.add_root(&canonical)
            };
            let change = registry.add_watch(&canonical);
            registry.save(&self.settings.state_path())?;
            (change, absorbed_roots)
        };
        self.forget_shards(&absorbed_roots).await;
        if let WatchChange::Added { absorbed } = change {
            for nested in &absorbed {
                self.hub.stop(nested).await;
            }
            self.hub.start(self, &canonical).await.map_err(|error| {
                ServeError::BadRequest(format!("cannot watch {}: {error}", canonical.display()))
            })?;
        }
        Ok(self.watch_list().await)
    }

    async fn watch_rm(&self, path: &Path) -> Result<Vec<String>, ServeError> {
        let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        let removed = {
            let mut registry = self.registry.lock().await;
            let removed = registry.rm_watch(&canonical);
            registry.save(&self.settings.state_path())?;
            removed
        };
        if !removed {
            return Err(ServeError::BadRequest(format!(
                "{} is not watched",
                canonical.display()
            )));
        }
        self.hub.stop(&canonical).await;
        Ok(self.watch_list().await)
    }

    async fn watch_list(&self) -> Vec<String> {
        let registry = self.registry.lock().await;
        registry
            .watches
            .iter()
            .map(|path| path.display().to_string())
            .collect()
    }

    /// Daemon start: every registered watch gets its watcher back, and its startup rescan is the first batch.
    async fn resume_watches(self: &Arc<Self>) {
        let watches = self.registry.lock().await.watches.clone();
        for path in watches {
            if let Err(error) = self.hub.start(self, &path).await {
                warn!("cannot resume watch", path = %path.display(), error = %error);
            }
        }
    }
}

fn canonical_dir(path: &Path) -> Result<PathBuf, ServeError> {
    std::fs::canonicalize(path)
        .map_err(|error| ServeError::BadRequest(format!("{}: {error}", path.display())))
}

pub struct SearchArgs {
    pub query: String,
    pub path: String,
    pub limit: usize,
}

/// `SearchArgs -> Vec<Hit>`: the hybrid query against whichever root contains the path.
pub struct SearchPipe {
    core: Arc<Core>,
}

impl Pipe for SearchPipe {
    type In = SearchArgs;
    type Out = Vec<Hit>;
    type Err = ServeError;

    async fn call(&self, input: Self::In) -> Result<Self::Out, Self::Err> {
        self.core
            .search_path(&input.query, &input.path, input.limit)
            .await
    }
}

/// `String -> IndexStats`: index (or re-index) a directory, creating its root on first sight.
pub struct IndexPipe {
    core: Arc<Core>,
}

impl Pipe for IndexPipe {
    type In = String;
    type Out = IndexStats;
    type Err = ServeError;

    async fn call(&self, input: Self::In) -> Result<Self::Out, Self::Err> {
        self.core.index_path(Path::new(&input)).await
    }
}

/// `Request -> Response`: every client call enters the arena here, through the admission gates.
pub struct Router {
    core: Arc<Core>,
    search: Admit<SearchPipe>,
    index: Admit<IndexPipe>,
}

impl Router {
    fn new(core: &Arc<Core>) -> Self {
        let settings = &core.settings;
        Self {
            core: Arc::clone(core),
            search: Admit::new(
                SearchPipe {
                    core: Arc::clone(core),
                },
                settings.query_cap,
                settings.query_depth,
            ),
            index: Admit::new(
                IndexPipe {
                    core: Arc::clone(core),
                },
                settings.index_cap,
                settings.index_depth,
            ),
        }
    }
}

impl Pipe for Router {
    type In = Request;
    type Out = Response;
    type Err = Infallible;

    async fn call(&self, input: Self::In) -> Result<Self::Out, Self::Err> {
        Ok(match input {
            Request::Search { query, path, limit } => {
                self.core.touch();
                let args = SearchArgs {
                    query,
                    path,
                    limit: limit as usize,
                };
                respond(
                    self.search
                        .call(args)
                        .await
                        .map(|hits| Response::Hits { hits }),
                )
            }
            Request::Index { path } => respond(
                self.index
                    .call(path)
                    .await
                    .map(|stats| Response::Indexed { stats }),
            ),
            Request::Status => Response::Status {
                status: self
                    .core
                    .status(&self.search.gauge(), &self.index.gauge())
                    .await,
            },
            Request::Stop => {
                self.core.shutdown.notify_one();
                Response::Ok
            }
            Request::WatchAdd { path } => watch_reply(self.core.watch_add(Path::new(&path)).await),
            Request::WatchRm { path } => watch_reply(self.core.watch_rm(Path::new(&path)).await),
            Request::WatchList => Response::Watches {
                paths: self.core.watch_list().await,
            },
        })
    }
}

fn watch_reply(outcome: Result<Vec<String>, ServeError>) -> Response {
    match outcome {
        Ok(paths) => Response::Watches { paths },
        Err(error) => Response::Error {
            error: error.kind(),
            message: error.to_string(),
        },
    }
}

fn respond(outcome: Result<Response, AdmitError<ServeError>>) -> Response {
    match outcome {
        Ok(response) => response,
        Err(AdmitError::Busy) => Response::Error {
            error: ErrorKind::Busy,
            message: "busy: all slots taken and the wait queue is full".to_owned(),
        },
        Err(AdmitError::Inner(error)) => Response::Error {
            error: error.kind(),
            message: error.to_string(),
        },
    }
}

/// The embed pool a settings value asks for.
#[must_use]
pub fn build_pool(settings: &Settings) -> EmbedPool {
    match settings.embedder_kind() {
        EmbedderKind::Hash => EmbedPool::spawn(settings.embed_workers, || Ok(hash_embed)),
        EmbedderKind::FastEmbed => {
            let (cache, threads) = (settings.model_cache_dir(), settings.embed_threads);
            EmbedPool::spawn(settings.embed_workers, move || {
                load_fastembed(&cache, threads)
            })
        }
    }
}

/// Runs the daemon until `Stop` or idle exit. A second instance returns [`RunOutcome::AlreadyRunning`] at once.
pub async fn run(settings: Settings, pool: Option<EmbedPool>) -> Result<RunOutcome, ServeError> {
    std::fs::create_dir_all(settings.home_dir())?;
    let Some(_instance) = InstanceLock::acquire(&settings.lock_path())? else {
        return Ok(RunOutcome::AlreadyRunning);
    };
    let listener = listen(&settings.endpoint())?;
    let registry = Registry::load(&settings.state_path())?;
    let pool = pool.unwrap_or_else(|| build_pool(&settings));
    let core = Arc::new(Core::new(settings, pool, registry));
    let router = Arc::new(Router::new(&core));
    core.resume_watches().await;
    tokio::spawn(idle_monitor(Arc::clone(&core)));
    loop {
        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok(stream) => { tokio::spawn(serve_connection(Arc::clone(&core), Arc::clone(&router), stream)); }
                Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
            },
            () = core.shutdown.notified() => break,
        }
    }
    drop(listener);
    drain(&core).await;
    Ok(RunOutcome::Stopped)
}

async fn drain(core: &Core) {
    let deadline = Instant::now() + DRAIN_GRACE;
    while core.clients.load(Ordering::Relaxed) > 0 && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn idle_monitor(core: Arc<Core>) {
    let idle = Duration::from_secs(core.settings.idle_secs);
    let tick = (idle / 4).clamp(Duration::from_millis(100), Duration::from_secs(5));
    loop {
        tokio::time::sleep(tick).await;
        let quiet = core.clients.load(Ordering::Relaxed) == 0
            && core.registry.lock().await.watches.is_empty();
        if quiet && core.idle_for() >= idle {
            core.shutdown.notify_one();
            return;
        }
    }
}

async fn serve_connection(core: Arc<Core>, router: Arc<Router>, stream: Stream) {
    core.clients.fetch_add(1, Ordering::Relaxed);
    let stream = Arc::new(stream);
    let mut reader = &*stream;
    let write_half = Arc::clone(&stream);
    let (replies, mut outbox) = mpsc::unbounded_channel::<Vec<u8>>();
    let writer_task = tokio::spawn(async move {
        let mut writer = &*write_half;
        while let Some(bytes) = outbox.recv().await {
            if writer.write_all(&bytes).await.is_err() {
                break;
            }
        }
    });
    let mut decoder = LineDecoder::new();
    let mut chunk = [0_u8; READ_CHUNK];
    while let Ok(read) = reader.read(&mut chunk).await {
        if read == 0 {
            break;
        }
        decoder.push(&chunk[..read]);
        while let Some(parsed) = decoder.next_frame::<Request>() {
            match parsed {
                Ok(frame) => {
                    tokio::spawn(answer(Arc::clone(&router), frame, replies.clone()));
                }
                Err(error) => {
                    let body = Response::Error {
                        error: ErrorKind::BadRequest,
                        message: error.to_string(),
                    };
                    send_frame(&replies, &Frame { id: 0, body });
                    break;
                }
            }
        }
    }
    drop(replies);
    let _ = writer_task.await;
    core.clients.fetch_sub(1, Ordering::Relaxed);
}

async fn answer(
    router: Arc<Router>,
    frame: Frame<Request>,
    replies: mpsc::UnboundedSender<Vec<u8>>,
) {
    let Ok(body) = router.call(frame.body).await;
    send_frame(&replies, &Frame { id: frame.id, body });
}

fn send_frame(replies: &mpsc::UnboundedSender<Vec<u8>>, frame: &Frame<Response>) {
    let mut bytes = Vec::new();
    if encode_frame(frame, &mut bytes).is_ok() {
        let _ = replies.send(bytes);
    }
}

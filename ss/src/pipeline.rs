use std::collections::HashMap;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::UNIX_EPOCH;

use proxima::pipe::{AndThen, Pipe, PipeExt};
use ss_proto::IndexStats;

use crate::chunk::{ChunkKey, Language, RawChunk, chunk_source};
use crate::embed::{EmbedPool, EmbedRequest, Lane};
use crate::error::IndexError;
use crate::store::{FileRow, Store, StoreConfig};

pub const MAX_FILE_BYTES: u64 = 1 << 20;
const SNIFF_BYTES: usize = 8192;

/// Where a path's journey ended: one file's contribution to the run counters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileReport {
    pub path: String,
    pub stats: IndexStats,
}

/// Either the file is finished early (unchanged, binary, vanished) or carries on to the next stage.
#[derive(Debug)]
pub enum Flow<Next> {
    Done(FileReport),
    Next(Next),
}

#[derive(Debug)]
pub struct SourceFile {
    pub rel: String,
    pub text: String,
    pub row: FileRow,
}

#[derive(Debug)]
pub struct FileChunks {
    pub rel: String,
    pub row: FileRow,
    pub chunks: Vec<RawChunk>,
}

#[derive(Debug)]
pub struct EmbeddedFile {
    pub rel: String,
    pub row: FileRow,
    pub chunks: Vec<RawChunk>,
    pub fresh: Vec<(ChunkKey, Vec<f32>)>,
}

fn report(rel: &str, stats: IndexStats) -> FileReport {
    FileReport {
        path: rel.to_owned(),
        stats,
    }
}

fn unchanged() -> IndexStats {
    IndexStats {
        files_seen: 1,
        files_unchanged: 1,
        ..IndexStats::default()
    }
}

async fn blocking<T, Work>(work: Work) -> Result<T, IndexError>
where
    T: Send + 'static,
    Work: FnOnce() -> Result<T, IndexError> + Send + 'static,
{
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|error| IndexError::Join(error.to_string()))?
}

/// transform: `PathBuf -> Flow<SourceFile>`; stats the file, skips unchanged (mtime+size, then content hash) and binary files.
#[derive(Clone)]
pub struct ReadFile {
    root: Arc<Path>,
    store: Arc<StoreConfig>,
}

impl ReadFile {
    #[must_use]
    pub fn new(root: &Path, store: Arc<StoreConfig>) -> Self {
        Self {
            root: root.into(),
            store,
        }
    }
}

impl Pipe for ReadFile {
    type In = PathBuf;
    type Out = Flow<SourceFile>;
    type Err = IndexError;

    fn call(&self, input: Self::In) -> impl Future<Output = Result<Self::Out, Self::Err>> {
        let (root, config) = (Arc::clone(&self.root), Arc::clone(&self.store));
        blocking(move || read_file(&root, &config, &input))
    }
}

fn relative(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .components()
        .map(|part| part.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

#[proxima::instrument]
fn read_file(
    root: &Path,
    config: &StoreConfig,
    path: &Path,
) -> Result<Flow<SourceFile>, IndexError> {
    let rel = relative(root, path);
    let meta = match std::fs::metadata(path) {
        Ok(meta) => meta,
        Err(error) if error.kind() == ErrorKind::NotFound => return forget_vanished(config, &rel),
        Err(error) => return Err(error.into()),
    };
    if meta.len() > MAX_FILE_BYTES {
        return Ok(Flow::Done(report(
            &rel,
            IndexStats {
                files_seen: 1,
                ..IndexStats::default()
            },
        )));
    }
    let mtime_ns = meta
        .modified()?
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_nanos()).unwrap_or(i64::MAX)
        });
    let size = i64::try_from(meta.len()).unwrap_or(i64::MAX);
    let store = Store::open(config)?;
    let known = store.file_row(&rel)?;
    if known.is_some_and(|row| row.mtime_ns == mtime_ns && row.size == size) {
        return Ok(Flow::Done(report(&rel, unchanged())));
    }
    let bytes = std::fs::read(path)?;
    let hash = *blake3::hash(&bytes).as_bytes();
    if known.is_some_and(|row| row.hash == hash) {
        store.touch_file(&rel, mtime_ns)?;
        return Ok(Flow::Done(report(&rel, unchanged())));
    }
    let Some(text) = decode_text(bytes) else {
        return Ok(Flow::Done(report(
            &rel,
            IndexStats {
                files_seen: 1,
                ..IndexStats::default()
            },
        )));
    };
    Ok(Flow::Next(SourceFile {
        rel,
        text,
        row: FileRow {
            mtime_ns,
            size,
            hash,
        },
    }))
}

/// A file that vanished between the walk and the read is a normal race, not a failure: drop its rows.
fn forget_vanished(config: &StoreConfig, rel: &str) -> Result<Flow<SourceFile>, IndexError> {
    let store = Store::open(config)?;
    let removed = u64::from(store.remove_file(rel)? > 0);
    Ok(Flow::Done(report(
        rel,
        IndexStats {
            files_removed: removed,
            ..IndexStats::default()
        },
    )))
}

fn decode_text(bytes: Vec<u8>) -> Option<String> {
    let head = bytes.get(..SNIFF_BYTES).unwrap_or(&bytes);
    if head.contains(&0) {
        return None;
    }
    String::from_utf8(bytes).ok()
}

/// transform: `Flow<SourceFile> -> Flow<FileChunks>`; sans-IO chunking wrapped as a pipe.
#[derive(Clone, Copy, Default)]
pub struct Chunk;

impl Pipe for Chunk {
    type In = Flow<SourceFile>;
    type Out = Flow<FileChunks>;
    type Err = IndexError;

    fn call(&self, input: Self::In) -> impl Future<Output = Result<Self::Out, Self::Err>> {
        std::future::ready(Ok(chunk_flow(input)))
    }
}

#[proxima::instrument]
fn chunk_flow(input: Flow<SourceFile>) -> Flow<FileChunks> {
    let source = match input {
        Flow::Done(done) => return Flow::Done(done),
        Flow::Next(source) => source,
    };
    let language = Path::new(&source.rel)
        .extension()
        .and_then(|ext| ext.to_str())
        .and_then(Language::from_extension);
    let chunks = chunk_source(language, &source.text);
    Flow::Next(FileChunks {
        rel: source.rel,
        row: source.row,
        chunks,
    })
}

/// transform: `Flow<FileChunks> -> Flow<EmbeddedFile>`; embeds only keys the store has not seen (hash-skip).
#[derive(Clone)]
pub struct Embed {
    store: Arc<StoreConfig>,
    pool: EmbedPool,
}

impl Embed {
    #[must_use]
    pub fn new(store: Arc<StoreConfig>, pool: EmbedPool) -> Self {
        Self { store, pool }
    }
}

impl Pipe for Embed {
    type In = Flow<FileChunks>;
    type Out = Flow<EmbeddedFile>;
    type Err = IndexError;

    fn call(&self, input: Self::In) -> impl Future<Output = Result<Self::Out, Self::Err>> {
        let config = Arc::clone(&self.store);
        let pool = self.pool.clone();
        async move {
            let file = match input {
                Flow::Done(done) => return Ok(Flow::Done(done)),
                Flow::Next(file) => file,
            };
            let unseen = unseen_texts(&config, &file.chunks).await?;
            let keys: Vec<ChunkKey> = unseen.iter().map(|(key, _)| *key).collect();
            let texts: Vec<String> = unseen.into_iter().map(|(_, text)| text).collect();
            let vectors = if texts.is_empty() {
                Vec::new()
            } else {
                pool.call(EmbedRequest {
                    texts,
                    lane: Lane::Index,
                })
                .await?
            };
            let fresh = keys.into_iter().zip(vectors).collect();
            Ok(Flow::Next(EmbeddedFile {
                rel: file.rel,
                row: file.row,
                chunks: file.chunks,
                fresh,
            }))
        }
    }
}

#[proxima::instrument]
async fn unseen_texts(
    config: &Arc<StoreConfig>,
    chunks: &[RawChunk],
) -> Result<Vec<(ChunkKey, String)>, IndexError> {
    let (config, keyed): (Arc<StoreConfig>, Vec<(ChunkKey, String)>) = (
        Arc::clone(config),
        chunks
            .iter()
            .map(|chunk| (chunk.key, chunk.text.clone()))
            .collect(),
    );
    blocking(move || {
        let keys: Vec<ChunkKey> = keyed.iter().map(|(key, _)| *key).collect();
        let known = Store::open(&config)?.existing_keys(&keys)?;
        let mut first_seen: HashMap<ChunkKey, String> = HashMap::new();
        let mut order = Vec::new();
        for (key, text) in keyed {
            if !known.contains(&key) && first_seen.insert(key, text).is_none() {
                order.push(key);
            }
        }
        Ok(order
            .into_iter()
            .filter_map(|key| first_seen.remove(&key).map(|text| (key, text)))
            .collect())
    })
    .await
}

/// sink-shaped transform: `Flow<EmbeddedFile> -> FileReport`; one transaction replaces the file's chunks.
#[derive(Clone)]
pub struct StoreFile {
    store: Arc<StoreConfig>,
}

impl StoreFile {
    #[must_use]
    pub fn new(store: Arc<StoreConfig>) -> Self {
        Self { store }
    }
}

impl Pipe for StoreFile {
    type In = Flow<EmbeddedFile>;
    type Out = FileReport;
    type Err = IndexError;

    fn call(&self, input: Self::In) -> impl Future<Output = Result<Self::Out, Self::Err>> {
        let config = Arc::clone(&self.store);
        async move {
            let file = match input {
                Flow::Done(done) => return Ok(done),
                Flow::Next(file) => file,
            };
            blocking(move || store_file(&config, &file)).await
        }
    }
}

#[proxima::instrument]
fn store_file(config: &StoreConfig, file: &EmbeddedFile) -> Result<FileReport, IndexError> {
    let stats = file_stats(file);
    Store::open(config)?.replace_file(&file.rel, &file.row, &file.chunks, &file.fresh)?;
    Ok(report(&file.rel, stats))
}

fn file_stats(file: &EmbeddedFile) -> IndexStats {
    let seen = file.chunks.len() as u64;
    let embedded = file.fresh.len() as u64;
    IndexStats {
        files_seen: 1,
        files_indexed: 1,
        chunks_seen: seen,
        chunks_embedded: embedded,
        chunks_skipped: seen - embedded,
        ..IndexStats::default()
    }
}

pub type FilePipeline = AndThen<AndThen<AndThen<ReadFile, Chunk>, Embed>, StoreFile>;

/// `ReadFile.and_then(Chunk).and_then(Embed).and_then(StoreFile)`: path in, report out.
#[must_use]
pub fn file_pipeline(root: &Path, store: &Arc<StoreConfig>, pool: &EmbedPool) -> FilePipeline {
    ReadFile::new(root, Arc::clone(store))
        .and_then(Chunk)
        .and_then(Embed::new(Arc::clone(store), pool.clone()))
        .and_then(StoreFile::new(Arc::clone(store)))
}

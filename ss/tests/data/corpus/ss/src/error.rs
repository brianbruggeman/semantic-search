use thiserror::Error;

#[derive(Debug, Error)]
pub enum IndexError {
    #[error("io failure: {0}")]
    Io(#[from] std::io::Error),
    #[error("store failure: {0}")]
    Store(#[from] rusqlite::Error),
    #[error("embedding failure: {0}")]
    Embed(#[from] EmbedError),
    #[error("blocking task failed: {0}")]
    Join(String),
}

#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum EmbedError {
    #[error("embedding model unavailable: {0}")]
    Model(String),
    #[error("embedding pool is closed")]
    PoolClosed,
    #[error("model returned {got} vectors for {want} texts")]
    CountMismatch { want: usize, got: usize },
}

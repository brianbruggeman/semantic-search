use alloc::string::String;
use alloc::vec::Vec;

use serde::{Deserialize, Serialize};

use crate::context::{Findings, GraphReply};
use crate::stats::{Hit, IndexStats};

/// One id-tagged message. The id pairs a response with its request; responses may arrive out of order.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Frame<Body> {
    pub id: u64,
    pub body: Body,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    Search {
        query: String,
        path: String,
        limit: u32,
    },
    /// Structured search: one result set per query, each hit with its symbol and neighbours (schema 1).
    Query {
        queries: Vec<String>,
        path: String,
        limit: u32,
    },
    /// The neighbourhood of a symbol name or `path:line` (schema 1).
    Graph {
        target: String,
        path: String,
        depth: u8,
    },
    Index {
        path: String,
    },
    WatchAdd {
        path: String,
    },
    WatchRm {
        path: String,
    },
    WatchList,
    Status,
    Stop,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    Busy,
    NotIndexed,
    BadRequest,
    Internal,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Response {
    Hits { hits: Vec<Hit> },
    Findings { findings: Findings },
    Graph { graph: GraphReply },
    Indexed { stats: IndexStats },
    Watches { paths: Vec<String> },
    Status { status: DaemonStatus },
    Ok,
    Error { error: ErrorKind, message: String },
}

/// Snapshot served by `ssd status`; counters are totals since the daemon started.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonStatus {
    pub pid: u32,
    pub uptime_secs: u64,
    pub roots: Vec<String>,
    pub watches: Vec<String>,
    pub active_watchers: u64,
    pub queries_busy: u64,
    pub index_busy: u64,
    pub chunks_seen: u64,
    pub chunks_embedded: u64,
    pub chunks_skipped: u64,
    /// Vector-cache counters; absent from older daemons.
    #[serde(default)]
    pub vector_cache: VectorCacheStatus,
}

/// Totals since the daemon started, except `rebuilds_in_flight` and `max_in_flight` which are a gauge and its high-water mark.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VectorCacheStatus {
    pub hits: u64,
    pub rebuilds_started: u64,
    pub rebuilds_published: u64,
    pub rebuilds_in_flight: u64,
    pub max_in_flight: u64,
}

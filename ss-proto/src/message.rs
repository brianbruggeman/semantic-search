use alloc::string::String;
use alloc::vec::Vec;

use serde::{Deserialize, Serialize};

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
}

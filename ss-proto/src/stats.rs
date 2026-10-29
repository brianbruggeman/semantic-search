use alloc::string::String;
use alloc::vec::Vec;

use serde::{Deserialize, Serialize};

/// One search result: where it is, how it scored, and the first lines of the chunk.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Hit {
    pub path: String,
    pub start_line: u32,
    pub end_line: u32,
    pub score: f64,
    pub preview: Vec<String>,
}

/// Counters for one index run; `chunks_seen = chunks_embedded + chunks_skipped`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexStats {
    pub files_seen: u64,
    pub files_unchanged: u64,
    pub files_indexed: u64,
    pub files_removed: u64,
    pub chunks_seen: u64,
    pub chunks_embedded: u64,
    pub chunks_skipped: u64,
}

impl IndexStats {
    pub fn absorb(&mut self, other: Self) {
        self.files_seen += other.files_seen;
        self.files_unchanged += other.files_unchanged;
        self.files_indexed += other.files_indexed;
        self.files_removed += other.files_removed;
        self.chunks_seen += other.chunks_seen;
        self.chunks_embedded += other.chunks_embedded;
        self.chunks_skipped += other.chunks_skipped;
    }
}

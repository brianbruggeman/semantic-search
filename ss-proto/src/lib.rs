#![no_std]

//! Tier 1 (`no_std` + `alloc`) wire vocabulary shared by `ss` and `ssd`: request and
//! response enums, the id-tagged [`Frame`], and a sans-IO [`LineDecoder`].

extern crate alloc;

mod context;
mod frame;
mod message;
mod stats;

pub use context::{
    Capped, EdgeRef, Facet, Findings, GraphReply, Member, Neighborhood, Record, Related, Resolved,
    SCHEMA_VERSION, Symbol,
};
pub use frame::{FrameError, LineDecoder, MAX_FRAME_BYTES, encode_frame};
pub use message::{DaemonStatus, ErrorKind, Frame, Request, Response, VectorCacheStatus};
pub use stats::{Hit, IndexStats};

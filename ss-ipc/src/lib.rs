//! Platform-coupled plumbing for `ss`: the per-OS IPC endpoint, the single-instance guard and detached
//! spawning. Kept in its own crate so the Windows arms can be cross-checked in isolation.

mod endpoint;
mod lock;
mod spawn;

pub use endpoint::{Endpoint, Listener, ListenerExt, Stream, StreamExt, connect, listen};
pub use lock::InstanceLock;
pub use spawn::{become_session_leader, spawn_detached};

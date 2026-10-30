//! `ss` core: gitignore-aware walk, chunking, embedding, SQLite store and hybrid query, all composed from pipes.

pub mod chunk;
pub mod client;
pub mod context;
pub mod daemon;
pub mod embed;
pub mod error;
pub mod graph;
pub mod index;
pub mod pipeline;
pub mod query;
pub mod registry;
pub mod settings;
pub mod store;
pub mod telemetry;
pub mod walk;
pub mod watch;

#[cfg(test)]
mod fixtures;

//! `ss` core: gitignore-aware walk, chunking, embedding, SQLite store and hybrid query, all composed from pipes.

pub mod chunk;
pub mod client;
pub mod context;
pub mod daemon;
pub mod embed;
pub mod error;
pub mod exact;
pub mod find;
pub mod graph;
pub mod history;
pub mod index;
pub mod map;
pub mod pipeline;
pub mod query;
pub mod read;
pub mod registry;
pub mod render;
pub mod scip;
pub mod settings;
pub mod store;
pub mod telemetry;
pub mod walk;
pub mod watch;

#[cfg(test)]
mod fixtures;

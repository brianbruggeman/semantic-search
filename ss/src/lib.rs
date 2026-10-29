//! `ss` core: gitignore-aware walk, chunking, embedding, SQLite store and hybrid query, all composed from pipes.

pub mod admit;
pub mod chunk;
pub mod client;
pub mod daemon;
pub mod embed;
pub mod error;
pub mod index;
pub mod pipeline;
pub mod query;
pub mod registry;
pub mod settings;
pub mod store;
pub mod walk;

#[cfg(test)]
mod fixtures;

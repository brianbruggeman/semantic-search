//! `ss` core: gitignore-aware walk, chunking, embedding, SQLite store and hybrid query, all composed from pipes.

pub mod chunk;
pub mod embed;
pub mod error;
pub mod index;
pub mod pipeline;
pub mod store;
pub mod walk;

#[cfg(test)]
mod fixtures;

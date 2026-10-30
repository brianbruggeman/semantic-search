//! Code graph extracted from one file's syntax tree: nodes (symbols) and edges (contains, imports, calls, ...).
//!
//! Sans-IO: `extract` takes text in and returns rows out. It is wrapped as the `Extract` transform pipe in
//! `pipeline.rs` and persisted by `StoreFile`. Call and import edges are NAME-RESOLVED ONLY: an edge stores the
//! name as written, and `resolve` counts the definitions that carry that name at query time.

mod golang;
mod python;
mod rust;
mod script;
mod walker;

use crate::chunk::{ChunkKey, Language, RawChunk};

pub use walker::extract_tree;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeKind {
    File,
    Module,
    Struct,
    Enum,
    Trait,
    Type,
    Function,
    Method,
    Const,
}

impl NodeKind {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::File => "file",
            Self::Module => "module",
            Self::Struct => "struct",
            Self::Enum => "enum",
            Self::Trait => "trait",
            Self::Type => "type",
            Self::Function => "fn",
            Self::Method => "method",
            Self::Const => "const",
        }
    }

    #[must_use]
    pub fn callable(self) -> bool {
        matches!(self, Self::Function | Self::Method)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EdgeKind {
    Contains,
    Imports,
    Calls,
    Implements,
    Extends,
    Tests,
}

impl EdgeKind {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Contains => "contains",
            Self::Imports => "imports",
            Self::Calls => "calls",
            Self::Implements => "implements",
            Self::Extends => "extends",
            Self::Tests => "tests",
        }
    }
}

/// One symbol: `qpath` is `module::Type::method`, `module` being the file stem (the directory for Go and mod-like files).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphNode {
    pub kind: NodeKind,
    pub name: String,
    pub qpath: String,
    pub signature: String,
    pub doc: String,
    pub start_line: u32,
    pub end_line: u32,
    pub chunk_key: Option<ChunkKey>,
}

/// One relation from the symbol `src` (a `qpath`, or the file path for file-level imports) to a name as written.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct GraphEdge {
    pub kind: EdgeKind,
    pub src: String,
    pub dst: String,
    pub qual: String,
    pub line: u32,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FileGraph {
    pub nodes: Vec<GraphNode>,
    pub edges: Vec<GraphEdge>,
}

/// How many definitions a name-only edge could mean: one, several, or none in the index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolution {
    Exact,
    Ambiguous,
    Unresolved,
}

impl Resolution {
    #[must_use]
    pub fn of(candidates: usize) -> Self {
        match candidates {
            0 => Self::Unresolved,
            1 => Self::Exact,
            _ => Self::Ambiguous,
        }
    }

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Exact => "exact",
            Self::Ambiguous => "ambiguous",
            Self::Unresolved => "unresolved",
        }
    }
}

/// Parses `text` and extracts its graph; a file with no grammar still yields its file node.
#[must_use]
pub fn extract(
    language: Option<Language>,
    rel: &str,
    text: &str,
    chunks: &[RawChunk],
) -> FileGraph {
    let mut graph = language.map_or_else(FileGraph::default, |lang| extract_tree(lang, rel, text));
    graph.nodes.insert(0, file_node(rel, text));
    for node in &mut graph.nodes {
        node.chunk_key = chunk_containing(chunks, node.start_line);
    }
    graph
}

fn file_node(rel: &str, text: &str) -> GraphNode {
    let name = rel.rsplit('/').next().unwrap_or(rel).to_owned();
    GraphNode {
        kind: NodeKind::File,
        name,
        qpath: rel.to_owned(),
        signature: String::new(),
        doc: String::new(),
        start_line: 1,
        end_line: u32::try_from(text.lines().count().max(1)).unwrap_or(u32::MAX),
        chunk_key: None,
    }
}

fn chunk_containing(chunks: &[RawChunk], line: u32) -> Option<ChunkKey> {
    chunks
        .iter()
        .find(|chunk| chunk.start_line <= line && line <= chunk.end_line)
        .map(|chunk| chunk.key)
}

/// The module name a file contributes to qualified paths.
#[must_use]
pub fn file_module(rel: &str, language: Language) -> String {
    let mut parts = rel.rsplit('/');
    let file = parts.next().unwrap_or(rel);
    let parent = parts.next().unwrap_or("");
    let stem = file.rsplit_once('.').map_or(file, |(stem, _)| stem);
    let dir_named = matches!(stem, "mod" | "lib" | "main" | "__init__" | "index");
    match language {
        Language::Go => parent,
        _ if dir_named && !parent.is_empty() => parent,
        _ => stem,
    }
    .to_owned()
}

/// The last segment of a path-like qualifier (`crate::query`, `./query.ts`, `os.path`).
#[must_use]
pub fn last_segment(qual: &str) -> &str {
    let tail = qual.rsplit(['/', '.', ':']).find(|part| !part.is_empty());
    tail.unwrap_or("")
}

/// Whether `qual`, as written at a call or import site, can name the file or scope of `qpath`.
#[must_use]
pub fn qual_matches(qual: &str, qpath: &str, same_file: bool) -> bool {
    let wanted = last_segment(qual.trim_end_matches(".ts").trim_end_matches(".js"));
    match wanted {
        "" | "crate" | "super" => true,
        "self" | "Self" => same_file,
        other => qpath.split("::").any(|part| part == other),
    }
}

#[cfg(test)]
mod tests;

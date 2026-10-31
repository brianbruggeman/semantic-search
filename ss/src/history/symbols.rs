//! Maps changed lines to the symbols that enclose them, using the same tree-sitter extractor as the graph.

use std::collections::{BTreeMap, BTreeSet};

use super::git::{FilePatch, Hunk};
use crate::chunk::Language;
use crate::graph::{GraphNode, NodeKind, extract_tree};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Added,
    Modified,
    Deleted,
}

impl Status {
    #[must_use]
    pub fn letter(self) -> char {
        match self {
            Self::Added => 'A',
            Self::Modified => 'M',
            Self::Deleted => 'D',
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Touch {
    pub qpath: String,
    pub kind: NodeKind,
    pub status: Status,
    pub added: u32,
    pub deleted: u32,
}

#[must_use]
pub fn language_of(path: &str) -> Option<Language> {
    path.rsplit_once('.')
        .and_then(|(_, extension)| Language::from_extension(extension))
}

#[must_use]
pub fn symbols(language: Language, path: &str, text: Option<&str>) -> Vec<GraphNode> {
    text.map(|body| extract_tree(language, path, body).nodes)
        .unwrap_or_default()
        .into_iter()
        .filter(|node| node.kind != NodeKind::File)
        .collect()
}

fn innermost(nodes: &[GraphNode], line: u32) -> Option<&GraphNode> {
    nodes
        .iter()
        .filter(|node| node.start_line <= line && line <= node.end_line)
        .min_by_key(|node| node.end_line - node.start_line)
}

/// Counts each changed line against the innermost symbol around it and classifies symbols by old-vs-new presence.
#[must_use]
pub fn touched(old: &[GraphNode], new: &[GraphNode], hunks: &[Hunk]) -> Vec<Touch> {
    let mut counts: BTreeMap<String, (NodeKind, u32, u32)> = BTreeMap::new();
    for line in hunks.iter().flat_map(|hunk| &hunk.lines) {
        let (nodes, plus) = match line.sign {
            '+' => (new, true),
            '-' => (old, false),
            _ => continue,
        };
        if let Some(node) = innermost(nodes, line.number) {
            let entry = counts.entry(node.qpath.clone()).or_insert((node.kind, 0, 0));
            if plus {
                entry.1 += 1;
            } else {
                entry.2 += 1;
            }
        }
    }
    let old_paths: BTreeSet<&str> = old.iter().map(|node| node.qpath.as_str()).collect();
    let new_paths: BTreeSet<&str> = new.iter().map(|node| node.qpath.as_str()).collect();
    counts
        .into_iter()
        .map(|(qpath, (kind, added, deleted))| Touch {
            status: match (old_paths.contains(qpath.as_str()), new_paths.contains(qpath.as_str())) {
                (false, _) => Status::Added,
                (true, false) => Status::Deleted,
                (true, true) => Status::Modified,
            },
            qpath,
            kind,
            added,
            deleted,
        })
        .collect()
}

fn matches(node: &GraphNode, name: &str) -> bool {
    node.name == name || node.qpath == name || node.qpath.ends_with(&format!("::{name}"))
}

/// The hunks of `patch` with at least one changed line inside a symbol called `name` (old or new side).
#[must_use]
pub fn hunks_in_symbol(
    patch: &FilePatch,
    old: &[GraphNode],
    new: &[GraphNode],
    name: &str,
) -> Vec<Hunk> {
    let inside = |nodes: &[GraphNode], line: u32| {
        nodes
            .iter()
            .any(|node| matches(node, name) && node.start_line <= line && line <= node.end_line)
    };
    patch
        .hunks
        .iter()
        .filter(|hunk| {
            hunk.lines.iter().any(|line| match line.sign {
                '+' => inside(new, line.number),
                '-' => inside(old, line.number),
                _ => false,
            })
        })
        .cloned()
        .collect()
}

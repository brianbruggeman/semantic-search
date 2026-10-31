//! Symbol trees of source files, taken from the graph extractor (`graph::extract_tree`).

use super::Entry;
use crate::chunk::Language;
use crate::graph::{NodeKind, extract_tree};

const SIGNATURE_CHARS: usize = 100;
const DOC_CHARS: usize = 100;

pub(super) fn language_of(path: &str) -> Option<Language> {
    path.rsplit_once('.')
        .and_then(|(_, extension)| Language::from_extension(extension))
}

pub(super) fn entries(language: Language, path: &str, text: &str) -> Vec<Entry> {
    extract_tree(language, path, text)
        .nodes
        .into_iter()
        .filter(|node| node.kind != NodeKind::File)
        .map(|node| {
            let mut segments: Vec<&str> = node.qpath.split("::").skip(1).collect();
            let depth = segments.len().saturating_sub(1);
            segments.pop();
            let owner = if segments.is_empty() {
                String::new()
            } else {
                format!("[{}] ", segments.join("::"))
            };
            let signature: String = node.signature.chars().take(SIGNATURE_CHARS).collect();
            Entry {
                depth,
                range: (node.start_line as usize, node.end_line as usize),
                label: format!("{} {owner}{signature}", node.kind.as_str()),
                name: node.qpath.split("::").skip(1).collect::<Vec<_>>().join("::"),
                gist: first_line(&node.doc),
            }
        })
        .collect()
}

fn first_line(doc: &str) -> String {
    doc.lines()
        .map(|line| line.trim().trim_start_matches('/').trim_start_matches('!').trim())
        .find(|line| !line.is_empty())
        .map(|line| line.chars().take(DOC_CHARS).collect())
        .unwrap_or_default()
}

//! Exact (lexical) search: an rg-style line scan of the files the indexer's walker lists.
//!
//! No index and no process: `walk::list_files` (gitignore-aware, hidden skipped) feeds a `regex` line match, with the
//! indexer's own skip rules (over `MAX_FILE_BYTES`, NUL in the first bytes, not UTF-8). Output shaping lives in
//! `render::exact_text`; this module only finds and counts.

use std::path::Path;

use regex::{Regex, RegexBuilder};
use thiserror::Error;

use crate::pipeline::{MAX_FILE_BYTES, decode_text};
use crate::walk::list_files;

/// Longest matching line kept; a minified bundle would otherwise flood one hit.
pub const LINE_CHARS: usize = 160;

#[derive(Debug, Error)]
pub enum ExactError {
    #[error("bad pattern: {0}")]
    Pattern(#[from] regex::Error),
}

/// The matches of one file: every matching line is counted, the first `keep` are stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileMatches {
    pub path: String,
    pub total: usize,
    pub lines: Vec<(usize, String)>,
}

/// A regex from `pattern`; `fixed` matches it literally, like `rg -F`.
pub fn compile(pattern: &str, fixed: bool, ignore_case: bool) -> Result<Regex, ExactError> {
    let source = if fixed {
        regex::escape(pattern)
    } else {
        pattern.to_owned()
    };
    Ok(RegexBuilder::new(&source)
        .case_insensitive(ignore_case)
        .build()?)
}

fn clipped(line: &str) -> String {
    let line = line.trim();
    if line.chars().count() <= LINE_CHARS {
        return line.to_owned();
    }
    let head: String = line.chars().take(LINE_CHARS).collect();
    format!("{head}...")
}

fn scan_text(regex: &Regex, text: &str, keep: usize) -> (usize, Vec<(usize, String)>) {
    let mut lines = Vec::new();
    let mut total = 0;
    for (index, line) in text.lines().enumerate() {
        if !regex.is_match(line) {
            continue;
        }
        total += 1;
        if lines.len() < keep {
            lines.push((index + 1, clipped(line)));
        }
    }
    (total, lines)
}

fn read_text(path: &Path) -> Option<String> {
    let meta = std::fs::metadata(path).ok()?;
    if meta.len() > MAX_FILE_BYTES {
        return None;
    }
    decode_text(std::fs::read(path).ok()?)
}

/// Matches per file under `root` (a directory or one file), in path order; files with no match are left out.
#[must_use]
pub fn scan(root: &Path, regex: &Regex, keep: usize) -> Vec<FileMatches> {
    list_files(root)
        .into_iter()
        .filter_map(|path| {
            let text = read_text(&path)?;
            let (total, lines) = scan_text(regex, &text, keep);
            (total > 0).then(|| FileMatches {
                path: path.to_string_lossy().into_owned(),
                total,
                lines,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests;

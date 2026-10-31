//! Budgeted, structure-first file reading behind `ss outline` and `ss section`.
//!
//! No daemon and no index: headings come from a line scan (Markdown, reStructuredText, plain text underlines),
//! symbols from the graph extractor (`graph::extract_tree`). Pure functions over text, so the budget (tokens =
//! chars / 4 of the exact output) is the same quantity the measurements report. Before: a whole-file `Read`.
//! After: `ss outline FILE` lists `start-end` ranges, `ss section FILE NAME` prints one range, and a ranged
//! `Read` follows from either.

mod code;
mod docs;

use std::fmt::Write as _;
use std::path::Path;

use thiserror::Error;

/// Default budget in tokens of an outline.
pub const OUTLINE_BUDGET: usize = 400;
/// Default budget in tokens of a section.
pub const SECTION_BUDGET: usize = 1200;
const FOOTER_CHARS: usize = 100;
const EDGE_LINES: usize = 5;
const EDGE_LINE_CHARS: usize = 200;
const CLOSE_MATCHES: usize = 5;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ReadError {
    #[error("{path}: {message}")]
    Io { path: String, message: String },
    #[error("{path} has no headings or symbols; use a ranged Read")]
    NoStructure { path: String },
    #[error("no heading or symbol {query:?} in {path}; close matches:\n{close}")]
    NotFound { path: String, query: String, close: String },
    #[error("{query:?} is ambiguous in {path}; {count} matches, name one fully or use --line N:\n{close}")]
    Ambiguous { path: String, query: String, count: usize, close: String },
    #[error("line {line} is outside every heading and symbol of {path}")]
    LineOutside { path: String, line: usize },
}

/// One heading or symbol: a `start-end` line range (1-based, inclusive) and the text that names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub depth: usize,
    pub range: (usize, usize),
    pub label: String,
    pub name: String,
    pub gist: String,
}

/// What a section is picked by.
#[derive(Debug, Clone, Copy)]
pub enum Target<'a> {
    Name(&'a str),
    Line(usize),
}

fn io_error(path: &Path, error: &std::io::Error) -> ReadError {
    ReadError::Io { path: path.display().to_string(), message: error.to_string() }
}

fn structure(path: &str, text: &str) -> Vec<Entry> {
    if let Some(kind) = docs::doc_kind(path) {
        return docs::entries(kind, text);
    }
    code::language_of(path).map_or_else(Vec::new, |language| code::entries(language, path, text))
}

/// The outline of the file at `path`, at most `budget` tokens, entries nested deeper than `depth` left out.
///
/// # Errors
/// Fails when the file cannot be read as UTF-8 text.
pub fn outline_file(path: &Path, budget: usize, depth: Option<usize>) -> Result<String, ReadError> {
    let text = std::fs::read_to_string(path).map_err(|error| io_error(path, &error))?;
    Ok(outline(&path.display().to_string(), &text, budget, depth))
}

/// Section of the file at `path` picked by `target`, `context` extra lines each side, at most `budget` tokens.
///
/// # Errors
/// Fails when the file is unreadable, has no structure, or `target` matches none or several entries.
pub fn section_file(
    path: &Path,
    target: Target<'_>,
    budget: usize,
    context: usize,
) -> Result<String, ReadError> {
    let text = std::fs::read_to_string(path).map_err(|error| io_error(path, &error))?;
    section(&path.display().to_string(), &text, target, budget, context)
}

#[must_use]
pub fn outline(path: &str, text: &str, budget: usize, depth: Option<usize>) -> String {
    let entries = structure(path, text);
    let total = text.lines().count();
    let header = format!("{path}: {total} lines\n");
    if entries.is_empty() {
        return head_and_tail(&header, text, total);
    }
    fit_outline(&header, &entries, budget, depth)
}

fn head_and_tail(header: &str, text: &str, total: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let mut out = header.to_owned();
    if total <= EDGE_LINES * 2 {
        push_numbered(&mut out, &lines, 1);
        return out;
    }
    push_numbered(&mut out, &lines[..EDGE_LINES], 1);
    let _ = writeln!(out, "... {} lines not shown", total - EDGE_LINES * 2);
    push_numbered(&mut out, &lines[total - EDGE_LINES..], total - EDGE_LINES + 1);
    out
}

fn push_numbered(out: &mut String, lines: &[&str], first: usize) {
    for (offset, line) in lines.iter().enumerate() {
        let shown: String = line.chars().take(EDGE_LINE_CHARS).collect();
        let more = line.chars().count().saturating_sub(EDGE_LINE_CHARS);
        let note = if more > 0 { format!(" ...+{more} chars") } else { String::new() };
        let _ = writeln!(out, "{}: {shown}{note}", first + offset);
    }
}

fn entry_line(entry: &Entry) -> String {
    let indent = "  ".repeat(entry.depth);
    let (start, end) = entry.range;
    let gist = if entry.gist.is_empty() { String::new() } else { format!(" -- {}", entry.gist) };
    format!("{indent}{start}-{end} {}{gist}\n", entry.label)
}

fn fit_outline(header: &str, entries: &[Entry], budget: usize, depth: Option<usize>) -> String {
    let limit = budget * 4;
    let deepest = entries.iter().map(|entry| entry.depth).max().unwrap_or(0) + 1;
    let top = depth.map_or(deepest, |wanted| wanted.clamp(1, deepest));
    for level in (1..=top).rev() {
        let shown: Vec<&Entry> = entries.iter().filter(|entry| entry.depth < level).collect();
        if let Some((text, kept)) = outline_text(header, &shown, limit, level == 1) {
            return with_cut(text, entries.len() - kept);
        }
    }
    header.to_owned()
}

/// Whole lines of `shown` that fit `limit`; at the last depth level a partial list is accepted, always the first entry.
fn outline_text(header: &str, shown: &[&Entry], limit: usize, last_level: bool) -> Option<(String, usize)> {
    let mut out = header.to_owned();
    for (index, entry) in shown.iter().enumerate() {
        let line = entry_line(entry);
        let full = out.len() + line.len() + FOOTER_CHARS > limit;
        if full && index > 0 {
            return last_level.then_some((out, index));
        }
        out.push_str(&line);
    }
    Some((out, shown.len()))
}

fn with_cut(mut text: String, dropped: usize) -> String {
    if dropped > 0 {
        let _ = writeln!(
            text,
            "cut: {dropped} entries not shown; raise --budget or --depth, or name one with `ss section`"
        );
    }
    text
}

/// The text of the entry picked by `target`, with line numbers, at most `budget` tokens.
///
/// # Errors
/// `NoStructure` for plain text, `NotFound` / `Ambiguous` (listing up to five close matches), `LineOutside`.
pub fn section(
    path: &str,
    text: &str,
    target: Target<'_>,
    budget: usize,
    context: usize,
) -> Result<String, ReadError> {
    let entries = structure(path, text);
    if entries.is_empty() {
        return Err(ReadError::NoStructure { path: path.to_owned() });
    }
    let entry = pick(path, &entries, target)?;
    let lines: Vec<&str> = text.lines().collect();
    let (start, end) = entry.range;
    let first = start.saturating_sub(context).max(1);
    let last = (end + context).min(lines.len());
    let header = format!("{path}:{start}-{end} {}\n", entry.label);
    Ok(fit_body(path, &header, &lines, first, last, budget))
}

fn fit_body(path: &str, header: &str, lines: &[&str], first: usize, last: usize, budget: usize) -> String {
    let limit = (budget * 4).saturating_sub(FOOTER_CHARS);
    let mut out = header.to_owned();
    let mut next = first;
    while next <= last {
        let line = format!("{next}: {}\n", lines[next - 1]);
        if next > first && out.len() + line.len() > limit {
            break;
        }
        out.push_str(&line);
        next += 1;
    }
    if next <= last {
        let _ = writeln!(
            out,
            "cut: {} lines not shown (resume at line {next} of {path}); raise --budget or Read the range",
            last + 1 - next
        );
    }
    out
}

fn pick<'e>(path: &str, entries: &'e [Entry], target: Target<'_>) -> Result<&'e Entry, ReadError> {
    match target {
        Target::Line(line) => entries
            .iter()
            .filter(|entry| entry.range.0 <= line && line <= entry.range.1)
            .min_by_key(|entry| entry.range.1 - entry.range.0)
            .ok_or_else(|| ReadError::LineOutside { path: path.to_owned(), line }),
        Target::Name(query) => pick_named(path, entries, query),
    }
}

fn is_exact(entry: &Entry, query: &str) -> bool {
    let name = entry.name.to_lowercase();
    name == query || name.rsplit("::").next() == Some(query)
}

fn pick_named<'e>(path: &str, entries: &'e [Entry], query: &str) -> Result<&'e Entry, ReadError> {
    let wanted = query.to_lowercase();
    let exact: Vec<&Entry> = entries.iter().filter(|entry| is_exact(entry, &wanted)).collect();
    let partial: Vec<&Entry> = entries
        .iter()
        .filter(|entry| entry.name.to_lowercase().contains(&wanted))
        .collect();
    let found = if exact.is_empty() { partial } else { exact };
    match found.as_slice() {
        [only] => Ok(*only),
        [] => Err(ReadError::NotFound {
            path: path.to_owned(),
            query: query.to_owned(),
            close: list(&close_to(entries, &wanted)),
        }),
        many => Err(ReadError::Ambiguous {
            path: path.to_owned(),
            query: query.to_owned(),
            count: many.len(),
            close: list(many),
        }),
    }
}

fn close_to<'e>(entries: &'e [Entry], wanted: &str) -> Vec<&'e Entry> {
    let words: Vec<&str> = wanted.split(|letter: char| !letter.is_alphanumeric()).filter(|word| word.len() >= 3).collect();
    let near: Vec<&Entry> = entries
        .iter()
        .filter(|entry| {
            let name = entry.name.to_lowercase();
            words.iter().any(|word| name.contains(word))
        })
        .collect();
    if near.is_empty() { entries.iter().collect() } else { near }
}

fn list(entries: &[&Entry]) -> String {
    let mut out = String::new();
    for entry in entries.iter().take(CLOSE_MATCHES) {
        let _ = writeln!(out, "  {}-{} {}", entry.range.0, entry.range.1, entry.label);
    }
    if entries.len() > CLOSE_MATCHES {
        let _ = writeln!(out, "  (+{} more)", entries.len() - CLOSE_MATCHES);
    }
    out.trim_end().to_owned()
}

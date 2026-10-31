//! Budgeted, symbol-level history view behind `ss show` and `ss diff`.
//!
//! No daemon and no index: `git` supplies numstat, patches and file contents; the graph extractor names the
//! symbols around each changed line. Pure functions over that text, so the budget (tokens = chars / 4 of the exact
//! output) is the same quantity the measurements report. Before: `git show <rev>` returns the whole patch. After:
//! `ss show <rev>` returns the touched symbols per file, and `--file` / `--symbol` / `--full` drill down.

mod git;
mod symbols;

use std::fmt::Write as _;
use std::path::Path;

use thiserror::Error;

pub use git::{Dump, Source, content, parse_patch, parse_stats, run};
use git::{FilePatch, Hunk, Side, Stat};
use symbols::{Touch, hunks_in_symbol, language_of, symbols, touched};

/// Default budget in tokens of the summary reply.
pub const SUMMARY_BUDGET: usize = 400;
/// Default budget in tokens of a drill-down reply.
pub const DRILL_BUDGET: usize = 1500;
const DEFAULT_CONTEXT: u32 = 3;
const FOOTER_CHARS: usize = 100;

#[derive(Debug, Error)]
pub enum HistoryError {
    #[error("git: {0}")]
    Git(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Clone)]
pub enum View {
    Summary,
    File(String),
    Symbol(String),
    Full,
}

#[derive(Debug)]
struct FileView {
    stat: Stat,
    touches: Vec<Touch>,
}

/// Renders `source` through `view`, trimmed to `budget` tokens (the view's default when `None`).
pub fn render(
    dir: &Path,
    source: &Source,
    view: &View,
    budget: Option<usize>,
    context: Option<u32>,
) -> Result<String, HistoryError> {
    let context = context.unwrap_or(DEFAULT_CONTEXT);
    match view {
        View::Summary => summary(dir, source, budget.unwrap_or(SUMMARY_BUDGET)),
        View::File(path) => {
            let raw = run(dir, &source.command(Dump::Patch(context), Some(path)))?;
            let body = if raw.is_empty() { format!("no changes in {path}\n") } else { raw };
            Ok(trim_lines(&body, budget.unwrap_or(DRILL_BUDGET)))
        }
        View::Full => {
            let patch = run(dir, &source.command(Dump::Patch(DEFAULT_CONTEXT), None))?;
            Ok(trim_lines(&patch, budget.unwrap_or(DRILL_BUDGET)))
        }
        View::Symbol(name) => {
            let body = symbol_hunks(dir, source, name, context)?;
            Ok(trim_lines(&body, budget.unwrap_or(DRILL_BUDGET)))
        }
    }
}

fn summary(dir: &Path, source: &Source, budget: usize) -> Result<String, HistoryError> {
    let (header, stats) = parse_stats(&run(dir, &source.command(Dump::Stat, None))?);
    let patches = parse_patch(&run(dir, &source.command(Dump::Patch(0), None))?);
    let (old_side, new_side) = source.sides();
    let mut files: Vec<FileView> = stats
        .into_iter()
        .map(|stat| {
            let hunks = patches.iter().find(|patch| patch.path == stat.path);
            let touches = file_touches(dir, (&old_side, &new_side), &stat, hunks);
            FileView { stat, touches }
        })
        .collect();
    files.sort_by(|left, right| {
        let size = |file: &FileView| file.stat.added + file.stat.deleted;
        size(right).cmp(&size(left)).then_with(|| left.stat.path.cmp(&right.stat.path))
    });
    Ok(fit_summary(header.as_deref(), &files, budget))
}

fn file_touches(
    dir: &Path,
    sides: (&Side, &Side),
    stat: &Stat,
    patch: Option<&FilePatch>,
) -> Vec<Touch> {
    let (Some(language), Some(patch), false) = (language_of(&stat.path), patch, stat.binary) else {
        return Vec::new();
    };
    let old = symbols(language, &stat.path, content(dir, sides.0, &stat.path).as_deref());
    let new = symbols(language, &stat.path, content(dir, sides.1, &stat.path).as_deref());
    touched(&old, &new, &patch.hunks)
}

fn file_line(stat: &Stat) -> String {
    if stat.binary {
        format!("{} binary\n", stat.path)
    } else {
        format!("{} +{} -{}\n", stat.path, stat.added, stat.deleted)
    }
}

fn touch_line(touch: &Touch) -> String {
    let kind = touch.kind.as_str();
    match touch.status {
        symbols::Status::Modified => format!(
            "  M {kind} {} (+{} -{})\n",
            touch.qpath, touch.added, touch.deleted
        ),
        status => format!("  {} {kind} {}\n", status.letter(), touch.qpath),
    }
}

struct Layout {
    text: String,
    dropped_files: usize,
    dropped_symbols: usize,
}

fn lay_out(header: Option<&str>, files: &[FileView], limit: usize) -> Layout {
    let mut layout = Layout {
        text: header.map(|line| format!("{line}\n")).unwrap_or_default(),
        dropped_files: 0,
        dropped_symbols: 0,
    };
    let mut listed = 0;
    for file in files {
        let line = file_line(&file.stat);
        if listed > 0 && layout.text.len() + line.len() > limit {
            layout.dropped_files += 1;
            layout.dropped_symbols += file.touches.len();
            continue;
        }
        listed += 1;
        layout.text.push_str(&line);
        for touch in &file.touches {
            let entry = touch_line(touch);
            if layout.text.len() + entry.len() > limit {
                layout.dropped_symbols += 1;
            } else {
                layout.text.push_str(&entry);
            }
        }
    }
    layout
}

fn fit_summary(header: Option<&str>, files: &[FileView], budget: usize) -> String {
    let limit = budget * 4;
    let full = lay_out(header, files, limit);
    if full.dropped_files + full.dropped_symbols == 0 {
        return full.text;
    }
    let mut cut = lay_out(header, files, limit.saturating_sub(FOOTER_CHARS));
    let _ = writeln!(
        cut.text,
        "cut: {} files, {} symbols not shown; ask --file PATH, --symbol NAME, --full",
        cut.dropped_files, cut.dropped_symbols
    );
    cut.text
}

fn symbol_hunks(
    dir: &Path,
    source: &Source,
    name: &str,
    context: u32,
) -> Result<String, HistoryError> {
    let patches = parse_patch(&run(dir, &source.command(Dump::Patch(context), None))?);
    let (old_side, new_side) = source.sides();
    let mut out = String::new();
    for patch in &patches {
        let Some(language) = language_of(&patch.path) else { continue };
        let old = symbols(language, &patch.path, content(dir, &old_side, &patch.path).as_deref());
        let new = symbols(language, &patch.path, content(dir, &new_side, &patch.path).as_deref());
        let hunks = hunks_in_symbol(patch, &old, &new, name);
        if !hunks.is_empty() {
            let _ = write!(out, "{}\n{}", patch.path, hunk_text(&hunks));
        }
    }
    Ok(if out.is_empty() { format!("no changes in symbol {name}\n") } else { out })
}

fn hunk_text(hunks: &[Hunk]) -> String {
    let mut out = String::new();
    for hunk in hunks {
        out.push_str(&hunk.header);
        out.push('\n');
        for line in &hunk.lines {
            out.push(line.sign);
            out.push_str(&line.text);
            out.push('\n');
        }
    }
    out
}

/// Keeps whole lines up to `budget` tokens, always the first line, and says how many lines were dropped.
fn trim_lines(text: &str, budget: usize) -> String {
    let limit = (budget * 4).saturating_sub(FOOTER_CHARS);
    if text.len() <= budget * 4 {
        return text.to_owned();
    }
    let mut out = String::new();
    let mut kept = 0;
    for line in text.lines() {
        if kept > 0 && out.len() + line.len() + 1 > limit {
            break;
        }
        out.push_str(line);
        out.push('\n');
        kept += 1;
    }
    let dropped = text.lines().count() - kept;
    let _ = writeln!(out, "cut: {dropped} lines not shown; raise --budget or narrow with --file PATH, --symbol NAME");
    out
}

#[cfg(test)]
mod tests;

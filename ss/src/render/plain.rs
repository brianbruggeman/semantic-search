//! The passthrough reply of an exact search: what `rg -n --no-heading` prints, used whenever it fits the budget.
//!
//! No header, no per-file title, no trailer, so the reply never costs more than the raw grep output. Past the budget
//! the grouped form (`exact_text`) takes over, or for `-l` and `-c` the list is cut with a `cut:` line.

use std::fmt::Write as _;
use std::path::Path;

use crate::exact::plain::{FileScan, Row};

use super::{EXACT_BUDGET, FOOTER_CHARS, PASSTHROUGH_CHARS, PASSTHROUGH_LINES, exact_text, shown};

/// What the reply lists: matching lines, only the paths (`-l`), or `path:count` (`-c`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Lines,
    Files,
    Count,
}

fn lines_block(files: &[FileScan], here: &Path, layout: Layout) -> String {
    let mut out = String::new();
    let mut last: Option<(usize, usize)> = None;
    for (slot, file) in files.iter().enumerate() {
        let name = shown(&file.path, here);
        for row in &file.rows {
            if layout.separators
                && last.is_some_and(|(prev, line)| prev != slot || line + 1 != row.line)
            {
                out.push_str("--\n");
            }
            let mark = if row.matched { ':' } else { '-' };
            if layout.prefix {
                let _ = write!(out, "{name}{mark}");
            }
            if layout.numbered {
                let _ = write!(out, "{}{mark}", row.line);
            }
            let _ = writeln!(out, "{}", row.text);
            last = Some((slot, row.line));
        }
    }
    out
}

fn list_lines(files: &[FileScan], here: &Path, mode: Mode, prefix: bool) -> Vec<String> {
    files
        .iter()
        .map(|file| match (mode, prefix) {
            (Mode::Count, true) => format!("{}:{}\n", shown(&file.path, here), file.total),
            (Mode::Count, false) => format!("{}\n", file.total),
            _ => format!("{}\n", shown(&file.path, here)),
        })
        .collect()
}

/// The full result in grep's form: `path:text` (`path:line:text` with `-n`), `path-text` context, `--` between blocks, `-c`, `-l`.
#[must_use]
pub fn plain_text(files: &[FileScan], here: &Path, layout: Layout) -> String {
    match layout.mode {
        Mode::Lines => lines_block(files, here, layout),
        mode => list_lines(files, here, mode, layout.prefix).concat(),
    }
}

fn cut_list(lines: &[String], limit: usize, budget: usize) -> String {
    let room = limit.saturating_sub(FOOTER_CHARS);
    let mut out = String::new();
    let mut kept = 0;
    for line in lines {
        if out.chars().count() + line.chars().count() > room && kept > 0 {
            break;
        }
        out.push_str(line);
        kept += 1;
    }
    let _ = writeln!(
        out,
        "cut: {} more (use --budget {})",
        lines.len() - kept,
        budget * 2
    );
    out
}

/// Layout switches of the passthrough form.
#[derive(Debug, Clone, Copy)]
pub struct Layout {
    pub mode: Mode,
    /// Print `line:` before each text (grep's `-n`).
    pub numbered: bool,
    /// Print `path:` before each line (off for a single explicit file, as rg does).
    pub prefix: bool,
    /// Print `--` between non-adjacent context blocks.
    pub separators: bool,
    /// Matches per file in the grouped form.
    pub per_file: usize,
}

fn fits(full: &str, budget: Option<usize>) -> bool {
    match budget {
        Some(tokens) => full.chars().count() <= tokens * 4,
        None => {
            full.lines().count() <= PASSTHROUGH_LINES && full.chars().count() <= PASSTHROUGH_CHARS
        }
    }
}

/// Runs of consecutive rows: a match with its requested context, merged where the contexts touch.
fn runs(rows: &[Row]) -> Vec<&[Row]> {
    let mut out = Vec::new();
    let mut start = 0;
    for index in 1..=rows.len() {
        if index == rows.len() || rows[index].line != rows[index - 1].line + 1 {
            out.push(&rows[start..index]);
            start = index;
        }
    }
    out
}

fn run_text(name: &str, rows: &[Row], layout: Layout) -> String {
    let mut out = String::new();
    for row in rows {
        let mark = if row.matched { ':' } else { '-' };
        if layout.prefix {
            let _ = write!(out, "{name}{mark}");
        }
        if layout.numbered {
            let _ = write!(out, "{}{mark}", row.line);
        }
        let _ = writeln!(out, "{}", row.text);
    }
    out
}

fn matches_in(rows: &[Row]) -> usize {
    rows.iter().filter(|row| row.matched).count()
}

/// Whole blocks, one tier at a time across files in file order, so a flood spreads over files before it deepens in one.
fn kept_blocks(texts: &[Vec<String>], limit: usize, used: usize) -> Vec<usize> {
    let mut keep = vec![0; texts.len()];
    let mut closed = vec![false; texts.len()];
    let mut used = used;
    let deepest = texts.iter().map(Vec::len).max().unwrap_or(0);
    for tier in 0..deepest {
        for (slot, file) in texts.iter().enumerate() {
            let Some(text) = file.get(tier).filter(|_| !closed[slot]) else {
                continue;
            };
            let cost = text.chars().count() + SEPARATOR.len();
            if used + cost > limit && keep.iter().sum::<usize>() > 0 {
                closed[slot] = true;
                continue;
            }
            used += cost;
            keep[slot] += 1;
        }
    }
    keep
}

const SEPARATOR: &str = "--\n";
const BLOCK_FOOTER_CHARS: usize = 150;

/// The grouped form for context requests: `N matches in F files`, whole blocks, and a `cut:` line.
fn blocks_text(files: &[FileScan], here: &Path, layout: Layout, budget: usize) -> String {
    let head = format!(
        "{} matches in {} files\n",
        files.iter().map(|file| file.total).sum::<usize>(),
        files.len()
    );
    let runs_of: Vec<Vec<&[Row]>> = files.iter().map(|file| runs(&file.rows)).collect();
    let texts: Vec<Vec<String>> = files
        .iter()
        .zip(&runs_of)
        .map(|(file, runs)| {
            let name = shown(&file.path, here);
            runs.iter()
                .map(|rows| run_text(&name, rows, layout))
                .collect()
        })
        .collect();
    let limit = budget * 4;
    let keep = kept_blocks(
        &texts,
        limit.saturating_sub(BLOCK_FOOTER_CHARS),
        head.chars().count(),
    );
    let mut out = head;
    let mut first = true;
    let (mut cut_matches, mut cut_blocks, mut cut_files) = (0, 0, 0);
    for (slot, runs) in runs_of.iter().enumerate() {
        cut_files += usize::from(keep[slot] == 0);
        cut_blocks += runs.len() - keep[slot];
        cut_matches += runs[keep[slot]..]
            .iter()
            .map(|rows| matches_in(rows))
            .sum::<usize>();
        for text in &texts[slot][..keep[slot]] {
            if !first {
                out.push_str(SEPARATOR);
            }
            first = false;
            out.push_str(text);
        }
    }
    if cut_blocks > 0 {
        let _ = writeln!(
            out,
            "cut: {cut_matches} matches in {cut_blocks} blocks ({cut_files} files) (use --budget {}, or a narrower path or glob)",
            budget * 2
        );
    }
    out
}

/// The passthrough text when it fits the flood boundary (or the explicit `budget`), else the grouped (or cut) form.
#[must_use]
pub fn exact_reply(
    files: &[FileScan],
    here: &Path,
    layout: Layout,
    budget: Option<usize>,
) -> String {
    let full = plain_text(files, here, layout);
    if fits(&full, budget) {
        return full;
    }
    let budget = budget.unwrap_or(EXACT_BUDGET);
    match (layout.mode, layout.separators) {
        (Mode::Lines, true) => blocks_text(files, here, layout, budget),
        (Mode::Lines, false) => {
            let grouped: Vec<_> = files
                .iter()
                .map(|file| file.matches(layout.per_file))
                .collect();
            exact_text(&grouped, here, budget)
        }
        (mode, _) => cut_list(
            &list_lines(files, here, mode, layout.prefix),
            budget * 4,
            budget,
        ),
    }
}

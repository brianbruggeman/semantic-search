//! The passthrough reply of an exact search: what `rg -n --no-heading` prints, used whenever it fits the budget.
//!
//! No header, no per-file title, no trailer, so the reply never costs more than the raw grep output. Past the budget
//! the grouped form (`exact_text`) takes over, or for `-l` and `-c` the list is cut with a `cut:` line.

use std::fmt::Write as _;
use std::path::Path;

use crate::exact::plain::FileScan;

use super::{FOOTER_CHARS, exact_text, shown};

/// What the reply lists: matching lines, only the paths (`-l`), or `path:count` (`-c`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Lines,
    Files,
    Count,
}

fn lines_block(files: &[FileScan], here: &Path, prefix: bool, separators: bool) -> String {
    let mut out = String::new();
    let mut last: Option<(usize, usize)> = None;
    for (slot, file) in files.iter().enumerate() {
        let name = shown(&file.path, here);
        for row in &file.rows {
            if separators && last.is_some_and(|(prev, line)| prev != slot || line + 1 != row.line) {
                out.push_str("--\n");
            }
            let mark = if row.matched { ':' } else { '-' };
            if prefix {
                let _ = write!(out, "{name}{mark}");
            }
            let _ = writeln!(out, "{}{mark}{}", row.line, row.text);
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

/// The full result in rg's form: `path:line:text`, `path-line-text` context, `--` between blocks, `-c`, `-l`.
#[must_use]
pub fn plain_text(
    files: &[FileScan],
    here: &Path,
    mode: Mode,
    prefix: bool,
    separators: bool,
) -> String {
    match mode {
        Mode::Lines => lines_block(files, here, prefix, separators),
        _ => list_lines(files, here, mode, prefix).concat(),
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
    /// Print `path:` before each line (off for a single explicit file, as rg does).
    pub prefix: bool,
    /// Print `--` between non-adjacent context blocks.
    pub separators: bool,
    /// Matches per file in the grouped form.
    pub per_file: usize,
}

/// The passthrough text when it fits `budget` tokens, else the grouped (or cut) form that fits.
#[must_use]
pub fn exact_reply(files: &[FileScan], here: &Path, layout: Layout, budget: usize) -> String {
    let full = plain_text(files, here, layout.mode, layout.prefix, layout.separators);
    let limit = budget * 4;
    if full.chars().count() <= limit {
        return full;
    }
    if layout.mode == Mode::Lines {
        let grouped: Vec<_> = files
            .iter()
            .map(|file| file.matches(layout.per_file))
            .collect();
        return exact_text(&grouped, here, budget);
    }
    cut_list(
        &list_lines(files, here, layout.mode, layout.prefix),
        limit,
        budget,
    )
}

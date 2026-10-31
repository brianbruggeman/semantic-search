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

/// The passthrough text when it fits `budget` tokens, else the grouped (or cut) form that fits.
#[must_use]
pub fn exact_reply(files: &[FileScan], here: &Path, layout: Layout, budget: usize) -> String {
    let full = plain_text(files, here, layout);
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

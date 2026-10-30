//! The budgeted reply of an exact search: grouped by file, at most the stored matches per file, then `+N more`.
//!
//! Tier 1 is every file's header with its first match (files with the most matches first); tier 2 the second
//! match of every file, tier 3 the third, and so on, so a flood spreads over files before it deepens in one.

use std::collections::HashSet;
use std::fmt::Write as _;
use std::path::Path;

use crate::exact::FileMatches;

use super::{FOOTER_CHARS, shown};

struct Piece {
    file: usize,
    depth: usize,
    text: String,
    /// Characters of the `+N more` line the file may need once it is shown.
    reserve: usize,
}

fn pieces(files: &[FileMatches], here: &Path) -> Vec<Piece> {
    let mut priority: Vec<usize> = (0..files.len()).collect();
    priority.sort_by_key(|index| std::cmp::Reverse(files[*index].total));
    let mut all: Vec<(usize, Piece)> = Vec::new();
    for (position, index) in priority.iter().enumerate() {
        let file = &files[*index];
        for (depth, (line, text)) in file.lines.iter().enumerate() {
            let mut body = String::new();
            if depth == 0 {
                let _ = writeln!(body, "{} ({})", shown(&file.path, here), file.total);
            }
            let _ = writeln!(body, "  {line}: {text}");
            all.push((
                position,
                Piece {
                    file: *index,
                    depth,
                    reserve: if depth == 0 && file.total > 1 {
                        format!("  +{} more\n", file.total - 1).chars().count()
                    } else {
                        0
                    },
                    text: body,
                },
            ));
        }
    }
    all.sort_by_key(|(position, piece)| (piece.depth, *position));
    all.into_iter().map(|(_, piece)| piece).collect()
}

fn select(all: &[Piece], limit: usize, overhead: usize) -> HashSet<usize> {
    let (mut kept, mut used, mut open) = (HashSet::new(), overhead, HashSet::new());
    for (index, piece) in all.iter().enumerate() {
        let cost = piece.text.chars().count() + piece.reserve;
        let first = kept.is_empty();
        let allowed = piece.depth == 0 || open.contains(&piece.file);
        if !allowed || (used + cost > limit && !first) {
            continue;
        }
        used += cost;
        kept.insert(index);
        if piece.depth == 0 {
            open.insert(piece.file);
        }
    }
    kept
}

fn body(files: &[FileMatches], all: &[Piece], kept: &HashSet<usize>) -> String {
    let mut out = String::new();
    for (index, file) in files.iter().enumerate() {
        let mine: Vec<&Piece> = all
            .iter()
            .enumerate()
            .filter(|(slot, piece)| piece.file == index && kept.contains(slot))
            .map(|(_, piece)| piece)
            .collect();
        if mine.is_empty() {
            continue;
        }
        let mut ordered = mine.clone();
        ordered.sort_by_key(|piece| piece.depth);
        for piece in &ordered {
            out.push_str(&piece.text);
        }
        let rest = file.total - ordered.len();
        if rest > 0 {
            let _ = writeln!(out, "  +{rest} more");
        }
    }
    out
}

/// `N matches in F files`, the files with their matches, and a `cut:` line when the budget dropped files.
#[must_use]
pub fn exact_text(files: &[FileMatches], here: &Path, budget: usize) -> String {
    if files.is_empty() {
        return String::new();
    }
    let total: usize = files.iter().map(|file| file.total).sum();
    let head = format!("{total} matches in {} files\n", files.len());
    let all = pieces(files, here);
    let limit = budget * 4;
    let overhead = head.chars().count();
    let mut kept = select(&all, limit, overhead);
    let mut note = String::new();
    if kept.len() < all.len() {
        kept = select(&all, limit.saturating_sub(FOOTER_CHARS), overhead);
        note = footer(files, &all, &kept, budget);
    }
    format!("{head}{}{note}", body(files, &all, &kept))
}

fn footer(files: &[FileMatches], all: &[Piece], kept: &HashSet<usize>, budget: usize) -> String {
    let shown_files: HashSet<usize> = kept.iter().map(|slot| all[*slot].file).collect();
    let dropped: Vec<&FileMatches> = files
        .iter()
        .enumerate()
        .filter(|(index, _)| !shown_files.contains(index))
        .map(|(_, file)| file)
        .collect();
    let lines = all.len() - kept.len();
    let matches: usize = dropped.iter().map(|file| file.total).sum();
    let mut parts = Vec::new();
    if !dropped.is_empty() {
        parts.push(format!("{} files ({matches} matches)", dropped.len()));
    }
    let hidden = lines.saturating_sub(dropped.iter().map(|f| f.lines.len()).sum());
    if hidden > 0 {
        parts.push(format!("{hidden} lines"));
    }
    if parts.is_empty() {
        return String::new();
    }
    format!("cut: {} (use --budget {})\n", parts.join(", "), budget * 2)
}

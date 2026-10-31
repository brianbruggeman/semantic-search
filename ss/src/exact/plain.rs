//! The rows behind the passthrough form: every matching line of every file with optional context, plus the
//! grep-flavoured pattern and path selection that feeds them.
//!
//! `scan_rows` is `scan` with the lines kept in full (no trim, no clip) so `render::exact_reply` can print what
//! `rg -n --no-heading` would.

use std::path::{Path, PathBuf};

use ignore::overrides::{Override, OverrideBuilder};
use regex::Regex;

use super::{FileMatches, clipped, read_text};
use crate::walk::list_files;

/// One printed line: a match, or context around one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub line: usize,
    pub text: String,
    pub matched: bool,
}

/// Every row of one file; `total` counts matching lines only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileScan {
    pub path: String,
    pub total: usize,
    pub rows: Vec<Row>,
}

impl FileScan {
    /// The grouped-form view: the first `keep` matching lines, clipped.
    #[must_use]
    pub fn matches(&self, keep: usize) -> FileMatches {
        let lines = self
            .rows
            .iter()
            .filter(|row| row.matched)
            .take(keep)
            .map(|row| (row.line, clipped(&row.text)))
            .collect();
        FileMatches {
            path: self.path.clone(),
            total: self.total,
            lines,
        }
    }
}

/// Lines of context printed before and after each match.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Context {
    pub before: usize,
    pub after: usize,
}

/// grep's basic-regex `\|` is alternation; a Rust regex spells it `|`. Everything else passes through unchanged.
#[must_use]
pub fn translate_bre(pattern: &str) -> String {
    let mut out = String::with_capacity(pattern.len());
    let mut chars = pattern.chars();
    while let Some(current) = chars.next() {
        if current != '\\' {
            out.push(current);
            continue;
        }
        match chars.next() {
            Some('|') => out.push('|'),
            Some(escaped) => {
                out.push('\\');
                out.push(escaped);
            }
            None => out.push('\\'),
        }
    }
    out
}

/// One regex source from every `-e` pattern: literal when `fixed`, `\|` translated when `translate`, whole-word when `word`.
#[must_use]
pub fn pattern_source(patterns: &[String], fixed: bool, word: bool, translate: bool) -> String {
    let parts: Vec<String> = patterns
        .iter()
        .map(|pattern| match (fixed, translate) {
            (true, _) => regex::escape(pattern),
            (false, true) => translate_bre(pattern),
            (false, false) => pattern.clone(),
        })
        .collect();
    let joined = match parts.as_slice() {
        [only] => only.clone(),
        _ => parts
            .iter()
            .map(|part| format!("(?:{part})"))
            .collect::<Vec<_>>()
            .join("|"),
    };
    if word {
        format!(r"\b(?:{joined})\b")
    } else {
        joined
    }
}

/// `--include` globs as whitelists and `--exclude` globs as negations, in gitignore syntax.
pub fn globs(
    root: &Path,
    include: &[String],
    exclude: &[String],
) -> Result<Override, ignore::Error> {
    let mut builder = OverrideBuilder::new(root);
    for glob in include {
        builder.add(glob)?;
    }
    for glob in exclude {
        builder.add(&format!("!{glob}"))?;
    }
    builder.build()
}

/// Files to search: an explicit file as given, a directory through the indexer's walker and the globs.
#[must_use]
pub fn files_of(paths: &[PathBuf], filter: &Override) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = paths
        .iter()
        .flat_map(|path| {
            if path.is_file() {
                return vec![path.clone()];
            }
            list_files(path)
                .into_iter()
                .filter(|file| !filter.matched(file, false).is_ignore())
                .collect()
        })
        .collect();
    files.sort();
    files.dedup();
    files
}

fn rows_of(regex: &Regex, text: &str, context: Context) -> (usize, Vec<Row>) {
    let lines: Vec<&str> = text.lines().collect();
    let hits: Vec<usize> = (0..lines.len())
        .filter(|index| regex.is_match(lines[*index]))
        .collect();
    let mut rows = Vec::new();
    let mut next = 0;
    for hit in &hits {
        let start = hit.saturating_sub(context.before).max(next);
        let end = (hit + context.after + 1).min(lines.len());
        for (index, text) in lines.iter().enumerate().take(end).skip(start) {
            rows.push(Row {
                line: index + 1,
                text: (*text).to_owned(),
                matched: hits.binary_search(&index).is_ok(),
            });
        }
        next = next.max(end);
    }
    (hits.len(), rows)
}

/// Rows per file in path order; files with no match are left out.
#[must_use]
pub fn scan_rows(files: &[PathBuf], regex: &Regex, context: Context) -> Vec<FileScan> {
    files
        .iter()
        .filter_map(|path| {
            let text = read_text(path)?;
            let (total, rows) = rows_of(regex, &text, context);
            (total > 0).then(|| FileScan {
                path: path.to_string_lossy().into_owned(),
                total,
                rows,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| (*item).to_owned()).collect()
    }

    #[test]
    fn bre_backslash_pipe_becomes_alternation() {
        assert_eq!(translate_bre(r"helper\|FOOTER"), "helper|FOOTER");
    }

    #[test]
    fn an_escaped_backslash_before_a_pipe_is_left_alone() {
        assert_eq!(translate_bre(r"a\\|b"), r"a\\|b");
    }

    #[test]
    fn other_escapes_and_a_trailing_backslash_pass_through() {
        assert_eq!(translate_bre(r"\w+\("), r"\w+\(");
        assert_eq!(translate_bre("x\\"), "x\\");
    }

    #[test]
    fn several_patterns_are_joined_as_alternatives() {
        let source = pattern_source(&strings(&["mod walk", "FOOTER"]), false, false, true);
        assert_eq!(source, "(?:mod walk)|(?:FOOTER)");
    }

    #[test]
    fn fixed_patterns_are_escaped_and_words_are_bounded() {
        let source = pattern_source(&strings(&["a.b"]), true, true, true);
        assert_eq!(source, r"\b(?:a\.b)\b");
    }

    #[test]
    fn basic_mode_keeps_backslash_pipe_for_the_regex_engine() {
        let source = pattern_source(&strings(&[r"a\|b"]), false, false, false);
        assert_eq!(source, r"a\|b");
    }

    #[test]
    fn rows_carry_context_without_repeating_a_line_shared_by_two_matches() {
        let regex = Regex::new("hit").expect("pattern");
        let text = "one\nhit\nthree\nhit\nfive\nsix\nseven\n";

        let (total, rows) = rows_of(
            &regex,
            text,
            Context {
                before: 1,
                after: 1,
            },
        );

        let seen: Vec<(usize, bool)> = rows.iter().map(|row| (row.line, row.matched)).collect();
        assert_eq!(total, 2);
        assert_eq!(
            seen,
            vec![(1, false), (2, true), (3, false), (4, true), (5, false)]
        );
    }

    #[test]
    fn rows_without_context_are_only_the_matching_lines() {
        let regex = Regex::new("hit").expect("pattern");

        let (total, rows) = rows_of(&regex, "a\nhit\nb\n", Context::default());

        assert_eq!((total, rows.len()), (1, 1));
        assert_eq!(rows[0].text, "hit");
    }
}

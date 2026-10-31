//! `ss find`: classify a query by fixed rules, run the useful combination, name the rule in the header.
//!
//! Pure functions over `exact::scan`, `read::outline_file` and `render::tokens`; no daemon and no store. The two
//! optional legs that need one (semantic file ranking, SCIP graph text) arrive as plain data from the caller.
//! See `ai_docs/find.md`.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::Path;

use serde_json::json;
use thiserror::Error;

use crate::exact::{ExactError, FileMatches, clipped, compile, read_text, scan};
use crate::read::outline_file;
use crate::render::tokens;
use crate::walk::list_files;

/// Default output budget in tokens (chars / 4).
pub const FIND_BUDGET: usize = 500;
/// Files shown at most.
pub const MAX_FILES: usize = 5;
/// Lines or symbols shown per file at most.
pub const MAX_LINES: usize = 3;
const RRF_K: f64 = 60.0;
const PATH_WEIGHT: usize = 3;
const PATH_MIN: usize = 4;
const PATH_SUFFIX: usize = 3;
const PATH_SHARE_DIV: usize = 4;
const PURPOSE_SCAN: usize = 30;
const GRAPH_LINES: usize = 6;
const EXTENSIONS: [&str; 16] = [
    "rs", "py", "ts", "tsx", "js", "go", "md", "mdx", "rst", "txt", "toml", "json", "yaml", "yml", "sh", "c",
];
const STOPWORDS: &str = "a an the of to in on for and or is are was be been it its that this these those with so only by as at \
from when how what which who does do did into not no if then than up per each all any can will would should has have had i you \
we they them their our your";

#[derive(Debug, Error)]
pub enum FindError {
    #[error(transparent)]
    Pattern(#[from] ExactError),
    #[error("{0}")]
    Read(#[from] crate::read::ReadError),
}

/// Which fixed rule picked the branch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    Path,
    Ident,
    Regex,
    Phrase,
}

impl Shape {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Path => "PATH",
            Self::Ident => "IDENT",
            Self::Regex => "REGEX",
            Self::Phrase => "PHRASE",
        }
    }
}

/// One file in the answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileHit {
    pub path: String,
    pub range: (usize, usize),
    pub purpose: String,
    pub lines: Vec<(usize, String)>,
    pub tags: Vec<&'static str>,
}

/// Everything `find` learned; `render_text` and `render_json` shape it.
#[derive(Debug, Clone)]
pub struct Found {
    pub shape: Shape,
    pub query: String,
    pub hits: Vec<FileHit>,
    pub beyond: usize,
    pub files: usize,
    pub matches: usize,
    pub definitions: Option<usize>,
    pub body: Option<String>,
    pub notes: Vec<String>,
    pub ident: Option<String>,
    pub graph: Option<String>,
}

impl Found {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.hits.is_empty() && self.body.is_none()
    }
}

struct Cand {
    path: String,
    named: usize,
    score: usize,
    total: usize,
    range: Option<(usize, usize)>,
    lines: Vec<(usize, String)>,
    tags: Vec<&'static str>,
}

fn is_ident(query: &str) -> bool {
    let mut chars = query.chars();
    chars.next().is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
        && chars.all(|letter| letter.is_ascii_alphanumeric() || letter == '_')
}

fn has_meta(query: &str) -> bool {
    query.chars().any(|letter| "\\.*+?()[]{}|^$".contains(letter))
}

fn path_target(root: &Path, query: &str) -> Option<std::path::PathBuf> {
    let known = query.rsplit_once('.').is_some_and(|(_, ext)| EXTENSIONS.contains(&ext));
    if query.contains(char::is_whitespace) || !(query.contains('/') || known) {
        return None;
    }
    let candidate = root.join(query);
    candidate.is_file().then_some(candidate)
}

/// The rule that applies to `query` under `root`; the first match wins: path, identifier, regex, phrase.
#[must_use]
pub fn classify(root: &Path, query: &str) -> Shape {
    let query = query.trim();
    if path_target(root, query).is_some() {
        Shape::Path
    } else if is_ident(query) {
        Shape::Ident
    } else if has_meta(query) {
        Shape::Regex
    } else {
        Shape::Phrase
    }
}

fn relative(root: &Path, path: &str) -> String {
    Path::new(path).strip_prefix(root).map_or_else(|_| path.to_owned(), |rest| rest.to_string_lossy().into_owned())
}

fn cand_of(root: &Path, matches: &FileMatches, tag: &'static str) -> Cand {
    let range = matches.lines.first().zip(matches.lines.last()).map(|(first, last)| (first.0, last.0));
    Cand { path: relative(root, &matches.path), named: 0, score: 1, total: matches.total, range, lines: matches.lines.clone(), tags: vec![tag] }
}

fn by_total(mut cands: Vec<Cand>) -> Vec<Cand> {
    cands.sort_by(|left, right| right.total.cmp(&left.total).then_with(|| left.path.cmp(&right.path)));
    cands
}

fn ident_cands(root: &Path, ident: &str, notes: &mut Vec<String>) -> Result<(Vec<Cand>, usize), FindError> {
    let kinds = "fn|struct|enum|trait|impl|type|const|class|def|function|interface";
    let defs = scan(root, &compile(&format!(r"\b(?:{kinds})\s+{ident}\b"), false, false)?, MAX_LINES);
    let all = scan(root, &compile(&format!(r"\b{ident}\b"), false, false)?, MAX_LINES);
    let defined: BTreeSet<&str> = defs.iter().map(|file| file.path.as_str()).collect();
    let others: Vec<&FileMatches> = all.iter().filter(|file| !defined.contains(file.path.as_str())).collect();
    let count: usize = others.iter().map(|file| file.total).sum();
    notes.push(format!("{count} other occurrences in {} files", others.len()));
    let definitions = defs.len();
    let mut ranked = by_total(defs.iter().map(|file| cand_of(root, file, "exact")).collect());
    ranked.extend(by_total(others.into_iter().map(|file| cand_of(root, file, "exact")).collect()));
    Ok((ranked, definitions))
}

fn regex_cands(root: &Path, query: &str) -> Result<Vec<Cand>, FindError> {
    let files = scan(root, &compile(query, false, false)?, MAX_LINES);
    Ok(by_total(files.iter().map(|file| cand_of(root, file, "exact")).collect()))
}

/// Terms of `query` by the R1 method: stopwords dropped, identifiers kept whole and split on underscore and case.
#[must_use]
pub fn terms_of(query: &str) -> Vec<String> {
    let mut terms: Vec<String> = Vec::new();
    for token in query.split(|letter: char| !(letter.is_ascii_alphanumeric() || letter == '_')).filter(|part| !part.is_empty()) {
        let parts = split_words(token);
        let compound = token.contains('_') || token.chars().zip(token.chars().skip(1)).any(|(low, high)| low.is_ascii_lowercase() && high.is_ascii_uppercase());
        if compound && !is_stop(&token.to_lowercase()) {
            terms.push(token.to_lowercase());
        }
        terms.extend(parts.into_iter().filter(|word| !is_stop(word)));
    }
    let mut seen = BTreeSet::new();
    terms.retain(|term| seen.insert(term.clone()));
    terms
}

fn is_stop(word: &str) -> bool {
    STOPWORDS.split_whitespace().any(|stop| stop == word)
}

fn split_words(token: &str) -> Vec<String> {
    let mut spaced = String::new();
    let mut previous = ' ';
    for letter in token.chars() {
        if (previous.is_ascii_lowercase() || previous.is_ascii_digit()) && letter.is_ascii_uppercase() {
            spaced.push(' ');
        }
        spaced.push(if letter == '_' { ' ' } else { letter });
        previous = letter;
    }
    spaced.split_whitespace().map(str::to_lowercase).collect()
}

fn path_parts(rel: &str) -> Vec<String> {
    let lower = rel.to_lowercase();
    lower.split(|letter: char| !letter.is_ascii_alphanumeric()).filter(|part| part.len() >= PATH_MIN).map(str::to_owned).collect()
}

/// Prefix in either direction within a few letters, so `personas` meets `persona` but `orchestrator` does not meet `orch`.
fn names_part(term: &str, parts: &[String]) -> bool {
    let close = |part: &str| part.len().abs_diff(term.len()) <= PATH_SUFFIX && (part.starts_with(term) || term.starts_with(part));
    term.len() >= PATH_MIN && parts.iter().any(|part| close(part))
}

/// Terms that name a path component of at most a quarter of the files: `queue` or `personas` point at an owner,
/// `trial` in a corpus that is mostly `trial/` points at everything and so at nothing.
fn path_terms(files: &[String], terms: &[String]) -> Vec<String> {
    let parts: Vec<Vec<String>> = files.iter().map(|rel| path_parts(rel)).collect();
    let limit = (files.len() / PATH_SHARE_DIV).max(1);
    terms.iter().filter(|term| parts.iter().filter(|found| names_part(term, found)).count() <= limit).cloned().collect()
}

fn term_scan_file(root: &Path, path: &Path, terms: &[String], named_terms: &[String]) -> Option<Cand> {
    let rel = relative(root, &path.to_string_lossy());
    let parts = path_parts(&rel);
    let named = named_terms.iter().filter(|term| names_part(term, &parts)).count();
    let text = match read_text(path) {
        Some(text) => text,
        None if named > 0 => String::new(),
        None => return None,
    };
    let mut seen: BTreeSet<usize> = BTreeSet::new();
    let mut total = 0;
    let mut lines: Vec<(usize, usize, String)> = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let lower = line.to_lowercase();
        let matched: Vec<usize> = (0..terms.len()).filter(|term| lower.contains(terms[*term].as_str())).collect();
        if matched.is_empty() {
            continue;
        }
        total += 1;
        seen.extend(matched.iter().copied());
        lines.push((matched.len(), index + 1, clipped(line)));
    }
    lines.sort_by(|left, right| right.0.cmp(&left.0).then(left.1.cmp(&right.1)));
    lines.truncate(MAX_LINES);
    lines.sort_by_key(|row| row.1);
    let range = lines.first().zip(lines.last()).map(|(first, last)| (first.1, last.1));
    let kept = lines.into_iter().map(|(_, number, text)| (number, text)).collect();
    (total > 0 || named > 0).then(|| Cand { path: rel, named, score: seen.len() + PATH_WEIGHT * named, total, range, lines: kept, tags: vec!["term-match"] })
}

fn phrase_cands(root: &Path, query: &str, semantic: &[(String, usize, usize)]) -> Vec<Cand> {
    let terms = terms_of(query);
    let files = list_files(root);
    let rels: Vec<String> = files.iter().map(|path| relative(root, &path.to_string_lossy())).collect();
    let named_terms = path_terms(&rels, &terms);
    let mut cands: Vec<Cand> = files.iter().filter_map(|path| term_scan_file(root, path, &terms, &named_terms)).collect();
    cands.sort_by(|left, right| right.score.cmp(&left.score).then(right.total.cmp(&left.total)).then_with(|| left.path.cmp(&right.path)));
    if semantic.is_empty() { cands } else { fuse(root, cands, semantic) }
}

/// A path component naming a query term counts as one more list in which the file is first.
fn named_share(named: usize) -> f64 {
    f64::from(u32::try_from(named).unwrap_or(u32::MAX)) * rrf_share(0)
}

fn rrf_share(rank: usize) -> f64 {
    1.0 / (RRF_K + f64::from(u32::try_from(rank).unwrap_or(u32::MAX)) + 1.0)
}

/// Reciprocal rank fusion (k = 60) of the term-match order and the semantic file order, at file level.
fn fuse(root: &Path, cands: Vec<Cand>, semantic: &[(String, usize, usize)]) -> Vec<Cand> {
    let mut scores: BTreeMap<String, f64> = BTreeMap::new();
    let mut table: BTreeMap<String, Cand> = BTreeMap::new();
    for (rank, cand) in cands.into_iter().enumerate() {
        *scores.entry(cand.path.clone()).or_default() += rrf_share(rank) + named_share(cand.named);
        table.insert(cand.path.clone(), cand);
    }
    let mut files: Vec<(String, usize, usize)> = Vec::new();
    for (path, start, end) in semantic {
        let rel = relative(root, path);
        if !files.iter().any(|seen| seen.0 == rel) {
            files.push((rel, *start, *end));
        }
    }
    for (rank, (rel, start, end)) in files.into_iter().enumerate() {
        *scores.entry(rel.clone()).or_default() += rrf_share(rank);
        let entry = table.entry(rel.clone()).or_insert_with(|| Cand { path: rel, named: 0, score: 0, total: 0, range: Some((start, end)), lines: Vec::new(), tags: Vec::new() });
        entry.tags.push("semantic");
    }
    let mut ranked: Vec<(String, f64)> = scores.into_iter().collect();
    ranked.sort_by(|left, right| right.1.total_cmp(&left.1).then_with(|| left.0.cmp(&right.0)));
    ranked.into_iter().filter_map(|(path, _)| table.remove(&path)).collect()
}

fn purpose_of(path: &Path) -> String {
    let Some(text) = read_text(path) else { return "-".to_owned() };
    for line in text.lines().take(PURPOSE_SCAN) {
        let line = line.trim();
        let found = line.strip_prefix("//!").or_else(|| line.strip_prefix("# ")).or_else(|| line.strip_prefix("\"\"\""));
        if let Some(rest) = found.map(str::trim).filter(|rest| !rest.is_empty()) {
            return clipped(rest.trim_end_matches("\"\"\""));
        }
    }
    "-".to_owned()
}

fn finish(root: &Path, shape: Shape, query: &str, cands: Vec<Cand>) -> Found {
    let (files, matches) = (cands.len(), cands.iter().map(|cand| cand.total).sum());
    let beyond = cands.len().saturating_sub(MAX_FILES);
    let hits = cands.into_iter().take(MAX_FILES).map(|cand| {
        let purpose = purpose_of(&root.join(&cand.path));
        let range = cand.range.unwrap_or((1, 1));
        FileHit { path: cand.path, range, purpose, lines: cand.lines, tags: cand.tags }
    });
    let ident = (shape == Shape::Ident).then(|| query.to_owned());
    Found { shape, query: query.to_owned(), hits: hits.collect(), beyond, files, matches, definitions: None, body: None, notes: Vec::new(), ident, graph: None }
}

/// Answer `query` under `root`. `semantic` is the daemon's file order (path, start, end) or empty; only a phrase uses it.
pub fn find(root: &Path, query: &str, semantic: &[(String, usize, usize)]) -> Result<Found, FindError> {
    let query = query.trim();
    let shape = classify(root, query);
    let mut notes = Vec::new();
    let mut definitions = None;
    let cands = match shape {
        Shape::Path => return path_found(root, query),
        Shape::Ident => {
            let (cands, defs) = ident_cands(root, query, &mut notes)?;
            definitions = Some(defs);
            cands
        }
        Shape::Regex => regex_cands(root, query)?,
        Shape::Phrase => phrase_cands(root, query, semantic),
    };
    let mut found = finish(root, shape, query, cands);
    found.notes = notes;
    found.definitions = definitions;
    Ok(found)
}

fn path_found(root: &Path, query: &str) -> Result<Found, FindError> {
    let mut found = finish(root, Shape::Path, query, Vec::new());
    found.body = Some(outline_file(&root.join(query), crate::read::OUTLINE_BUDGET, None)?);
    Ok(found)
}

fn tag_text(tags: &[&str]) -> String {
    if tags.is_empty() { String::new() } else { format!("  [{}]", tags.join(",")) }
}

fn file_block(hit: &FileHit, lines: usize) -> String {
    let mut text = format!("{}:{}-{}  {}{}\n", hit.path, hit.range.0, hit.range.1, hit.purpose, tag_text(&hit.tags));
    for (number, line) in hit.lines.iter().take(lines) {
        let _ = writeln!(text, "  {number}: {line}");
    }
    text
}

fn next_line(found: &Found) -> Option<String> {
    let top = found.hits.first()?;
    if let Some(ident) = &found.ident {
        return Some(format!("next: ss section {} {ident}; ss graph {ident}\n", top.path));
    }
    Some(top.lines.first().map_or_else(|| format!("next: ss outline {}\n", top.path), |line| format!("next: ss section {} --line {}\n", top.path, line.0)))
}

fn graph_block(found: &Found) -> String {
    let Some(graph) = &found.graph else { return String::new() };
    let head: Vec<&str> = graph.lines().take(GRAPH_LINES).collect();
    format!("graph [resolved-fresh]:\n{}\n", head.join("\n"))
}

fn assemble(found: &Found, files: usize, lines: usize) -> String {
    let tags: BTreeSet<&str> = found.hits.iter().flat_map(|hit| hit.tags.iter().copied()).collect();
    let tagged = tags.into_iter().collect::<Vec<_>>().join("+");
    let mut text = format!("find: {} \"{}\" [{}]\n", found.shape.as_str(), found.query, tagged_or_syntactic(found, &tagged));
    text.push_str(&found_line(found));
    if let Some(body) = &found.body {
        text.push_str(body);
        return text;
    }
    for hit in found.hits.iter().take(files) {
        text.push_str(&file_block(hit, lines));
    }
    for note in &found.notes {
        let _ = writeln!(text, "note: {note}");
    }
    text.push_str(&graph_block(found));
    text.push_str(&next_line(found).unwrap_or_default());
    let dropped_files = found.hits.len() - files + found.beyond;
    let dropped_lines: usize = found.hits.iter().take(files).map(|hit| hit.lines.len().saturating_sub(lines)).sum();
    if dropped_files + dropped_lines > 0 {
        let _ = writeln!(text, "cut: {dropped_files} files, {dropped_lines} lines");
    }
    text
}

/// The sufficiency line: files and matches found, and for an identifier whether a definition was.
#[must_use]
pub fn found_line(found: &Found) -> String {
    if found.body.is_some() {
        return "found: 1 file\n".to_owned();
    }
    if found.files == 0 {
        return "found: 0\n".to_owned();
    }
    let definition = match found.definitions {
        None => String::new(),
        Some(0) => "; definition: no".to_owned(),
        Some(1) => "; definition: yes (1)".to_owned(),
        Some(count) => format!("; definition: ambiguous ({count})"),
    };
    format!("found: {} files, {} matches{definition}\n", found.files, found.matches)
}

fn tagged_or_syntactic(found: &Found, tagged: &str) -> String {
    if found.body.is_some() { "syntactic".to_owned() } else { tagged.to_owned() }
}

/// Text output within `budget` tokens: tail files go first, then lines; the top file always prints.
#[must_use]
pub fn render_text(found: &Found, budget: usize) -> String {
    let (mut files, mut lines) = (found.hits.len(), MAX_LINES);
    while tokens(&assemble(found, files, lines)) > budget && found.body.is_none() {
        if files > 1 {
            files -= 1;
        } else if lines > 0 {
            lines -= 1;
        } else {
            break;
        }
    }
    assemble(found, files, lines)
}

/// The same fields as JSON, one document.
#[must_use]
pub fn render_json(found: &Found) -> String {
    let files: Vec<_> = found
        .hits
        .iter()
        .map(|hit| json!({"path": hit.path, "start": hit.range.0, "end": hit.range.1, "purpose": hit.purpose, "tags": hit.tags, "lines": hit.lines}))
        .collect();
    let next = next_line(found).map(|line| line.trim_start_matches("next: ").trim().to_owned());
    json!({"shape": found.shape.as_str(), "query": found.query, "found": {"files": found.files, "matches": found.matches, "definitions": found.definitions}, "files": files, "cut": found.beyond, "notes": found.notes, "next": next, "outline": found.body}).to_string()
}

#[cfg(test)]
mod tests;

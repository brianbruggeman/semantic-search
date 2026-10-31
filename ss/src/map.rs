//! Budgeted, structure-first map of a directory tree behind `ss map`; design in `ai_docs/map.md`.
//!
//! No daemon and no index: the gitignore-aware walker (`walk::list_files`) lists files, `purpose` reads one line
//! of intent per file from text already on disk (leading docs, headings, the TOML description, or the first
//! symbol's doc through `read::structure`). Pure functions over text, so the budget (tokens = chars / 4 of the
//! output) is the quantity the measurements report. Before: `ls -R` plus the head of N files. After: `ss map DIR`.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;

use serde_json::{Value, json};

use crate::read::structure;
use crate::walk::list_files;

/// Default budget in tokens of a map.
pub const MAP_BUDGET: usize = 600;
const MAX_FILE_BYTES: u64 = 1 << 20;
const PURPOSE_CHARS: usize = 100;
const BINARY_PROBE: usize = 8192;
const CUT_RESERVE_CHARS: usize = 160;

/// One scanned file: relative path, line count, `ceil(bytes / 4)` tokens and the purpose when one was found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MapFile {
    pub path: String,
    pub lines: usize,
    pub tokens: usize,
    pub purpose: Option<String>,
}

/// The files directly inside one directory (`.` is the root), with their summed tokens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MapDir {
    pub path: String,
    pub files: Vec<MapFile>,
    pub tokens: usize,
}

/// A selected map: directories by tokens descending, what the budget and depth dropped, and coverage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Map {
    pub root: String,
    pub dirs: Vec<MapDir>,
    pub counts: Vec<(usize, usize)>,
    pub cut: usize,
    pub with_purpose: usize,
    pub total: usize,
}

/// Scan `root` and select a map of at most `budget` tokens, directories deeper than `depth` left out.
#[must_use]
pub fn map_dir(root: &Path, budget: usize, depth: Option<usize>) -> Map {
    let files: Vec<MapFile> = list_files(root)
        .iter()
        .filter_map(|path| scan_file(root, path))
        .collect();
    select(&root.display().to_string(), group(files, depth), budget)
}

fn scan_file(root: &Path, path: &Path) -> Option<MapFile> {
    if std::fs::metadata(path).ok()?.len() > MAX_FILE_BYTES {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    if bytes.iter().take(BINARY_PROBE).any(|byte| *byte == 0) {
        return None;
    }
    let text = String::from_utf8(bytes).ok()?;
    let relative = path.strip_prefix(root).ok()?.to_string_lossy().into_owned();
    Some(MapFile {
        lines: text.lines().count(),
        tokens: text.len().div_ceil(4),
        purpose: purpose(&relative, &text),
        path: relative,
    })
}

fn dir_of(path: &str) -> &str {
    path.rsplit_once('/').map_or(".", |(dir, _)| dir)
}

fn dir_depth(dir: &str) -> usize {
    if dir == "." { 0 } else { dir.split('/').count() }
}

fn group(files: Vec<MapFile>, depth: Option<usize>) -> (Vec<MapDir>, usize) {
    let mut by_dir: BTreeMap<String, Vec<MapFile>> = BTreeMap::new();
    let mut hidden = 0;
    for file in files {
        let dir = dir_of(&file.path).to_owned();
        if depth.is_some_and(|limit| dir_depth(&dir) > limit) {
            hidden += 1;
        } else {
            by_dir.entry(dir).or_default().push(file);
        }
    }
    let mut dirs: Vec<MapDir> = by_dir
        .into_iter()
        .map(|(path, mut members)| {
            members.sort_by(|left, right| right.tokens.cmp(&left.tokens).then(left.path.cmp(&right.path)));
            MapDir { tokens: members.iter().map(|file| file.tokens).sum(), path, files: members }
        })
        .collect();
    dirs.sort_by(|left, right| right.tokens.cmp(&left.tokens).then(left.path.cmp(&right.path)));
    (dirs, hidden)
}

fn dir_line(dir: &MapDir, files: usize) -> String {
    let name = if dir.path == "." { "./".to_owned() } else { format!("{}/", dir.path) };
    format!("{name} ({files} files, {} tokens)\n", dir.tokens)
}

fn file_line(file: &MapFile) -> String {
    let purpose = file.purpose.as_deref().unwrap_or("-");
    format!("{}  {}  ~{}  {purpose}\n", file.path, file.lines, file.tokens)
}

fn select(root: &str, grouped: (Vec<MapDir>, usize), budget: usize) -> Map {
    let (dirs, hidden) = grouped;
    let total: usize = dirs.iter().map(|dir| dir.files.len()).sum::<usize>() + hidden;
    let with_purpose = dirs.iter().flat_map(|dir| &dir.files).filter(|file| file.purpose.is_some()).count();
    let room = (budget * 4).saturating_sub(CUT_RESERVE_CHARS);
    let mut used = 0;
    let mut kept: Vec<MapDir> = Vec::new();
    let mut counts = Vec::new();
    let mut cut = hidden;
    for dir in dirs {
        let header = dir_line(&dir, dir.files.len());
        let mut shown = Vec::new();
        for file in &dir.files {
            let cost = file_line(file).len() + if shown.is_empty() { header.len() } else { 0 };
            if used + cost > room && !(kept.is_empty() && shown.is_empty()) {
                cut += 1;
            } else {
                used += cost;
                shown.push(file.clone());
            }
        }
        cut += usize::from(shown.is_empty());
        if !shown.is_empty() {
            counts.push((dir.files.len(), dir.tokens));
            kept.push(MapDir { path: dir.path, files: shown, tokens: dir.tokens });
        }
    }
    Map { root: root.to_owned(), dirs: kept, counts, cut, with_purpose, total }
}

fn percent(with_purpose: usize, total: usize) -> usize {
    if total == 0 { 0 } else { with_purpose * 100 / total }
}

/// The default text output of a map: directory lines, file lines, the `cut:` line and the coverage line.
#[must_use]
pub fn map_text(map: &Map) -> String {
    let mut out = String::new();
    for (dir, (files, _)) in map.dirs.iter().zip(&map.counts) {
        out.push_str(&dir_line(dir, *files));
        dir.files.iter().for_each(|file| out.push_str(&file_line(file)));
    }
    if map.cut > 0 {
        let _ = writeln!(out, "cut: {} entries not shown; --budget N, --depth D, or ss map SUBDIR", map.cut);
    }
    let _ = writeln!(
        out,
        "purpose coverage: {} of {} files have one ({}%)",
        map.with_purpose,
        map.total,
        percent(map.with_purpose, map.total)
    );
    out
}

/// The stable JSON document of a map; schema in `ai_docs/map.md`.
#[must_use]
pub fn map_json(map: &Map) -> Value {
    let dirs: Vec<Value> = map
        .dirs
        .iter()
        .zip(&map.counts)
        .map(|(dir, (files, _))| json!({"path": dir.path, "files": files, "tokens": dir.tokens}))
        .collect();
    let files: Vec<Value> = map
        .dirs
        .iter()
        .flat_map(|dir| dir.files.iter().map(move |file| (dir, file)))
        .map(|(dir, file)| {
            json!({"path": file.path, "dir": dir.path, "lines": file.lines, "tokens": file.tokens, "purpose": file.purpose})
        })
        .collect();
    json!({
        "root": map.root,
        "dirs": dirs,
        "files": files,
        "cut": map.cut,
        "coverage": {"with": map.with_purpose, "total": map.total, "percent": percent(map.with_purpose, map.total)},
    })
}

/// One line of intent for a file, or `None`; rules and order in `ai_docs/map.md`.
#[must_use]
pub fn purpose(path: &str, text: &str) -> Option<String> {
    let name = path.rsplit('/').next().unwrap_or(path);
    let found = match name {
        "Cargo.toml" => toml_description(text, "package"),
        "pyproject.toml" => toml_description(text, "project"),
        _ => leading_doc(text).or_else(|| structural(path, text)),
    };
    found.and_then(|raw| sentence(&raw))
}

fn structural(path: &str, text: &str) -> Option<String> {
    let entries = structure(path, text);
    let first = entries.first()?;
    let is_heading = first.label.starts_with('#') || first.label.chars().next().is_some_and(|mark| mark.is_ascii_digit());
    if is_heading {
        return Some(if first.gist.is_empty() { first.name.clone() } else { format!("{}: {}", first.name, first.gist) });
    }
    entries.iter().find(|entry| entry.depth == 0).map(|entry| entry.gist.clone())
}

fn toml_description(text: &str, table: &str) -> Option<String> {
    let header = format!("[{table}]");
    let mut inside = false;
    for line in text.lines().map(str::trim) {
        if line.starts_with('[') {
            inside = line == header;
        } else if inside && let Some(rest) = line.strip_prefix("description") {
            let value = rest.trim_start().strip_prefix('=')?.trim();
            return Some(value.trim_matches(|quote| quote == '"' || quote == '\'').to_owned());
        }
    }
    None
}

fn leading_doc(text: &str) -> Option<String> {
    let body = text
        .lines()
        .skip_while(|line| line.starts_with("#!") || line.trim().is_empty())
        .collect::<Vec<_>>();
    let first = body.first()?.trim_start();
    if first.starts_with("//!") {
        let lines = body.iter().map(|line| line.trim_start()).take_while(|line| line.starts_with("//!"));
        return Some(lines.map(|line| line.trim_start_matches("//!").trim()).collect::<Vec<_>>().join("\n"));
    }
    if first.starts_with("/*") {
        return block_comment(&body.join("\n"));
    }
    docstring(&body.join("\n"))
}

fn block_comment(text: &str) -> Option<String> {
    let inner = text.trim_start().strip_prefix("/*")?;
    let inner = inner.split_once("*/").map_or(inner, |(head, _)| head);
    let lines = inner.lines().map(|line| line.trim().trim_start_matches('*').trim());
    Some(lines.collect::<Vec<_>>().join("\n"))
}

fn docstring(text: &str) -> Option<String> {
    let trimmed = text.trim_start();
    let quote = ["\"\"\"", "'''"].into_iter().find(|quote| trimmed.starts_with(quote))?;
    let inner = &trimmed[quote.len()..];
    Some(inner.split_once(quote).map_or(inner, |(head, _)| head).trim().to_owned())
}

fn sentence(raw: &str) -> Option<String> {
    let paragraph: Vec<&str> = raw
        .lines()
        .map(str::trim)
        .skip_while(|line| line.is_empty())
        .take_while(|line| !line.is_empty())
        .collect();
    let joined = paragraph.join(" ");
    let first = joined.split_once(". ").map_or(joined.as_str(), |(head, _)| head);
    let cleaned = first.trim().trim_end_matches('.').trim();
    (!cleaned.is_empty()).then(|| cleaned.chars().take(PURPOSE_CHARS).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        for (name, body) in files {
            let path = dir.path().join(name);
            std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
            std::fs::write(path, body).expect("write");
        }
        dir
    }

    fn find<'a>(map: &'a Map, path: &str) -> Option<&'a MapFile> {
        map.dirs.iter().flat_map(|dir| &dir.files).find(|file| file.path == path)
    }

    #[test]
    fn rust_module_doc_is_the_purpose_and_missing_doc_falls_to_first_item_or_dash() {
        let documented = "//! Walks the tree. More detail follows.\n\npub fn walk() {}\n";
        let item_doc = "/// Counts things.\npub fn count() {}\n";
        let bare = "pub fn bare() {}\n";

        assert_eq!(purpose("a.rs", documented).as_deref(), Some("Walks the tree"));
        assert_eq!(purpose("b.rs", item_doc).as_deref(), Some("Counts things"));
        assert_eq!(purpose("c.rs", bare), None);
    }

    #[test]
    fn python_docstring_markdown_heading_and_toml_description_give_purposes() {
        let python = "\"\"\"Greets people.\"\"\"\n\ndef greet(name):\n    return name\n";
        let markdown = "# Map guide\n\nHow to read a map.\n\n## More\n";
        let cargo = "[package]\nname = \"ss\"\ndescription = \"Semantic search\"\n\n[dependencies]\n";
        let pyproject = "[project]\nname = \"x\"\ndescription = 'A tool'\n";

        assert_eq!(purpose("m.py", python).as_deref(), Some("Greets people"));
        assert_eq!(purpose("g.md", markdown).as_deref(), Some("Map guide: How to read a map"));
        assert_eq!(purpose("sub/Cargo.toml", cargo).as_deref(), Some("Semantic search"));
        assert_eq!(purpose("pyproject.toml", pyproject).as_deref(), Some("A tool"));
        assert_eq!(purpose("Cargo.toml", "[package]\nname = \"x\"\n"), None);
    }

    #[test]
    fn leading_block_comment_is_a_purpose_and_long_text_truncates_to_100_chars() {
        let block = "/*\n * Parses the config.\n */\nint main() {}\n";
        let long = format!("//! {}\n", "word ".repeat(40));

        assert_eq!(purpose("c.c", block).as_deref(), Some("Parses the config"));
        assert_eq!(purpose("d.rs", &long).map(|text| text.chars().count()), Some(100));
    }

    #[test]
    fn map_skips_binary_gitignored_and_large_files_and_counts_exact_coverage() {
        let dir = tree(&[
            ("Cargo.toml", "[package]\ndescription = \"demo\"\n"),
            ("src/lib.rs", "//! Library root.\npub fn x() {}\n"),
            ("src/bare.rs", "pub fn y() {}\n"),
            ("docs/guide.md", "# Guide\nRead me.\n"),
            ("target/out.rs", "//! ignored\n"),
            (".gitignore", "target/\n"),
        ]);
        std::fs::write(dir.path().join("blob.bin"), [0u8, 159, 146, 150]).expect("binary");
        std::fs::write(dir.path().join("huge.txt"), vec![b'a'; (1 << 20) + 1]).expect("huge");

        let map = map_dir(dir.path(), MAP_BUDGET, None);

        assert_eq!(map.total, 4, "{map:?}");
        assert_eq!(map.with_purpose, 3);
        assert!(find(&map, "blob.bin").is_none() && find(&map, "huge.txt").is_none());
        assert!(find(&map, "target/out.rs").is_none());
        assert!(map_text(&map).ends_with("purpose coverage: 3 of 4 files have one (75%)\n"));
        assert!(map_text(&map).contains("src/bare.rs  1  ~4  -\n"));
    }

    #[test]
    fn budget_cut_line_counts_dropped_entries_and_names_the_flags() {
        let files: Vec<(String, String)> = (0..30)
            .map(|index| (format!("d{index}/f.rs"), format!("//! File number {index} does its work.\n")))
            .collect();
        let pairs: Vec<(&str, &str)> = files.iter().map(|(name, body)| (name.as_str(), body.as_str())).collect();
        let dir = tree(&pairs);

        let map = map_dir(dir.path(), 100, None);
        let text = map_text(&map);

        assert!(map.cut > 0 && map.cut < 60, "{}", map.cut);
        assert!(text.contains(&format!("cut: {} entries not shown; --budget N, --depth D, or ss map SUBDIR", map.cut)));
        assert!(text.len().div_ceil(4) <= 100, "{}", text.len());
        assert_eq!(map.total, 30);
    }

    #[test]
    fn depth_limit_drops_deeper_directories_but_keeps_them_in_coverage() {
        let dir = tree(&[("top.rs", "//! Top.\n"), ("a/mid.rs", "//! Mid.\n"), ("a/b/deep.rs", "//! Deep.\n")]);

        let map = map_dir(dir.path(), MAP_BUDGET, Some(1));

        assert!(find(&map, "a/mid.rs").is_some() && find(&map, "a/b/deep.rs").is_none());
        assert_eq!((map.cut, map.with_purpose, map.total), (1, 2, 3));
    }

    #[test]
    fn json_document_carries_files_dirs_cut_and_coverage() {
        let dir = tree(&[("src/lib.rs", "//! Root.\n"), ("src/bare.rs", "pub fn a() {}\n")]);

        let value = map_json(&map_dir(dir.path(), MAP_BUDGET, None));

        assert_eq!(value["files"].as_array().map(Vec::len), Some(2));
        assert_eq!(value["dirs"][0]["path"], "src");
        assert_eq!(value["coverage"], json!({"with": 1, "total": 2, "percent": 50}));
        assert_eq!(value["files"][0]["purpose"].is_null() || value["files"][1]["purpose"].is_null(), true);
    }
}

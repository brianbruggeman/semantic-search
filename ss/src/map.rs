//! Budgeted, structure-first map of a directory tree behind `ss map`; design in `ai_docs/map.md`.
//!
//! No daemon and no index: the gitignore-aware walker (`walk::list_files`) lists files, `purpose` reads one line
//! of intent per file from text already on disk (leading docs, headings, the TOML description, or the first
//! symbol's doc through `read::structure`). Pure functions over text, so the budget (tokens = chars / 4 of the
//! output) is the quantity the measurements report. Before: `ls -R` plus the head of N files. After: `ss map DIR`.

use std::cmp::Reverse;
use std::collections::{BTreeMap, HashSet};
use std::fmt::Write as _;
use std::path::Path;

use serde_json::{Value, json};

use crate::read::structure;
use crate::walk::list_files;

/// Default budget in tokens of a map.
pub const MAP_BUDGET: usize = 600;
const MAX_FILE_BYTES: u64 = 1 << 20;
const PURPOSE_CHARS: usize = 100;
const MIN_PURPOSE_CHARS: usize = 4;
const MIN_PROSE_WORDS: usize = 4;
const BINARY_PROBE: usize = 8192;
const CUT_RESERVE_CHARS: usize = 160;
const TOKENS_PER_DEPTH: usize = 250;
const README_STEMS: [&str; 2] = ["readme", "index"];

/// One scanned file: relative path, line count, `ceil(bytes / 4)` tokens and the purpose when one was found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MapFile {
    pub path: String,
    pub lines: usize,
    pub tokens: usize,
    pub purpose: Option<String>,
}

/// One printed line: a file, or a directory with the recursive file count, lines and tokens beneath it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MapEntry {
    pub path: String,
    pub is_dir: bool,
    pub lines: usize,
    pub files: usize,
    pub tokens: usize,
    pub purpose: Option<String>,
}

/// A selected map: entries in print order, what the budget and depth dropped, and coverage over all files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Map {
    pub root: String,
    pub entries: Vec<MapEntry>,
    pub cut: usize,
    pub with_purpose: usize,
    pub total: usize,
}

struct Tree {
    nodes: BTreeMap<String, MapEntry>,
    children: BTreeMap<String, Vec<String>>,
}

/// Scan `root` and select a map of at most `budget` tokens; `depth` counts directory levels shown (at least 1).
#[must_use]
pub fn map_dir(root: &Path, budget: usize, depth: Option<usize>) -> Map {
    let files: Vec<MapFile> = list_files(root)
        .iter()
        .filter_map(|path| scan_file(root, path))
        .collect();
    let total = files.len();
    let with_purpose = files.iter().filter(|file| file.purpose.is_some()).count();
    let tree = build_tree(files);
    let levels = depth.map_or((budget / TOKENS_PER_DEPTH).max(1), |limit| limit.max(1));
    let shown = pick(&tree, budget, levels);
    let mut entries = Vec::new();
    collect(&tree, &shown, "", &mut entries);
    let cut = tree.nodes.len() - entries.len();
    Map {
        root: root.display().to_string(),
        entries,
        cut,
        with_purpose,
        total,
    }
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

fn parent_of(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(dir, _)| dir)
}

fn is_readme(entry: &MapEntry) -> bool {
    let name = entry.path.rsplit('/').next().unwrap_or(&entry.path);
    let stem = name.split('.').next().unwrap_or(name).to_lowercase();
    !entry.is_dir && README_STEMS.contains(&stem.as_str())
}

fn build_tree(files: Vec<MapFile>) -> Tree {
    let mut tree = Tree {
        nodes: BTreeMap::new(),
        children: BTreeMap::new(),
    };
    for file in files {
        tree.add_ancestors(&file);
        tree.children
            .entry(parent_of(&file.path).to_owned())
            .or_default()
            .push(file.path.clone());
        let entry = MapEntry {
            path: file.path.clone(),
            is_dir: false,
            lines: file.lines,
            files: 1,
            tokens: file.tokens,
            purpose: file.purpose,
        };
        tree.nodes.insert(file.path, entry);
    }
    tree.order_and_label();
    tree
}

impl Tree {
    fn add_ancestors(&mut self, file: &MapFile) {
        let parts: Vec<&str> = file.path.split('/').collect();
        for end in 1..parts.len() {
            let dir = parts[..end].join("/");
            if !self.nodes.contains_key(&dir) {
                self.children
                    .entry(parent_of(&dir).to_owned())
                    .or_default()
                    .push(dir.clone());
                let entry = MapEntry {
                    path: dir.clone(),
                    is_dir: true,
                    lines: 0,
                    files: 0,
                    tokens: 0,
                    purpose: None,
                };
                self.nodes.insert(dir.clone(), entry);
            }
            if let Some(entry) = self.nodes.get_mut(&dir) {
                entry.files += 1;
                entry.lines += file.lines;
                entry.tokens += file.tokens;
            }
        }
    }

    fn order_and_label(&mut self) {
        let nodes = &self.nodes;
        for list in self.children.values_mut() {
            list.sort_by_cached_key(|path| {
                let entry = nodes.get(path);
                (
                    !entry.is_some_and(is_readme),
                    Reverse(entry.map_or(0, |found| found.tokens)),
                    path.clone(),
                )
            });
        }
        let labels: Vec<(String, String)> = self
            .children
            .iter()
            .filter(|(dir, _)| !dir.is_empty())
            .filter_map(|(dir, list)| Some((dir.clone(), readme_purpose(nodes, list)?)))
            .collect();
        for (dir, label) in labels {
            if let Some(entry) = self.nodes.get_mut(&dir) {
                entry.purpose = Some(label);
            }
        }
    }
}

fn readme_purpose(nodes: &BTreeMap<String, MapEntry>, list: &[String]) -> Option<String> {
    list.iter()
        .filter_map(|path| nodes.get(path))
        .filter(|entry| is_readme(entry))
        .find_map(|entry| entry.purpose.clone())
}

struct Picker<'a> {
    tree: &'a Tree,
    shown: HashSet<&'a str>,
    used: usize,
    room: usize,
}

fn pick(tree: &Tree, budget: usize, levels: usize) -> HashSet<&str> {
    let room = (budget * 4).saturating_sub(CUT_RESERVE_CHARS);
    let mut picker = Picker {
        tree,
        shown: HashSet::new(),
        used: 0,
        room,
    };
    let mut opened = picker.top();
    for _ in 1..levels {
        if opened.is_empty() {
            break;
        }
        opened = picker.expand(&opened);
    }
    picker.shown
}

impl<'a> Picker<'a> {
    fn kids(&self, dir: &str) -> &'a [String] {
        let tree: &'a Tree = self.tree;
        tree.children.get(dir).map_or(&[], Vec::as_slice)
    }

    fn mass(&self, dir: &str) -> usize {
        self.tree.nodes.get(dir).map_or(0, |entry| entry.tokens)
    }

    fn add_within(&mut self, path: &'a str, limit: usize) -> Option<&'a str> {
        let entry = self.tree.nodes.get(path)?;
        let cost = entry_line(entry).len();
        if self.used + cost > limit && !self.shown.is_empty() {
            return None;
        }
        self.used += cost;
        self.shown.insert(path);
        entry.is_dir.then_some(path)
    }

    fn top(&mut self) -> Vec<&'a str> {
        let room = self.room;
        let mut opened = Vec::new();
        for path in self.kids("") {
            opened.extend(self.add_within(path, room));
        }
        opened
    }

    fn expand(&mut self, parents: &[&'a str]) -> Vec<&'a str> {
        let room = self.room;
        let mut opened = Vec::new();
        for parent in parents {
            if let Some(first) = self.kids(parent).first() {
                opened.extend(self.add_within(first, room));
            }
        }
        opened.extend(self.share(parents));
        opened
    }

    fn share(&mut self, parents: &[&'a str]) -> Vec<&'a str> {
        let mass: usize = parents.iter().map(|dir| self.mass(dir)).sum();
        let remaining = self.room.saturating_sub(self.used);
        let mut ordered = parents.to_vec();
        ordered.sort_by_key(|dir| Reverse(self.mass(dir)));
        let (mut carry, mut opened) = (0, Vec::new());
        for parent in ordered {
            let quota = remaining * self.mass(parent) / mass.max(1) + carry;
            let (spent, found) = self.fill(parent, quota);
            carry = quota - spent;
            opened.extend(found);
        }
        opened
    }

    fn fill(&mut self, parent: &str, quota: usize) -> (usize, Vec<&'a str>) {
        let start = self.used;
        let limit = (start + quota).min(self.room);
        let mut opened = Vec::new();
        for child in self.kids(parent) {
            if self.shown.contains(child.as_str()) {
                continue;
            }
            let added = self.add_within(child, limit);
            if !self.shown.contains(child.as_str()) {
                break;
            }
            opened.extend(added);
        }
        (self.used - start, opened)
    }
}

fn collect(tree: &Tree, shown: &HashSet<&str>, dir: &str, out: &mut Vec<MapEntry>) {
    for child in tree.children.get(dir).into_iter().flatten() {
        if let Some(entry) = tree
            .nodes
            .get(child)
            .filter(|_| shown.contains(child.as_str()))
        {
            out.push(entry.clone());
            collect(tree, shown, child, out);
        }
    }
}

fn entry_line(entry: &MapEntry) -> String {
    let tail = entry
        .purpose
        .as_deref()
        .map_or_else(String::new, |text| format!("  {text}"));
    if entry.is_dir {
        format!(
            "{}/  {} files  ~{}{tail}\n",
            entry.path, entry.files, entry.tokens
        )
    } else {
        format!("{}  {}  ~{}{tail}\n", entry.path, entry.lines, entry.tokens)
    }
}

fn percent(with_purpose: usize, total: usize) -> usize {
    (with_purpose * 100).checked_div(total).unwrap_or(0)
}

/// The default text output of a map: one line per entry, the `cut:` line and the coverage line.
#[must_use]
pub fn map_text(map: &Map) -> String {
    let mut out: String = map.entries.iter().map(entry_line).collect();
    if map.cut > 0 {
        let _ = writeln!(
            out,
            "cut: {} entries not shown; --budget N, --depth D, or ss map SUBDIR",
            map.cut
        );
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
    let entries: Vec<Value> = map
        .entries
        .iter()
        .map(|entry| {
            json!({"path": entry.path, "is_dir": entry.is_dir, "lines": entry.lines, "files": entry.files, "tokens": entry.tokens, "purpose": entry.purpose})
        })
        .collect();
    json!({
        "root": map.root,
        "entries": entries,
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
        _ if is_markdown(name) => markdown_purpose(text),
        _ => leading_doc(text).or_else(|| structural(path, text)),
    };
    found
        .and_then(|raw| sentence(&raw))
        .filter(|text| is_real_purpose(text))
}

fn is_real_purpose(text: &str) -> bool {
    text.chars().count() >= MIN_PURPOSE_CHARS && text.chars().any(char::is_alphabetic)
}

fn is_markdown(name: &str) -> bool {
    let extension = Path::new(name).extension().and_then(|found| found.to_str());
    extension.is_some_and(|found| {
        found.eq_ignore_ascii_case("md") || found.eq_ignore_ascii_case("markdown")
    })
}

fn markdown_purpose(text: &str) -> Option<String> {
    let (front, body) = split_front_matter(text);
    let lines = outside_fences(body);
    first_h1(&lines)
        .or_else(|| front_field(front))
        .or_else(|| first_prose(&lines))
}

fn split_front_matter(text: &str) -> (&str, &str) {
    let Some(rest) = text.strip_prefix("---\n") else {
        return ("", text);
    };
    match rest.find("\n---") {
        Some(end) => (
            &rest[..end],
            rest[end + 4..]
                .split_once('\n')
                .map_or("", |(_, body)| body),
        ),
        None => ("", text),
    }
}

fn outside_fences(body: &str) -> Vec<&str> {
    let mut fenced = false;
    let mut keep = |line: &&str| {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            fenced = !fenced;
            return false;
        }
        !fenced
    };
    body.lines().filter(|line| keep(line)).collect()
}

fn first_h1(lines: &[&str]) -> Option<String> {
    lines
        .iter()
        .find_map(|line| line.trim_end().strip_prefix("# "))
        .map(|title| title.trim().trim_end_matches('#').trim().to_owned())
}

fn front_field(front: &str) -> Option<String> {
    ["description", "name"].into_iter().find_map(|key| {
        let prefix = format!("{key}:");
        let value = front
            .lines()
            .find_map(|line| line.strip_prefix(prefix.as_str()))?;
        Some(
            value
                .trim()
                .trim_matches(|quote| quote == '"' || quote == '\'')
                .to_owned(),
        )
    })
}

fn first_prose(lines: &[&str]) -> Option<String> {
    lines
        .iter()
        .find(|line| is_prose(line))
        .map(|line| line.trim().to_owned())
}

fn is_prose(line: &str) -> bool {
    let trimmed = line.trim();
    let indented = line.starts_with("    ") || line.starts_with('\t');
    let marked = trimmed.starts_with(['-', '*', '+', '|', '#', '>', '<']);
    let digits = trimmed.trim_start_matches(|digit: char| digit.is_ascii_digit());
    let numbered = digits.len() < trimmed.len() && digits.starts_with(['.', ')']);
    !indented && !marked && !numbered && trimmed.split_whitespace().count() >= MIN_PROSE_WORDS
}

fn structural(path: &str, text: &str) -> Option<String> {
    let entries = structure(path, text);
    let first = entries.first()?;
    let is_heading = first.label.starts_with('#')
        || first
            .label
            .chars()
            .next()
            .is_some_and(|mark| mark.is_ascii_digit());
    if is_heading {
        return Some(if first.gist.is_empty() {
            first.name.clone()
        } else {
            format!("{}: {}", first.name, first.gist)
        });
    }
    entries
        .iter()
        .find(|entry| entry.depth == 0)
        .map(|entry| entry.gist.clone())
}

fn toml_description(text: &str, table: &str) -> Option<String> {
    let header = format!("[{table}]");
    let mut inside = false;
    for line in text.lines().map(str::trim) {
        if line.starts_with('[') {
            inside = line == header;
        } else if inside && let Some(rest) = line.strip_prefix("description") {
            let value = rest.trim_start().strip_prefix('=')?.trim();
            return Some(
                value
                    .trim_matches(|quote| quote == '"' || quote == '\'')
                    .to_owned(),
            );
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
        let lines = body
            .iter()
            .map(|line| line.trim_start())
            .take_while(|line| line.starts_with("//!"));
        return Some(
            lines
                .map(|line| line.trim_start_matches("//!").trim())
                .collect::<Vec<_>>()
                .join("\n"),
        );
    }
    if first.starts_with("/*") {
        return block_comment(&body.join("\n"));
    }
    docstring(&body.join("\n"))
}

fn block_comment(text: &str) -> Option<String> {
    let inner = text.trim_start().strip_prefix("/*")?;
    let inner = inner.split_once("*/").map_or(inner, |(head, _)| head);
    let lines = inner
        .lines()
        .map(|line| line.trim().trim_start_matches('*').trim());
    Some(lines.collect::<Vec<_>>().join("\n"))
}

fn docstring(text: &str) -> Option<String> {
    let trimmed = text.trim_start();
    let quote = ["\"\"\"", "'''"]
        .into_iter()
        .find(|quote| trimmed.starts_with(quote))?;
    let inner = &trimmed[quote.len()..];
    Some(
        inner
            .split_once(quote)
            .map_or(inner, |(head, _)| head)
            .trim()
            .to_owned(),
    )
}

fn sentence(raw: &str) -> Option<String> {
    let paragraph: Vec<&str> = raw
        .lines()
        .map(str::trim)
        .skip_while(|line| line.is_empty())
        .take_while(|line| !line.is_empty())
        .collect();
    let joined = paragraph.join(" ");
    let first = joined
        .split_once(". ")
        .map_or(joined.as_str(), |(head, _)| head);
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

    fn find<'a>(map: &'a Map, path: &str) -> Option<&'a MapEntry> {
        map.entries.iter().find(|entry| entry.path == path)
    }

    fn sized_fixture() -> tempfile::TempDir {
        let mut files: Vec<(String, String)> = vec![(
            "README.md".into(),
            "# Fixture root\n\nSome words here.\n".into(),
        )];
        for (name, count) in [("big", 40), ("mid", 6), ("one", 1)] {
            for index in 0..count {
                files.push((
                    format!("{name}/f{index}.md"),
                    format!(
                        "# {name} file {index} with a descriptive title of some length\n\n{}\n",
                        "padding words ".repeat(20 + index)
                    ),
                ));
            }
        }
        let junk = [
            "1\n",
            "-\n",
            "1. first item\n2. second item\n",
            "- a bullet\n",
        ];
        files.extend(
            junk.iter()
                .enumerate()
                .map(|(index, body)| (format!("junk/j{index}.md"), (*body).to_owned())),
        );
        let pairs: Vec<(&str, &str)> = files
            .iter()
            .map(|(name, body)| (name.as_str(), body.as_str()))
            .collect();
        tree(&pairs)
    }

    #[test]
    fn rust_module_doc_is_the_purpose_and_missing_doc_falls_to_first_item_or_none() {
        let documented = "//! Walks the tree. More detail follows.\n\npub fn walk() {}\n";
        let item_doc = "/// Counts things.\npub fn count() {}\n";
        let bare = "pub fn bare() {}\n";

        assert_eq!(
            purpose("a.rs", documented).as_deref(),
            Some("Walks the tree")
        );
        assert_eq!(purpose("b.rs", item_doc).as_deref(), Some("Counts things"));
        assert_eq!(purpose("c.rs", bare), None);
    }

    #[test]
    fn python_docstring_markdown_heading_and_toml_description_give_purposes() {
        let python = "\"\"\"Greets people.\"\"\"\n\ndef greet(name):\n    return name\n";
        let markdown = "# Map guide\n\nHow to read a map.\n\n## More\n";
        let cargo =
            "[package]\nname = \"ss\"\ndescription = \"Semantic search\"\n\n[dependencies]\n";
        let pyproject = "[project]\nname = \"x\"\ndescription = 'A tool'\n";

        assert_eq!(purpose("m.py", python).as_deref(), Some("Greets people"));
        assert_eq!(purpose("g.md", markdown).as_deref(), Some("Map guide"));
        assert_eq!(
            purpose("sub/Cargo.toml", cargo).as_deref(),
            Some("Semantic search")
        );
        assert_eq!(
            purpose("pyproject.toml", pyproject).as_deref(),
            Some("A tool")
        );
        assert_eq!(purpose("Cargo.toml", "[package]\nname = \"x\"\n"), None);
    }

    #[test]
    fn markdown_purpose_skips_fenced_h1_then_front_matter_then_prose_and_rejects_junk() {
        let fenced = "```sh\n# not a title\n```\n\n# Real title\n";
        let front = "---\nname: skill-name\ndescription: \"Does the thing well\"\n---\n\n- item\n";
        let prose = "- list item with many words here\n| a | b |\n> quoted text with many words\n1. numbered item with many words\nThis is the first real prose line.\n";

        assert_eq!(purpose("a.md", fenced).as_deref(), Some("Real title"));
        assert_eq!(
            purpose("b.md", front).as_deref(),
            Some("Does the thing well")
        );
        assert_eq!(
            purpose("c.md", prose).as_deref(),
            Some("This is the first real prose line")
        );
        assert_eq!(purpose("d.md", "1\n"), None);
        assert_eq!(purpose("e.md", "-\n"), None);
        assert_eq!(purpose("f.md", "1. first item\n2. second item\n"), None);
        assert_eq!(purpose("g.rs", "//! 1234\n"), None);
    }

    #[test]
    fn leading_block_comment_is_a_purpose_and_long_text_truncates_to_100_chars() {
        let block = "/*\n * Parses the config.\n */\nint main() {}\n";
        let long = format!("//! {}\n", "word ".repeat(40));

        assert_eq!(purpose("c.c", block).as_deref(), Some("Parses the config"));
        assert_eq!(
            purpose("d.rs", &long).map(|text| text.chars().count()),
            Some(100)
        );
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
        assert!(map_text(&map).contains("src/bare.rs  1  ~4\n"));
    }

    #[test]
    fn budget_cut_line_counts_dropped_entries_and_names_the_flags() {
        let files: Vec<(String, String)> = (0..30)
            .map(|index| {
                (
                    format!("d{index}/f.rs"),
                    format!("//! File number {index} does its work.\n"),
                )
            })
            .collect();
        let pairs: Vec<(&str, &str)> = files
            .iter()
            .map(|(name, body)| (name.as_str(), body.as_str()))
            .collect();
        let dir = tree(&pairs);

        let map = map_dir(dir.path(), 100, None);
        let text = map_text(&map);

        assert!(map.cut > 0 && map.cut <= 60, "{}", map.cut);
        assert_eq!(map.cut + map.entries.len(), 60);
        assert!(text.contains(&format!(
            "cut: {} entries not shown; --budget N, --depth D, or ss map SUBDIR",
            map.cut
        )));
        assert!(text.len().div_ceil(4) <= 100, "{}", text.len());
        assert_eq!(map.total, 30);
    }

    #[test]
    fn depth_one_shows_only_top_level_entries_and_depth_two_adds_children() {
        let dir = tree(&[
            ("top.rs", "//! Top level module.\n"),
            ("a/mid.rs", "//! Mid level module.\n"),
            ("a/b/deep.rs", "//! Deep level module.\n"),
        ]);

        let one = map_dir(dir.path(), MAP_BUDGET, Some(1));
        let two = map_dir(dir.path(), MAP_BUDGET, Some(2));
        let zero = map_dir(dir.path(), MAP_BUDGET, Some(0));

        let paths = |map: &Map| {
            map.entries
                .iter()
                .map(|entry| entry.path.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(paths(&one), ["a", "top.rs"]);
        assert_eq!(paths(&zero), paths(&one));
        assert_eq!(paths(&two), ["a", "a/b", "a/mid.rs", "top.rs"]);
        assert_eq!((one.cut, one.with_purpose, one.total), (3, 3, 3));
        assert_eq!(two.cut, 1);
    }

    #[test]
    fn every_top_level_entry_appears_at_the_default_budget_and_the_big_dir_does_not_crowd_out_small_ones()
     {
        let dir = sized_fixture();

        let map = map_dir(dir.path(), MAP_BUDGET, None);

        for top in ["README.md", "big/", "mid/", "one/", "junk/"] {
            let path = top.trim_end_matches('/');
            assert!(
                find(&map, path).is_some_and(|entry| entry.is_dir == top.ends_with('/')),
                "{top}: {}",
                map_text(&map)
            );
        }
        let under = |dir: &str| {
            map.entries
                .iter()
                .filter(|entry| entry.path.starts_with(&format!("{dir}/")))
                .count()
        };
        assert!(
            under("one") >= 1 && under("mid") >= 1 && under("junk") >= 1,
            "{}",
            map_text(&map)
        );
        assert!(under("big") < 40, "{}", map_text(&map));
        assert_eq!(find(&map, "big").map(|entry| entry.files), Some(40));
    }

    #[test]
    fn junk_first_lines_have_no_purpose_and_are_not_counted_in_coverage() {
        let dir = sized_fixture();

        let map = map_dir(dir.path(), 4000, Some(2));

        assert!((0..4).all(|index| {
            find(&map, &format!("junk/j{index}.md")).is_some_and(|entry| entry.purpose.is_none())
        }));
        assert_eq!(
            find(&map, "junk").and_then(|entry| entry.purpose.clone()),
            None
        );
        assert_eq!((map.with_purpose, map.total), (48, 52));
        assert!(map_text(&map).ends_with("purpose coverage: 48 of 52 files have one (92%)\n"));
    }

    #[test]
    fn a_directory_takes_its_purpose_only_from_a_readme_directly_inside() {
        let dir = tree(&[
            ("with/README.md", "# With readme\n"),
            ("with/other.md", "# Other\n"),
            ("without/other.md", "# Other file\n"),
        ]);

        let map = map_dir(dir.path(), MAP_BUDGET, Some(2));

        assert_eq!(
            find(&map, "with").and_then(|entry| entry.purpose.as_deref()),
            Some("With readme")
        );
        assert_eq!(
            find(&map, "without").and_then(|entry| entry.purpose.clone()),
            None
        );
    }

    #[test]
    fn json_document_carries_entries_cut_and_coverage() {
        let dir = tree(&[
            ("src/lib.rs", "//! Root crate.\n"),
            ("src/bare.rs", "pub fn a() {}\n"),
        ]);

        let value = map_json(&map_dir(dir.path(), MAP_BUDGET, None));

        assert_eq!(value["entries"].as_array().map(Vec::len), Some(3));
        assert_eq!(value["entries"][0]["path"], "src");
        assert_eq!(value["entries"][0]["is_dir"], true);
        assert_eq!(value["entries"][0]["files"], 2);
        assert_eq!(
            value["coverage"],
            json!({"with": 1, "total": 2, "percent": 50})
        );
        assert!(
            value["entries"][1]["purpose"].is_null() || value["entries"][2]["purpose"].is_null()
        );
    }
}

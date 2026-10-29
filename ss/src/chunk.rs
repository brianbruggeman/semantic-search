use tree_sitter::{Node, Parser};

pub const MAX_CHUNK_LINES: usize = 80;
pub const MIN_CHUNK_LINES: usize = 6;
pub const WINDOW_LINES: usize = 40;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Language {
    Rust,
    Python,
    TypeScript,
    Tsx,
    JavaScript,
    Go,
}

impl Language {
    #[must_use]
    pub fn from_extension(extension: &str) -> Option<Self> {
        match extension {
            "rs" => Some(Self::Rust),
            "py" => Some(Self::Python),
            "ts" => Some(Self::TypeScript),
            "tsx" => Some(Self::Tsx),
            "js" | "jsx" | "mjs" | "cjs" => Some(Self::JavaScript),
            "go" => Some(Self::Go),
            _ => None,
        }
    }

    fn grammar(self) -> tree_sitter::Language {
        match self {
            Self::Rust => tree_sitter_rust::LANGUAGE.into(),
            Self::Python => tree_sitter_python::LANGUAGE.into(),
            Self::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            Self::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
            Self::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
            Self::Go => tree_sitter_go::LANGUAGE.into(),
        }
    }
}

pub type ChunkKey = [u8; 32];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawChunk {
    pub start_line: u32,
    pub end_line: u32,
    pub text: String,
    pub key: ChunkKey,
}

#[must_use]
pub fn chunk_key(text: &str) -> ChunkKey {
    *blake3::hash(text.as_bytes()).as_bytes()
}

#[must_use]
pub fn chunk_source(language: Option<Language>, text: &str) -> Vec<RawChunk> {
    let lines: Vec<&str> = text.lines().collect();
    let spans = language
        .and_then(|lang| syntax_spans(lang, text))
        .unwrap_or_else(|| window_spans(&lines));
    spans
        .into_iter()
        .filter_map(|(first, last)| build_chunk(&lines, first, last))
        .collect()
}

fn build_chunk(lines: &[&str], first: usize, last: usize) -> Option<RawChunk> {
    let last = last.min(lines.len().checked_sub(1)?);
    let body = lines.get(first..=last)?.join("\n");
    if body.trim().is_empty() {
        return None;
    }
    Some(RawChunk {
        start_line: u32::try_from(first + 1).ok()?,
        end_line: u32::try_from(last + 1).ok()?,
        key: chunk_key(&body),
        text: body,
    })
}

fn window_spans(lines: &[&str]) -> Vec<(usize, usize)> {
    (0..lines.len())
        .step_by(WINDOW_LINES)
        .map(|first| (first, first + WINDOW_LINES - 1))
        .collect()
}

fn syntax_spans(language: Language, text: &str) -> Option<Vec<(usize, usize)>> {
    let mut parser = Parser::new();
    parser.set_language(&language.grammar()).ok()?;
    let tree = parser.parse(text, None)?;
    let mut units = Vec::new();
    explode(tree.root_node(), &mut units);
    Some(merge_small(attach_leading(units)))
}

#[derive(Debug, Clone, Copy)]
struct Unit {
    first: usize,
    last: usize,
    leading: bool,
}

fn explode(node: Node<'_>, units: &mut Vec<Unit>) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        let first = child.start_position().row;
        let last = child.end_position().row;
        let kind = child.kind();
        let leading = kind.contains("comment") || kind == "attribute_item";
        if last - first + 1 > MAX_CHUNK_LINES && child.child_count() > 0 {
            explode_oversized(child, units);
        } else if child.is_named() {
            units.push(Unit {
                first,
                last,
                leading,
            });
        }
    }
}

fn explode_oversized(node: Node<'_>, units: &mut Vec<Unit>) {
    let before = units.len();
    explode(node, units);
    if units.len() == before {
        units.push(Unit {
            first: node.start_position().row,
            last: node.end_position().row,
            leading: false,
        });
    }
}

fn attach_leading(units: Vec<Unit>) -> Vec<Unit> {
    let mut merged: Vec<Unit> = Vec::with_capacity(units.len());
    let mut pending: Option<Unit> = None;
    for unit in units {
        let held = pending.take();
        let adjacent = held.filter(|lead| lead.last + 1 >= unit.first);
        if adjacent.is_none() {
            merged.extend(held);
        }
        let first = adjacent.map_or(unit.first, |lead| lead.first);
        let combined = Unit {
            first,
            last: unit.last,
            leading: unit.leading,
        };
        if unit.leading {
            pending = Some(combined);
        } else {
            merged.push(combined);
        }
    }
    merged.extend(pending);
    merged
}

fn merge_small(units: Vec<Unit>) -> Vec<(usize, usize)> {
    let mut spans: Vec<(usize, usize)> = Vec::new();
    let mut group: Option<(usize, usize)> = None;
    for unit in units {
        let size = unit.last - unit.first + 1;
        if size >= MIN_CHUNK_LINES {
            spans.extend(group.take());
            spans.push((unit.first, unit.last));
            continue;
        }
        group = match group {
            Some((first, last)) if unit.last - first < MAX_CHUNK_LINES => {
                Some((first, unit.last.max(last)))
            }
            Some(done) => {
                spans.push(done);
                Some((unit.first, unit.last))
            }
            None => Some((unit.first, unit.last)),
        };
    }
    spans.extend(group);
    spans
}

#[cfg(test)]
mod tests {
    use std::fmt::Write;

    use super::*;

    const RUST_SOURCE: &str = "use std::path::Path;\n\n/// walks a tree\npub fn walk(root: &Path) -> Vec<String> {\n    let mut found = Vec::new();\n    for step in 0..3 {\n        found.push(format!(\"{}-{step}\", root.display()));\n    }\n    found\n}\n\npub fn tiny() -> u32 { 1 }\n\nfn other(value: u32) -> u32 {\n    let doubled = value * 2;\n    let tripled = value * 3;\n    let sum = doubled + tripled;\n    sum + 1\n}\n";

    #[test]
    fn rust_functions_become_their_own_chunks_with_doc_comment_attached() {
        let chunks = chunk_source(Some(Language::Rust), RUST_SOURCE);

        let walker = chunks
            .iter()
            .find(|chunk| chunk.text.contains("pub fn walk"))
            .expect("walk fn chunk");
        assert!(
            walker.text.starts_with("/// walks a tree"),
            "doc comment must lead the chunk"
        );
        assert_eq!((walker.start_line, walker.end_line), (3, 10));
        assert!(chunks.len() >= 2, "got {} chunks", chunks.len());
    }

    #[test]
    fn editing_one_function_changes_exactly_one_chunk_key() {
        let before = chunk_source(Some(Language::Rust), RUST_SOURCE);
        let edited = RUST_SOURCE.replace("value * 3", "value * 30");

        let after = chunk_source(Some(Language::Rust), &edited);

        let changed = after
            .iter()
            .filter(|chunk| !before.iter().any(|old| old.key == chunk.key))
            .count();
        assert_eq!(
            (before.len(), after.len(), changed),
            (before.len(), before.len(), 1)
        );
    }

    #[test]
    fn unknown_language_falls_back_to_forty_line_windows() {
        let text: String = (1..=100).fold(String::new(), |mut acc, n| {
            writeln!(acc, "line {n}").expect("write to string");
            acc
        });

        let chunks = chunk_source(None, &text);

        let spans: Vec<(u32, u32)> = chunks
            .iter()
            .map(|chunk| (chunk.start_line, chunk.end_line))
            .collect();
        assert_eq!(spans, vec![(1, 40), (41, 80), (81, 100)]);
    }

    #[test]
    fn identical_text_has_identical_key_regardless_of_position() {
        let text = "a\nb\nc\n";
        let first = chunk_source(None, text);
        let second = chunk_source(None, &format!("\n\n{text}"));

        assert_eq!(first[0].key, chunk_key("a\nb\nc"));
        assert_eq!(second[0].key, chunk_key("\n\na\nb\nc"));
    }

    #[test]
    fn blank_input_yields_no_chunks() {
        assert_eq!(chunk_source(Some(Language::Python), "\n\n   \n").len(), 0);
    }

    #[test]
    fn python_class_methods_chunk_separately_when_class_is_large() {
        let methods: String = (0..30).fold(String::new(), |mut acc, n| {
            write!(acc, "    def method_{n}(self):\n        total = {n}\n        total += 1\n        return total\n\n").expect("write to string");
            acc
        });
        let source = format!("class Big:\n{methods}");

        let chunks = chunk_source(Some(Language::Python), &source);

        assert!(
            chunks.len() > 1,
            "large class should split, got {}",
            chunks.len()
        );
        assert!(
            chunks
                .iter()
                .all(|chunk| chunk.end_line - chunk.start_line < 90)
        );
    }

    #[proxima::test]
    #[case::go(
        Language::Go,
        "package main\n\nfunc add(a int, b int) int {\n\tx := a\n\ty := b\n\tz := x + y\n\treturn z + 1\n}\n"
    )]
    #[case::javascript(
        Language::JavaScript,
        "function add(a, b) {\n  const x = a;\n  const y = b;\n  const z = x + y;\n  return z + 1;\n}\n"
    )]
    #[case::typescript(
        Language::TypeScript,
        "export function add(a: number, b: number): number {\n  const x = a;\n  const y = b;\n  const z = x + y;\n  return z + 1;\n}\n"
    )]
    #[case::tsx(
        Language::Tsx,
        "export const View = () => {\n  const x = 1;\n  const y = 2;\n  const z = x + y;\n  return <div>{z}</div>;\n};\n"
    )]
    async fn every_supported_language_parses_a_function(
        #[case] language: Language,
        #[case] source: &str,
    ) {
        let chunks = chunk_source(Some(language), source);

        assert!(!chunks.is_empty(), "{language:?} produced no chunks");
    }
}

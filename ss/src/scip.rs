//! SCIP (compiler-resolved definitions and references) as plain rows. `parse` and `convert` are pure functions:
//! bytes in, rows out; `Store::load_scip` (in `store/scip.rs`) is the only part that touches SQLite.
//! A transform `Vec<u8> -> Result<ScipRows, ScipError>` composed of two steps; no new pipe type is needed.

use std::collections::{HashMap, HashSet};
use std::fmt::Write;
use std::path::Path;

use protobuf::Message;
use scip::types::{Document, Index, SymbolInformation, SymbolRole};
use thiserror::Error;

use crate::chunk::ChunkKey;
use crate::store::{DefRow, Store};

#[derive(Debug, Error)]
pub enum ScipError {
    #[error("scip index did not parse: {0}")]
    Parse(String),
    #[error("scip index unreadable: {0}")]
    Io(#[from] std::io::Error),
    #[error("store failure: {0}")]
    Store(#[from] rusqlite::Error),
}

/// Reads `index_path`, hashes each indexed file under `root` now, and replaces the store's SCIP rows.
/// Files unreadable at load time get no `scip_files` row, so they never count as fresh.
pub fn load_file(
    store: &mut Store,
    root: &Path,
    index_path: &Path,
) -> Result<(u64, u64, u64), ScipError> {
    let rows = convert(&parse(&std::fs::read(index_path)?)?);
    let hashes: HashMap<String, ChunkKey> = rows
        .files
        .iter()
        .filter_map(|file| {
            let bytes = std::fs::read(root.join(file)).ok()?;
            Some((file.clone(), *blake3::hash(&bytes).as_bytes()))
        })
        .collect();
    store.load_scip(&rows, &hashes)?;
    Ok(store.scip_counts()?)
}

/// One symbol the index names, with the facts a graph needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolRow {
    pub symbol: String,
    pub display_name: String,
    pub kind: String,
    pub signature_doc: String,
}

/// One place a symbol appears; `role` is the raw SCIP `symbol_roles` bitset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OccurrenceRow {
    pub file: String,
    pub start_line: u32,
    pub start_col: u32,
    pub end_line: u32,
    pub end_col: u32,
    pub symbol: String,
    pub role: i32,
    pub enclosing: Option<(u32, u32)>,
}

impl OccurrenceRow {
    #[must_use]
    pub fn is_definition(&self) -> bool {
        self.role & SymbolRole::Definition as i32 != 0
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScipRows {
    pub files: Vec<String>,
    pub symbols: Vec<SymbolRow>,
    pub occurrences: Vec<OccurrenceRow>,
}

pub fn parse(bytes: &[u8]) -> Result<Index, ScipError> {
    Index::parse_from_bytes(bytes).map_err(|error| ScipError::Parse(error.to_string()))
}

#[must_use]
pub fn convert(index: &Index) -> ScipRows {
    let mut rows = ScipRows::default();
    let mut seen: HashSet<String> = HashSet::new();
    let external = index.external_symbols.iter().map(symbol_row);
    let documented = index
        .documents
        .iter()
        .flat_map(|doc| doc.symbols.iter().map(symbol_row));
    rows.symbols = documented
        .chain(external)
        .filter(|row| seen.insert(row.symbol.clone()))
        .collect();
    for document in &index.documents {
        rows.files.push(document.relative_path.clone());
        rows.occurrences.extend(document_occurrences(document));
    }
    rows
}

fn symbol_row(info: &SymbolInformation) -> SymbolRow {
    let signature = info
        .signature_documentation
        .as_ref()
        .map(|doc| doc.text.lines().next().unwrap_or_default().to_string())
        .unwrap_or_default();
    SymbolRow {
        symbol: info.symbol.clone(),
        display_name: info.display_name.clone(),
        kind: format!("{:?}", info.kind.enum_value_or_default()),
        signature_doc: signature,
    }
}

fn document_occurrences(document: &Document) -> impl Iterator<Item = OccurrenceRow> + '_ {
    document.occurrences.iter().filter_map(|occurrence| {
        let (start_line, start_col, end_line, end_col) = span(&occurrence.range)?;
        let enclosing = span(&occurrence.enclosing_range).map(|(start, _, end, _)| (start, end));
        Some(OccurrenceRow {
            file: document.relative_path.clone(),
            start_line,
            start_col,
            end_line,
            end_col,
            symbol: occurrence.symbol.clone(),
            role: occurrence.symbol_roles,
            enclosing,
        })
    })
}

/// SCIP ranges are `[line, col, endcol]` (single line) or `[line, col, endline, endcol]`.
fn span(range: &[i32]) -> Option<(u32, u32, u32, u32)> {
    let values: Vec<u32> = range
        .iter()
        .map(|value| u32::try_from(*value).ok())
        .collect::<Option<_>>()?;
    match values.as_slice() {
        [line, col, end_col] => Some((*line, *col, *line, *end_col)),
        [line, col, end_line, end_col] => Some((*line, *col, *end_line, *end_col)),
        _ => None,
    }
}

pub const REFS_BUDGET: usize = 600;
pub const GRAPH_BUDGET: usize = 400;

/// Tag for a file's SCIP rows: `resolved-fresh` while the file still hashes to the recorded blake3,
/// `resolved-stale` once it differs, `syntactic` when the file never had SCIP rows (callers fall back to name-resolved edges there).
#[must_use]
pub fn freshness(store: &Store, root: &Path, file: &str) -> &'static str {
    let recorded = store.scip_hash(file).ok().flatten();
    let current = std::fs::read(root.join(file))
        .ok()
        .map(|bytes| *blake3::hash(&bytes).as_bytes());
    match (recorded, current) {
        (None, _) => "syntactic",
        (Some(old), Some(now)) if old == now => "resolved-fresh",
        _ => "resolved-stale",
    }
}

/// `path:line:col` (1-based, as editors print) or a symbol name; returns its definitions.
pub fn find_defs(store: &Store, target: &str) -> rusqlite::Result<Vec<DefRow>> {
    let parts: Vec<&str> = target.rsplitn(3, ':').collect();
    if let [col, line, file] = parts.as_slice()
        && let (Ok(line), Ok(col)) = (line.parse::<u32>(), col.parse::<u32>())
    {
        return Ok(store
            .scip_def_at(file, line.saturating_sub(1), col.saturating_sub(1))?
            .into_iter()
            .collect());
    }
    store.scip_defs_named(target)
}

fn fits(text: &mut String, line: &str, budget: usize) -> bool {
    if (text.len() + line.len()).div_ceil(4) > budget {
        return false;
    }
    text.push_str(line);
    true
}

fn cut_line(text: &mut String, cut: usize) {
    if cut > 0 {
        let _ = writeln!(text, "cut: {cut} more; raise --budget");
    }
}

#[must_use]
pub fn render_defs(store: &Store, root: &Path, defs: &[DefRow]) -> String {
    let mut text = String::new();
    for def in defs {
        let tag = freshness(store, root, &def.file);
        let _ = writeln!(
            text,
            "{}:{}:{} {} {} {tag}",
            def.file,
            def.line + 1,
            def.col + 1,
            def.kind,
            def.name
        );
    }
    text
}

/// References grouped by file as `file:line enclosing`, cut at `budget` tokens (chars / 4) with a `cut:` line.
pub fn render_refs(
    store: &Store,
    root: &Path,
    defs: &[DefRow],
    budget: usize,
) -> rusqlite::Result<String> {
    let mut text = String::new();
    let mut cut = 0;
    for def in defs {
        let refs = store.scip_refs(def.symbol_id)?;
        let header = format!(
            "{} {} {}:{} refs={}\n",
            def.kind,
            def.name,
            def.file,
            def.line + 1,
            refs.len()
        );
        if !fits(&mut text, &header, budget) {
            cut += refs.len();
            continue;
        }
        let mut last_file = "";
        for reference in &refs {
            let owner = store
                .scip_enclosing(&reference.file, reference.line)?
                .map_or_else(String::new, |owner| owner.name);
            let heading = if last_file == reference.file {
                String::new()
            } else {
                format!(
                    "  {} [{}]\n",
                    reference.file,
                    freshness(store, root, &reference.file)
                )
            };
            let line = format!("{heading}    :{} {owner}\n", reference.line + 1);
            if fits(&mut text, &line, budget) {
                last_file = &reference.file;
            } else {
                cut += 1;
            }
        }
    }
    cut_line(&mut text, cut);
    Ok(text)
}

/// Callees (references inside the item span, resolved to definitions) and callers (references mapped to their
/// enclosing definition) of each definition, each edge tagged with the freshness of the file stating it.
pub fn render_calls(
    store: &Store,
    root: &Path,
    defs: &[DefRow],
    budget: usize,
) -> rusqlite::Result<String> {
    let mut text = String::new();
    let mut cut = 0;
    for def in defs {
        let tag = freshness(store, root, &def.file);
        fits(
            &mut text,
            &format!(
                "{} {} {}:{} {tag}\n",
                def.kind,
                def.name,
                def.file,
                def.line + 1
            ),
            budget,
        );
        for (line, callee) in store.scip_callees(def)? {
            let row = format!(
                "  callee {} {}:{} (at :{})\n",
                callee.name,
                callee.file,
                callee.line + 1,
                line + 1
            );
            if !fits(&mut text, &row, budget) {
                cut += 1;
            }
        }
        for reference in store.scip_refs(def.symbol_id)? {
            let Some(owner) = store.scip_enclosing(&reference.file, reference.line)? else {
                continue;
            };
            let row = format!(
                "  caller {} {}:{} {}\n",
                owner.name,
                reference.file,
                reference.line + 1,
                freshness(store, root, &reference.file)
            );
            if !fits(&mut text, &row, budget) {
                cut += 1;
            }
        }
    }
    cut_line(&mut text, cut);
    Ok(text)
}

/// The SCIP view of `ss graph <target>`: `None` when the root has no SCIP rows or the target has no SCIP
/// definition, so the caller falls back to the name-resolved daemon reply (those edges are `syntactic`).
pub fn scip_graph(store: &Store, root: &Path, target: &str) -> Option<String> {
    let (_, _, files) = store.scip_counts().ok()?;
    let defs = find_defs(store, target)
        .ok()
        .filter(|defs| files > 0 && !defs.is_empty())?;
    render_calls(store, root, &defs, GRAPH_BUDGET).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use scip::types::Occurrence;

    fn sample_index() -> Index {
        let mut definition = Occurrence::new();
        definition.range = vec![3, 7, 14];
        definition.symbol = "rust-analyzer cargo ss 0.1.0 store/Store#".into();
        definition.symbol_roles = SymbolRole::Definition as i32;
        definition.enclosing_range = vec![3, 0, 9, 1];
        let mut reference = Occurrence::new();
        reference.range = vec![12, 4, 13, 9];
        reference.symbol = definition.symbol.clone();
        let mut info = SymbolInformation::new();
        info.symbol = definition.symbol.clone();
        info.display_name = "Store".into();
        let mut document = Document::new();
        document.relative_path = "ss/src/store.rs".into();
        document.occurrences = vec![definition, reference];
        document.symbols = vec![info];
        let mut index = Index::new();
        index.documents = vec![document];
        index
    }

    #[test]
    fn round_trip_through_protobuf_bytes_keeps_definition_and_reference() {
        let bytes = sample_index()
            .write_to_bytes()
            .expect("serialize sample index");
        let rows = convert(&parse(&bytes).expect("parse sample index"));
        assert_eq!(rows.files, vec!["ss/src/store.rs".to_string()]);
        assert_eq!(rows.symbols.len(), 1);
        assert_eq!(rows.symbols[0].display_name, "Store");
        assert_eq!(rows.occurrences.len(), 2);
        assert!(rows.occurrences[0].is_definition());
        assert_eq!(rows.occurrences[0].enclosing, Some((3, 9)));
        assert_eq!(
            (rows.occurrences[1].start_line, rows.occurrences[1].end_line),
            (12, 13)
        );
        assert!(!rows.occurrences[1].is_definition());
    }

    #[test]
    fn garbage_bytes_are_a_parse_error_not_a_panic() {
        assert!(matches!(
            parse(&[0xff, 0xff, 0xff]),
            Err(ScipError::Parse(_))
        ));
    }

    #[test]
    fn malformed_ranges_are_dropped() {
        assert_eq!(span(&[1, 2]), None);
        assert_eq!(span(&[-1, 0, 3]), None);
    }

    const FIXTURE: &str = "fn callee() {}\nfn caller() { callee(); }\n";

    fn fixture_index() -> Index {
        let mut index = Index::new();
        let mut document = Document::new();
        document.relative_path = "lib.rs".into();
        for (name, line, col, enclosing, role) in [
            ("callee", 0, 3, vec![0, 0, 0, 14], 1),
            ("caller", 1, 3, vec![1, 0, 1, 26], 1),
            ("callee", 1, 14, vec![], 0),
        ] {
            let mut occurrence = Occurrence::new();
            occurrence.range = vec![line, col, col + 6];
            occurrence.symbol = format!("rust-analyzer cargo fixture 0.1.0 {name}().");
            occurrence.symbol_roles = role;
            occurrence.enclosing_range = enclosing;
            document.occurrences.push(occurrence);
        }
        for name in ["callee", "caller"] {
            let mut info = SymbolInformation::new();
            info.symbol = format!("rust-analyzer cargo fixture 0.1.0 {name}().");
            info.display_name = name.into();
            info.kind = scip::types::symbol_information::Kind::Function.into();
            document.symbols.push(info);
        }
        index.documents = vec![document];
        index
    }

    fn loaded_fixture() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().expect("scratch dir for the fixture crate");
        std::fs::write(dir.path().join("lib.rs"), FIXTURE).expect("write fixture source");
        let index_path = dir.path().join("index.scip");
        std::fs::write(
            &index_path,
            fixture_index().write_to_bytes().expect("serialize"),
        )
        .expect("write index");
        let config = crate::store::StoreConfig {
            dir: dir.path().join("shard"),
            model_id: "test-model".into(),
        };
        let mut store = Store::open(&config).expect("open writer");
        load_file(&mut store, dir.path(), &index_path).expect("load fixture index");
        (dir, store)
    }

    #[test]
    fn graph_of_an_unchanged_file_is_tagged_resolved_fresh() {
        let (dir, store) = loaded_fixture();
        let text =
            scip_graph(&store, dir.path(), "callee").expect("scip rows exist for the target");
        assert!(text.contains("callee lib.rs:1 resolved-fresh"), "{text}");
        assert!(
            text.contains("caller caller lib.rs:2 resolved-fresh"),
            "{text}"
        );
    }

    #[test]
    fn graph_of_an_edited_file_is_tagged_resolved_stale() {
        let (dir, store) = loaded_fixture();
        std::fs::write(dir.path().join("lib.rs"), format!("// edited\n{FIXTURE}"))
            .expect("edit source");
        let text =
            scip_graph(&store, dir.path(), "callee").expect("scip rows exist for the target");
        assert!(text.contains("resolved-stale"), "{text}");
        assert!(!text.contains("resolved-fresh"), "{text}");
    }

    #[test]
    fn a_file_outside_the_index_is_syntactic_and_unknown_targets_fall_back() {
        let (dir, store) = loaded_fixture();
        assert_eq!(freshness(&store, dir.path(), "other.rs"), "syntactic");
        assert_eq!(scip_graph(&store, dir.path(), "no_such_symbol"), None);
    }
}

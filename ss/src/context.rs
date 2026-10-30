//! Turns stored graph rows into the structured replies: a hit's symbol with its neighbours, and a symbol's neighbourhood.
//!
//! Resolution is decided here, at read time: the definitions an edge's written name could mean are the nodes of that
//! name whose `qpath` fits the written qualifier. One is `exact`, several `ambiguous`, none `unresolved`; unresolved
//! edges (std, other crates, locals) are counted in `hidden` and not listed.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::Path;

use ss_proto::{Capped, EdgeRef, Member, Neighborhood, Record, Related, Resolved, Symbol};

use crate::error::IndexError;
use crate::graph::{EdgeKind, NodeKind, Resolution, callee_fits, qual_matches, resolution_of};
use crate::store::{ChunkRow, EdgeRow, NodeRow, Store};

pub const LIST_CAP: usize = 5;
pub const RELATED_CAP: usize = 8;
pub const PREVIEW_LINES: usize = 2;
const MEMBER_SIGNATURE_CHARS: usize = 70;

type Found<Item> = Result<Item, IndexError>;
type Linked = Vec<(EdgeRef, Option<NodeRow>)>;

fn capped<Item>(mut items: Vec<Item>, hidden: usize) -> Capped<Item> {
    let more = items.len().saturating_sub(LIST_CAP);
    items.truncate(LIST_CAP);
    Capped {
        items,
        more: u32::try_from(more).unwrap_or(u32::MAX),
        hidden: u32::try_from(hidden).unwrap_or(u32::MAX),
    }
}

fn hex(key: Option<[u8; 32]>) -> String {
    key.map_or_else(String::new, |bytes| {
        bytes.iter().fold(String::new(), |mut text, byte| {
            let _ = write!(text, "{byte:02x}");
            text
        })
    })
}

fn absolute(root: &Path, file: &str) -> String {
    root.join(file).to_string_lossy().into_owned()
}

fn symbol_of(root: &Path, row: &NodeRow) -> Symbol {
    Symbol {
        kind: row.node.kind.as_str().to_owned(),
        name: row.node.name.clone(),
        qpath: row.node.qpath.clone(),
        signature: row.node.signature.clone(),
        doc: row.node.doc.clone(),
        path: absolute(root, &row.file),
        start_line: row.node.start_line,
        end_line: row.node.end_line,
        chunk_key: hex(row.node.chunk_key),
    }
}

fn resolved_of(resolution: Resolution) -> Resolved {
    match resolution {
        Resolution::Exact => Resolved::Exact,
        Resolution::Ambiguous => Resolved::Ambiguous,
        Resolution::Unresolved => Resolved::Unresolved,
    }
}

/// The definitions an edge's written name could mean, given its qualifier.
fn candidates(store: &Store, edge: &EdgeRow) -> Found<Vec<NodeRow>> {
    let call = matches!(edge.edge.kind, EdgeKind::Calls | EdgeKind::Tests);
    Ok(store
        .nodes_named(&edge.edge.dst)?
        .into_iter()
        .filter(|row| !call || callee_fits(&edge.edge.qual, row.node.kind))
        .filter(|row| qual_matches(&edge.edge.qual, &row.node.qpath, row.file == edge.file))
        .collect())
}

fn display_name(qpath: &str) -> String {
    match qpath.split_once("::") {
        Some((_, rest)) => rest.to_owned(),
        None => qpath.rsplit('/').next().unwrap_or(qpath).to_owned(),
    }
}

fn rank_order(refs: &mut [EdgeRef]) {
    refs.sort_by_key(|found| matches!(found.resolved, Resolved::Ambiguous));
}

/// Callees of `node` (calls and, for a test symbol, tests edges): resolved ones with their definition site.
fn callees(store: &Store, root: &Path, node: &NodeRow, depth: u8) -> Found<(Linked, usize)> {
    let edges = store.edges_from(
        &node.file,
        &node.node.qpath,
        &[EdgeKind::Calls, EdgeKind::Tests],
    )?;
    let (mut found, mut hidden) = (Vec::new(), 0);
    for edge in edges {
        let mut targets = candidates(store, &edge)?;
        let resolution = resolution_of(&edge.edge.qual, targets.len());
        if resolution == Resolution::Unresolved {
            hidden += 1;
            continue;
        }
        let first = targets.swap_remove(0);
        let target = (resolution == Resolution::Exact).then(|| first.clone());
        found.push((
            EdgeRef {
                name: edge.edge.dst.clone(),
                path: absolute(root, &first.file),
                line: first.node.start_line,
                resolved: resolved_of(resolution),
                depth,
            },
            target,
        ));
    }
    found.sort_by_key(|(entry, _)| matches!(entry.resolved, Resolved::Ambiguous));
    Ok((found, hidden))
}

/// Incoming edges of `kinds` that can mean `node`, each with the symbol that states it.
fn incoming(store: &Store, root: &Path, node: &NodeRow, kinds: &[EdgeKind]) -> Found<Linked> {
    let mut found = Vec::new();
    for edge in store.edges_to(&node.node.name, kinds)? {
        let targets = candidates(store, &edge)?;
        let here = targets
            .iter()
            .any(|row| row.file == node.file && row.node.qpath == node.node.qpath);
        if !here {
            continue;
        }
        let caller = store.node_by_qpath(&edge.file, &edge.edge.src)?;
        let (line, name) = match (&caller, edge.edge.kind) {
            (Some(row), EdgeKind::Tests) => (row.node.start_line, display_name(&edge.edge.src)),
            _ => (edge.edge.line, display_name(&edge.edge.src)),
        };
        found.push((
            EdgeRef {
                name,
                path: absolute(root, &edge.file),
                line,
                resolved: resolved_of(resolution_of(&edge.edge.qual, targets.len())),
                depth: 1,
            },
            caller,
        ));
    }
    found.sort_by_key(|(entry, _)| matches!(entry.resolved, Resolved::Ambiguous));
    Ok(found)
}

fn member_of(root: &Path, row: &NodeRow) -> Member {
    Member {
        kind: row.node.kind.as_str().to_owned(),
        name: row.node.name.clone(),
        signature: row
            .node
            .signature
            .chars()
            .take(MEMBER_SIGNATURE_CHARS)
            .collect(),
        path: absolute(root, &row.file),
        line: row.node.start_line,
    }
}

/// A type's own members, or the siblings of any other symbol.
fn members(store: &Store, root: &Path, node: &NodeRow) -> Found<Capped<Member>> {
    let own = matches!(
        node.node.kind,
        NodeKind::Struct | NodeKind::Enum | NodeKind::Trait | NodeKind::Type
    );
    let container = if own {
        node.node.qpath.as_str()
    } else {
        node.node
            .qpath
            .rsplit_once("::")
            .map_or("", |(parent, _)| parent)
    };
    let rows = store.members_of(&node.file, container)?;
    let listed = rows
        .iter()
        .filter(|row| row.node.qpath != node.node.qpath)
        .map(|row| member_of(root, row))
        .collect();
    Ok(capped(listed, 0))
}

fn container_label(store: &Store, node: &NodeRow) -> Found<String> {
    let Some((parent, _)) = node.node.qpath.rsplit_once("::") else {
        return Ok(String::new());
    };
    let label = match store.node_by_qpath(&node.file, parent)? {
        Some(row) => format!("{} {}", row.node.kind.as_str(), row.node.name),
        None => format!("mod {}", parent.rsplit("::").next().unwrap_or(parent)),
    };
    Ok(label)
}

/// One hit as a record: its symbol (when the chunk has one) and the neighbours an agent would search for next.
pub fn record(
    store: &Store,
    root: &Path,
    rank: usize,
    fusion: f64,
    chunk: &ChunkRow,
) -> Found<Record> {
    let node = store.primary_node(&chunk.path, chunk.start_line, chunk.end_line)?;
    let mut record = Record {
        rank: u32::try_from(rank).unwrap_or(u32::MAX),
        score: fusion,
        path: absolute(root, &chunk.path),
        start_line: chunk.start_line,
        end_line: chunk.end_line,
        chunk_key: hex(Some(chunk.key)),
        preview: Vec::new(),
        symbol: None,
        container: String::new(),
        members: Capped::default(),
        calls: Capped::default(),
        called_by: Capped::default(),
        tests: Capped::default(),
    };
    let Some(node) = node else {
        record.preview = chunk
            .text
            .lines()
            .take(PREVIEW_LINES)
            .map(str::to_owned)
            .collect();
        return Ok(record);
    };
    let (callee_rows, hidden) = callees(store, root, &node, 1)?;
    record.symbol = Some(symbol_of(root, &node));
    record.container = container_label(store, &node)?;
    record.members = members(store, root, &node)?;
    record.calls = capped(
        callee_rows.into_iter().map(|(entry, _)| entry).collect(),
        hidden,
    );
    record.called_by = capped(refs(incoming(store, root, &node, &[EdgeKind::Calls])?), 0);
    record.tests = capped(refs(incoming(store, root, &node, &[EdgeKind::Tests])?), 0);
    Ok(record)
}

fn refs(rows: Linked) -> Vec<EdgeRef> {
    rows.into_iter().map(|(entry, _)| entry).collect()
}

/// The symbols a target names: `path:line` is the innermost symbol there, anything else matches by name or `qpath` suffix.
pub fn resolve_target(
    store: &Store,
    root: &Path,
    scope_prefix: &str,
    target: &str,
) -> Found<Vec<NodeRow>> {
    if let Some((file, line)) = target
        .rsplit_once(':')
        .and_then(|(file, line)| Some((file, line.parse::<u32>().ok()?)))
    {
        let rel = Path::new(file)
            .strip_prefix(root)
            .unwrap_or(Path::new(file));
        let rel = rel.to_string_lossy().replace('\\', "/");
        return Ok(store.node_at(&rel, line)?.into_iter().collect());
    }
    let found = store.symbols_matching(target, 64)?;
    Ok(found
        .into_iter()
        .filter(|row| row.file.starts_with(scope_prefix))
        .collect())
}

/// A symbol's neighbourhood; with `depth` 2 the callers of callers and callees of callees follow the direct ones.
pub fn neighborhood(store: &Store, root: &Path, node: &NodeRow, depth: u8) -> Found<Neighborhood> {
    let (downstream, hidden) = callees(store, root, node, 1)?;
    let upstream = incoming(store, root, node, &[EdgeKind::Calls])?;
    let mut outgoing_refs: Vec<EdgeRef> =
        downstream.iter().map(|(entry, _)| entry.clone()).collect();
    let mut incoming_refs = refs(upstream.clone());
    if depth >= 2 {
        for (_, target) in &downstream {
            let Some(target) = target else { continue };
            outgoing_refs.extend(second_hop_callees(store, root, target)?);
        }
        for (_, caller) in &upstream {
            let Some(caller) = caller else { continue };
            incoming_refs.extend(second_hop_callers(store, root, caller)?);
        }
    }
    Ok(Neighborhood {
        symbol: symbol_of(root, node),
        members: members(store, root, node)?,
        callers: capped_by_depth(incoming_refs, 0),
        callees: capped_by_depth(outgoing_refs, hidden),
        importers: capped(importers(store, root, node)?, 0),
        tests: capped(refs(incoming(store, root, node, &[EdgeKind::Tests])?), 0),
    })
}

fn second_hop_callees(store: &Store, root: &Path, target: &NodeRow) -> Found<Vec<EdgeRef>> {
    let (rows, _) = callees(store, root, target, 2)?;
    Ok(refs(rows))
}

fn second_hop_callers(store: &Store, root: &Path, caller: &NodeRow) -> Found<Vec<EdgeRef>> {
    let rows = incoming(store, root, caller, &[EdgeKind::Calls])?;
    Ok(refs(rows)
        .into_iter()
        .map(|entry| EdgeRef { depth: 2, ..entry })
        .collect())
}

/// Caps each depth separately so a wide first hop cannot push the second hop out of the reply.
fn capped_by_depth(items: Vec<EdgeRef>, hidden: usize) -> Capped<EdgeRef> {
    let (first, second): (Vec<EdgeRef>, Vec<EdgeRef>) =
        items.into_iter().partition(|entry| entry.depth == 1);
    let mut out = capped(first, hidden);
    let tail = capped(second, 0);
    out.items.extend(tail.items);
    out.more += tail.more;
    out
}

fn importers(store: &Store, root: &Path, node: &NodeRow) -> Found<Vec<EdgeRef>> {
    let module = node.node.qpath.split("::").next().unwrap_or("");
    let mut found: HashMap<String, EdgeRef> = HashMap::new();
    for (name, exact) in [(node.node.name.as_str(), true), (module, false)] {
        for edge in store.edges_to(name, &[EdgeKind::Imports])? {
            let fits = !edge.edge.qual.is_empty()
                && qual_matches(&edge.edge.qual, &node.node.qpath, false);
            if edge.file == node.file || (exact && !fits) {
                continue;
            }
            found.entry(edge.file.clone()).or_insert_with(|| EdgeRef {
                name: edge
                    .file
                    .rsplit('/')
                    .next()
                    .unwrap_or(&edge.file)
                    .to_owned(),
                path: absolute(root, &edge.file),
                line: edge.edge.line,
                resolved: if exact && fits {
                    Resolved::Exact
                } else {
                    Resolved::Ambiguous
                },
                depth: 1,
            });
        }
    }
    let mut listed: Vec<EdgeRef> = found.into_values().collect();
    listed.sort_by(|left, right| left.path.cmp(&right.path));
    rank_order(&mut listed);
    Ok(listed)
}

/// Symbols that several hits point at and that are not hits themselves, most-mentioned first.
#[must_use]
pub fn related(records: &[&Record]) -> Capped<Related> {
    let hit_keys: Vec<(&str, &str)> = records
        .iter()
        .filter_map(|record| record.symbol.as_ref())
        .map(|symbol| (symbol.path.as_str(), symbol.name.as_str()))
        .collect();
    let mut tally: Vec<Related> = Vec::new();
    for record in records {
        for (relation, entries) in relations(record) {
            for (name, path, line) in entries {
                if hit_keys.contains(&(path, name)) {
                    continue;
                }
                match tally
                    .iter_mut()
                    .find(|seen| seen.name == name && seen.path == path)
                {
                    Some(seen) => seen.mentions += 1,
                    None => tally.push(Related {
                        name: name.to_owned(),
                        path: path.to_owned(),
                        line,
                        relation: relation.to_owned(),
                        mentions: 1,
                    }),
                }
            }
        }
    }
    let mut shared: Vec<Related> = tally
        .into_iter()
        .filter(|item| item.mentions >= 2)
        .collect();
    shared.sort_by_key(|item| std::cmp::Reverse(item.mentions));
    let more = shared.len().saturating_sub(RELATED_CAP);
    shared.truncate(RELATED_CAP);
    Capped {
        items: shared,
        more: u32::try_from(more).unwrap_or(u32::MAX),
        hidden: 0,
    }
}

type Relation<'r> = (&'static str, Vec<(&'r str, &'r str, u32)>);

fn edge_triples(list: &Capped<EdgeRef>) -> Vec<(&str, &str, u32)> {
    list.items
        .iter()
        .map(|entry| (entry.name.as_str(), entry.path.as_str(), entry.line))
        .collect()
}

fn relations(record: &Record) -> [Relation<'_>; 4] {
    [
        ("calls", edge_triples(&record.calls)),
        ("called_by", edge_triples(&record.called_by)),
        ("tests", edge_triples(&record.tests)),
        (
            "member",
            record
                .members
                .items
                .iter()
                .map(|member| (member.name.as_str(), member.path.as_str(), member.line))
                .collect(),
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embed::EmbedderKind;
    use crate::fixtures::{Corpus, hash_pool, own_src_corpus};
    use crate::index::index_tree;
    use crate::query::{graph_reply, search_findings};

    async fn indexed() -> Corpus {
        let corpus = own_src_corpus(&hash_pool(), EmbedderKind::Hash);
        index_tree(&corpus.shard, &corpus.root())
            .await
            .expect("index");
        corpus
    }

    fn names(list: &Capped<EdgeRef>) -> Vec<&str> {
        list.items.iter().map(|entry| entry.name.as_str()).collect()
    }

    #[proxima::test(runtime = "tokio")]
    async fn a_hit_carries_symbol_signature_doc_callers_callees_and_tests() {
        let corpus = indexed().await;
        let queries = vec!["rrf_fuse".to_owned()];

        let findings = search_findings(&corpus.shard, &corpus.root(), &queries, 30)
            .await
            .expect("findings");

        let record = findings.facets[0]
            .records
            .iter()
            .find(|record| {
                record
                    .symbol
                    .as_ref()
                    .is_some_and(|symbol| symbol.name == "rrf_fuse")
            })
            .expect("rrf_fuse among the hits");
        let symbol = record.symbol.as_ref().expect("symbol");
        assert!(
            symbol.signature.starts_with("pub fn rrf_fuse("),
            "{}",
            symbol.signature
        );
        assert!(
            symbol.doc.starts_with("Reciprocal rank fusion"),
            "{}",
            symbol.doc
        );
        assert_eq!(record.container, "mod query");
        assert!(
            names(&record.called_by).contains(&"fuse"),
            "{:?}",
            record.called_by
        );
        assert!(
            !record.tests.items.is_empty(),
            "no tests found for rrf_fuse"
        );
        assert!(
            record
                .members
                .items
                .iter()
                .any(|member| member.name == "search")
        );
        assert_eq!(findings.schema, ss_proto::SCHEMA_VERSION);
    }

    #[proxima::test(runtime = "tokio")]
    async fn callees_list_only_resolved_names_and_count_the_unresolved_ones_as_hidden() {
        let corpus = indexed().await;
        let queries = vec!["rrf_fuse".to_owned()];
        let findings = search_findings(&corpus.shard, &corpus.root(), &queries, 30)
            .await
            .expect("findings");
        let fuse = findings.facets[0].records.iter().find(|record| {
            record
                .symbol
                .as_ref()
                .is_some_and(|symbol| symbol.name == "fuse")
        });

        let hit = fuse.expect("the fuse helper is a hit for its own name");

        assert!(
            hit.calls
                .items
                .iter()
                .all(|entry| entry.resolved != Resolved::Unresolved)
        );
        assert!(names(&hit.calls).contains(&"rrf_fuse"), "{:?}", hit.calls);
        assert!(
            hit.calls.hidden > 0,
            "std calls such as new/len must be counted, not listed"
        );
    }

    #[proxima::test(runtime = "tokio")]
    async fn graph_depth_two_adds_the_callers_of_callers() {
        let corpus = indexed().await;

        let reply = graph_reply(&corpus.shard, &corpus.root(), "rrf_fuse", 2)
            .await
            .expect("graph");

        let near = &reply.neighborhoods.items[0];
        let direct: Vec<&EdgeRef> = near.callers.items.iter().filter(|e| e.depth == 1).collect();
        let second: Vec<&EdgeRef> = near.callers.items.iter().filter(|e| e.depth == 2).collect();
        assert!(
            direct.iter().any(|entry| entry.name == "fuse"),
            "{direct:?}"
        );
        assert!(
            second
                .iter()
                .any(|entry| entry.name == "scan" || entry.name == "gather"),
            "{second:?}"
        );
    }

    #[proxima::test(runtime = "tokio")]
    async fn importers_are_the_files_that_import_the_symbol_by_name_and_module() {
        let corpus = indexed().await;

        let reply = graph_reply(&corpus.shard, &corpus.root(), "search_findings", 1)
            .await
            .expect("graph");

        let importers = &reply.neighborhoods.items[0].importers;
        assert!(
            importers
                .items
                .iter()
                .any(|entry| entry.name == "daemon.rs" && entry.resolved == Resolved::Exact),
            "{importers:?}"
        );
    }

    #[proxima::test(runtime = "tokio")]
    async fn graph_target_may_be_a_path_and_line() {
        let corpus = indexed().await;
        let text = std::fs::read_to_string(corpus.root().join("src/query.rs")).expect("read");
        let line = text
            .lines()
            .position(|row| row.starts_with("pub fn rrf_fuse"))
            .expect("line")
            + 2;
        let target = format!("src/query.rs:{line}");

        let reply = graph_reply(&corpus.shard, &corpus.root(), &target, 1)
            .await
            .expect("graph");

        assert_eq!(reply.neighborhoods.items[0].symbol.name, "rrf_fuse");
    }

    #[test]
    fn related_lists_symbols_two_hits_share_and_skips_the_hits_themselves() {
        let shared = |name: &str| EdgeRef {
            name: name.to_owned(),
            path: "/w/a.rs".to_owned(),
            line: 3,
            resolved: Resolved::Exact,
            depth: 1,
        };
        let record = |calls: Vec<EdgeRef>| Record {
            rank: 1,
            score: 0.1,
            path: "/w/a.rs".to_owned(),
            start_line: 1,
            end_line: 9,
            chunk_key: String::new(),
            preview: Vec::new(),
            symbol: None,
            container: String::new(),
            members: Capped::default(),
            calls: Capped {
                items: calls,
                more: 0,
                hidden: 0,
            },
            called_by: Capped::default(),
            tests: Capped::default(),
        };
        let first = record(vec![shared("open"), shared("only_first")]);
        let second = record(vec![shared("open")]);

        let found = related(&[&first, &second]);

        let listed: Vec<(&str, u32)> = found
            .items
            .iter()
            .map(|item| (item.name.as_str(), item.mentions))
            .collect();
        assert_eq!(listed, vec![("open", 2)]);
    }
}

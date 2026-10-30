use std::path::Path;

use super::*;
use crate::chunk::chunk_source;

fn fixture(name: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/data/graph")
        .join(name);
    std::fs::read_to_string(path).expect("graph fixture")
}

fn graph_for(rel: &str, text: &str) -> FileGraph {
    let language = rel.rsplit('.').next().and_then(Language::from_extension);
    extract(language, rel, text, &chunk_source(language, text))
}

fn node<'a>(graph: &'a FileGraph, qpath: &str) -> &'a GraphNode {
    graph
        .nodes
        .iter()
        .find(|found| found.qpath == qpath)
        .unwrap_or_else(|| {
            let known: Vec<&String> = graph.nodes.iter().map(|found| &found.qpath).collect();
            panic!("no node {qpath} in {known:?}")
        })
}

fn has_edge(graph: &FileGraph, kind: EdgeKind, src: &str, dst: &str) -> bool {
    graph
        .edges
        .iter()
        .any(|edge| edge.kind == kind && edge.src == src && edge.dst == dst)
}

#[test]
fn rust_source_yields_nodes_and_edges_with_signature_doc_and_chunk_key() {
    let text = include_str!("../query.rs");

    let graph = graph_for("ss/src/query.rs", text);

    let fuse = node(&graph, "query::rrf_fuse");
    assert_eq!(fuse.kind, NodeKind::Function);
    assert!(
        fuse.signature.starts_with("pub fn rrf_fuse("),
        "{}",
        fuse.signature
    );
    assert!(
        fuse.doc.starts_with("Reciprocal rank fusion"),
        "{}",
        fuse.doc
    );
    assert!(fuse.chunk_key.is_some());
    assert!(has_edge(&graph, EdgeKind::Calls, "query::fuse", "rrf_fuse"));
    assert!(has_edge(
        &graph,
        EdgeKind::Imports,
        "ss/src/query.rs",
        "EmbedPool"
    ));
    assert!(has_edge(
        &graph,
        EdgeKind::Contains,
        "ss/src/query.rs",
        "query::search"
    ));
    assert!(
        graph.nodes.len() > 5 && graph.edges.len() > 20,
        "{} nodes {} edges",
        graph.nodes.len(),
        graph.edges.len()
    );
}

#[test]
fn rust_impl_blocks_qualify_methods_and_record_trait_implementation() {
    let text = "pub struct Walk;\n\nimpl Pipe for Walk {\n    fn call(&self) -> u32 {\n        helper()\n    }\n}\n\nimpl Walk {\n    /// builds one\n    pub fn new() -> Self {\n        Self\n    }\n}\n\n#[test]\nfn walks_ok() {\n    Walk::new();\n}\n";

    let graph = graph_for("src/walk.rs", text);

    assert_eq!(node(&graph, "walk::Walk::call").kind, NodeKind::Method);
    assert_eq!(node(&graph, "walk::Walk::new").doc, "builds one");
    assert!(has_edge(&graph, EdgeKind::Implements, "walk::Walk", "Pipe"));
    assert!(has_edge(
        &graph,
        EdgeKind::Calls,
        "walk::Walk::call",
        "helper"
    ));
    assert!(has_edge(&graph, EdgeKind::Tests, "walk::walks_ok", "new"));
    assert!(!has_edge(&graph, EdgeKind::Calls, "walk::walks_ok", "new"));
}

#[test]
fn rust_use_lists_expand_to_one_import_per_leaf() {
    let text = "use std::collections::{HashMap, HashSet as Set};\nuse crate::chunk::*;\n";

    let graph = graph_for("src/a.rs", text);

    let imported: Vec<(&str, &str)> = graph
        .edges
        .iter()
        .filter(|edge| edge.kind == EdgeKind::Imports)
        .map(|edge| (edge.dst.as_str(), edge.qual.as_str()))
        .collect();
    assert_eq!(
        imported,
        vec![
            ("HashMap", "std::collections"),
            ("HashSet", "std::collections"),
            ("*", "crate::chunk")
        ]
    );
}

#[test]
fn python_classes_methods_constants_imports_and_tests() {
    let graph = graph_for("shop/inventory.py", &fixture("inventory.py"));

    assert_eq!(node(&graph, "inventory::Bin").kind, NodeKind::Type);
    assert_eq!(node(&graph, "inventory::Bin::add").kind, NodeKind::Method);
    assert_eq!(
        node(&graph, "inventory::Bin::add").doc,
        "Put amount units into the bin, refusing to overfill it."
    );
    assert_eq!(
        node(&graph, "inventory::MAX_BIN_ITEMS").kind,
        NodeKind::Const
    );
    assert!(has_edge(
        &graph,
        EdgeKind::Extends,
        "inventory::Bin",
        "Storage"
    ));
    assert!(has_edge(
        &graph,
        EdgeKind::Imports,
        "shop/inventory.py",
        "defaultdict"
    ));
    assert!(has_edge(
        &graph,
        EdgeKind::Calls,
        "inventory::load_bins",
        "Bin"
    ));
    assert!(has_edge(
        &graph,
        EdgeKind::Tests,
        "inventory::test_add_refuses_to_overfill",
        "add"
    ));
    assert!(graph.nodes.len() > 5 && graph.edges.len() > 10);
}

#[test]
fn typescript_classes_interfaces_arrows_heritage_and_test_callbacks() {
    let graph = graph_for("web/scheduler.ts", &fixture("scheduler.ts"));

    assert_eq!(node(&graph, "scheduler::Scheduler").kind, NodeKind::Type);
    assert_eq!(
        node(&graph, "scheduler::Scheduler::enqueue").doc,
        "Adds a job to the back of the queue."
    );
    assert_eq!(node(&graph, "scheduler::Job").kind, NodeKind::Trait);
    assert_eq!(
        node(&graph, "scheduler::describeJob").kind,
        NodeKind::Function
    );
    assert_eq!(node(&graph, "scheduler::MAX_RETRIES").kind, NodeKind::Const);
    assert!(has_edge(
        &graph,
        EdgeKind::Extends,
        "scheduler::Scheduler",
        "EventEmitter"
    ));
    assert!(has_edge(
        &graph,
        EdgeKind::Implements,
        "scheduler::Scheduler",
        "Retrying"
    ));
    assert!(has_edge(
        &graph,
        EdgeKind::Extends,
        "scheduler::Retrying",
        "Job"
    ));
    assert!(has_edge(
        &graph,
        EdgeKind::Imports,
        "web/scheduler.ts",
        "Clock"
    ));
    assert!(has_edge(
        &graph,
        EdgeKind::Calls,
        "scheduler::Scheduler::drain",
        "runOne"
    ));
    assert!(has_edge(
        &graph,
        EdgeKind::Tests,
        "web/scheduler.ts",
        "backoffMs"
    ));
}

#[test]
fn javascript_functions_classes_and_commonjs_calls() {
    let graph = graph_for("lib/tally.js", &fixture("tally.js"));

    assert_eq!(
        node(&graph, "tally::countWords").doc,
        "Counts words in a text file."
    );
    assert_eq!(node(&graph, "tally::Counter::add").kind, NodeKind::Method);
    assert_eq!(node(&graph, "tally::topWord").kind, NodeKind::Function);
    assert!(has_edge(
        &graph,
        EdgeKind::Calls,
        "tally::Counter::report",
        "parseLine"
    ));
    assert!(has_edge(
        &graph,
        EdgeKind::Calls,
        "tally::topWord",
        "report"
    ));
    assert!(graph.nodes.len() > 4 && graph.edges.len() > 6);
}

#[test]
fn go_methods_take_their_receiver_type_and_test_functions_are_marked() {
    let graph = graph_for("pkg/cache/cache.go", &fixture("cache.go"));
    let tests = graph_for("pkg/cache/cache_test.go", &fixture("cache_test.go"));

    assert_eq!(node(&graph, "cache::Store").kind, NodeKind::Struct);
    assert_eq!(node(&graph, "cache::Getter").kind, NodeKind::Trait);
    assert_eq!(node(&graph, "cache::Store::Get").kind, NodeKind::Method);
    assert_eq!(
        node(&graph, "cache::Store::Get").doc,
        "Get returns the value for key unless it has expired."
    );
    assert_eq!(node(&graph, "cache::DefaultTTL").kind, NodeKind::Const);
    assert!(has_edge(
        &graph,
        EdgeKind::Calls,
        "cache::Store::Put",
        "log"
    ));
    assert!(has_edge(
        &graph,
        EdgeKind::Imports,
        "pkg/cache/cache.go",
        "sync"
    ));
    assert!(has_edge(
        &tests,
        EdgeKind::Tests,
        "cache::TestPutThenGet",
        "Put"
    ));
}

#[test]
fn a_file_without_a_grammar_still_has_its_file_node() {
    let graph = graph_for("notes.txt", "one\ntwo\nthree\n");

    assert_eq!(graph.nodes.len(), 1);
    assert_eq!(graph.nodes[0].kind, NodeKind::File);
    assert_eq!(graph.nodes[0].end_line, 3);
}

#[test]
fn qualifiers_narrow_by_module_segment_and_self_means_same_file() {
    assert!(qual_matches("crate::query", "query::rrf_fuse", false));
    assert!(!qual_matches("os.path", "query::rrf_fuse", false));
    assert!(qual_matches(
        "./scheduler.ts",
        "scheduler::Scheduler",
        false
    ));
    assert!(qual_matches("self", "a::b", true));
    assert!(!qual_matches("Self", "a::b", false));
}

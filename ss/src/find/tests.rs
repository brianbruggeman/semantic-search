// tests assert with expect messages
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::fs;
use std::path::Path;

use tempfile::TempDir;

use super::{FIND_BUDGET, Shape, classify, find, render_json, render_text, terms_of};

const WALK: &str = "//! Walk files respecting gitignore rules.\n\npub fn list_files(root: &str) -> Vec<String> {\n    Vec::new()\n}\n\npub fn skip_hidden(name: &str) -> bool {\n    name.starts_with('.')\n}\n";
const WATCH: &str = "//! Watcher with debounce for modified events.\n\npub struct Debounce {\n    pub window_ms: u64,\n}\n\nimpl Debounce {\n    pub fn modified_event(&self) -> bool {\n        self.window_ms > 0\n    }\n}\n";
const MAIN: &str = "use crate::walk::list_files;\n\nfn main() {\n    let files = list_files(\".\");\n    println!(\"{}\", files.len());\n}\n";

fn crate_dir() -> TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    let src = dir.path().join("src");
    fs::create_dir_all(&src).expect("mkdir");
    fs::write(src.join("walk.rs"), WALK).expect("write");
    fs::write(src.join("watch.rs"), WATCH).expect("write");
    fs::write(src.join("main.rs"), MAIN).expect("write");
    dir
}

fn shape_of(root: &Path, query: &str) -> Shape {
    classify(root, query)
}

#[test]
fn each_query_shape_picks_its_branch_and_the_header_says_so() {
    let dir = crate_dir();
    let cases = [
        ("src/walk.rs", Shape::Path, "find: PATH"),
        ("list_files", Shape::Ident, "find: IDENT"),
        (r"fn \w+_files", Shape::Regex, "find: REGEX"),
        ("debounce modified event", Shape::Phrase, "find: PHRASE"),
    ];
    for (query, shape, header) in cases {
        assert_eq!(shape_of(dir.path(), query), shape, "{query}");
        let found = find(dir.path(), query, &[]).expect("find");
        assert!(!found.is_empty(), "N==0 for {query}");
        assert!(render_text(&found, FIND_BUDGET).starts_with(header), "{query}");
    }
}

#[test]
fn a_missing_path_is_not_a_path() {
    let dir = crate_dir();
    assert_eq!(shape_of(dir.path(), "src/nothing.rs"), Shape::Regex);
}

#[test]
fn an_identifier_lists_the_definition_before_other_occurrences() {
    let dir = crate_dir();
    let found = find(dir.path(), "list_files", &[]).expect("find");
    assert_eq!(found.hits[0].path, "src/walk.rs");
    assert!(found.hits.iter().any(|hit| hit.path == "src/main.rs"));
    let text = render_text(&found, FIND_BUDGET);
    assert!(text.contains("next: ss section src/walk.rs list_files; ss graph list_files"), "{text}");
    assert!(text.contains("note: 2 other occurrences in 1 files"), "{text}");
    assert!(text.contains("Walk files respecting gitignore rules."), "{text}");
}

#[test]
fn a_phrase_ranks_the_file_holding_most_distinct_terms_first() {
    let dir = crate_dir();
    let found = find(dir.path(), "the watcher debounce for modified events", &[]).expect("find");
    assert_eq!(found.hits[0].path, "src/watch.rs");
    assert_eq!(found.hits[0].tags, vec!["term-match"]);
}

#[test]
fn semantic_results_fuse_with_term_matches_and_carry_their_tag() {
    let dir = crate_dir();
    let semantic = vec![(dir.path().join("src/main.rs").to_string_lossy().into_owned(), 1, 6)];
    let found = find(dir.path(), "debounce modified event", &semantic).expect("find");
    let main = found.hits.iter().find(|hit| hit.path == "src/main.rs").expect("semantic file present");
    assert_eq!(main.tags, vec!["semantic"]);
    let watch = found.hits.iter().position(|hit| hit.path == "src/watch.rs").expect("term file present");
    assert!(watch < 2, "equal rank-1 votes tie by path, so the term file stays in the top two");
}

#[test]
fn terms_drop_stopwords_and_split_identifiers() {
    assert_eq!(terms_of("the InstanceLock try_lock"), vec!["instancelock", "instance", "lock", "try_lock", "try"]);
}

#[test]
fn a_tight_budget_drops_tail_files_and_says_so_on_a_cut_line() {
    let dir = crate_dir();
    let found = find(dir.path(), "list_files", &[]).expect("find");
    let text = render_text(&found, 40);
    assert!(text.contains("cut: "), "{text}");
    assert!(text.contains("src/walk.rs"), "top file always prints: {text}");
}

#[test]
fn nothing_found_is_empty_and_a_bad_regex_is_an_error() {
    let dir = crate_dir();
    assert!(find(dir.path(), "zzqxnonexistent", &[]).expect("find").is_empty());
    assert!(find(dir.path(), "fn (unclosed", &[]).is_err());
}

#[test]
fn json_carries_shape_files_and_next() {
    let dir = crate_dir();
    let found = find(dir.path(), "list_files", &[]).expect("find");
    let value: serde_json::Value = serde_json::from_str(&render_json(&found)).expect("json");
    assert_eq!(value["shape"], "IDENT");
    assert_eq!(value["files"][0]["path"], "src/walk.rs");
    assert!(value["next"].as_str().unwrap().starts_with("ss section src/walk.rs"));
}

#[test]
fn the_found_line_reports_files_matches_and_definition_state() {
    let dir = crate_dir();
    let ident = render_text(&find(dir.path(), "list_files", &[]).expect("find"), FIND_BUDGET);
    assert!(ident.contains("\nfound: 2 files, 3 matches; definition: yes (1)\n"), "{ident}");
    let undefined = render_text(&find(dir.path(), "files", &[]).expect("find"), FIND_BUDGET);
    assert!(undefined.contains("definition: no"), "{undefined}");
    let none = render_text(&find(dir.path(), "zzqxnonexistent", &[]).expect("find"), FIND_BUDGET);
    assert!(none.contains("\nfound: 0\n"), "{none}");
    let phrase = render_text(&find(dir.path(), "debounce modified event", &[]).expect("find"), FIND_BUDGET);
    assert!(phrase.contains("[term-match]") && phrase.contains("\nfound: 1 files"), "{phrase}");
}

#[test]
fn two_definitions_of_one_name_are_ambiguous() {
    let dir = crate_dir();
    fs::write(dir.path().join("src/other.rs"), "pub fn list_files() {}\n").expect("write");
    let text = render_text(&find(dir.path(), "list_files", &[]).expect("find"), FIND_BUDGET);
    assert!(text.contains("definition: ambiguous (2)"), "{text}");
}

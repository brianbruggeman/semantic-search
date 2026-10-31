// tests assert with expect messages
#![allow(clippy::expect_used, clippy::unwrap_used)]
//! `ss outline` and `ss section` on real files in a tempfile tree, through the built binary.

use std::fmt::Write as _;
use std::fs;
use std::path::Path;
use std::process::{Command, Output};

const GUIDE: &str = "# Guide\nIntro line of the guide.\n\n## Install\nRun the installer.\n\n### Linux\nUse the package manager.\n\n### macOS\nUse brew.\n\n## Usage\nCall it.\n\n```sh\n# not a heading\nss go\n```\n";

const COUNTER_RS: &str = "//! crate doc\n\n/// Adds two numbers.\npub fn add(left: u32, right: u32) -> u32 {\n    left + right\n}\n\npub struct Counter {\n    total: u32,\n}\n\nimpl Counter {\n    /// Bumps the total.\n    pub fn bump(&mut self) {\n        self.total += 1;\n    }\n}\n";

const BOX_PY: &str = "\"\"\"module\"\"\"\n\ndef greet(name):\n    \"\"\"Say hello.\"\"\"\n    return \"hi \" + name\n\n\nclass Box:\n    \"\"\"A box.\"\"\"\n\n    def open(self):\n        \"\"\"Open it.\"\"\"\n        return True\n";

const NOTES_RST: &str = "Notes\n=====\n\nTop level text.\n\nSetup\n-----\n\nInstall first.\n\nDetails\n~~~~~~~\n\nFine print.\n";

fn rows(count: usize) -> String {
    let mut text = String::new();
    for number in 1..=count {
        writeln!(text, "row {number}").expect("write");
    }
    text
}

fn tree() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    for (name, body) in [
        ("guide.md", GUIDE.to_owned()),
        ("counter.rs", COUNTER_RS.to_owned()),
        ("box.py", BOX_PY.to_owned()),
        ("notes.rst", NOTES_RST.to_owned()),
        ("data.log", rows(30)),
    ] {
        fs::write(dir.path().join(name), body).expect("write");
    }
    dir
}

fn ss(args: &[&str], cwd: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_ss"))
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("spawn")
}

fn stdout(args: &[&str], cwd: &Path) -> String {
    let out = ss(args, cwd);
    assert!(out.status.success(), "{args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8(out.stdout).expect("utf8")
}

#[test]
fn markdown_outline_nests_headings_with_ranges_and_first_lines() {
    let dir = tree();

    let out = stdout(&["outline", "guide.md"], dir.path());

    let expected = "guide.md: 19 lines\n\
        1-19 # Guide -- Intro line of the guide.\n\
        \x20 4-11 ## Install -- Run the installer.\n\
        \x20   7-8 ### Linux -- Use the package manager.\n\
        \x20   10-11 ### macOS -- Use brew.\n\
        \x20 13-19 ## Usage -- Call it.\n";
    assert_eq!(out, expected);
}

#[test]
fn a_fenced_hash_line_is_not_a_heading() {
    let dir = tree();

    let out = stdout(&["outline", "guide.md"], dir.path());

    assert!(!out.contains("not a heading"), "{out}");
}

#[test]
fn section_prints_the_lines_of_its_range_with_numbers() {
    let dir = tree();

    let out = stdout(&["section", "guide.md", "install"], dir.path());

    let body: Vec<&str> = GUIDE.lines().skip(3).take(8).collect();
    let mut expected = String::from("guide.md:4-11 ## Install\n");
    for (offset, line) in body.iter().enumerate() {
        writeln!(expected, "{}: {line}", 4 + offset).expect("write");
    }
    assert_eq!(out, expected);
}

#[test]
fn an_ambiguous_name_lists_the_matches_and_fails() {
    let dir = tree();

    let out = ss(&["section", "guide.md", "u"], dir.path());

    let err = String::from_utf8(out.stderr).expect("utf8");
    assert_eq!(out.status.code(), Some(2), "{err}");
    assert!(err.contains("ambiguous"), "{err}");
    for listed in ["1-19 # Guide", "7-8 ### Linux", "13-19 ## Usage"] {
        assert!(err.contains(listed), "{listed} missing from {err}");
    }
}

#[test]
fn a_missing_name_lists_close_matches() {
    let dir = tree();

    let out = ss(&["section", "guide.md", "installation"], dir.path());

    let err = String::from_utf8(out.stderr).expect("utf8");
    assert_eq!(out.status.code(), Some(2), "{err}");
    assert!(err.contains("no heading or symbol"), "{err}");
    assert!(err.contains("4-11 ## Install") || err.contains("1-19 # Guide"), "{err}");
}

#[test]
fn line_selects_the_innermost_section() {
    let dir = tree();

    let leaf = stdout(&["section", "guide.md", "--line", "8"], dir.path());
    let parent = stdout(&["section", "guide.md", "--line", "5"], dir.path());

    assert!(leaf.starts_with("guide.md:7-8 ### Linux\n7: ### Linux\n8: Use the package manager.\n"), "{leaf}");
    assert!(parent.starts_with("guide.md:4-11 ## Install\n"), "{parent}");
}

#[test]
fn context_adds_lines_around_the_range() {
    let dir = tree();

    let out = stdout(&["section", "guide.md", "linux", "--context", "1"], dir.path());

    assert!(out.starts_with("guide.md:7-8 ### Linux\n6: \n7: ### Linux\n"), "{out}");
    assert!(out.ends_with("8: Use the package manager.\n9: \n"), "{out}");
}

#[test]
fn a_tiny_budget_keeps_the_first_entry_and_counts_the_cut() {
    let dir = tree();

    let out = stdout(&["outline", "guide.md", "--budget", "5"], dir.path());

    assert!(out.contains("1-19 # Guide"), "{out}");
    assert!(out.contains("cut: 4 entries not shown"), "{out}");
    let body = stdout(&["section", "guide.md", "guide", "--budget", "30"], dir.path());
    assert!(body.contains("cut: "), "{body}");
    assert!(body.contains("resume at line"), "{body}");
}

#[test]
fn depth_limits_the_nesting_and_says_what_was_dropped() {
    let dir = tree();

    let out = stdout(&["outline", "guide.md", "--depth", "2"], dir.path());

    assert!(out.contains("## Install") && !out.contains("### Linux"), "{out}");
    assert!(out.contains("cut: 2 entries not shown"), "{out}");
}

#[test]
fn a_rust_outline_lists_symbols_with_signature_doc_and_range() {
    let dir = tree();

    let out = stdout(&["outline", "counter.rs"], dir.path());

    assert!(out.starts_with("counter.rs: 17 lines\n"), "{out}");
    assert!(out.contains("4-6 fn pub fn add(left: u32, right: u32) -> u32 -- Adds two numbers.\n"), "{out}");
    assert!(out.contains("8-10 struct pub struct Counter\n"), "{out}");
    assert!(out.contains("  14-16 method [Counter] pub fn bump(&mut self) -- Bumps the total.\n"), "{out}");
}

#[test]
fn a_rust_section_by_symbol_prints_its_body() {
    let dir = tree();

    let out = stdout(&["section", "counter.rs", "add"], dir.path());

    let expected = "counter.rs:4-6 fn pub fn add(left: u32, right: u32) -> u32\n\
        4: pub fn add(left: u32, right: u32) -> u32 {\n5:     left + right\n6: }\n";
    assert_eq!(out, expected);
}

#[test]
fn a_qualified_method_name_matches_exactly() {
    let dir = tree();

    let out = stdout(&["section", "counter.rs", "Counter::bump"], dir.path());

    assert!(out.starts_with("counter.rs:14-16 method [Counter] pub fn bump"), "{out}");
}

#[test]
fn a_python_outline_lists_functions_classes_and_methods() {
    let dir = tree();

    let out = stdout(&["outline", "box.py"], dir.path());

    assert!(out.contains("3-5 fn def greet(name): -- Say hello.\n"), "{out}");
    assert!(out.contains("8-13 type class Box: -- A box.\n"), "{out}");
    assert!(out.contains("  11-13 method [Box] def open(self): -- Open it.\n"), "{out}");
}

#[test]
fn an_rst_outline_follows_underline_styles_by_first_appearance() {
    let dir = tree();

    let out = stdout(&["outline", "notes.rst"], dir.path());

    let expected = "notes.rst: 14 lines\n\
        1-14 1. Notes -- Top level text.\n\
        \x20 6-14 2. Setup -- Install first.\n\
        \x20   11-14 3. Details -- Fine print.\n";
    assert_eq!(out, expected);
}

#[test]
fn plain_text_shows_five_lines_each_end_and_the_total() {
    let dir = tree();

    let out = stdout(&["outline", "data.log"], dir.path());

    let expected = "data.log: 30 lines\n1: row 1\n2: row 2\n3: row 3\n4: row 4\n5: row 5\n\
        ... 20 lines not shown\n26: row 26\n27: row 27\n28: row 28\n29: row 29\n30: row 30\n";
    assert_eq!(out, expected);
}

#[test]
fn a_file_with_no_structure_refuses_a_section() {
    let dir = tree();

    let out = ss(&["section", "data.log", "row"], dir.path());

    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("no headings or symbols"));
}

#[test]
fn the_outline_of_a_large_doc_is_smaller_than_the_doc() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut doc = String::from("# Spec\n");
    for part in 1..=40 {
        write!(doc, "\n## Part {part}\nSummary of part {part}.\n").expect("write");
        doc.push_str(&"filler text for the part body\n".repeat(20));
    }
    fs::write(dir.path().join("spec.md"), &doc).expect("write");

    let out = stdout(&["outline", "spec.md"], dir.path());

    assert!(!out.is_empty() && out.len() * 4 < doc.len(), "{} vs {}", out.len(), doc.len());
    assert!(out.chars().count() <= 400 * 4, "{}", out.chars().count());
}

#[test]
fn a_line_in_the_doc_comment_above_a_symbol_selects_that_symbol() {
    let dir = tree();

    let out = stdout(&["section", "counter.rs", "--line", "3"], dir.path());

    assert!(out.starts_with("counter.rs:4-6 fn pub fn add"), "{out}");
}

#[test]
fn a_line_in_the_attributes_above_a_method_selects_the_method() {
    let dir = tempfile::tempdir().expect("tempdir");
    let source = "struct Gate;\n\nimpl Gate {\n    /// Opens.\n    #[must_use]\n    pub fn open(&self) -> bool {\n        true\n    }\n}\n";
    fs::write(dir.path().join("gate.rs"), source).expect("write");

    let out = stdout(&["section", "gate.rs", "--line", "5"], dir.path());

    assert!(out.starts_with("gate.rs:6-8 method [Gate] pub fn open"), "{out}");
}

#[test]
fn a_line_past_a_blank_gap_after_code_still_fails_as_outside() {
    let dir = tempfile::tempdir().expect("tempdir");
    let source = "pub fn first() -> u8 {\n    1\n}\n\nconst BETWEEN: u8 = 2 + 3;\n\npub fn second() -> u8 {\n    2\n}\n";
    fs::write(dir.path().join("gap.rs"), source).expect("write");

    let out = ss(&["section", "gap.rs", "--line", "4"], dir.path());

    assert!(!out.status.success(), "a blank line after a body must not leak into the next symbol");
}

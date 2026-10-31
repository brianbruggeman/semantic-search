// tests assert with expect messages
#![allow(clippy::expect_used, clippy::unwrap_used)]
//! `ss outline` and `ss section` on Rust `impl` blocks, through the library entry points on real files in a tempdir.

use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use ss::read::{OUTLINE_BUDGET, ReadError, SECTION_BUDGET, Target, outline_file, section_file};

const SOURCE: &str = "pub enum ChunkConfig {\n    Small,\n}\n\nimpl ChunkConfig {\n    pub fn small() -> Self {\n        Self::Small\n    }\n}\n\n/// Runs the chunker.\n#[allow(dead_code)]\nimpl Pipe for ChunkConfig {\n    type Out = u32;\n\n    fn call(&self) -> u32 {\n        1\n    }\n\n    fn name(&self) -> u32 {\n        2\n    }\n}\n";

fn write_source() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("config.rs");
    fs::write(&path, SOURCE).expect("write source");
    (dir, path)
}

#[test]
fn outline_lists_impl_blocks_with_ranges_and_nests_their_methods() {
    let (_dir, path) = write_source();

    let out = outline_file(&path, OUTLINE_BUDGET, None).expect("outline");

    let expected = "1-3 enum pub enum ChunkConfig\n5-9 impl ChunkConfig\n  6-8 method [ChunkConfig] pub fn small() -> Self\n13-23 impl Pipe for ChunkConfig -- Runs the chunker.\n  16-18 method [ChunkConfig] fn call(&self) -> u32\n  20-22 method [ChunkConfig] fn name(&self) -> u32\n";
    assert!(out.ends_with(expected), "{out}");
    assert!(!out.contains("cut:"), "{out}");
}

fn section_header(path: &Path, target: Target<'_>) -> Result<String, ReadError> {
    let out = section_file(path, target, SECTION_BUDGET, 0)?;
    let header = out.lines().next().expect("a header line");
    Ok(header.rsplit_once(".rs:").expect("path then range").1.to_owned())
}

#[test]
fn section_by_impl_name_is_case_insensitive_and_selects_the_whole_block() {
    let (_dir, path) = write_source();

    let header = section_header(&path, Target::Name("IMPL pipe FOR chunkconfig")).expect("section");

    assert_eq!(header, "13-23 impl Pipe for ChunkConfig");
}

#[test]
fn a_bare_type_name_is_ambiguous_between_the_type_and_its_impl_blocks() {
    let (_dir, path) = write_source();

    let error = section_header(&path, Target::Name("ChunkConfig")).expect_err("ambiguous");

    let ReadError::Ambiguous { count, close, .. } = error else { panic!("{error:?}") };
    assert_eq!(count, 3);
    assert_eq!(close, "  1-3 enum pub enum ChunkConfig\n  5-9 impl ChunkConfig\n  13-23 impl Pipe for ChunkConfig");
}

#[test]
fn lines_on_the_impl_header_doc_attribute_associated_type_and_gaps_select_the_impl() {
    let (_dir, path) = write_source();

    for (line, expected) in [
        (5, "5-9 impl ChunkConfig"),
        (9, "5-9 impl ChunkConfig"),
        (11, "13-23 impl Pipe for ChunkConfig"),
        (12, "13-23 impl Pipe for ChunkConfig"),
        (13, "13-23 impl Pipe for ChunkConfig"),
        (14, "13-23 impl Pipe for ChunkConfig"),
        (15, "13-23 impl Pipe for ChunkConfig"),
        (19, "13-23 impl Pipe for ChunkConfig"),
        (23, "13-23 impl Pipe for ChunkConfig"),
    ] {
        let header = section_header(&path, Target::Line(line)).expect("section");
        assert_eq!(header, expected, "line {line}");
    }
}

#[test]
fn lines_inside_methods_still_select_the_method() {
    let (_dir, path) = write_source();

    for (line, expected) in [
        (6, "6-8 method [ChunkConfig] pub fn small() -> Self"),
        (7, "6-8 method [ChunkConfig] pub fn small() -> Self"),
        (17, "16-18 method [ChunkConfig] fn call(&self) -> u32"),
        (21, "20-22 method [ChunkConfig] fn name(&self) -> u32"),
    ] {
        let header = section_header(&path, Target::Line(line)).expect("section");
        assert_eq!(header, expected, "line {line}");
    }
}

#[test]
fn a_doc_comment_above_a_method_inside_an_impl_selects_the_method_not_the_impl() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("gate.rs");
    fs::write(&path, "struct Gate;\n\nimpl Gate {\n    /// Opens.\n    #[must_use]\n    pub fn open(&self) -> bool {\n        true\n    }\n}\n").expect("write");

    let header = section_header(&path, Target::Line(4)).expect("section");

    assert_eq!(header, "6-8 method [Gate] pub fn open(&self) -> bool");
}

#[test]
fn a_gap_line_in_an_impl_over_the_token_budget_selects_the_nearest_member() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("big.rs");
    let mut source = String::from("struct Big;\n\nimpl Big {\n    fn first(&self) -> u8 {\n        1\n    }\n\n    fn second(&self) -> u8 {\n");
    for index in 0..400 {
        writeln!(source, "        let value_{index} = self.compute_something_long_enough_to_count({index});").expect("write to string");
    }
    source.push_str("        2\n    }\n}\n");
    fs::write(&path, source).expect("write");

    let header = section_header(&path, Target::Line(7)).expect("section");

    assert!(header.contains("method [Big] fn first(&self) -> u8"), "{header}");
}

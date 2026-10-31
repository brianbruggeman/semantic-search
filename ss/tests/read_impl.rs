//! `ss outline` and `ss section` on Rust `impl` blocks, through the library entry points on real files in a tempdir.

use std::fs;
use std::path::PathBuf;

use ss::read::{OUTLINE_BUDGET, outline_file};

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

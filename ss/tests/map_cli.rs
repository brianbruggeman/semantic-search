// tests assert with expect messages
#![allow(clippy::expect_used, clippy::unwrap_used)]
//! `ss map` on a real tempfile tree, through the built binary.

use std::fs;
use std::path::Path;
use std::process::Command;

fn tree() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    for index in 0..12 {
        let sub = dir.path().join(format!("mod{index}"));
        fs::create_dir_all(&sub).expect("mkdir");
        fs::write(sub.join("lib.rs"), format!("//! Module number {index} does its work.\npub fn run() {{}}\n")).expect("write");
        fs::write(sub.join("bare.rs"), "pub fn bare() {}\n").expect("write");
    }
    dir
}

fn run(args: &[&str], cwd: &Path) -> (bool, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_ss")).args(args).current_dir(cwd).output().expect("spawn");
    (out.status.success(), String::from_utf8(out.stdout).expect("utf8"))
}

#[test]
fn map_prints_directory_lines_coverage_and_a_cut_line_under_a_tiny_budget() {
    let dir = tree();

    let (ok, out) = run(&["map", ".", "--budget", "60"], dir.path());

    assert!(ok, "{out}");
    assert!(out.contains("/  2 files  ~"), "{out}");
    assert!(out.contains("cut: "), "{out}");
    assert!(out.ends_with("purpose coverage: 12 of 24 files have one (50%)\n"), "{out}");
}

#[test]
fn map_json_reports_the_same_coverage() {
    let dir = tree();

    let (ok, out) = run(&["map", "--json", "--budget", "60"], dir.path());

    assert!(ok, "{out}");
    assert!(out.contains("\"coverage\":{\"percent\":50,\"total\":24,\"with\":12}"), "{out}");
}

// tests assert with expect messages
#![allow(clippy::expect_used, clippy::unwrap_used)]
//! `ss --exact` against `rg -n --no-heading` on a small real-looking tree: same lines, never more bytes.

use std::fmt::Write as _;
use std::fs;
use std::path::Path;
use std::process::Command;

const RG: &str = "/opt/homebrew/bin/rg";

fn write(root: &Path, name: &str, body: &str) {
    let path = root.join(name);
    fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    fs::write(path, body).expect("write");
}

fn tree() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    write(
        root,
        "src/lib.rs",
        "//! walk files\npub mod walk;\npub mod render;\n\npub fn list_files(root: &Path) -> Vec<PathBuf> {\n    let mut files = Vec::new();\n    files.sort();\n    files\n}\n\npub fn render_files(files: &[PathBuf]) -> String {\n    files.iter().map(|f| f.display().to_string()).collect()\n}\n",
    );
    write(
        root,
        "src/walk.rs",
        "use ignore::WalkBuilder;\n\n/// Files honoring gitignore.\npub fn walk_files(root: &Path) -> Vec<PathBuf> {\n    WalkBuilder::new(root).build().collect()\n}\n\nfn helper() {}\n",
    );
    write(
        root,
        "src/render.rs",
        "pub fn shown(path: &str) -> String {\n    path.to_owned()\n}\n\nconst FOOTER: usize = 72;\n",
    );
    write(
        root,
        "docs/notes.txt",
        "Walk the tree.\nwalk is lowercase here.\nA files list: files, files.\n",
    );
    write(root, "README.md", "# ss\n\nsearch files. FILES in caps.\n");
    dir
}

fn run(program: &str, args: &[&str], cwd: &Path) -> Vec<u8> {
    Command::new(program)
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("spawn")
        .stdout
}

fn ss(args: &[&str], cwd: &Path) -> Vec<u8> {
    let mut all = vec!["--exact", "-n"];
    all.extend_from_slice(args);
    run(env!("CARGO_BIN_EXE_ss"), &all, cwd)
}

fn ss_args(args: &[&str], cwd: &Path) -> Vec<u8> {
    let mut all = vec!["--exact"];
    all.extend_from_slice(args);
    run(env!("CARGO_BIN_EXE_ss"), &all, cwd)
}

fn rg(args: &[&str], cwd: &Path) -> Vec<u8> {
    let mut all = vec!["-n", "--no-heading"];
    all.extend_from_slice(args);
    run(RG, &all, cwd)
}

fn sorted_lines(bytes: &[u8]) -> Vec<String> {
    let mut lines: Vec<String> = String::from_utf8_lossy(bytes)
        .lines()
        .map(str::to_owned)
        .collect();
    lines.sort();
    lines
}

fn assert_same_as_rg(args: &[&str], cwd: &Path) -> usize {
    let (ours, theirs) = (ss(args, cwd), rg(args, cwd));
    assert!(!theirs.is_empty(), "rg printed nothing for {args:?}");
    assert_eq!(sorted_lines(&ours), sorted_lines(&theirs), "args {args:?}");
    assert!(
        ours.len() <= theirs.len(),
        "ss is larger than rg for {args:?}"
    );
    ours.len()
}

#[test]
fn a_plain_pattern_prints_the_lines_rg_prints() {
    let dir = tree();
    assert_same_as_rg(&["files"], dir.path());
}

#[test]
fn files_only_prints_one_path_per_line() {
    let dir = tree();
    assert_same_as_rg(&["-l", "files"], dir.path());
}

#[test]
fn count_prints_path_colon_count() {
    let dir = tree();
    assert_same_as_rg(&["-c", "files"], dir.path());
}

#[test]
fn word_match_skips_substrings() {
    let dir = tree();
    assert_same_as_rg(&["-w", "walk"], dir.path());
}

#[test]
fn context_flags_print_dash_lines_and_separators() {
    let dir = tree();
    for flag in [["-A", "1"], ["-B", "1"], ["-C", "1"]] {
        let mut args = flag.to_vec();
        args.push("sort");
        assert_same_as_rg(&args, dir.path());
    }
}

#[test]
fn include_globs_limit_the_files() {
    let dir = tree();
    assert_same_as_rg(&["--glob", "*.rs", "files"], dir.path());
    assert_same_as_rg(&["--glob", "!*.rs", "files"], dir.path());
}

#[test]
fn repeated_patterns_and_paths_are_all_searched() {
    let dir = tree();
    assert_same_as_rg(&["-e", "helper", "-e", "FOOTER"], dir.path());
    assert_same_as_rg(&["files", "src", "docs"], dir.path());
}

#[test]
fn a_single_explicit_file_prints_no_path_prefix() {
    let dir = tree();
    let ours = ss(&["files", "src/lib.rs"], dir.path());
    assert!(!ours.is_empty(), "the file matches");
    assert_eq!(ours, rg(&["files", "src/lib.rs"], dir.path()));
}

#[test]
fn grep_alternation_backslash_pipe_means_alternation() {
    let dir = tree();
    let ours = ss(&["-rn", "helper\\|FOOTER", "."], dir.path());
    let theirs = rg(&["helper|FOOTER"], dir.path());
    assert!(!theirs.is_empty(), "rg finds both");
    assert_eq!(sorted_lines(&ours).len(), sorted_lines(&theirs).len());
}

#[test]
fn twenty_small_cases_never_cost_more_bytes_than_rg() {
    let dir = tree();
    let cases: Vec<Vec<&str>> = vec![
        vec!["files"],
        vec!["-i", "files"],
        vec!["-F", "Vec<PathBuf>"],
        vec!["-w", "files"],
        vec!["-l", "walk"],
        vec!["-c", "pub fn"],
        vec!["-C", "2", "helper"],
        vec!["-A", "2", "FOOTER"],
        vec!["-B", "1", "shown"],
        vec!["pub fn \\w+_files"],
        vec!["--glob", "*.md", "-i", "files"],
        vec!["-e", "mod walk", "-e", "mod render"],
        vec!["WalkBuilder", "src"],
        vec!["-l", "-i", "FILES"],
        vec!["-c", "-i", "files", "docs"],
        vec!["gitignore"],
        vec!["sort", "src/lib.rs"],
        vec!["72"],
        vec!["Walk"],
        vec!["-w", "-i", "walk", "docs"],
    ];
    let mut total = 0;
    for args in &cases {
        total += assert_same_as_rg(args, dir.path());
    }
    assert!(total > 0, "{} cases produced output", cases.len());
}

#[test]
fn a_result_over_the_budget_switches_to_the_grouped_form_with_a_cut_line() {
    let dir = tempfile::tempdir().expect("tempdir");
    for index in 0..6 {
        let mut body = String::new();
        for line in 0..80 {
            writeln!(body, "    let value_{line} = compute(needle, {index});")
                .expect("write to string");
        }
        write(dir.path(), &format!("src/part{index}.rs"), &body);
    }

    let out = String::from_utf8(ss(&["--budget", "100", "needle"], dir.path())).expect("utf8");

    assert!(out.starts_with("480 matches in 6 files"), "{out}");
    assert!(out.contains("cut:"), "{out}");
    assert!(out.chars().count() <= 400, "{} chars", out.chars().count());
}

#[test]
fn a_file_list_over_the_budget_is_cut_with_a_count() {
    let dir = tempfile::tempdir().expect("tempdir");
    for index in 0..60 {
        write(
            dir.path(),
            &format!("src/module_number_{index}.rs"),
            "needle\n",
        );
    }

    let out = String::from_utf8(ss(&["-l", "--budget", "50", "needle"], dir.path())).expect("utf8");

    assert!(out.contains("cut:"), "{out}");
    assert!(out.chars().count() <= 200, "{} chars", out.chars().count());
}

#[test]
fn without_n_the_output_has_no_line_numbers_like_grep() {
    let dir = tree();
    let ours = ss_args(&["files", "src/lib.rs"], dir.path());
    let grep = run("/usr/bin/grep", &["files", "src/lib.rs"], dir.path());
    assert!(!grep.is_empty(), "grep finds matches");
    assert_eq!(ours, grep);
}

#[test]
fn context_without_n_matches_grep_bytes() {
    let dir = tree();
    let ours = ss_args(&["-A", "1", "sort", "src/lib.rs"], dir.path());
    let grep = run(
        "/usr/bin/grep",
        &["-A", "1", "sort", "src/lib.rs"],
        dir.path(),
    );
    assert!(!grep.is_empty(), "grep finds matches");
    assert_eq!(ours, grep);
}

#[test]
fn dash_h_drops_the_path_prefix_instead_of_printing_help() {
    let dir = tree();
    let ours = ss_args(&["-rh", "files", "src"], dir.path());
    let grep = run("/usr/bin/grep", &["-rh", "files", "src"], dir.path());
    assert!(!grep.is_empty(), "grep finds matches");
    assert_eq!(sorted_lines(&ours), sorted_lines(&grep));
}

#[test]
fn a_two_file_grep_matches_byte_for_byte() {
    let dir = tree();
    let ours = ss_args(&["files", "src/lib.rs", "src/walk.rs"], dir.path());
    let grep = run(
        "/usr/bin/grep",
        &["files", "src/lib.rs", "src/walk.rs"],
        dir.path(),
    );
    assert_eq!(ours, grep);
}

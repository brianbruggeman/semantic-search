use std::path::{Path, PathBuf};

use super::*;

fn corpus() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/corpus")
}

fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).expect("create destination");
    for entry in std::fs::read_dir(from).expect("read dir").flatten() {
        let target = to.join(entry.file_name());
        if entry.path().is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).expect("copy file");
        }
    }
}

fn lines_matching(root: &Path, needle: &str) -> usize {
    let mut count = 0;
    for path in list_files(root) {
        let text = std::fs::read_to_string(&path).expect("corpus files are utf-8");
        count += text.lines().filter(|line| line.contains(needle)).count();
    }
    count
}

#[test]
fn a_literal_count_equals_the_lines_containing_it_in_the_corpus() {
    let regex = compile("rrf_fuse", true, false).expect("pattern");

    let found = scan(&corpus(), &regex, 3);

    let total: usize = found.iter().map(|file| file.total).sum();
    assert_eq!(total, lines_matching(&corpus(), "rrf_fuse"));
    assert!(total > 0, "the corpus defines rrf_fuse");
    assert!(found.iter().all(|file| file.lines.len() <= 3));
}

#[test]
fn fixed_strings_escape_regex_metacharacters_that_the_regex_form_reads_as_syntax() {
    let literal = compile("Result<", true, false).expect("pattern");
    let broken = compile("Vec<(i64", false, false);

    let found = scan(&corpus(), &literal, 1);

    assert!(found.iter().map(|file| file.total).sum::<usize>() > 0);
    assert!(matches!(broken, Err(ExactError::Pattern(_))), "{broken:?}");
}

#[test]
fn a_regex_matches_line_by_line_and_records_one_based_line_numbers() {
    let regex = compile(r"pub fn rrf_fuse\(", false, false).expect("pattern");

    let found = scan(&corpus(), &regex, 5);

    let file = found
        .iter()
        .find(|file| file.path.ends_with("ss/src/query.rs"))
        .expect("query.rs defines it");
    let text = std::fs::read_to_string(&file.path).expect("read");
    let (line, shown) = &file.lines[0];
    assert_eq!(
        text.lines().nth(line - 1).map(str::trim),
        Some(shown.as_str())
    );
}

#[test]
fn case_folding_is_opt_in() {
    let exact = compile("embedpool", true, false).expect("pattern");
    let folded = compile("embedpool", true, true).expect("pattern");

    assert_eq!(scan(&corpus(), &exact, 1).len(), 0);
    assert!(scan(&corpus(), &folded, 1).len() > 3);
}

#[test]
fn gitignored_binary_and_oversize_files_are_not_scanned() {
    let dir = tempfile::tempdir().expect("tempdir");
    copy_tree(&corpus().join("ss/src"), &dir.path().join("src"));
    std::fs::write(dir.path().join(".gitignore"), "ignored.rs\n").expect("gitignore");
    std::fs::write(dir.path().join("ignored.rs"), "fn rrf_fuse() {}\n").expect("write");
    std::fs::write(dir.path().join("blob.bin"), b"\0\0rrf_fuse\0").expect("write");
    let big = "rrf_fuse\n".repeat(usize::try_from(MAX_FILE_BYTES / 9).expect("fits") + 2);
    std::fs::write(dir.path().join("big.txt"), big).expect("write");
    let regex = compile("rrf_fuse", true, false).expect("pattern");

    let found = scan(dir.path(), &regex, 1);

    let names: Vec<&str> = found.iter().map(|file| file.path.as_str()).collect();
    assert!(names.iter().all(|name| name.contains("/src/")), "{names:?}");
}

#[test]
fn a_very_long_line_is_clipped_in_the_stored_match() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        dir.path().join("min.js"),
        format!("var a = '{}';\n", "x".repeat(900)),
    )
    .expect("write");
    let regex = compile("var a", true, false).expect("pattern");

    let found = scan(dir.path(), &regex, 1);

    let shown = &found[0].lines[0].1;
    assert_eq!(shown.chars().count(), LINE_CHARS + 3);
    assert!(shown.ends_with("..."));
}

use std::path::Path;

use tempfile::TempDir;

use super::*;

const RUST_BEFORE: &str = "pub fn keep(value: u32) -> u32 {
    value + 1
}

pub fn change(items: &[u32]) -> u32 {
    let mut total = 0;
    for item in items {
        total += item;
    }
    total
}

pub fn remove(value: u32) -> u32 {
    value * 2
}
";

const RUST_AFTER: &str = "pub fn keep(value: u32) -> u32 {
    value + 1
}

pub fn change(items: &[u32]) -> u32 {
    items.iter().sum()
}

pub fn added(value: u32) -> u32 {
    value - 1
}
";

const PY_BEFORE: &str = "def greet(name):
    return \"hello \" + name


class Counter:
    def __init__(self):
        self.count = 0

    def bump(self):
        self.count += 1
";

const PY_AFTER: &str = "def greet(name):
    return \"hello \" + name


class Counter:
    def __init__(self):
        self.count = 0

    def bump(self):
        self.count += 2
";

fn git(dir: &Path, args: &[&str]) {
    let owned: Vec<String> = args.iter().map(|arg| (*arg).to_owned()).collect();
    run(dir, &owned).expect("git command in the fixture repo");
}

fn commit(dir: &Path, files: &[(&str, &str)], message: &str) {
    for (name, body) in files {
        std::fs::write(dir.join(name), body).expect("write fixture file");
    }
    git(dir, &["add", "-A"]);
    git(dir, &["-c", "user.name=t", "-c", "user.email=t@users.noreply.github.com", "commit", "-q", "-m", message]);
}

fn fixture() -> TempDir {
    let dir = tempfile::tempdir().expect("temp dir");
    git(dir.path(), &["init", "-q"]);
    commit(
        dir.path(),
        &[("calc.rs", RUST_BEFORE), ("app.py", PY_BEFORE), ("notes.txt", "one\n")],
        "add calc and app",
    );
    commit(
        dir.path(),
        &[("calc.rs", RUST_AFTER), ("app.py", PY_AFTER), ("notes.txt", "one\ntwo\nthree\n")],
        "rework calc and bump",
    );
    dir
}

fn show(dir: &Path, view: &View, budget: Option<usize>) -> String {
    render(dir, &Source::Show("HEAD".into()), view, budget, None).expect("render show")
}

#[test]
fn summary_names_exact_symbols_with_counts() {
    let dir = fixture();
    let text = show(dir.path(), &View::Summary, None);
    assert!(text.lines().next().is_some_and(|line| line.ends_with("rework calc and bump")), "{text}");
    assert!(text.contains("calc.rs +"), "{text}");
    assert!(text.contains("  M fn calc::change (+1 -5)"), "{text}");
    assert!(text.contains("  D fn calc::remove"), "{text}");
    assert!(text.contains("  A fn calc::added"), "{text}");
    assert!(!text.contains("calc::keep"), "{text}");
    assert!(text.contains("  M method app::Counter::bump (+1 -1)"), "{text}");
    assert!(text.contains("notes.txt +2 -0"), "{text}");
    assert!(!text.contains("cut:"), "{text}");
}

#[test]
fn tiny_budget_cuts_and_says_how_to_ask() {
    let dir = fixture();
    let text = show(dir.path(), &View::Summary, Some(30));
    let footer = text.lines().find(|line| line.starts_with("cut:")).expect("cut line");
    assert!(footer.contains("--file PATH") && footer.contains("--symbol NAME") && footer.contains("--full"), "{text}");
    assert!(text.lines().count() >= 3, "{text}");
}

#[test]
fn symbol_drill_down_returns_only_that_symbols_hunks() {
    let dir = fixture();
    let source = Source::Show("HEAD".into());
    let text = render(dir.path(), &source, &View::Symbol("remove".into()), None, Some(0)).expect("render symbol");
    assert!(text.contains("-    value * 2"), "{text}");
    assert!(!text.contains("items.iter().sum()"), "{text}");
    assert!(!text.contains("count += 2"), "{text}");
}

#[test]
fn file_drill_down_prints_that_files_hunks_with_context() {
    let dir = fixture();
    let text = show(dir.path(), &View::File("app.py".into()), None);
    assert!(text.contains("+        self.count += 2"), "{text}");
    assert!(text.contains("     def bump(self):") || text.contains(" def bump(self):"), "{text}");
    assert!(!text.contains("calc.rs"), "{text}");
}

#[test]
fn full_view_is_the_whole_patch_and_cuts_under_a_small_budget() {
    let dir = fixture();
    let whole = show(dir.path(), &View::Full, Some(5000));
    assert!(whole.contains("calc.rs") && whole.contains("app.py") && whole.contains("notes.txt"), "{whole}");
    let cut = show(dir.path(), &View::Full, Some(60));
    assert!(cut.contains("cut:"), "{cut}");
}

#[test]
fn diff_of_working_tree_names_the_edited_symbol() {
    let dir = fixture();
    std::fs::write(dir.path().join("calc.rs"), RUST_AFTER.replace("value - 1", "value - 2")).expect("edit");
    let source = Source::Diff { revs: Vec::new(), staged: false };
    let text = render(dir.path(), &source, &View::Summary, None, None).expect("render diff");
    assert!(text.contains("calc.rs +1 -1"), "{text}");
    assert!(text.contains("  M fn calc::added (+1 -1)"), "{text}");
}

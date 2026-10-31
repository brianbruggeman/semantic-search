//! The structured output through the real `ss` and `ssd` binaries over a copy of the repo's own source.
// tests assert with expect messages
#![allow(clippy::expect_used, clippy::unwrap_used)]

mod support;

use std::path::Path;

use serde_json::Value;
use ss::render::tokens;
use support::{Env, copy_dir, text};

fn indexed_env() -> Env {
    let env = Env::new();
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    copy_dir(&source, &env.root().join("src"));
    let indexed = env.run("ss", &["index", env.root().to_str().expect("utf8")]);
    assert_eq!(indexed.status.code(), Some(0), "{}", text(&indexed));
    env
}

fn search(env: &Env, extra: &[&str]) -> std::process::Output {
    let root = env.root();
    let mut args = vec!["--path", root.to_str().expect("utf8")];
    args.extend_from_slice(extra);
    env.run("ss", &args)
}

#[proxima::test(runtime = "tokio")]
async fn default_output_is_one_structured_record_per_hit_and_lines_keeps_the_old_format() {
    let env = indexed_env();

    let structured = text(&search(&env, &["-n", "30", "rrf_fuse"]));
    let plain = text(&search(&env, &["-n", "3", "--lines", "rrf_fuse"]));

    assert!(
        structured.contains("fn rrf_fuse  tree/src/query.rs:"),
        "{structured}"
    );
    assert!(
        structured.contains("\n  sig: pub fn rrf_fuse("),
        "{structured}"
    );
    assert!(
        !plain.contains("sig:") && plain.contains("\n    "),
        "{plain}"
    );
    assert!(
        plain
            .lines()
            .next()
            .is_some_and(|head| head.contains(".rs:") && head.contains("  0.")),
        "{plain}"
    );
}

#[proxima::test(runtime = "tokio")]
async fn budget_caps_the_response_and_says_how_many_hits_it_cut() {
    let env = indexed_env();

    let output = text(&search(
        &env,
        &["-n", "10", "--budget", "400", "embed", "pool", "workers"],
    ));

    assert!(
        tokens(&output) <= 400,
        "{} tokens\n{output}",
        tokens(&output)
    );
    assert!(output.contains("hits cut by --budget 400)"), "{output}");
}

#[proxima::test(runtime = "tokio")]
async fn several_queries_come_back_grouped_in_one_response() {
    let env = indexed_env();

    let output = text(&search(&env, &["-n", "2", "idle exit", "watch registry"]));

    assert!(
        output.contains("# idle exit\n") && output.contains("# watch registry\n"),
        "{output}"
    );
}

#[proxima::test(runtime = "tokio")]
async fn json_output_is_one_document_with_schema_ranks_and_chunk_keys() {
    let env = indexed_env();

    let output = text(&search(&env, &["-n", "2", "--json", "rrf_fuse"]));

    let body: Value = serde_json::from_str(output.trim()).expect("one json document");
    assert_eq!(body["schema"], 1);
    assert_eq!(body["facets"][0]["records"][0]["rank"], 1);
    assert_eq!(
        body["facets"][0]["records"][0]["chunk_key"]
            .as_str()
            .map(str::len),
        Some(64)
    );
}

#[proxima::test(runtime = "tokio")]
async fn graph_subcommand_lists_callers_and_exits_one_for_an_unknown_symbol() {
    let env = indexed_env();
    let root = env.root();
    let root = root.to_str().expect("utf8");

    let near = env.run("ss", &["graph", "--path", root, "rrf_fuse"]);
    let json = env.run(
        "ss",
        &[
            "graph", "--path", root, "--depth", "2", "--json", "rrf_fuse",
        ],
    );
    let none = env.run("ss", &["graph", "--path", root, "no_such_symbol_zzz"]);

    let near_text = text(&near);
    assert!(near_text.contains("\n  callers: fuse (:"), "{near_text}");
    let body: Value = serde_json::from_str(text(&json).trim()).expect("json");
    assert_eq!(
        body["neighborhoods"]["items"][0]["symbol"]["name"],
        "rrf_fuse"
    );
    assert_eq!(none.status.code(), Some(1));
}

#[proxima::test(runtime = "tokio")]
async fn compact_prints_each_file_once_stays_in_budget_and_names_what_it_cut() {
    let env = indexed_env();

    let output = text(&search(
        &env,
        &[
            "--compact",
            "--budget",
            "120",
            "-n",
            "8",
            "embed",
            "pool",
            "workers",
        ],
    ));

    assert!(
        tokens(&output) <= 120,
        "{} tokens\n{output}",
        tokens(&output)
    );
    assert!(output.contains("cut: "), "{output}");
    assert!(output.contains("(use --budget 240)"), "{output}");
    let files: Vec<&str> = output
        .lines()
        .filter(|line| line.contains('/') && !line.starts_with(' ') && !line.starts_with("cut"))
        .collect();
    let unique: std::collections::HashSet<&&str> = files.iter().collect();
    assert_eq!(files.len(), unique.len(), "{output}");
}

#[proxima::test(runtime = "tokio")]
async fn exact_prints_one_path_text_line_per_match_and_exits_one_without_a_match() {
    let env = indexed_env();

    let found = env.run(
        "ss",
        &[
            "--path",
            env.root().to_str().expect("utf8"),
            "--exact",
            r"^pub fn rrf_\w+\(",
        ],
    );
    let none = env.run(
        "ss",
        &[
            "--path",
            env.root().to_str().expect("utf8"),
            "--exact",
            "zzz_no_such_text",
        ],
    );

    let found_text = text(&found);
    assert_eq!(found.status.code(), Some(0), "{found_text}");
    assert!(!found_text.contains("matches in"), "{found_text}");
    assert!(
        found_text.contains("query.rs:pub fn rrf_fuse("),
        "{found_text}"
    );
    assert_eq!(found_text.lines().count(), 1, "{found_text}");
    assert_eq!(none.status.code(), Some(1));
}

#[proxima::test(runtime = "tokio")]
async fn fixed_strings_search_a_flood_inside_the_budget_and_report_the_cut() {
    let env = indexed_env();

    let output = text(&env.run(
        "ss",
        &[
            "--path",
            env.root().to_str().expect("utf8"),
            "-F",
            "--budget",
            "150",
            "fn ",
        ],
    ));

    assert!(
        tokens(&output) <= 150,
        "{} tokens\n{output}",
        tokens(&output)
    );
    assert!(output.contains("cut: "), "{output}");
    assert!(output.contains("more\n"), "{output}");
}

#[proxima::test(runtime = "tokio")]
async fn a_bad_regex_is_an_error_not_a_panic() {
    let env = indexed_env();

    let output = env.run(
        "ss",
        &[
            "--path",
            env.root().to_str().expect("utf8"),
            "--exact",
            "(unclosed",
        ],
    );

    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("bad pattern"), "{stderr}");
}

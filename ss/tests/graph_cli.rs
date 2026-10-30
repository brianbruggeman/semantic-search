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

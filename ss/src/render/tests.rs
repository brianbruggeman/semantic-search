use super::*;
use crate::embed::EmbedderKind;
use crate::fixtures::{Corpus, hash_pool, own_src_corpus};
use crate::index::index_tree;
use crate::query::search_findings;

async fn findings_for(queries: &[&str], limit: usize) -> (Findings, Corpus) {
    let corpus = own_src_corpus(&hash_pool(), EmbedderKind::Hash);
    index_tree(&corpus.shard, &corpus.root())
        .await
        .expect("index");
    let queries: Vec<String> = queries.iter().map(|query| (*query).to_owned()).collect();
    let found = search_findings(&corpus.shard, &corpus.root(), &queries, limit)
        .await
        .expect("findings");
    (found, corpus)
}

#[test]
fn spaced_words_are_separate_queries_and_bare_words_are_one() {
    let words = |items: &[&str]| {
        items
            .iter()
            .map(|item| (*item).to_owned())
            .collect::<Vec<_>>()
    };

    assert_eq!(facets(&words(&["walk", "files"]), &[]), vec!["walk files"]);
    assert_eq!(
        facets(&words(&["idle exit", "watch registry"]), &[]),
        vec!["idle exit", "watch registry"]
    );
    assert_eq!(
        facets(&words(&["walk"]), &words(&["idle exit"])),
        vec!["idle exit", "walk"]
    );
}

#[test]
fn tokens_are_chars_over_four_rounded_up() {
    assert_eq!((tokens(""), tokens("abcd"), tokens("abcde")), (0, 1, 2));
}

#[proxima::test(runtime = "tokio")]
async fn a_symbol_hit_prints_header_signature_doc_and_neighbour_lines() {
    let (findings, corpus) = findings_for(&["rrf_fuse"], 30).await;
    let root = corpus.root();
    let record = findings.facets[0]
        .records
        .iter()
        .find(|record| {
            record
                .symbol
                .as_ref()
                .is_some_and(|symbol| symbol.name == "rrf_fuse")
        })
        .expect("rrf_fuse hit");

    let text = record_text(record, &root);

    assert!(text.starts_with("fn rrf_fuse  src/query.rs:"), "{text}");
    assert!(text.contains("  mod query  "), "{text}");
    assert!(text.contains("\n  sig: pub fn rrf_fuse("), "{text}");
    assert!(text.contains("\n  doc: Reciprocal rank fusion"), "{text}");
    assert!(text.contains("\n  called by: fuse (:"), "{text}");
    assert!(text.contains("\n  tests: "), "{text}");
}

#[proxima::test(runtime = "tokio")]
async fn a_budget_keeps_the_top_hit_reports_the_cut_and_stays_under_the_limit() {
    let (findings, corpus) = findings_for(&["embed pool worker threads"], 10).await;
    let root = corpus.root();
    let whole = findings_text(&findings, &root, None);

    let trimmed = findings_text(&findings, &root, Some(150));

    assert!(
        tokens(&whole) > 150,
        "need a response larger than the budget"
    );
    assert!(
        tokens(&trimmed) <= 150,
        "{} tokens\n{trimmed}",
        tokens(&trimmed)
    );
    assert!(trimmed.starts_with(&record_text(&findings.facets[0].records[0], &root)));
    assert!(trimmed.contains("hits cut by --budget 150)"), "{trimmed}");
}

#[proxima::test(runtime = "tokio")]
async fn a_budget_smaller_than_one_record_still_returns_the_top_hit() {
    let (findings, corpus) = findings_for(&["rrf_fuse"], 5).await;
    let root = corpus.root();

    let text = findings_text(&findings, &root, Some(1));

    let top = record_text(&findings.facets[0].records[0], &root);
    assert!(text.starts_with(&top), "{text}");
    assert!(text.contains("hits cut"), "{text}");
}

#[proxima::test(runtime = "tokio")]
async fn several_queries_are_grouped_per_facet_and_share_one_response() {
    let (findings, corpus) = findings_for(&["idle exit", "watch registry"], 3).await;
    let root = corpus.root();

    let text = findings_text(&findings, &root, None);

    assert_eq!(findings.facets.len(), 2);
    assert!(
        text.contains("# idle exit\n") && text.contains("# watch registry\n"),
        "{text}"
    );
    assert!(
        text.find("# idle exit").expect("first") < text.find("# watch registry").expect("second")
    );
}

#[proxima::test(runtime = "tokio")]
async fn json_carries_schema_ranks_chunk_keys_and_the_cut_count() {
    let (findings, _) = findings_for(&["rrf_fuse"], 5).await;

    let body: Value = serde_json::from_str(&findings_json(&findings, Some(60))).expect("json");

    assert_eq!(body["schema"], 1);
    assert_eq!(body["facets"][0]["records"][0]["rank"], 1);
    assert_eq!(
        body["facets"][0]["records"][0]["chunk_key"]
            .as_str()
            .map(str::len),
        Some(64)
    );
    assert!(body["cut"].as_u64().is_some_and(|cut| cut > 0), "{body}");
    assert_eq!(body["budget"], 60);
}

#[test]
fn the_lines_format_is_path_range_score_and_indented_preview() {
    let hit = Hit {
        path: "/w/src/walk.rs".to_owned(),
        start_line: 38,
        end_line: 50,
        score: 0.0328,
        preview: vec!["pub fn list_files() {".to_owned()],
    };

    let text = lines_text(&[hit], Path::new("/w"));

    assert_eq!(
        text,
        "src/walk.rs:38-50  0.0328\n    pub fn list_files() {\n"
    );
}

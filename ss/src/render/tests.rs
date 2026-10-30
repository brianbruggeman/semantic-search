use super::*;
use crate::embed::EmbedderKind;
use crate::exact::FileMatches;
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

fn exact_files() -> Vec<FileMatches> {
    let regex = crate::exact::compile("fn ", true, false).expect("pattern");
    let corpus = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/corpus");
    crate::exact::scan(&corpus, &regex, 5)
}

#[test]
fn an_exact_flood_is_capped_at_the_budget_and_says_what_it_cut() {
    let files = exact_files();
    let total: usize = files.iter().map(|file| file.total).sum();
    let here = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/corpus");

    let text = exact_text(&files, &here, 120);

    assert!(total > 200, "{total} matching lines expected in the corpus");
    assert!(tokens(&text) <= 120, "{} tokens\n{text}", tokens(&text));
    assert!(text.starts_with(&format!("{total} matches in {} files\n", files.len())));
    assert!(text.contains("\ncut: "), "{text}");
    assert!(text.contains("(use --budget 240)"), "{text}");
}

#[test]
fn an_exact_reply_groups_by_file_with_the_per_file_remainder_counted() {
    let files = exact_files();
    let here = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/corpus");

    let text = exact_text(&files, &here, 5000);

    let query = files
        .iter()
        .find(|file| file.path.ends_with("ss/src/query.rs"))
        .expect("query.rs");
    assert!(
        text.contains(&format!("ss/src/query.rs ({})\n  ", query.total)),
        "{text}"
    );
    assert!(
        text.contains(&format!("  +{} more\n", query.total - 5)),
        "{text}"
    );
    assert!(!text.contains("cut: "), "{text}");
    assert_eq!(text.matches("ss/src/query.rs").count(), 1);
}

#[test]
fn an_exact_search_with_no_match_prints_nothing() {
    let here = Path::new("/w");

    assert_eq!(exact_text(&[], here, 300), "");
}

#[proxima::test(runtime = "tokio")]
async fn the_compact_reply_keeps_every_hit_header_and_stays_inside_the_budget() {
    let (findings, corpus) = findings_for(&["embed pool worker threads"], 5).await;
    let root = corpus.root();
    let opts = Compact::lean(220);

    let text = compact_text(&findings, &root, &opts);

    assert!(tokens(&text) <= 220, "{} tokens\n{text}", tokens(&text));
    let hits = findings.facets[0].records.len();
    let shown = text
        .lines()
        .filter(|line| line.starts_with(' ') && !line.starts_with("  "))
        .count();
    let cut = text
        .lines()
        .find_map(|line| line.strip_prefix("cut: "))
        .and_then(|rest| rest.split(", ").find(|part| part.contains(" hits")))
        .and_then(|part| part.split(' ').next())
        .and_then(|number| number.parse::<usize>().ok())
        .unwrap_or(0);
    assert_eq!(shown + cut, hits, "{text}");
}

#[proxima::test(runtime = "tokio")]
async fn compact_groups_hits_under_one_file_line_and_lists_members_as_names_only() {
    let (findings, corpus) = findings_for(&["Store open sqlite wal"], 8).await;
    let root = corpus.root();
    let opts = Compact::lean(5000);

    let text = compact_text(&findings, &root, &opts);

    let files: Vec<&str> = text
        .lines()
        .filter(|line| line.contains('/') && !line.starts_with(' ') && !line.starts_with("cut"))
        .collect();
    let unique: std::collections::HashSet<&&str> = files.iter().collect();
    assert_eq!(files.len(), unique.len(), "a file is printed once\n{text}");
    assert!(!text.contains("cut: "), "{text}");
    assert!(
        text.lines()
            .filter(|line| line.trim_start().starts_with("members: "))
            .all(|line| !line.contains(".rs") && !line.contains(" (:")),
        "{text}"
    );
}

#[proxima::test(runtime = "tokio")]
async fn compact_with_a_tiny_budget_still_returns_the_top_hit_and_the_cut_line() {
    let (findings, corpus) = findings_for(&["rrf_fuse"], 5).await;
    let root = corpus.root();

    let text = compact_text(&findings, &root, &Compact::new(1));

    let top = findings.facets[0].records[0]
        .symbol
        .as_ref()
        .expect("symbol");
    assert!(
        text.starts_with(&format!("{} {}  ", top.kind, top.name)),
        "{text}"
    );
    assert!(text.contains("cut: 4 hits"), "{text}");
    assert!(text.contains("(use --budget 2)"), "{text}");
}

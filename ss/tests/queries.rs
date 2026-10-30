//! The eval query list is only meaningful while every gold chunk still exists in the frozen corpus.
// tests assert with expect messages
#![allow(clippy::expect_used)]

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use ss::chunk::{Language, chunk_source};

fn data_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data")
}

fn rust_files(dir: &Path, found: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("read corpus dir").flatten() {
        let path = entry.path();
        if path.is_dir() {
            rust_files(&path, found);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            found.push(path);
        }
    }
}

fn corpus_chunk_ids() -> HashSet<String> {
    let root = data_dir().join("corpus");
    let mut files = Vec::new();
    rust_files(&root, &mut files);
    files
        .iter()
        .flat_map(|file| {
            let rel = file
                .strip_prefix(&root)
                .expect("under root")
                .to_string_lossy()
                .into_owned();
            let text = std::fs::read_to_string(file).expect("read source");
            chunk_source(Language::from_extension("rs"), &text)
                .into_iter()
                .map(move |chunk| format!("{rel}:{}-{}", chunk.start_line, chunk.end_line))
        })
        .collect()
}

#[test]
fn every_gold_chunk_in_queries_tsv_exists_in_the_corpus_and_the_mix_is_balanced() {
    let ids = corpus_chunk_ids();
    let text = std::fs::read_to_string(data_dir().join("queries.tsv")).expect("queries.tsv");
    let rows: Vec<Vec<&str>> = text
        .lines()
        .filter(|line| !line.starts_with('#') && !line.trim().is_empty())
        .map(|line| line.split('\t').collect())
        .collect();

    assert!(rows.len() >= 25, "only {} queries", rows.len());
    for row in &rows {
        assert_eq!(row.len(), 4, "bad row {row:?}");
        let gold = format!("{}:{}", row[1], row[2]);
        assert!(
            ids.contains(&gold),
            "gold chunk {gold} is not a corpus chunk for {:?}",
            row[0]
        );
    }
    let identifiers = rows.iter().filter(|row| row[3] == "ident").count();
    let descriptive = rows.iter().filter(|row| row[3] == "desc").count();
    assert!(
        identifiers >= 8 && descriptive >= 8,
        "ident={identifiers} desc={descriptive}"
    );
}

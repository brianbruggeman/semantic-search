use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use proxima::pipe::Pipe;
use ss_proto::Hit;

use crate::embed::{EmbedPool, EmbedRequest, Lane, tokens};
use crate::error::IndexError;
use crate::index::Shard;
use crate::store::{Store, StoreConfig};

pub const RRF_K: f64 = 60.0;
pub const CANDIDATES: usize = 50;
pub const PREVIEW_LINES: usize = 3;

/// Reciprocal rank fusion of best-first id lists; returns ids with fused score, best first.
#[must_use]
pub fn rrf_fuse(rankings: &[Vec<i64>]) -> Vec<(i64, f64)> {
    let mut scores: HashMap<i64, f64> = HashMap::new();
    for ranking in rankings {
        for (position, id) in ranking.iter().enumerate() {
            *scores.entry(*id).or_default() +=
                1.0 / (RRF_K + f64::from(u32::try_from(position).unwrap_or(u32::MAX)) + 1.0);
        }
    }
    let mut fused: Vec<(i64, f64)> = scores.into_iter().collect();
    fused.sort_unstable_by(|left, right| right.1.total_cmp(&left.1).then(left.0.cmp(&right.0)));
    fused
}

/// Hybrid query: exact cosine scan + FTS5 BM25, fused by RRF. `scope` limits results to a subtree of the shard root.
#[proxima::instrument]
pub async fn search(
    shard: &Shard,
    scope: &Path,
    query: &str,
    limit: usize,
) -> Result<Vec<Hit>, IndexError> {
    let terms = tokens(query);
    if terms.is_empty() {
        return Ok(Vec::new());
    }
    let embedding = embed_query(shard.pool(), query).await?;
    let (config, root) = (Arc::clone(&shard.config), shard.root.clone());
    let prefix = scope.strip_prefix(&root).map_or_else(
        |_| String::new(),
        |rel| rel.to_string_lossy().replace('\\', "/"),
    );
    tokio::task::spawn_blocking(move || scan(&config, &root, &embedding, &terms, &prefix, limit))
        .await
        .map_err(|error| IndexError::Join(error.to_string()))?
}

/// Waits in the query lane for a worker, then embeds the query text; the span covers both the wait and the model.
#[proxima::instrument]
async fn embed_query(pool: &EmbedPool, query: &str) -> Result<Vec<f32>, IndexError> {
    let vectors = pool
        .call(EmbedRequest {
            texts: vec![query.to_owned()],
            lane: Lane::Query,
        })
        .await?;
    Ok(vectors.into_iter().next().unwrap_or_default())
}

#[proxima::instrument]
fn scan(
    config: &StoreConfig,
    root: &Path,
    embedding: &[f32],
    terms: &[String],
    prefix: &str,
    limit: usize,
) -> Result<Vec<Hit>, IndexError> {
    let store = Store::open(config)?;
    let by_vector = store.vector_search(embedding, prefix, CANDIDATES)?;
    let by_text = store.text_search(terms, prefix, CANDIDATES)?;
    render_hits(&store, root, &rrf_fuse(&[by_vector, by_text]), limit)
}

#[proxima::instrument]
fn render_hits(
    store: &Store,
    root: &Path,
    fused: &[(i64, f64)],
    limit: usize,
) -> Result<Vec<Hit>, IndexError> {
    let mut hits = Vec::new();
    for (id, score) in fused.iter().take(limit) {
        if let Some(row) = store.chunk(*id)? {
            hits.push(Hit {
                path: root.join(&row.path).to_string_lossy().into_owned(),
                start_line: row.start_line,
                end_line: row.end_line,
                score: *score,
                preview: row
                    .text
                    .lines()
                    .take(PREVIEW_LINES)
                    .map(str::to_owned)
                    .collect(),
            });
        }
    }
    Ok(hits)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embed::EmbedderKind;
    use crate::fixtures::{fastembed_pool, hash_pool, own_src_corpus};
    use crate::index::index_tree;

    #[test]
    fn rrf_rewards_agreement_between_rankings() {
        let fused = rrf_fuse(&[vec![1, 2, 3], vec![3, 2, 9]]);

        let order: Vec<i64> = fused.iter().map(|(id, _)| *id).collect();
        assert_eq!(order, vec![3, 2, 1, 9]);
    }

    #[test]
    fn rrf_scores_follow_one_over_k_plus_rank() {
        let fused = rrf_fuse(&[vec![7]]);

        assert!((fused[0].1 - 1.0 / 61.0).abs() < 1e-12);
    }

    #[test]
    fn rrf_of_nothing_is_empty() {
        assert_eq!(rrf_fuse(&[]).len(), 0);
    }

    #[proxima::test(runtime = "tokio")]
    async fn exact_identifier_is_found_through_the_text_leg() {
        let corpus = own_src_corpus(&hash_pool(), EmbedderKind::Hash);
        index_tree(&corpus.shard, &corpus.root())
            .await
            .expect("index");

        let hits = search(&corpus.shard, &corpus.root(), "require_git", 5)
            .await
            .expect("search");

        assert!(!hits.is_empty(), "no hits");
        assert!(
            hits.iter().any(|hit| hit.path.ends_with("walk.rs")),
            "hits={hits:?}"
        );
    }

    #[proxima::test(runtime = "tokio")]
    async fn hits_carry_path_line_span_score_and_three_line_preview_best_first() {
        let corpus = own_src_corpus(&hash_pool(), EmbedderKind::Hash);
        index_tree(&corpus.shard, &corpus.root())
            .await
            .expect("index");

        let hits = search(
            &corpus.shard,
            &corpus.root(),
            "embed pool worker threads",
            10,
        )
        .await
        .expect("search");

        assert!(hits.len() > 1, "need several hits, got {}", hits.len());
        assert!(hits.windows(2).all(|pair| pair[0].score >= pair[1].score));
        for hit in &hits {
            assert!(
                hit.path
                    .starts_with(corpus.root().to_str().expect("utf8 root")),
                "{}",
                hit.path
            );
            assert!(hit.start_line >= 1 && hit.start_line <= hit.end_line);
            assert!(!hit.preview.is_empty() && hit.preview.len() <= PREVIEW_LINES);
        }
    }

    #[proxima::test(runtime = "tokio")]
    async fn scope_limits_results_to_one_file() {
        let corpus = own_src_corpus(&hash_pool(), EmbedderKind::Hash);
        index_tree(&corpus.shard, &corpus.root())
            .await
            .expect("index");

        let hits = search(
            &corpus.shard,
            &corpus.root().join("src/walk.rs"),
            "walk",
            10,
        )
        .await
        .expect("search");

        assert!(!hits.is_empty(), "no hits in scope");
        assert!(
            hits.iter().all(|hit| hit.path.ends_with("src/walk.rs")),
            "hits={hits:?}"
        );
    }

    #[proxima::test(runtime = "tokio")]
    async fn query_without_searchable_terms_returns_nothing() {
        let corpus = own_src_corpus(&hash_pool(), EmbedderKind::Hash);
        index_tree(&corpus.shard, &corpus.root())
            .await
            .expect("index");

        let hits = search(&corpus.shard, &corpus.root(), "  ?! ", 5)
            .await
            .expect("search");

        assert_eq!(hits.len(), 0);
    }

    #[proxima::test(runtime = "tokio")]
    async fn real_model_puts_the_walker_chunk_in_the_top_three_for_the_gitignore_query() {
        let cache = tempfile::tempdir().expect("model cache dir");
        let pool = fastembed_pool(cache.path().to_path_buf());
        let corpus = own_src_corpus(&pool, EmbedderKind::FastEmbed);
        let started = std::time::Instant::now();
        let stats = index_tree(&corpus.shard, &corpus.root())
            .await
            .expect("index");
        let indexed_in = started.elapsed();

        let hits = search(
            &corpus.shard,
            &corpus.root(),
            "walk files respecting gitignore",
            3,
        )
        .await
        .expect("search");

        println!(
            "real index: chunks={} embedded={} in {:.2}s; top3={:?}",
            stats.chunks_seen,
            stats.chunks_embedded,
            indexed_in.as_secs_f64(),
            hits.iter()
                .map(|hit| format!("{}:{}-{}", hit.path, hit.start_line, hit.end_line))
                .collect::<Vec<_>>()
        );
        assert!(
            stats.chunks_embedded > 0 && hits.len() == 3,
            "hits={}",
            hits.len()
        );
        assert!(
            hits.iter().any(|hit| hit.path.ends_with("src/walk.rs")),
            "walker chunk missing from top 3: {hits:?}"
        );
    }
}

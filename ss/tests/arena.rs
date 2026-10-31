// tests assert with expect messages
#![allow(clippy::expect_used, clippy::unwrap_used)]

mod support;

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use ss::client::Client;
use ss::daemon::{RunOutcome, ServeError, run};
use ss::embed::{EmbedPool, hash_embed};
use ss::settings::Settings;
use ss_proto::{DaemonStatus, ErrorKind, Request, Response};
use support::{Env, copy_dir, wait_until_listening};
use tokio::task::JoinHandle;

const QUERY_TRIGGER: &str = "embed pool worker";
const INDEX_TRIGGER: &str = "gate_marker_unique_token";

type Daemon = JoinHandle<Result<RunOutcome, ServeError>>;

fn search(path: &Path, limit: u32) -> Request {
    Request::Search {
        query: QUERY_TRIGGER.to_owned(),
        path: path.to_string_lossy().into_owned(),
        limit,
    }
}

fn index(path: &Path) -> Request {
    Request::Index {
        path: path.to_string_lossy().into_owned(),
    }
}

/// Embed workers that block on `wait` while armed, but only for batches containing the trigger text.
struct Gate {
    armed: Arc<AtomicBool>,
    entered: Arc<AtomicUsize>,
    release: flume::Sender<()>,
    wait: flume::Receiver<()>,
}

impl Gate {
    fn new() -> Self {
        let (release, wait) = flume::unbounded::<()>();
        Self {
            armed: Arc::new(AtomicBool::new(false)),
            entered: Arc::new(AtomicUsize::new(0)),
            release,
            wait,
        }
    }

    fn pool(&self, workers: usize, trigger: &'static str) -> EmbedPool {
        let (armed, entered, wait) = (
            Arc::clone(&self.armed),
            Arc::clone(&self.entered),
            self.wait.clone(),
        );
        EmbedPool::spawn(workers, move || {
            let (armed, entered, wait) = (Arc::clone(&armed), Arc::clone(&entered), wait.clone());
            Ok(move |texts: &[String]| {
                let held =
                    armed.load(Ordering::SeqCst) && texts.iter().any(|text| text.contains(trigger));
                if held {
                    entered.fetch_add(1, Ordering::SeqCst);
                    let _ = wait.recv();
                }
                hash_embed(texts)
            })
        })
    }

    fn arm(&self) {
        self.armed.store(true, Ordering::SeqCst);
    }

    fn release_one(&self) {
        self.release.send(()).expect("release one blocked embed");
    }

    fn open(&self) {
        self.armed.store(false, Ordering::SeqCst);
        self.release_one();
    }

    async fn wait_entered(&self, count: usize) {
        for _ in 0..400 {
            if self.entered.load(Ordering::SeqCst) >= count {
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("embed gate never reached {count} blocked batches");
    }
}

async fn settle() {
    tokio::time::sleep(Duration::from_millis(150)).await;
}

async fn start(env: &Env, settings: Settings, pool: EmbedPool) -> (Client, Daemon) {
    copy_dir(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
        &env.root().join("src"),
    );
    let daemon = tokio::spawn(run(settings.clone(), Some(pool)));
    let client = Client::new(settings);
    wait_until_listening(&client).await;
    let indexed = client.send(index(&env.root())).await;
    assert!(
        matches!(indexed, Ok(Response::Indexed { .. })),
        "index failed: {indexed:?}"
    );
    (client, daemon)
}

async fn stop(client: &Client, daemon: Daemon) {
    drop(client.send(Request::Stop).await);
    assert_eq!(
        daemon.await.expect("daemon task").expect("daemon result"),
        RunOutcome::Stopped
    );
}

async fn status_of(client: &Client) -> DaemonStatus {
    match client.send(Request::Status).await {
        Ok(Response::Status { status }) => status,
        other => panic!("status failed: {other:?}"),
    }
}

fn write_marked_file(env: &Env) {
    let body = format!(
        "pub fn marked() -> &'static str {{\n    let token = \"{INDEX_TRIGGER}\";\n    token\n}}\n"
    );
    std::fs::write(env.root().join("src/marked.rs"), body).expect("write marked file");
}

#[proxima::test(runtime = "tokio")]
async fn flood_beyond_queue_depth_gets_busy_for_exactly_the_excess_and_hits_for_the_admitted() {
    let env = Env::new();
    let gate = Gate::new();
    let settings = Settings {
        query_cap: 1,
        query_depth: 2,
        embed_workers: 1,
        ..env.settings()
    };
    let (client, daemon) = start(&env, settings, gate.pool(1, QUERY_TRIGGER)).await;
    gate.arm();
    let (results_tx, mut results) = tokio::sync::mpsc::unbounded_channel();
    for flooded in 0..20 {
        let (client, results_tx, scope) = (client.clone(), results_tx.clone(), env.root());
        tokio::spawn(async move { drop(results_tx.send(client.send(search(&scope, 5)).await)) });
        if flooded == 0 {
            gate.wait_entered(1).await;
        }
    }

    let mut busy = 0;
    while busy < 17 {
        let reply = tokio::time::timeout(Duration::from_secs(20), results.recv()).await;
        match reply
            .expect("17 busy replies arrive without waiting on the gate")
            .expect("channel open")
        {
            Ok(Response::Error {
                error: ErrorKind::Busy,
                ..
            }) => busy += 1,
            other => panic!("only busy replies may arrive while the gate is shut, got {other:?}"),
        }
    }
    for _ in 0..3 {
        gate.release_one();
    }
    let mut admitted_hits = Vec::new();
    for _ in 0..3 {
        match results.recv().await.expect("admitted reply") {
            Ok(Response::Hits { hits }) => admitted_hits.push(hits.len()),
            other => panic!("admitted request must return hits, got {other:?}"),
        }
    }

    assert_eq!(admitted_hits.len(), 3, "1 running + depth 2 admitted");
    assert!(
        admitted_hits.iter().all(|count| *count > 0),
        "hits={admitted_hits:?}"
    );
    assert_eq!(status_of(&client).await.queries_busy, 17);
    stop(&client, daemon).await;
}

#[proxima::test(runtime = "tokio")]
async fn admitted_queries_complete_in_arrival_order() {
    let env = Env::new();
    let gate = Gate::new();
    let settings = Settings {
        query_cap: 1,
        query_depth: 4,
        embed_workers: 1,
        ..env.settings()
    };
    let (client, daemon) = start(&env, settings, gate.pool(1, QUERY_TRIGGER)).await;
    gate.arm();
    let (done_tx, mut done) = tokio::sync::mpsc::unbounded_channel();
    for id in 1..=5_u32 {
        let (client, done_tx, scope) = (client.clone(), done_tx.clone(), env.root());
        tokio::spawn(async move {
            let reply = client.send(search(&scope, id)).await;
            drop(done_tx.send((id, reply)));
        });
        if id == 1 {
            gate.wait_entered(1).await;
        }
        settle().await;
    }

    let mut completed = Vec::new();
    for _ in 0..5 {
        gate.release_one();
        let (id, reply) = tokio::time::timeout(Duration::from_secs(20), done.recv())
            .await
            .expect("completion")
            .expect("channel open");
        let Ok(Response::Hits { hits }) = reply else {
            panic!("query {id} must be admitted, got {reply:?}")
        };
        assert_eq!(hits.len(), id as usize, "limit {id} returns {id} hits");
        completed.push(id);
    }

    assert_eq!(completed, vec![1, 2, 3, 4, 5]);
    stop(&client, daemon).await;
}

#[proxima::test(runtime = "tokio")]
async fn a_query_during_a_reindex_returns_while_the_reindex_is_still_running() {
    let env = Env::new();
    let gate = Gate::new();
    let settings = Settings {
        embed_workers: 2,
        ..env.settings()
    };
    let (client, daemon) = start(&env, settings, gate.pool(2, INDEX_TRIGGER)).await;
    write_marked_file(&env);
    gate.arm();
    let reindex = {
        let (client, scope) = (client.clone(), env.root());
        tokio::spawn(async move { client.send(index(&scope)).await })
    };
    gate.wait_entered(1).await;

    let reply = tokio::time::timeout(Duration::from_secs(20), client.send(search(&env.root(), 5)))
        .await
        .expect("query returns while the reindex is blocked");
    let still_running = !reindex.is_finished();

    let Ok(Response::Hits { hits }) = reply else {
        panic!("query must return hits during a reindex, got {reply:?}")
    };
    assert!(!hits.is_empty(), "hits during reindex");
    assert!(
        still_running,
        "reindex was still running when the query returned"
    );
    gate.open();
    let finished = reindex.await.expect("reindex task");
    assert!(
        matches!(finished, Ok(Response::Indexed { .. })),
        "{finished:?}"
    );
    stop(&client, daemon).await;
}

#[proxima::test(runtime = "tokio")]
async fn a_full_index_queue_under_drop_oldest_evicts_the_oldest_waiting_runs() {
    let env = Env::new();
    let gate = Gate::new();
    let settings = Settings {
        index_cap: 1,
        index_depth: 2,
        index_overflow: "drop_oldest".to_owned(),
        embed_workers: 1,
        ..env.settings()
    };
    let (client, daemon) = start(&env, settings, gate.pool(1, INDEX_TRIGGER)).await;
    write_marked_file(&env);
    gate.arm();
    let labels = ["running", "evicted-a", "evicted-b", "kept-a", "kept-b"];
    let mut runs = Vec::new();
    for label in labels {
        let (client, scope) = (client.clone(), env.root());
        runs.push(tokio::spawn(
            async move { client.send(index(&scope)).await },
        ));
        if label == "running" {
            gate.wait_entered(1).await;
        }
        settle().await;
    }
    gate.open();

    let mut busy = Vec::new();
    for (label, run) in labels.into_iter().zip(runs) {
        match run.await.expect("index task") {
            Ok(Response::Indexed { .. }) => {}
            Ok(Response::Error {
                error: ErrorKind::Busy,
                ..
            }) => busy.push(label),
            other => panic!("{label}: expected indexed or busy, got {other:?}"),
        }
    }

    assert_eq!(busy, vec!["evicted-a", "evicted-b"]);
    assert_eq!(status_of(&client).await.index_busy, 2);
    stop(&client, daemon).await;
}

#[proxima::test(runtime = "tokio")]
async fn status_reports_warm_for_a_warmed_pool_and_not_for_a_lazy_one() {
    let env = Env::new();
    let warm = EmbedPool::with_lanes_warm(1, 1, || Ok(hash_embed));
    let (client, daemon) = start(&env, env.settings(), warm).await;
    let warm_status = status_of(&client).await;
    stop(&client, daemon).await;
    let lazy_env = Env::new();
    let lazy = EmbedPool::spawn(1, || Ok(hash_embed));
    let (lazy_client, lazy_daemon) = start(&lazy_env, lazy_env.settings(), lazy).await;
    let lazy_status = status_of(&lazy_client).await;
    stop(&lazy_client, lazy_daemon).await;

    assert!(warm_status.warm, "{warm_status:?}");
    assert!(!lazy_status.warm, "{lazy_status:?}");
}

#[proxima::test(runtime = "tokio")]
async fn a_model_load_slower_than_the_warm_bound_serves_degraded_instead_of_hanging() {
    let env = Env::new();
    let settings = Settings {
        spawn_timeout_ms: 400,
        ..env.settings()
    };
    let slow = EmbedPool::with_lanes_warm(1, 1, || {
        std::thread::sleep(Duration::from_secs(3));
        Ok(hash_embed)
    });
    let started = std::time::Instant::now();

    let daemon = tokio::spawn(run(settings.clone(), Some(slow)));
    let client = Client::new(settings);
    wait_until_listening(&client).await;
    let listening_after = started.elapsed();
    let status = status_of(&client).await;
    stop(&client, daemon).await;

    assert!(
        listening_after < Duration::from_secs(2),
        "{listening_after:?}"
    );
    assert!(!status.warm, "{status:?}");
}

#[proxima::test(runtime = "tokio")]
async fn the_first_query_after_a_restart_is_ready_with_segments_loaded_and_never_scans_sqlite() {
    let env = Env::new();
    let pool = EmbedPool::with_lanes_warm(1, 1, || Ok(hash_embed));
    let (client, daemon) = start(&env, env.settings(), pool).await;
    stop(&client, daemon).await;

    let restarted = EmbedPool::with_lanes_warm(1, 1, || Ok(hash_embed));
    let daemon = tokio::spawn(run(env.settings(), Some(restarted)));
    let client = Client::new(env.settings());
    wait_until_listening(&client).await;
    let scans_before = status_of(&client).await.vector_cache.scans;
    let answer = client.send(search(&env.root(), 5)).await;
    let status = status_of(&client).await;
    stop(&client, daemon).await;

    assert!(status.segments_ready, "{status:?}");
    assert!(status.vector_cache.segments > 0, "{status:?}");
    assert!(
        matches!(answer, Ok(Response::Hits { ref hits, .. }) if !hits.is_empty()),
        "{answer:?}"
    );
    assert_eq!(status.vector_cache.scans, scans_before, "{status:?}");
}

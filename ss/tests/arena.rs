// tests assert with expect messages
#![allow(clippy::expect_used, clippy::unwrap_used)]

mod support;

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use ss::client::Client;
use ss::daemon::{RunOutcome, run};
use ss::embed::{EmbedPool, hash_embed};
use ss::settings::Settings;
use ss_proto::{ErrorKind, Request, Response};
use support::{Env, copy_dir, wait_until_listening};

fn search(path: &Path) -> Request {
    Request::Search {
        query: "embed pool worker".to_owned(),
        path: path.to_string_lossy().into_owned(),
        limit: 5,
    }
}

fn gated_pool(armed: &Arc<AtomicBool>, gate: &flume::Receiver<()>) -> EmbedPool {
    let (armed, gate) = (Arc::clone(armed), gate.clone());
    EmbedPool::spawn(1, move || {
        let (armed, gate) = (Arc::clone(&armed), gate.clone());
        Ok(move |texts: &[String]| {
            if armed.load(Ordering::SeqCst) {
                let _ = gate.recv();
            }
            hash_embed(texts)
        })
    })
}

#[proxima::test(runtime = "tokio")]
async fn flood_beyond_queue_depth_gets_busy_for_the_excess_and_hits_for_the_admitted() {
    let env = Env::new();
    let settings = Settings {
        query_cap: 1,
        query_depth: 2,
        embed_workers: 1,
        ..env.settings()
    };
    let (armed, (gate_tx, gate_rx)) = (Arc::new(AtomicBool::new(false)), flume::unbounded::<()>());
    copy_dir(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
        &env.root().join("src"),
    );
    let daemon = tokio::spawn(run(settings.clone(), Some(gated_pool(&armed, &gate_rx))));
    let client = Client::new(settings);
    wait_until_listening(&client).await;
    let indexed = client
        .send(Request::Index {
            path: env.root().to_string_lossy().into_owned(),
        })
        .await;
    assert!(
        matches!(indexed, Ok(Response::Indexed { .. })),
        "index failed: {indexed:?}"
    );
    armed.store(true, Ordering::SeqCst);

    let (results_tx, mut results) = tokio::sync::mpsc::unbounded_channel();
    for _ in 0..20 {
        let (client, results_tx, scope) = (client.clone(), results_tx.clone(), env.root());
        tokio::spawn(async move { drop(results_tx.send(client.send(search(&scope)).await)) });
    }
    let mut busy = 0;
    while busy < 17 {
        let reply = tokio::time::timeout(std::time::Duration::from_secs(20), results.recv()).await;
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
        gate_tx.send(()).expect("release one embed");
    }
    let mut admitted_hits = Vec::new();
    for _ in 0..3 {
        match results.recv().await.expect("admitted reply") {
            Ok(Response::Hits { hits }) => admitted_hits.push(hits.len()),
            other => panic!("admitted request must return hits, got {other:?}"),
        }
    }

    println!(
        "flood: 20 sent, busy={busy}, admitted={}, hits per admitted={admitted_hits:?}",
        admitted_hits.len()
    );
    assert!(
        admitted_hits.iter().all(|count| *count > 0),
        "hits={admitted_hits:?}"
    );
    let status = client.send(Request::Status).await;
    let Ok(Response::Status { status }) = status else {
        panic!("status failed: {status:?}")
    };
    assert_eq!(status.queries_busy, 17);
    drop(client.send(Request::Stop).await);
    assert_eq!(
        daemon.await.expect("daemon task").expect("daemon result"),
        RunOutcome::Stopped
    );
}

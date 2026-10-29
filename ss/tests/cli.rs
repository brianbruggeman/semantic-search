// tests assert with expect messages
#![allow(clippy::expect_used, clippy::unwrap_used)]

mod support;

use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use support::{Env, percentile, tagged_copies, text};

fn index_root(env: &Env) -> std::process::Output {
    env.run("ss", &["index", env.root().to_str().expect("utf8")])
}

#[proxima::test(runtime = "tokio")]
async fn client_with_no_daemon_spawns_one_and_gets_a_result() {
    let env = Env::new();
    tagged_copies(&env.root(), 1, "first");
    assert!(
        !env.run("ssd", &["status"]).status.success(),
        "no daemon may be running yet"
    );

    let started = Instant::now();
    let indexed = index_root(&env);
    let spawn_and_index = started.elapsed();
    let searched = env.run(
        "ss",
        &[
            "--path",
            env.root().to_str().expect("utf8"),
            "walk",
            "files",
            "respecting",
            "gitignore",
        ],
    );

    println!(
        "spawn+index: {spawn_and_index:?}; index says: {}",
        text(&indexed).trim()
    );
    assert_eq!(
        indexed.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&indexed.stderr)
    );
    assert_eq!(
        searched.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&searched.stderr)
    );
    let output = text(&searched);
    let headers: Vec<&str> = output
        .lines()
        .filter(|line| line.contains(".rs:") && line.contains('-'))
        .collect();
    assert!(!headers.is_empty(), "no hit headers in: {output}");
    assert!(
        env.run("ssd", &["status"]).status.success(),
        "daemon should be running now"
    );
}

#[proxima::test(runtime = "tokio")]
async fn exit_codes_follow_rg_zero_for_hits_one_for_none_two_for_error() {
    let env = Env::new();
    tagged_copies(&env.root(), 1, "codes");
    std::fs::create_dir_all(env.root().join("empty")).expect("empty dir");
    assert_eq!(index_root(&env).status.code(), Some(0));
    let root = env.root();
    let root = root.to_str().expect("utf8");

    let hit = env.run("ss", &["--path", root, "embed"]).status.code();
    let none = env
        .run("ss", &["--path", &format!("{root}/empty"), "embed"])
        .status
        .code();
    let unindexed = tempfile::tempdir().expect("unindexed dir");
    let error = env.run(
        "ss",
        &["--path", unindexed.path().to_str().expect("utf8"), "embed"],
    );

    assert_eq!(
        (hit, none, error.status.code()),
        (Some(0), Some(1), Some(2))
    );
    assert!(
        String::from_utf8_lossy(&error.stderr).contains("not indexed"),
        "stderr: {}",
        String::from_utf8_lossy(&error.stderr)
    );
}

#[proxima::test(runtime = "tokio")]
async fn two_simultaneous_daemon_starts_leave_exactly_one_running() {
    let env = Env::new();
    let spawn = || {
        env.command("ssd")
            .arg("serve")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn ssd")
    };
    let mut daemons = vec![spawn(), spawn()];
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline
        && daemons
            .iter_mut()
            .all(|child| child.try_wait().expect("poll").is_none())
    {
        std::thread::sleep(Duration::from_millis(20));
    }

    let exits: Vec<Option<i32>> = daemons
        .iter_mut()
        .map(|child| {
            child
                .try_wait()
                .expect("poll")
                .map(|code| code.code().unwrap_or(-1))
        })
        .collect();
    let alive: Vec<u32> = daemons
        .iter_mut()
        .filter_map(|child| {
            child
                .try_wait()
                .expect("poll")
                .is_none()
                .then(|| child.id())
        })
        .collect();
    let status = text(&env.run("ssd", &["status"]));

    println!(
        "exit codes {exits:?}, alive pids {alive:?}, status: {}",
        status.lines().next().unwrap_or("")
    );
    assert_eq!(alive.len(), 1, "exits={exits:?}");
    assert_eq!(
        exits.iter().flatten().copied().collect::<Vec<_>>(),
        vec![0],
        "the loser must exit cleanly"
    );
    assert!(
        status.contains(&format!("pid={}", alive[0])),
        "status={status}"
    );
    env.stop_daemon();
    for child in &mut daemons {
        let _ = child.wait();
    }
}

#[proxima::test(runtime = "tokio")]
async fn thirty_two_concurrent_clients_all_get_hits_while_a_reindex_runs() {
    let env = Env::new();
    tagged_copies(&env.root(), 20, "v1-");
    let first = Instant::now();
    assert_eq!(index_root(&env).status.code(), Some(0));
    let first_index = first.elapsed();
    let baseline = env.run_timed(&[
        "--path",
        env.root().to_str().expect("utf8"),
        "-n",
        "3",
        "embed",
        "pool",
        "worker",
        "threads",
    ]);
    tagged_copies(&env.root(), 20, "v2-");
    let reindex = env
        .command("ss")
        .args(["index", env.root().to_str().expect("utf8")])
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn reindex");
    let reindex_started = Instant::now();
    tokio::time::sleep(Duration::from_millis(300)).await;

    let env = Arc::new(env);
    let mut clients = Vec::new();
    for number in 0..32 {
        let env = Arc::clone(&env);
        clients.push(tokio::task::spawn_blocking(move || {
            let begun = Instant::now();
            let output = env.run(
                "ss",
                &[
                    "--path",
                    env.root().to_str().expect("utf8"),
                    "-n",
                    "3",
                    "embed",
                    "pool",
                    "worker",
                    "threads",
                ],
            );
            (number, begun.elapsed(), Instant::now(), output)
        }));
    }
    let mut latencies = Vec::new();
    let mut finished_at = Vec::new();
    for client in clients {
        let (number, latency, done, output) = client.await.expect("client task");
        assert_eq!(
            output.status.code(),
            Some(0),
            "client {number} stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let hit_lines = text(&output)
            .lines()
            .filter(|line| line.contains(".rs:"))
            .count();
        assert!(hit_lines > 0, "client {number} got no hits");
        latencies.push(latency);
        finished_at.push(done);
    }
    let reindex_output =
        tokio::task::spawn_blocking(move || reindex.wait_with_output().expect("reindex wait"))
            .await
            .expect("reindex task");
    let reindex_done = Instant::now();

    latencies.sort();
    let overlapped = finished_at
        .iter()
        .filter(|done| **done < reindex_done)
        .count();
    println!(
        "idle-daemon single query {baseline:?}; first index {first_index:?}; reindex {:?} ({}); 32 clients: p50={:?} p95={:?} max={:?}; {overlapped}/32 finished before the reindex did",
        reindex_done - reindex_started,
        text(&reindex_output).trim(),
        percentile(&latencies, 50),
        percentile(&latencies, 95),
        latencies[31]
    );
    assert_eq!(reindex_output.status.code(), Some(0));
    assert!(
        overlapped > 0,
        "no client overlapped the re-index; corpus too small to prove it"
    );
}

#[proxima::test(runtime = "tokio")]
async fn daemon_exits_after_idle_period_when_no_watches_and_no_clients() {
    let mut env = Env::new();
    env.idle_secs = "1".to_owned();
    tagged_copies(&env.root(), 1, "idle");
    assert_eq!(index_root(&env).status.code(), Some(0));
    assert!(
        env.run("ssd", &["status"]).status.success(),
        "daemon should be up right after the index"
    );

    let begun = Instant::now();
    while env.run("ssd", &["status"]).status.success() && begun.elapsed() < Duration::from_secs(30)
    {
        std::thread::sleep(Duration::from_millis(200));
    }

    let exited_after = begun.elapsed();
    println!("idle exit observed {exited_after:?} after the last request (SS_IDLE_SECS=1)");
    assert!(
        exited_after < Duration::from_secs(30),
        "daemon never idled out"
    );
    assert!(!env.run("ssd", &["status"]).status.success());
}

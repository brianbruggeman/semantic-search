// tests assert with expect messages
#![allow(clippy::expect_used, clippy::unwrap_used)]

mod support;

use std::io::Read;
use std::process::Stdio;
use std::time::{Duration, Instant};

use ss::client::Client;
use support::{Env, tagged_copies, text, wait_until_listening};

#[proxima::test(runtime = "tokio")]
async fn daemon_writes_spans_and_chunk_counters_to_the_file_sink_and_the_console() {
    let env = Env::new();
    tagged_copies(&env.root(), 1, "telemetry");
    let mut daemon = env
        .command("ssd")
        .arg("serve")
        .env("RUST_LOG", "warn,ss=debug")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn ssd");
    wait_until_listening(&Client::new(env.settings())).await;
    let indexed = env.run("ss", &["index", env.root().to_str().expect("utf8")]);
    assert_eq!(indexed.status.code(), Some(0));
    env.stop_daemon();
    let begun = Instant::now();
    while daemon.try_wait().expect("poll").is_none() && begun.elapsed() < Duration::from_secs(20) {
        std::thread::sleep(Duration::from_millis(50));
    }
    let mut console = String::new();
    daemon
        .stdout
        .take()
        .expect("piped stdout")
        .read_to_string(&mut console)
        .expect("read console");
    let file =
        std::fs::read_to_string(env.home.path().join("logs/ssd.log")).expect("file sink written");

    let embedded: u64 = text(&indexed)
        .split_whitespace()
        .zip(text(&indexed).split_whitespace().skip(1))
        .find(|(_, word)| word.trim_end_matches(',') == "embedded")
        .and_then(|(count, _)| count.parse().ok())
        .expect("embedded count in index output");
    let span_lines = file
        .lines()
        .filter(|line| line.contains("\"kind\":\"span\""))
        .count();
    println!(
        "file sink: {} lines, {span_lines} span lines; console: {} bytes; index embedded {embedded}",
        file.lines().count(),
        console.len()
    );
    assert!(span_lines > 0, "no span records in the file sink");
    for stage in [
        "read_file",
        "chunk_flow",
        "store_file",
        "index_tree",
        "embed_checked",
    ] {
        assert!(
            file.contains(&format!("\"name\":\"{stage}\"")),
            "no {stage} span in file sink"
        );
    }
    assert!(embedded > 0);
    let run_line = file
        .lines()
        .find(|line| line.contains("index run complete"))
        .expect("structured index-run log in file sink");
    assert!(
        run_line.contains("\"chunks_embedded\":") && run_line.contains(&embedded.to_string()),
        "{run_line}"
    );
    let counter_lines: Vec<&str> = file
        .lines()
        .filter(|line| line.contains("\"kind\":\"counter\""))
        .collect();
    assert!(
        counter_lines.len() >= 3,
        "expected the three chunk counters, got {counter_lines:?}"
    );
    assert!(
        counter_lines
            .iter()
            .any(|line| line.contains(&format!("U64({embedded})"))),
        "{counter_lines:?}"
    );
    assert!(
        console.contains("index run complete"),
        "console sink missing the log: {console:.300}"
    );
}

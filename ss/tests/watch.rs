// tests assert with expect messages
#![allow(clippy::expect_used, clippy::unwrap_used)]

mod support;

use std::path::Path;
use std::time::{Duration, Instant};

use support::{Env, copy_dir, text};

const BOUND: Duration = Duration::from_secs(15);

fn seed(env: &Env) -> std::path::PathBuf {
    let root = env.root();
    copy_dir(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
        &root.join("src"),
    );
    root.canonicalize().expect("canonical root")
}

fn found_in(env: &Env, root: &Path, terms: &[&str], needle: &str) -> bool {
    let mut args = vec!["--path", root.to_str().expect("utf8")];
    args.extend_from_slice(terms);
    let output = env.run("ss", &args);
    let marker = format!("{needle}:");
    output.status.code() == Some(0)
        && text(&output)
            .lines()
            .any(|line| !line.starts_with(' ') && line.contains(&marker))
}

fn poll(bound: Duration, mut check: impl FnMut() -> bool) -> Option<Duration> {
    let begun = Instant::now();
    while begun.elapsed() < bound {
        if check() {
            return Some(begun.elapsed());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    None
}

fn watch_line(env: &Env) -> String {
    text(&env.run("ssd", &["status"]))
        .lines()
        .find(|line| line.starts_with("watches:"))
        .unwrap_or_default()
        .to_owned()
}

#[proxima::test(runtime = "tokio")]
async fn watch_add_then_a_new_file_becomes_searchable_within_a_bounded_wait() {
    let env = Env::new();
    let root = seed(&env);
    let added = env.run("ssd", &["watch", "add", root.to_str().expect("utf8")]);
    assert_eq!(
        added.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&added.stderr)
    );
    let initial = poll(BOUND, || {
        found_in(&env, &root, &["walk", "files"], "walk.rs")
    });
    assert!(
        initial.is_some(),
        "startup rescan never indexed the existing tree"
    );

    std::fs::write(
        root.join("src/zebra_quarantine.rs"),
        "/// quarantines flux capacitor readings\npub fn quarantine_zebra_flux(reading: u32) -> u32 {\n    let adjusted = reading * 7;\n    let clamped = adjusted.min(900);\n    clamped + 1\n}\n",
    )
    .expect("write new file");
    let wrote = Instant::now();
    let found = poll(BOUND, || {
        found_in(
            &env,
            &root,
            &["quarantine", "zebra", "flux"],
            "zebra_quarantine.rs",
        )
    });

    let elapsed = wrote.elapsed();
    println!(
        "watch: startup rescan searchable after {:?}; new file searchable {} ms after write (bound {} ms)",
        initial.expect("checked above"),
        elapsed.as_millis(),
        BOUND.as_millis()
    );
    assert!(found.is_some(), "new file not searchable within {BOUND:?}");
}

#[proxima::test(runtime = "tokio")]
async fn double_watch_add_and_a_nested_add_leave_exactly_one_watcher() {
    let env = Env::new();
    let root = seed(&env);
    let root_arg = root.to_str().expect("utf8");
    let nested = root.join("src");

    let first = env.run("ssd", &["watch", "add", root_arg]);
    let again = env.run("ssd", &["watch", "add", root_arg]);
    let covered = env.run("ssd", &["watch", "add", nested.to_str().expect("utf8")]);

    let line = watch_line(&env);
    println!(
        "after add, add again, add nested: `{line}`; list: {}",
        text(&covered).trim()
    );
    assert_eq!(
        (
            first.status.code(),
            again.status.code(),
            covered.status.code()
        ),
        (Some(0), Some(0), Some(0))
    );
    assert_eq!(line, "watches: 1 (1 active watchers)");
    assert_eq!(text(&covered).lines().count(), 1);
}

#[proxima::test(runtime = "tokio")]
async fn registrations_persist_and_a_restarted_daemon_rescans_changes_made_while_it_was_down() {
    let env = Env::new();
    let root = seed(&env);
    assert_eq!(
        env.run("ssd", &["watch", "add", root.to_str().expect("utf8")])
            .status
            .code(),
        Some(0)
    );
    assert!(
        poll(BOUND, || found_in(
            &env,
            &root,
            &["walk", "files"],
            "walk.rs"
        ))
        .is_some()
    );
    env.stop_daemon();
    assert!(
        poll(BOUND, || !env.run("ssd", &["status"]).status.success()).is_some(),
        "daemon did not stop"
    );
    std::fs::write(
        root.join("src/offline_marker.rs"),
        "pub fn written_while_the_daemon_was_down() -> &'static str {\n    let note = \"offline\";\n    let again = note;\n    again\n}\n",
    )
    .expect("write offline file");

    let listed = env.run("ssd", &["watch", "list"]);
    let found = poll(BOUND, || {
        found_in(
            &env,
            &root,
            &["written", "while", "daemon", "down"],
            "offline_marker.rs",
        )
    });

    assert_eq!(text(&listed).trim(), root.to_str().expect("utf8"));
    println!(
        "restart rescan: offline file searchable after {:?}",
        found.expect("rescan on start")
    );
}

#[proxima::test(runtime = "tokio")]
async fn deleting_a_watched_file_removes_it_from_results_and_rm_stops_watching() {
    let env = Env::new();
    let root = seed(&env);
    assert_eq!(
        env.run("ssd", &["watch", "add", root.to_str().expect("utf8")])
            .status
            .code(),
        Some(0)
    );
    assert!(
        poll(BOUND, || found_in(
            &env,
            &root,
            &["walk", "files"],
            "walk.rs"
        ))
        .is_some()
    );

    std::fs::remove_file(root.join("src/walk.rs")).expect("delete");
    let gone = poll(BOUND, || {
        !found_in(&env, &root, &["walk", "files", "gitignore"], "walk.rs")
    });
    let removed = env.run("ssd", &["watch", "rm", root.to_str().expect("utf8")]);
    std::fs::write(root.join("src/after_rm.rs"), "pub fn indexed_after_rm() {\n    let a = 1;\n    let b = 2;\n    let c = a + b;\n    drop(c);\n}\n").expect("write");
    std::thread::sleep(Duration::from_millis(1500));
    let unknown = env.run("ssd", &["watch", "rm", root.to_str().expect("utf8")]);

    assert!(
        gone.is_some(),
        "deleted file still searchable after {BOUND:?}"
    );
    assert_eq!(removed.status.code(), Some(0));
    assert!(
        !found_in(&env, &root, &["indexed", "after", "rm"], "after_rm.rs"),
        "file written after rm must not be indexed"
    );
    assert_eq!(
        unknown.status.code(),
        Some(2),
        "removing an unwatched path is an error"
    );
    assert_eq!(watch_line(&env), "watches: 0 (0 active watchers)");
}

#[proxima::test(runtime = "tokio")]
async fn daemon_stays_up_past_the_idle_period_while_a_watch_exists_and_exits_after_it_is_removed() {
    let mut env = Env::new();
    env.idle_secs = "1".to_owned();
    let root = seed(&env);
    assert_eq!(
        env.run("ssd", &["watch", "add", root.to_str().expect("utf8")])
            .status
            .code(),
        Some(0)
    );

    std::thread::sleep(Duration::from_secs(3));
    let still_up = env.run("ssd", &["status"]).status.success();
    assert_eq!(
        env.run("ssd", &["watch", "rm", root.to_str().expect("utf8")])
            .status
            .code(),
        Some(0)
    );
    let exited = poll(Duration::from_secs(20), || {
        !env.run("ssd", &["status"]).status.success()
    });

    assert!(still_up, "daemon idled out despite a registered watch");
    println!(
        "after watch rm, daemon exited {:?} later",
        exited.expect("idle exit after rm")
    );
}

fn write_zebra(root: &Path) -> Instant {
    std::fs::write(
        root.join("src/zebra_quarantine.rs"),
        "/// quarantines flux capacitor readings\npub fn quarantine_zebra_flux(reading: u32) -> u32 {\n    let adjusted = reading * 7;\n    let clamped = adjusted.min(900);\n    clamped + 1\n}\n",
    )
    .expect("write new file");
    Instant::now()
}

#[proxima::test(runtime = "tokio")]
#[ignore = "depends on fseventsd latency, which was observed from 3s to over 60s on a loaded host; run with --run-ignored all"]
async fn native_os_events_deliver_a_new_file_when_the_safety_rescan_is_disabled() {
    let mut env = Env::new();
    env.watch_backend = "native".to_owned();
    env.rescan_secs = "0".to_owned();
    let root = seed(&env);
    assert_eq!(
        env.run("ssd", &["watch", "add", root.to_str().expect("utf8")])
            .status
            .code(),
        Some(0)
    );
    assert!(
        poll(Duration::from_secs(60), || found_in(
            &env,
            &root,
            &["walk", "files"],
            "walk.rs"
        ))
        .is_some()
    );

    let wrote = write_zebra(&root);
    let found = poll(Duration::from_secs(60), || {
        found_in(
            &env,
            &root,
            &["quarantine", "zebra", "flux"],
            "zebra_quarantine.rs",
        )
    });

    println!(
        "native events: new file searchable {:?} after write",
        wrote.elapsed()
    );
    assert!(
        found.is_some(),
        "no native event reached the index within 60s"
    );
}

#[proxima::test(runtime = "tokio")]
async fn safety_rescan_finds_a_new_file_even_when_the_watcher_reports_nothing() {
    let mut env = Env::new();
    env.poll_ms = "3600000".to_owned();
    env.rescan_secs = "2".to_owned();
    let root = seed(&env);
    assert_eq!(
        env.run("ssd", &["watch", "add", root.to_str().expect("utf8")])
            .status
            .code(),
        Some(0)
    );
    assert!(
        poll(BOUND, || found_in(
            &env,
            &root,
            &["walk", "files"],
            "walk.rs"
        ))
        .is_some()
    );

    let wrote = write_zebra(&root);
    let found = poll(BOUND, || {
        found_in(
            &env,
            &root,
            &["quarantine", "zebra", "flux"],
            "zebra_quarantine.rs",
        )
    });

    println!(
        "safety rescan: new file searchable {:?} after write (poll watcher interval 1h, rescan 2s)",
        wrote.elapsed()
    );
    assert!(found.is_some(), "safety rescan never indexed the new file");
}

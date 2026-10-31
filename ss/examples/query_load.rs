//! Per-stage client timing against a real daemon: exec cost, connect+status, idle query, N concurrent queries.
//!
//! `cargo run --release --example query_load -- <embedder> <clients> <copies>`
//! Builds a throwaway home and tree, indexes it, then prints wall-clock milliseconds per stage. The daemon's
//! span records stay in `<home>/logs/ssd.log`; the home path is printed so they can be read.
// timing harnesses abort on any setup failure and divide counts into milliseconds for display
#![allow(clippy::expect_used, clippy::cast_precision_loss)]

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

const QUERY: [&str; 4] = ["embed", "pool", "worker", "threads"];
const IDLE_REPEATS: usize = 7;

struct Rig {
    ss: PathBuf,
    ssd: PathBuf,
    home: PathBuf,
    tree: PathBuf,
    embedder: String,
}

impl Rig {
    fn command(&self, binary: &Path) -> Command {
        let mut command = Command::new(binary);
        command
            .env("SS_HOME", &self.home)
            .env("SS_EMBEDDER", &self.embedder)
            .env("SS_DAEMON_BIN", &self.ssd)
            .env("SS_IDLE_SECS", "900")
            .env("RUST_LOG", "warn,ss=debug")
            .current_dir(&self.tree);
        command
    }

    fn timed(&self, binary: &Path, args: &[&str]) -> (Duration, i32, String) {
        let started = Instant::now();
        let output = self
            .command(binary)
            .args(args)
            .stdin(Stdio::null())
            .output()
            .expect("run binary");
        (
            started.elapsed(),
            output.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&output.stdout).into_owned(),
        )
    }
}

fn millis(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

fn arg<T: std::str::FromStr>(position: usize, default: T) -> T {
    std::env::args()
        .nth(position)
        .and_then(|text| text.parse().ok())
        .unwrap_or(default)
}

fn copy_tagged(from: &Path, to: &Path, tag: &str) {
    std::fs::create_dir_all(to).expect("create dir");
    for entry in std::fs::read_dir(from).expect("read dir").flatten() {
        let target = to.join(entry.file_name());
        if entry.path().is_dir() {
            copy_tagged(&entry.path(), &target, tag);
        } else if let Ok(text) = std::fs::read_to_string(entry.path()) {
            let tagged: String = text.lines().fold(String::new(), |mut acc, line| {
                let _ = writeln!(acc, "// {tag}\n{line}");
                acc
            });
            std::fs::write(target, tagged).expect("write tagged");
        }
    }
}

fn summary(label: &str, mut samples: Vec<f64>) {
    samples.sort_by(f64::total_cmp);
    let at = |percent: usize| samples[(samples.len() * percent / 100).min(samples.len() - 1)];
    println!(
        "{label}: n={} min={:.1} p50={:.1} p95={:.1} max={:.1}",
        samples.len(),
        samples[0],
        at(50),
        at(95),
        samples[samples.len() - 1]
    );
}

fn idle(rig: &Rig, label: &str, binary: &Path, args: &[&str]) {
    let samples = (0..IDLE_REPEATS)
        .map(|_| millis(rig.timed(binary, args).0))
        .collect();
    summary(label, samples);
}

fn concurrent(rig: &Arc<Rig>, label: &str, binary: &Path, args: &[&str], clients: usize) {
    let args: Arc<Vec<String>> = Arc::new(args.iter().map(|text| (*text).to_owned()).collect());
    let started = Instant::now();
    let handles: Vec<_> = (0..clients)
        .map(|_| {
            let (rig, binary, args) = (Arc::clone(rig), binary.to_path_buf(), Arc::clone(&args));
            std::thread::spawn(move || {
                let refs: Vec<&str> = args.iter().map(String::as_str).collect();
                rig.timed(&binary, &refs)
            })
        })
        .collect();
    let results: Vec<_> = handles
        .into_iter()
        .map(|handle| handle.join().expect("client"))
        .collect();
    let failed = results.iter().filter(|result| result.1 != 0).count();
    summary(
        label,
        results.iter().map(|result| millis(result.0)).collect(),
    );
    println!(
        "{label}: wall_ms={:.1} nonzero_exit={failed}",
        millis(started.elapsed())
    );
}

fn during_reindex(rig: &Arc<Rig>, root: &str, search_args: &[&str], clients: usize) {
    copy_tagged(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
        &rig.tree.join("copy_reindex"),
        "reindex",
    );
    let started = Instant::now();
    let background = {
        let (rig, root) = (Arc::clone(rig), root.to_owned());
        std::thread::spawn(move || rig.timed(&rig.ss, &["index", &root]))
    };
    std::thread::sleep(Duration::from_millis(300));
    concurrent(
        rig,
        "concurrent_query_during_reindex",
        &rig.ss,
        search_args,
        clients,
    );
    let (_, code, text) = background.join().expect("reindex");
    println!(
        "reindex_total_ms={:.1} exit={code} {}",
        millis(started.elapsed()),
        text.trim()
    );
}

fn main() {
    let embedder: String = arg(1, "fastembed".to_owned());
    let clients: usize = arg(2, 32);
    let copies: usize = arg(3, 1);
    let bin_dir = std::env::current_exe()
        .expect("exe")
        .parent()
        .and_then(Path::parent)
        .expect("target dir")
        .to_path_buf();
    let scratch = std::env::var_os("TMPDIR").map_or_else(std::env::temp_dir, PathBuf::from);
    let base = scratch.join(format!("ss-query-load-{}", std::process::id()));
    let rig = Arc::new(Rig {
        ss: bin_dir.join("ss"),
        ssd: bin_dir.join("ssd"),
        home: base.join("home"),
        tree: base.join("tree"),
        embedder,
    });
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    for copy in 0..copies {
        copy_tagged(
            &source,
            &rig.tree.join(format!("copy_{copy}")),
            &format!("c{copy}"),
        );
    }
    let root = rig.tree.to_string_lossy().into_owned();
    println!("home={} embedder={}", rig.home.display(), rig.embedder);

    idle(&rig, "exec_only(ss --version)", &rig.ss, &["--version"]);
    let (cold, code, _) = rig.timed(&rig.ssd, &["status"]);
    println!("status_no_daemon: ms={:.1} exit={code}", millis(cold));
    let (spawn_index, code, text) = rig.timed(&rig.ss, &["index", &root]);
    println!(
        "cold_spawn_and_index: ms={:.1} exit={code} {}",
        millis(spawn_index),
        text.trim()
    );
    let (reindex, _, text) = rig.timed(&rig.ss, &["index", &root]);
    println!(
        "reindex_unchanged: ms={:.1} {}",
        millis(reindex),
        text.trim()
    );

    let mut search_args = vec!["--path", root.as_str(), "-n", "3"];
    search_args.extend(QUERY);
    idle(&rig, "daemon_status(ssd status)", &rig.ssd, &["status"]);
    idle(&rig, "idle_query(ss ...)", &rig.ss, &search_args);
    concurrent(&rig, "concurrent_status", &rig.ssd, &["status"], clients);
    concurrent(&rig, "concurrent_query", &rig.ss, &search_args, clients);
    concurrent(
        &rig,
        "concurrent_query_again",
        &rig.ss,
        &search_args,
        clients,
    );
    during_reindex(&rig, &root, &search_args, clients);
    let (_, _, status) = rig.timed(&rig.ssd, &["status"]);
    println!("{}", status.trim());
    let _ = rig.timed(&rig.ssd, &["stop"]);
    println!("log={}", rig.home.join("logs/ssd.log").display());
}

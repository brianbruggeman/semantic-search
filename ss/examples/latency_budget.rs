//! Latency-budget harness against real `ss`/`ssd` release binaries; budgets B1 to B5 are registered in `ai_docs/design.md`.
//!
//! `cargo run --release --example latency_budget -- <embedder> <copies>`
//! Wall time and CPU time (getrusage user+sys of waited-for children) are reported separately. Exit 0 only if every
//! budget holds; exit 1 lists each failed budget with its number.
// timing harness aborts on setup failure and divides counts into milliseconds for display
#![allow(clippy::expect_used, clippy::cast_precision_loss)]

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

const QUERY: [&str; 4] = ["embed", "pool", "worker", "threads"];
const IDLE_CALLS: usize = 200;
const CLIENTS: usize = 32;
const NEW_FILES: usize = 220;
const READY_TIMEOUT: Duration = Duration::from_secs(60);

struct Rig {
    ss: PathBuf,
    ssd: PathBuf,
    home: PathBuf,
    tree: PathBuf,
    embedder: String,
}

struct Call {
    wall_ms: f64,
    cpu_ms: f64,
    code: i32,
    stdout: String,
}

struct Row {
    id: &'static str,
    metric: String,
    kind: &'static str,
    value: f64,
    limit: Option<f64>,
}

impl Row {
    fn passed(&self) -> bool {
        self.limit.is_none_or(|limit| self.value <= limit)
    }
}

#[cfg(unix)]
fn children_cpu() -> Duration {
    // SAFETY: getrusage fills the zeroed struct it is handed
    let usage = unsafe {
        let mut usage: libc::rusage = std::mem::zeroed();
        libc::getrusage(libc::RUSAGE_CHILDREN, &raw mut usage);
        usage
    };
    let micros = |value: libc::timeval| {
        u64::try_from(value.tv_sec).unwrap_or(0) * 1_000_000
            + u64::try_from(value.tv_usec).unwrap_or(0)
    };
    Duration::from_micros(micros(usage.ru_utime) + micros(usage.ru_stime))
}

#[cfg(not(unix))]
fn children_cpu() -> Duration {
    Duration::ZERO
}

fn millis(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

impl Rig {
    fn command(&self, binary: &Path) -> Command {
        let mut command = Command::new(binary);
        command
            .env("SS_HOME", &self.home)
            .env("SS_EMBEDDER", &self.embedder)
            .env("SS_DAEMON_BIN", &self.ssd)
            .env("SS_IDLE_SECS", "900")
            .env("RUST_LOG", "warn")
            .current_dir(&self.tree)
            .stdin(Stdio::null());
        command
    }

    fn run(&self, binary: &Path, args: &[&str]) -> Call {
        let cpu_before = children_cpu();
        let started = Instant::now();
        let output = self
            .command(binary)
            .args(args)
            .output()
            .expect("run binary");
        Call {
            wall_ms: millis(started.elapsed()),
            cpu_ms: millis(children_cpu().saturating_sub(cpu_before)),
            code: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        }
    }

    fn query(&self) -> Call {
        let root = self.tree.to_string_lossy().into_owned();
        let mut args = vec!["--path", root.as_str(), "-n", "3"];
        args.extend(QUERY);
        self.run(&self.ss, &args)
    }

    fn index(&self) -> Call {
        let root = self.tree.to_string_lossy().into_owned();
        self.run(&self.ss, &["index", &root])
    }

    fn running(&self) -> bool {
        self.run(&self.ssd, &["status"]).code == 0
    }

    fn wait_until(&self, want_running: bool) {
        let deadline = Instant::now() + READY_TIMEOUT;
        while self.running() != want_running {
            assert!(Instant::now() < deadline, "daemon state timeout");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn stop(&self) {
        let _ = self.run(&self.ssd, &["stop"]);
        self.wait_until(false);
    }

    // the daemon is deliberately left running as a detached session leader; `stop` ends it
    #[allow(clippy::zombie_processes)]
    fn start(&self) {
        self.command(&self.ssd)
            .args(["serve", "--setsid"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn ssd");
        self.wait_until(true);
    }
}

fn copy_tagged(from: &Path, to: &Path, tag: &str) {
    std::fs::create_dir_all(to).expect("create dir");
    for entry in std::fs::read_dir(from).expect("read dir").flatten() {
        let target = to.join(entry.file_name());
        if entry.path().is_dir() {
            copy_tagged(&entry.path(), &target, tag);
        } else if let Ok(text) = std::fs::read_to_string(entry.path()) {
            let tagged = text.lines().fold(String::new(), |mut acc, line| {
                let _ = writeln!(acc, "// {tag}\n{line}");
                acc
            });
            std::fs::write(target, tagged).expect("write tagged");
        }
    }
}

fn write_new_files(rig: &Rig, count: usize) {
    let dir = rig.tree.join("new_files");
    std::fs::create_dir_all(&dir).expect("create dir");
    for index in 0..count {
        let body = (0..30).fold(String::new(), |mut acc, line| {
            let _ = writeln!(
                acc,
                "pub fn latency_new_{index}_{line}(value: u64) -> u64 {{ value + {line} }}"
            );
            acc
        });
        std::fs::write(dir.join(format!("new_{index}.rs")), body).expect("write new file");
    }
}

fn percentile(samples: &[f64], percent: usize) -> f64 {
    let mut sorted = samples.to_vec();
    sorted.sort_by(f64::total_cmp);
    sorted[(sorted.len() * percent / 100).min(sorted.len() - 1)]
}

fn row(id: &'static str, metric: &str, kind: &'static str, value: f64, limit: Option<f64>) -> Row {
    Row {
        id,
        metric: metric.to_owned(),
        kind,
        value,
        limit,
    }
}

fn hit_row(id: &'static str, calls: &[Call]) -> Row {
    let empty = calls
        .iter()
        .filter(|call| call.code != 0 || call.stdout.trim().is_empty())
        .count();
    row(
        id,
        "clients with no hits or nonzero exit",
        "count",
        empty as f64,
        Some(0.0),
    )
}

fn b1_idle(rig: &Rig) -> Vec<Row> {
    let calls: Vec<Call> = (0..IDLE_CALLS).map(|_| rig.query()).collect();
    let wall: Vec<f64> = calls.iter().map(|call| call.wall_ms).collect();
    let cpu: Vec<f64> = calls.iter().map(|call| call.cpu_ms).collect();
    vec![
        row(
            "B1",
            "idle query p50",
            "wall ms",
            percentile(&wall, 50),
            Some(100.0),
        ),
        row(
            "B1",
            "idle query p99",
            "wall ms",
            percentile(&wall, 99),
            Some(500.0),
        ),
        row(
            "B1",
            "idle query p99 (client process only)",
            "cpu ms",
            percentile(&cpu, 99),
            Some(20.0),
        ),
        hit_row("B1", &calls),
    ]
}

fn b2_after_ready(rig: &Rig) -> Vec<Row> {
    rig.stop();
    rig.start();
    let call = rig.query();
    vec![
        row(
            "B2",
            "first query after ready",
            "wall ms",
            call.wall_ms,
            Some(300.0),
        ),
        row("B2", "first query after ready", "cpu ms", call.cpu_ms, None),
    ]
}

fn b3_after_write(rig: &Rig) -> Vec<Row> {
    let victim = rig.tree.join("copy_0").join("lib.rs");
    let text = std::fs::read_to_string(&victim).expect("read victim");
    std::fs::write(&victim, format!("{text}\n// latency_budget touch\n")).expect("touch");
    let indexed = rig.index();
    let call = rig.query();
    vec![
        row(
            "B3",
            "index run after touch (not gated)",
            "wall ms",
            indexed.wall_ms,
            None,
        ),
        row(
            "B3",
            "first query after write",
            "wall ms",
            call.wall_ms,
            Some(300.0),
        ),
        row("B3", "first query after write", "cpu ms", call.cpu_ms, None),
    ]
}

fn b4_during_reindex(rig: &Arc<Rig>) -> Vec<Row> {
    write_new_files(rig, NEW_FILES);
    let background = {
        let rig = Arc::clone(rig);
        std::thread::spawn(move || rig.index())
    };
    std::thread::sleep(Duration::from_millis(300));
    let cpu_before = children_cpu();
    let handles: Vec<_> = (0..CLIENTS)
        .map(|_| {
            let rig = Arc::clone(rig);
            std::thread::spawn(move || rig.query())
        })
        .collect();
    let calls: Vec<Call> = handles
        .into_iter()
        .map(|handle| handle.join().expect("client"))
        .collect();
    let cpu_total = millis(children_cpu().saturating_sub(cpu_before));
    let reindex = background.join().expect("reindex");
    let wall: Vec<f64> = calls.iter().map(|call| call.wall_ms).collect();
    vec![
        row(
            "B4",
            "query during reindex p50",
            "wall ms",
            percentile(&wall, 50),
            Some(150.0),
        ),
        row(
            "B4",
            "query during reindex p99",
            "wall ms",
            percentile(&wall, 99),
            Some(500.0),
        ),
        row(
            "B4",
            "mean per client (all children, includes the reindex)",
            "cpu ms",
            cpu_total / CLIENTS as f64,
            None,
        ),
        row(
            "B4",
            "reindex exit code",
            "code",
            f64::from(reindex.code),
            Some(0.0),
        ),
        hit_row("B4", &calls),
    ]
}

fn b5_cold(rig: &Rig) -> Vec<Row> {
    rig.stop();
    let call = rig.query();
    let limit = (rig.embedder == "hash").then_some(1000.0);
    vec![
        row(
            "B5",
            "cold client call (spawn + ready + query)",
            "wall ms",
            call.wall_ms,
            limit,
        ),
        row(
            "B5",
            "cold client call (spawn + ready + query)",
            "cpu ms",
            call.cpu_ms,
            None,
        ),
    ]
}

fn report(rows: &[Row]) -> ExitCode {
    println!(
        "{:<4} {:<54} {:<8} {:>10} {:>9} result",
        "id", "metric", "kind", "value", "limit"
    );
    for entry in rows {
        let limit = entry
            .limit
            .map_or("-".to_owned(), |limit| format!("{limit:.1}"));
        let result = if entry.limit.is_none() {
            "record"
        } else if entry.passed() {
            "PASS"
        } else {
            "FAIL"
        };
        println!(
            "{:<4} {:<54} {:<8} {:>10.1} {:>9} {result}",
            entry.id, entry.metric, entry.kind, entry.value, limit
        );
    }
    let failed: Vec<&Row> = rows.iter().filter(|entry| !entry.passed()).collect();
    for entry in &failed {
        println!(
            "FAILED {} {} [{}] value={:.1} limit={:.1}",
            entry.id,
            entry.metric,
            entry.kind,
            entry.value,
            entry.limit.unwrap_or(0.0)
        );
    }
    if failed.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

fn main() -> ExitCode {
    let embedder = std::env::args().nth(1).unwrap_or_else(|| "hash".to_owned());
    let copies: usize = std::env::args()
        .nth(2)
        .and_then(|text| text.parse().ok())
        .unwrap_or(20);
    let bin_dir = std::env::current_exe()
        .expect("exe")
        .parent()
        .and_then(Path::parent)
        .expect("target dir")
        .to_path_buf();
    let scratch = std::env::var_os("TMPDIR").map_or_else(std::env::temp_dir, PathBuf::from);
    let base = scratch.join(format!("ss-latency-budget-{}", std::process::id()));
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
    println!(
        "home={} embedder={} copies={copies}",
        rig.home.display(),
        rig.embedder
    );
    let indexed = rig.index();
    println!(
        "initial index: exit={} wall_ms={:.1} {}",
        indexed.code,
        indexed.wall_ms,
        indexed.stdout.trim()
    );
    let mut rows = b1_idle(&rig);
    rows.extend(b2_after_ready(&rig));
    rows.extend(b3_after_write(&rig));
    rows.extend(b4_during_reindex(&rig));
    rows.extend(b5_cold(&rig));
    rig.stop();
    let code = report(&rows);
    let _ = std::fs::remove_dir_all(&base);
    code
}

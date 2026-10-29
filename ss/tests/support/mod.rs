// each test binary uses a different subset of these helpers, and tests assert with expect messages
#![allow(dead_code, clippy::expect_used, clippy::unwrap_used)]

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use ss::client::Client;
use ss::settings::Settings;
use ss_proto::Request;
use tempfile::TempDir;

pub fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).expect("create destination");
    for entry in std::fs::read_dir(from).expect("read source dir") {
        let entry = entry.expect("dir entry");
        let target = to.join(entry.file_name());
        if entry.path().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).expect("copy file");
        }
    }
}

/// The repo's own `src`, copied `copies` times with every line prefixed by a copy tag so no two chunks share text.
pub fn tagged_copies(root: &Path, copies: usize, tag: &str) {
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    for copy in 0..copies {
        let target = root.join(format!("copy_{copy}"));
        copy_dir(&source, &target);
        retag(&target, &format!("{tag}{copy}"));
    }
}

pub fn retag(dir: &Path, tag: &str) {
    for entry in std::fs::read_dir(dir).expect("read dir") {
        let path = entry.expect("entry").path();
        if path.is_dir() {
            retag(&path, tag);
        } else if let Ok(text) = std::fs::read_to_string(&path) {
            let tagged = text.lines().fold(String::new(), |mut acc, line| {
                writeln!(
                    acc,
                    "// {tag}
{line}"
                )
                .expect("write to string");
                acc
            });
            std::fs::write(&path, tagged).expect("write tagged file");
        }
    }
}

pub struct Env {
    pub home: TempDir,
    pub work: TempDir,
    pub idle_secs: String,
    pub watch_backend: String,
    pub rescan_secs: String,
    pub poll_ms: String,
}

impl Env {
    pub fn new() -> Self {
        Self {
            home: tempfile::tempdir().expect("home"),
            work: tempfile::tempdir().expect("work"),
            idle_secs: "600".to_owned(),
            watch_backend: "poll".to_owned(),
            rescan_secs: "0".to_owned(),
            poll_ms: "200".to_owned(),
        }
    }

    pub fn settings(&self) -> Settings {
        Settings::load()
            .expect("settings")
            .with_home(self.home.path())
            .with_embedder("hash")
    }

    pub fn command(&self, binary: &str) -> Command {
        let exe = match binary {
            "ss" => env!("CARGO_BIN_EXE_ss"),
            _ => env!("CARGO_BIN_EXE_ssd"),
        };
        let mut command = Command::new(exe);
        command
            .env("SS_HOME", self.home.path())
            .env("SS_EMBEDDER", "hash")
            .env("SS_WATCH_BACKEND", &self.watch_backend)
            .env("SS_RESCAN_SECS", &self.rescan_secs)
            .env("SS_WATCH_POLL_MS", &self.poll_ms)
            .env("SS_DAEMON_BIN", env!("CARGO_BIN_EXE_ssd"))
            .env("SS_IDLE_SECS", &self.idle_secs)
            .current_dir(self.work.path());
        command
    }

    pub fn run(&self, binary: &str, args: &[&str]) -> Output {
        self.command(binary)
            .args(args)
            .output()
            .expect("run binary")
    }

    pub fn run_timed(&self, args: &[&str]) -> Duration {
        let begun = Instant::now();
        let output = self.run("ss", args);
        assert_eq!(
            output.status.code(),
            Some(0),
            "stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        begun.elapsed()
    }

    pub fn stop_daemon(&self) {
        let _ = self.command("ssd").arg("stop").output();
    }

    pub fn root(&self) -> PathBuf {
        self.work.path().join("tree")
    }
}

pub fn text(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

pub fn percentile(sorted: &[Duration], percent: usize) -> Duration {
    let rank = (sorted.len() * percent).div_ceil(100);
    sorted[rank.saturating_sub(1).min(sorted.len() - 1)]
}

pub async fn wait_until_listening(client: &Client) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if matches!(client.send_if_running(Request::Status).await, Ok(Some(_))) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("daemon never started listening");
}

impl Drop for Env {
    fn drop(&mut self) {
        self.stop_daemon();
    }
}

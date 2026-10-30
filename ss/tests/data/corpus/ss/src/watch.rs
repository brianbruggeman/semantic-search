use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use ignore::gitignore::{Gitignore, GitignoreBuilder};
use notify::{Config, Event, EventKind, PollWatcher, RecommendedWatcher, RecursiveMode, Watcher};
use proxima::telemetry::{info, warn};
use tokio::sync::{Mutex, mpsc};
use tokio::task::JoinHandle;

use crate::daemon::Core;

/// Which event paths are worth indexing: not hidden, not gitignored, not the daemon's own state.
pub struct EventFilter {
    root: PathBuf,
    ignore: Gitignore,
    state_dir: PathBuf,
}

impl EventFilter {
    #[must_use]
    pub fn new(root: &Path, state_dir: &Path) -> Self {
        let mut builder = GitignoreBuilder::new(root);
        for name in [".gitignore", ".ignore"] {
            let _ = builder.add(root.join(name));
        }
        Self {
            root: root.to_path_buf(),
            ignore: builder.build().unwrap_or_else(|_| Gitignore::empty()),
            state_dir: state_dir.to_path_buf(),
        }
    }

    #[must_use]
    pub fn admits(&self, path: &Path) -> bool {
        let Ok(rel) = path.strip_prefix(&self.root) else {
            return false;
        };
        let hidden = rel
            .components()
            .any(|part| part.as_os_str().to_string_lossy().starts_with('.'));
        let ignored = self
            .ignore
            .matched_path_or_any_parents(rel, path.is_dir())
            .is_ignore();
        !hidden && !ignored && !path.starts_with(&self.state_dir)
    }
}

/// One debounce window of events folded into "which paths to look at" and "must we rescan everything".
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Batch {
    pub paths: BTreeSet<PathBuf>,
    pub rescan: bool,
}

impl Batch {
    pub fn absorb(&mut self, event: notify::Result<Event>, filter: &EventFilter) {
        let Ok(event) = event else {
            self.rescan = true;
            return;
        };
        if event.need_rescan() {
            self.rescan = true;
        }
        if matches!(event.kind, EventKind::Access(_)) {
            return;
        }
        self.paths
            .extend(event.paths.into_iter().filter(|path| filter.admits(path)));
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.paths.is_empty() && !self.rescan
    }
}

/// The OS-event watcher or the stat-polling one; held only to keep it alive.
enum Live {
    Native { _watcher: RecommendedWatcher },
    Poll { _watcher: PollWatcher },
}

impl Live {
    fn begin(
        poll: bool,
        interval: Duration,
        path: &Path,
        events: mpsc::UnboundedSender<notify::Result<Event>>,
    ) -> notify::Result<Self> {
        let handler = move |event| drop(events.send(event));
        if poll {
            let mut watcher =
                PollWatcher::new(handler, Config::default().with_poll_interval(interval))?;
            watcher.watch(path, RecursiveMode::Recursive)?;
            return Ok(Self::Poll { _watcher: watcher });
        }
        let mut watcher = notify::recommended_watcher(handler)?;
        watcher.watch(path, RecursiveMode::Recursive)?;
        Ok(Self::Native { _watcher: watcher })
    }
}

struct ActiveWatch {
    _watcher: Live,
    task: JoinHandle<()>,
}

impl Drop for ActiveWatch {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// The set of live watchers, one per registered watch path.
#[derive(Default)]
pub struct WatchHub {
    active: Mutex<HashMap<PathBuf, ActiveWatch>>,
}

impl WatchHub {
    pub async fn count(&self) -> usize {
        self.active.lock().await.len()
    }

    pub async fn stop(&self, path: &Path) -> bool {
        self.active.lock().await.remove(path).is_some()
    }

    /// Starts watching `path` (a no-op when already live) and kicks off its startup rescan.
    pub async fn start(&self, core: &Arc<Core>, path: &Path) -> notify::Result<()> {
        let mut active = self.active.lock().await;
        if active.contains_key(path) {
            return Ok(());
        }
        let (events, inbox) = mpsc::unbounded_channel();
        let settings = core.settings();
        let interval = Duration::from_millis(settings.watch_poll_ms);
        let watcher = Live::begin(settings.watch_backend == "poll", interval, path, events)?;
        let task = tokio::spawn(watch_loop(Arc::clone(core), path.to_path_buf(), inbox));
        active.insert(
            path.to_path_buf(),
            ActiveWatch {
                _watcher: watcher,
                task,
            },
        );
        Ok(())
    }
}

async fn watch_loop(
    core: Arc<Core>,
    watch: PathBuf,
    mut inbox: mpsc::UnboundedReceiver<notify::Result<Event>>,
) {
    let debounce = Duration::from_millis(core.settings().debounce_ms);
    let filter = EventFilter::new(
        &watch_root(&core, &watch).await,
        &core.settings().home_dir(),
    );
    let mut pending = Batch {
        rescan: true,
        ..Batch::default()
    };
    let safety_net =
        (core.settings().rescan_secs > 0).then(|| Duration::from_secs(core.settings().rescan_secs));
    loop {
        if !pending.is_empty() {
            settle(&core, &watch, std::mem::take(&mut pending)).await;
        }
        let first = match wait_for_event(&mut inbox, safety_net).await {
            Wake::Event(first) => first,
            Wake::Quiet => {
                pending.rescan = true;
                continue;
            }
            Wake::Closed => return,
        };
        pending.absorb(first, &filter);
        loop {
            match tokio::time::timeout(debounce, inbox.recv()).await {
                Ok(Some(event)) => pending.absorb(event, &filter),
                Ok(None) => return,
                Err(_) => break,
            }
        }
    }
}

enum Wake {
    Event(notify::Result<Event>),
    Quiet,
    Closed,
}

/// Events are hints, so a quiet period of `safety_net` demands a full rescan even if the OS reported nothing.
async fn wait_for_event(
    inbox: &mut mpsc::UnboundedReceiver<notify::Result<Event>>,
    safety_net: Option<Duration>,
) -> Wake {
    let received = match safety_net {
        Some(period) => match tokio::time::timeout(period, inbox.recv()).await {
            Ok(received) => received,
            Err(_) => return Wake::Quiet,
        },
        None => inbox.recv().await,
    };
    received.map_or(Wake::Closed, Wake::Event)
}

async fn watch_root(core: &Core, watch: &Path) -> PathBuf {
    core.root_of(watch)
        .await
        .unwrap_or_else(|| watch.to_path_buf())
}

async fn settle(core: &Arc<Core>, watch: &Path, batch: Batch) {
    let rescan = batch.rescan;
    let outcome = core.apply_batch(watch, batch).await;
    match outcome {
        Ok(stats) => info!(
            "watch batch indexed",
            full_rescan = rescan,
            path = %watch.display(),
            files = stats.files_indexed,
            removed = stats.files_removed,
            embedded = stats.chunks_embedded
        ),
        Err(error) => warn!("watch batch failed", path = %watch.display(), error = %error),
    }
}

#[cfg(test)]
mod tests {
    use notify::event::{AccessKind, CreateKind, Flag, ModifyKind};

    use super::*;

    fn filter(root: &Path) -> EventFilter {
        std::fs::write(root.join(".gitignore"), "target/\n*.log\n").expect("gitignore");
        EventFilter::new(root, &root.join("state"))
    }

    fn created(path: &Path) -> Event {
        Event::new(EventKind::Create(CreateKind::File)).add_path(path.to_path_buf())
    }

    #[test]
    fn filter_drops_hidden_ignored_and_state_paths_but_keeps_sources() {
        let dir = tempfile::tempdir().expect("dir");
        let filter = filter(dir.path());

        assert!(filter.admits(&dir.path().join("src/lib.rs")));
        assert!(!filter.admits(&dir.path().join(".git/index")));
        assert!(!filter.admits(&dir.path().join("target/debug/out.rs")));
        assert!(!filter.admits(&dir.path().join("run.log")));
        assert!(!filter.admits(&dir.path().join("state/index.db")));
        assert!(!filter.admits(Path::new("/elsewhere/x.rs")));
    }

    #[test]
    fn batch_collapses_duplicate_events_and_ignores_access() {
        let dir = tempfile::tempdir().expect("dir");
        let filter = filter(dir.path());
        let file = dir.path().join("a.rs");
        let mut batch = Batch::default();

        batch.absorb(Ok(created(&file)), &filter);
        batch.absorb(
            Ok(Event::new(EventKind::Modify(ModifyKind::Any)).add_path(file.clone())),
            &filter,
        );
        batch.absorb(
            Ok(Event::new(EventKind::Access(AccessKind::Any)).add_path(dir.path().join("b.rs"))),
            &filter,
        );

        assert_eq!(batch.paths.len(), 1);
        assert!(!batch.rescan);
    }

    #[test]
    fn watcher_error_or_overflow_flag_demands_a_full_rescan() {
        let dir = tempfile::tempdir().expect("dir");
        let filter = filter(dir.path());
        let mut after_error = Batch::default();
        let mut after_overflow = Batch::default();

        after_error.absorb(
            Err(notify::Error::generic("inotify queue overflow")),
            &filter,
        );
        after_overflow.absorb(
            Ok(Event::new(EventKind::Other).set_flag(Flag::Rescan)),
            &filter,
        );

        assert!(after_error.rescan && after_overflow.rescan);
        assert!(after_error.paths.is_empty());
    }

    #[test]
    fn an_empty_batch_is_idle() {
        assert!(Batch::default().is_empty());
        assert!(
            !Batch {
                rescan: true,
                ..Batch::default()
            }
            .is_empty()
        );
    }
}

use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// What `add_watch` did, so callers can tell a new watcher from an idempotent repeat.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchChange {
    /// A new watch was recorded; `absorbed` descendants it replaced must have their watchers stopped.
    Added { absorbed: Vec<PathBuf> },
    /// Already watched exactly; nothing changed.
    Exists,
    /// An ancestor already watches this path; nothing changed.
    Covered { by: PathBuf },
}

/// The daemon's persisted state: indexed roots and the watched subset. Pure data plus pure rules;
/// reading and writing the file is the only IO and lives in [`Registry::load`] and [`Registry::save`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Registry {
    pub roots: Vec<PathBuf>,
    pub watches: Vec<PathBuf>,
}

impl Registry {
    /// The nearest (longest) root that contains `path`.
    #[must_use]
    pub fn resolve(&self, path: &Path) -> Option<&Path> {
        self.roots
            .iter()
            .filter(|root| path.starts_with(root))
            .max_by_key(|root| root.components().count())
            .map(PathBuf::as_path)
    }

    /// Records `path` as a root unless an ancestor root covers it; roots below it are absorbed.
    /// Returns the roots that were absorbed (their shards are now stale).
    pub fn add_root(&mut self, path: &Path) -> Vec<PathBuf> {
        if self.resolve(path).is_some() {
            return Vec::new();
        }
        let (absorbed, kept): (Vec<PathBuf>, Vec<PathBuf>) = std::mem::take(&mut self.roots)
            .into_iter()
            .partition(|root| root.starts_with(path));
        self.roots = kept;
        self.roots.push(path.to_path_buf());
        absorbed
    }

    pub fn add_watch(&mut self, path: &Path) -> WatchChange {
        if self.watches.iter().any(|watch| watch == path) {
            return WatchChange::Exists;
        }
        if let Some(by) = self.watches.iter().find(|watch| path.starts_with(watch)) {
            return WatchChange::Covered { by: by.clone() };
        }
        let (absorbed, kept): (Vec<PathBuf>, Vec<PathBuf>) = std::mem::take(&mut self.watches)
            .into_iter()
            .partition(|watch| watch.starts_with(path));
        self.watches = kept;
        self.watches.push(path.to_path_buf());
        WatchChange::Added { absorbed }
    }

    /// Removes an exact watch; `false` when it was not registered.
    pub fn rm_watch(&mut self, path: &Path) -> bool {
        let before = self.watches.len();
        self.watches.retain(|watch| watch != path);
        self.watches.len() != before
    }

    /// A missing file is an empty registry; a corrupt one is an error rather than silent data loss.
    pub fn load(path: &Path) -> io::Result<Self> {
        match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(io::Error::other),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(error),
        }
    }

    /// Writes atomically: a temp file in the same directory, then a rename.
    pub fn save(&self, path: &Path) -> io::Result<()> {
        let staging = path.with_extension("json.tmp");
        std::fs::write(
            &staging,
            serde_json::to_vec_pretty(self).map_err(io::Error::other)?,
        )?;
        std::fs::rename(staging, path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry(roots: &[&str], watches: &[&str]) -> Registry {
        Registry {
            roots: roots.iter().map(PathBuf::from).collect(),
            watches: watches.iter().map(PathBuf::from).collect(),
        }
    }

    #[test]
    fn resolve_picks_the_nearest_enclosing_root() {
        let reg = registry(&["/a", "/a/b"], &[]);

        assert_eq!(reg.resolve(Path::new("/a/b/c.rs")), Some(Path::new("/a/b")));
        assert_eq!(reg.resolve(Path::new("/a/x.rs")), Some(Path::new("/a")));
        assert_eq!(reg.resolve(Path::new("/z")), None);
    }

    #[test]
    fn adding_a_root_under_an_existing_root_changes_nothing() {
        let mut reg = registry(&["/a"], &[]);

        let absorbed = reg.add_root(Path::new("/a/b"));

        assert_eq!((absorbed.len(), reg.roots.len()), (0, 1));
    }

    #[test]
    fn adding_a_parent_root_absorbs_nested_roots_but_keeps_watches_that_now_resolve_to_it() {
        let mut reg = registry(&["/a/b", "/a/c"], &["/a/b"]);

        let absorbed = reg.add_root(Path::new("/a"));

        assert_eq!(absorbed.len(), 2);
        assert_eq!(reg, registry(&["/a"], &["/a/b"]));
    }

    #[test]
    fn watch_add_is_idempotent_and_nested_paths_are_covered() {
        let mut reg = registry(&[], &[]);

        let first = reg.add_watch(Path::new("/w"));
        let again = reg.add_watch(Path::new("/w"));
        let nested = reg.add_watch(Path::new("/w/sub"));

        assert_eq!(
            first,
            WatchChange::Added {
                absorbed: Vec::new()
            }
        );
        assert_eq!(again, WatchChange::Exists);
        assert_eq!(
            nested,
            WatchChange::Covered {
                by: PathBuf::from("/w")
            }
        );
        assert_eq!(reg.watches.len(), 1);
    }

    #[test]
    fn watching_a_parent_absorbs_child_watches() {
        let mut reg = registry(&[], &["/w/a", "/w/b"]);

        let change = reg.add_watch(Path::new("/w"));

        assert_eq!(
            change,
            WatchChange::Added {
                absorbed: vec![PathBuf::from("/w/a"), PathBuf::from("/w/b")]
            }
        );
        assert_eq!(reg.watches, vec![PathBuf::from("/w")]);
    }

    #[test]
    fn rm_watch_reports_whether_anything_was_removed() {
        let mut reg = registry(&[], &["/w"]);

        assert!(reg.rm_watch(Path::new("/w")));
        assert!(!reg.rm_watch(Path::new("/w")));
    }

    #[test]
    fn save_then_load_round_trips_and_missing_file_is_empty() {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("state.json");
        let reg = registry(&["/a"], &["/a"]);

        assert_eq!(
            Registry::load(&path).expect("missing is empty"),
            Registry::default()
        );
        reg.save(&path).expect("save");

        assert_eq!(Registry::load(&path).expect("load"), reg);
    }

    #[test]
    fn corrupt_state_file_is_an_error_not_an_empty_registry() {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("state.json");
        std::fs::write(&path, b"{ nope").expect("write");

        assert!(Registry::load(&path).is_err());
    }
}

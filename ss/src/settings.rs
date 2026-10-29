use std::path::{Path, PathBuf};

use conflaguration::Settings as ConfigSettings;
use ss_ipc::Endpoint;

use crate::embed::EmbedderKind;
use crate::store::StoreConfig;

/// Every tunable, loadable from `SS_*` environment variables (`SS_QUERY_CAP=4`) or built in code.
#[derive(Debug, Clone, PartialEq, Eq, ConfigSettings)]
#[settings(prefix = "SS")]
pub struct Settings {
    /// State directory; empty selects the platform cache dir
    #[setting(default_str = "")]
    pub home: String,
    /// Embedding backend: `fastembed` (local ONNX model) or `hash` (offline feature hashing)
    #[setting(default_str = "fastembed")]
    pub embedder: String,
    /// Embedding worker threads, each owning a model; the cap on concurrent embedding work
    #[setting(default = 2)]
    pub embed_workers: usize,
    /// ONNX intra-op threads per embedding worker
    #[setting(default = 2)]
    pub embed_threads: usize,
    /// Searches running at once
    #[setting(default = 8)]
    pub query_cap: usize,
    /// Searches allowed to wait for a slot before `busy` is returned
    #[setting(default = 64)]
    pub query_depth: usize,
    /// Index runs at once
    #[setting(default = 2)]
    pub index_cap: usize,
    /// Index runs allowed to wait for a slot before `busy` is returned
    #[setting(default = 8)]
    pub index_depth: usize,
    /// Seconds with no watches and no clients before the daemon exits
    #[setting(default = 600)]
    pub idle_secs: u64,
    /// How long a client waits for a freshly spawned daemon to accept connections
    #[setting(default = 15000)]
    pub spawn_timeout_ms: u64,
    /// Quiet time after a file event before the batch is indexed
    #[setting(default = 200)]
    pub debounce_ms: u64,
    /// Path to the `ssd` binary; empty selects the sibling of the running executable
    #[setting(default_str = "")]
    pub daemon_bin: String,
}

impl Settings {
    /// Reads `SS_*` environment variables over the defaults.
    pub fn load() -> conflaguration::Result<Self> {
        <Self as ConfigSettings>::from_env()
    }

    #[must_use]
    pub fn home_dir(&self) -> PathBuf {
        if self.home.is_empty() {
            dirs::cache_dir()
                .unwrap_or_else(std::env::temp_dir)
                .join("ss")
        } else {
            PathBuf::from(&self.home)
        }
    }

    #[must_use]
    pub fn with_home(mut self, home: &Path) -> Self {
        self.home = home.to_string_lossy().into_owned();
        self
    }

    #[must_use]
    pub fn with_embedder(mut self, kind: &str) -> Self {
        kind.clone_into(&mut self.embedder);
        self
    }

    #[must_use]
    pub fn embedder_kind(&self) -> EmbedderKind {
        EmbedderKind::parse(&self.embedder).unwrap_or_default()
    }

    #[must_use]
    pub fn endpoint(&self) -> Endpoint {
        Endpoint::new(&self.home_dir())
    }

    #[must_use]
    pub fn lock_path(&self) -> PathBuf {
        self.home_dir().join("ssd.lock")
    }

    #[must_use]
    pub fn state_path(&self) -> PathBuf {
        self.home_dir().join("state.json")
    }

    #[must_use]
    pub fn log_dir(&self) -> PathBuf {
        self.home_dir().join("logs")
    }

    #[must_use]
    pub fn model_cache_dir(&self) -> PathBuf {
        self.home_dir().join("models")
    }

    #[must_use]
    pub fn store_config(&self, root: &Path) -> StoreConfig {
        let digest = blake3::hash(root.to_string_lossy().as_bytes()).to_hex();
        let short = digest.as_str().chars().take(16).collect::<String>();
        StoreConfig {
            dir: self.home_dir().join("roots").join(short),
            model_id: self.embedder_kind().model_id().to_owned(),
        }
    }

    #[must_use]
    pub fn daemon_executable(&self) -> PathBuf {
        if !self.daemon_bin.is_empty() {
            return PathBuf::from(&self.daemon_bin);
        }
        let name = if cfg!(windows) { "ssd.exe" } else { "ssd" };
        std::env::current_exe().map_or_else(|_| PathBuf::from(name), |exe| exe.with_file_name(name))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_overrides_defaults_and_equals_the_fluent_build() {
        let from_env = temp_env::with_vars(
            [
                ("SS_HOME", Some("/data/ss")),
                ("SS_EMBEDDER", Some("hash")),
                ("SS_QUERY_CAP", Some("3")),
            ],
            || Settings::from_env().expect("settings from env"),
        );

        let fluent = Settings {
            query_cap: 3,
            ..Settings::from_env().expect("defaults")
        }
        .with_home(Path::new("/data/ss"))
        .with_embedder("hash");

        assert_eq!(from_env, fluent);
    }

    #[test]
    fn defaults_are_the_documented_caps() {
        let settings =
            temp_env::with_vars_unset(["SS_QUERY_CAP", "SS_EMBED_WORKERS", "SS_IDLE_SECS"], || {
                Settings::from_env().expect("defaults")
            });

        assert_eq!(
            (
                settings.query_cap,
                settings.embed_workers,
                settings.idle_secs
            ),
            (8, 2, 600)
        );
    }

    #[test]
    fn shard_dir_is_stable_per_root_and_distinct_between_roots() {
        let settings = Settings::from_env()
            .expect("defaults")
            .with_home(Path::new("/h"));

        let first = settings.store_config(Path::new("/work/a"));
        let again = settings.store_config(Path::new("/work/a"));
        let other = settings.store_config(Path::new("/work/b"));

        assert_eq!(first, again);
        assert_ne!(first.dir, other.dir);
    }
}

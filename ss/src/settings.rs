use std::path::{Path, PathBuf};

use conflaguration::Settings as ConfigSettings;
use proxima::pipe::FailMode;
use ss_ipc::Endpoint;

use crate::embed::{EmbedProfile, EmbedderKind, Provider};
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
    /// Embedding worker threads for indexing, each owning a model; the cap on concurrent indexing work
    #[setting(default = 2)]
    pub embed_workers: usize,
    /// Embedding worker threads reserved for search queries, each owning a model; indexing never uses them
    #[setting(default = 1)]
    pub embed_query_workers: usize,
    /// Where the fastembed model runs: `cpu` or `coreml` (macos GPU); changing it re-embeds every index
    #[setting(default_str = "cpu")]
    pub embed_provider: String,
    /// Tokens kept per chunk before the model truncates it; changing it re-embeds every index
    #[setting(default = 256)]
    pub embed_max_length: usize,
    /// ONNX intra-op threads per embedding worker
    #[setting(default = 2)]
    pub embed_threads: usize,
    /// Searches running at once
    #[setting(default = 8)]
    pub query_cap: usize,
    /// Searches allowed to wait for a worker, rounded up to a power of two; beyond it the overflow strategy sheds
    #[setting(default = 64)]
    pub query_depth: usize,
    /// What a full search queue does with a new search: `drop_newest`, `drop_oldest` or `refuse`; the shed caller gets `busy`
    #[setting(default_str = "drop_newest")]
    pub query_overflow: String,
    /// Index runs at once
    #[setting(default = 2)]
    pub index_cap: usize,
    /// Index runs allowed to wait for a worker, rounded up to a power of two; beyond it the overflow strategy sheds
    #[setting(default = 8)]
    pub index_depth: usize,
    /// What a full index queue does with a new run: `drop_newest`, `drop_oldest` or `refuse`; a rescan recovers what was shed
    #[setting(default_str = "drop_oldest")]
    pub index_overflow: String,
    /// Seconds with no watches and no clients before the daemon exits
    #[setting(default = 600)]
    pub idle_secs: u64,
    /// How long a client waits for a freshly spawned daemon to accept connections
    #[setting(default = 15000)]
    pub spawn_timeout_ms: u64,
    /// How long a client waits for the reply to a search, status or watch request before giving up; zero waits forever. Index requests are never bounded
    #[setting(default = 30000)]
    pub request_timeout_ms: u64,
    /// Quiet time after a file event before the batch is indexed
    #[setting(default = 200)]
    pub debounce_ms: u64,
    /// Watch backend: `native` (OS file events) or `poll` (stat polling, for network drives and containers)
    #[setting(default_str = "native")]
    pub watch_backend: String,
    /// Stat-polling interval for the `poll` backend
    #[setting(default = 500)]
    pub watch_poll_ms: u64,
    /// Seconds of event silence after which a watched root is fully rescanned anyway; zero disables
    #[setting(default = 300)]
    pub rescan_secs: u64,
    /// Adds a rank contribution to chunks whose callers, callees or container also rank in the fused top 20
    #[setting(default = false)]
    pub graph_boost: bool,
    /// Megabytes of vectors the daemon may hold in memory per process; a larger index scans SQLite instead
    #[setting(default = 512)]
    pub vector_cache_mb: usize,
    /// Path to the `ssd` binary; empty selects the sibling of the running executable
    #[setting(default_str = "")]
    pub daemon_bin: String,
}

fn parse_fail_mode(name: &str) -> Option<FailMode> {
    match name {
        "drop_newest" => Some(FailMode::DropNewest),
        "drop_oldest" => Some(FailMode::DropOldest),
        "refuse" => Some(FailMode::FailClosed),
        _ => None,
    }
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

    /// Unknown provider names fall back to `cpu`, the way unknown embedder names fall back to the default.
    #[must_use]
    pub fn embed_profile(&self) -> EmbedProfile {
        EmbedProfile {
            provider: Provider::parse(&self.embed_provider).unwrap_or_default(),
            max_length: self.embed_max_length.max(1),
        }
    }

    #[must_use]
    pub fn query_fail_mode(&self) -> FailMode {
        parse_fail_mode(&self.query_overflow).unwrap_or(FailMode::DropNewest)
    }

    #[must_use]
    pub fn index_fail_mode(&self) -> FailMode {
        parse_fail_mode(&self.index_overflow).unwrap_or(FailMode::DropOldest)
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
            model_id: self.embed_profile().store_id(self.embedder_kind()),
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
    fn graph_boost_is_off_by_default_and_on_with_the_env_variable() {
        let off = temp_env::with_vars_unset(["SS_GRAPH_BOOST"], || {
            Settings::from_env().expect("defaults")
        });
        let on = temp_env::with_vars([("SS_GRAPH_BOOST", Some("true"))], || {
            Settings::from_env().expect("settings from env")
        });

        assert!(!off.graph_boost);
        assert!(on.graph_boost);
    }

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
    fn overflow_names_map_to_proxima_fail_modes_and_unknown_names_take_the_lane_default() {
        let settings = Settings {
            query_overflow: "drop_oldest".to_owned(),
            index_overflow: "bogus".to_owned(),
            ..Settings::from_env().expect("defaults")
        };

        assert_eq!(settings.query_fail_mode(), FailMode::DropOldest);
        assert_eq!(settings.index_fail_mode(), FailMode::DropOldest);
        assert_eq!(parse_fail_mode("refuse"), Some(FailMode::FailClosed));
        assert_eq!(parse_fail_mode("drop_newest"), Some(FailMode::DropNewest));
    }

    #[test]
    fn embed_provider_and_max_length_reach_the_store_identity_and_default_to_cpu_256() {
        let defaults =
            temp_env::with_vars_unset(["SS_EMBED_PROVIDER", "SS_EMBED_MAX_LENGTH"], || {
                Settings::from_env().expect("defaults")
            });
        let coreml = temp_env::with_vars(
            [
                ("SS_EMBED_PROVIDER", Some("coreml")),
                ("SS_EMBED_MAX_LENGTH", Some("128")),
            ],
            || Settings::from_env().expect("coreml settings"),
        );

        let root = Path::new("/work/a");
        assert_eq!(
            defaults.store_config(root).model_id,
            "bge-small-en-v1.5+cpu+len256"
        );
        assert_eq!(
            coreml.store_config(root).model_id,
            "bge-small-en-v1.5+coreml+len128"
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

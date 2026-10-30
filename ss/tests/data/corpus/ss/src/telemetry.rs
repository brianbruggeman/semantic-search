use std::sync::Arc;

use proxima::telemetry::emit::{EnvFilter, global};
use proxima::telemetry::error::Error as TelemetryError;
use proxima::telemetry::export::{Exporter, Formatter, default_recorder};
use proxima::telemetry::recorder::Recorder;
use ss_proto::IndexStats;

use crate::settings::Settings;

const LOG_ROTATE_BYTES: u64 = 8 << 20;
const LOG_FILES_KEPT: usize = 3;
const DEFAULT_FILTER: &str = "warn,ss=info";

/// Installs the process-default recorder with BOTH sinks: level-routed console (stdout/stderr) and a rotating
/// JSON-lines file under the state dir. `RUST_LOG` overrides the default filter. Never OTLP-only.
pub fn init(settings: &Settings) -> Result<Arc<Recorder>, TelemetryError> {
    let directive = std::env::var("RUST_LOG").unwrap_or_else(|_| DEFAULT_FILTER.to_owned());
    global::install(EnvFilter::parse(&directive));
    std::fs::create_dir_all(settings.log_dir())
        .map_err(|error| TelemetryError::ThreadSpawn(error.to_string()))?;
    let exporter = Exporter::fan(vec![
        Exporter::std(),
        Exporter::file_rotating(
            settings.log_dir().join("ssd.log"),
            LOG_ROTATE_BYTES,
            LOG_FILES_KEPT,
        )
        .format(Formatter::Json),
    ])?;
    let recorder = Recorder::builder().export(exporter)?.install()?;
    let pump = Arc::clone(&recorder);
    std::thread::Builder::new()
        .name("ss-telemetry-drain".to_owned())
        .spawn(move || pump.run_drain_loop())
        .map_err(|error| TelemetryError::ThreadSpawn(error.to_string()))?;
    Ok(recorder)
}

/// Adds one index run to the `chunks_seen` / `chunks_embedded` / `chunks_skipped` counters.
pub fn record_index(recorder: &Recorder, stats: &IndexStats) {
    recorder.counter("chunks_seen").add(stats.chunks_seen, &[]);
    recorder
        .counter("chunks_embedded")
        .add(stats.chunks_embedded, &[]);
    recorder
        .counter("chunks_skipped")
        .add(stats.chunks_skipped, &[]);
}

/// Same as [`record_index`] against the process-default recorder; a no-op when none is installed.
pub fn record_index_default(stats: &IndexStats) {
    if let Some(recorder) = default_recorder() {
        record_index(&recorder, stats);
    }
}

#[cfg(test)]
mod tests {
    use proxima::telemetry::capture::capture;

    use super::*;

    #[test]
    fn record_index_adds_the_three_chunk_counters() {
        let stats = IndexStats {
            chunks_seen: 10,
            chunks_embedded: 3,
            chunks_skipped: 7,
            ..IndexStats::default()
        };

        let captured = capture(|recorder| {
            record_index(recorder, &stats);
            recorder.drain();
        });

        let metrics = captured.metrics();
        let names: Vec<String> = metrics.iter().map(|metric| format!("{metric:?}")).collect();
        println!("captured metrics: {names:?}");
        for expected in ["chunks_seen", "chunks_embedded", "chunks_skipped"] {
            assert!(
                names.iter().any(|name| name.contains(expected)),
                "missing {expected} in {names:?}"
            );
        }
        assert_eq!(metrics.len(), 3);
        let joined = names.join(" ");
        for expected in ["U64(10)", "U64(3)", "U64(7)"] {
            assert!(
                joined.contains(expected),
                "missing value {expected} in {joined}"
            );
        }
    }
}

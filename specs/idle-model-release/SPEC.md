# idle-model-release

status: audited
implementation: present, uncommitted
evidence: see TASKS.md and command/process artifacts
owner: brian
created: 2026-11-02

## problem

An ss daemon must release each loaded embedding model after its configured interval without a job, observed through model-drop notifications and process RSS after a completed real index.

## refutation condition

If dropping the last worker-owned model does not reduce post-index memory, worker model retention does not explain that remaining memory and must not be reported as its cause.

## requirements

| id | requirement | testable in isolation |
|---|---|---|
| R1 | query and index workers drop their model after idle expiry and reload for later jobs | yes |
| R2 | warm readiness decreases on expiry and returns after a successful reload | yes |
| R3 | callers using existing pool constructors retain model reuse | yes |
| R4 | daemon defaults to lazy startup, exposes SS_EMBED_IDLE_SECS, and wires that value to both lanes | yes |
| R5 | vector-cache tests that mutate the process-global memory cap cannot evict lists in concurrent tests | yes |

## architecture

Use daemon::configured_pool(settings, factory) as the common construction path for real and fixture models.
Add EmbedPool::with_idle_timeout(query_workers, index_workers, warm, Option<Duration>, factory).
Existing constructors delegate with None; the daemon supplies Some(Duration::from_secs(settings.embed_idle_secs)).
Each dedicated worker owns its Option<Model>. A loaded worker receives with flume recv_timeout; expiry drops the model, updates readiness, then waits with blocking recv until the next job. There is no periodic polling when cold.
SS_EMBED_IDLE_SECS defaults to 30; zero releases as soon as the queue empties. SS_WARM_START defaults to false; explicit true still preloads.

### decisions

| decision | chosen | why not the alternative |
|---|---|---|
| ownership | worker drops its own model | a shared model mutex or timer task would introduce synchronization across owners |
| cold waiting | blocking receive on dedicated thread | periodic polling would spend idle CPU |
| compatibility | existing constructors keep indefinite reuse | silently expiring all library pools would change callers that explicitly warm them |
| evidence | capture real process after completed index and repeat request | a Drop counter alone cannot establish that an inference runtime releases its memory |

## failure evidence

The initial workspace run recorded `compaction_leaves_results_unchanged_and_drops_tombstoned_rows` failing at `ss/tests/vector_cache.rs:160-162`; the exact harness record is in `workspace-tests-attempt1.log`. The exact test passed alone in `vector-cache-single.log`, and the shared vector-cache test binary passed in ten consecutive reruns recorded as `vector-cache-repeat-1.log` through `vector-cache-repeat-10.log`.

The causal path is visible in the artifacts: `ss/tests/vector_cache.rs:304-306` sets the cap to zero around a write; `ss/src/store/vector_cache.rs:16,56-57` stores that cap in a process-global atomic; `ss/src/store/vector_cache.rs:302-305` clears any publishing shard list when its bytes exceed the cap; and `ss/src/store/vector_cache.rs:340-342` returns from compaction when the list is absent. The compaction fixture explicitly requires the compaction counter to increase at `ss/tests/vector_cache.rs:159-162`. The cap fixture now requires both a child marker and exact single-test arguments before mutating the cap, and verifies the child's one-test success before returning to the parent test harness.

After isolation, `vector-cache-isolated.log` records 9 vector-cache tests passed and `workspace-tests.log` records a successful workspace test command, including the vector-cache test binary. These observations are reruns after the isolation change; they do not replace the original failure record.

## acceptance criteria

Oracle: Rust channel ownership is independent of EmbedPool: the last Sender dropping disconnects Receiver. Expected vectors are the hand-authored constant [1.0, 0.0]. Each correctness fixture uses that oracle. The named incumbent is direct Rust ownership: construct a closure owning the sender, call it directly, and std::mem::drop it, bypassing EmbedPool. AC5 runs this incumbent through the exact same vector/lifetime assertions as AC1.

| id | discharges | command | expected |
|---|---|---|---|
| AC1 | R1 | `cargo test -p ss --lib idle_model_ -- --nocapture` | 4 tests passed, 0 failed; zero and positive timeout, busy retention, timer reset; Rust channel-disconnection oracle plus constant-vector oracle; each zero-timeout lane observes 2 loads/drops over 2 requests; each positive-timeout lane observes 2 loads/drops over 3 requests |
| AC2 | R2 | `cargo test -p ss --lib idle_readiness_ -- --nocapture` | 1 test passed, 0 failed; channel-disconnection oracle establishes expiry before readiness is false after drop and true after query/index reload |
| AC3 | R3 | `cargo test -p ss --lib idle_disabled_ -- --nocapture` | 1 test passed, 0 failed; constant-vector and atomic factory-counter oracles; 2 requests load 1 model |
| AC4 | R4 | `cargo test -p ss --lib idle_settings_ -- --nocapture` | 1 test passed, 0 failed; literal false/30 default-policy oracle measured separately from channel-disconnection oracle under explicit zero expiry on both lanes |

| AC5 | R1 | `cargo test -p ss --lib idle_oracle_ -- --nocapture` | 1 test passed, 0 failed; direct Rust closure ownership passes the shared vector/lifetime assertions without EmbedPool |

| AC6 | R1, R4 | `python specs/idle-model-release/probe.py` | 2 process arms, 2 completed one-file indexes, 4 query payloads identifying the independently authored example.md fixture using the same path-presence assertion run against named incumbent `rg --files` output, and an empty-path negative control rejected, 1 retained control rejected by release-log gate, at least 1 release-site telemetry event in expiry arm; raw RSS/CPU and executable identity recorded as measurements only. Destruction oracle remains AC1/AC5, not telemetry. |
| AC7 | R5 | `cargo test -p ss --test vector_cache -- --nocapture` | 9 tests passed, 0 failed; cap mutation and its list-drop/SQLite-fallback assertions run in a child test process whose output and one-test count the parent checks, while the eight remaining tests run in the parent process |

## out of scope

- changing embedding vectors, ranking, store identity, or stopping an active production index before its result is delivered

## risks

| risk | likelihood | what it costs | what we do about it |
|---|---|---|---|
| next request reloads model | certain after expiry | cold request latency | configurable interval and real request measurement |
| runtime allocator retains freed pages | unknown | RSS remains high | real-process experiment; separate retained allocator memory from owned model lifetime |
| ready counter races | possible | misleading status | update on owning worker and test drop notification before status |

## context

- /Users/brianbruggeman/repos/mine/ss/ss/src/embed.rs
- /Users/brianbruggeman/repos/mine/ss/ss/src/daemon.rs
- /Users/brianbruggeman/repos/mine/ss/ss/src/settings.rs
- /private/tmp/ss-resource-resume.jsonl

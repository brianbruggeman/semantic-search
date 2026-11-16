# search-resource-cost

status: audited
implementation: present, uncommitted
evidence: see TASKS.md and command/process artifacts
owner: brian
created: 2026-11-02

## problem

On this macOS ss daemon, explicitly selected CPU embeddings must avoid automatic CoreML allocation, disconnected client requests must stop submitting indexing work, and the corrected process must use less CPU time and RSS on the same captured inputs.

## refutation condition

If the same-input process comparison still shows CoreML native allocations under CPU selection or does not reduce measured cost, the provider correction cannot be reported as reducing that cost; if client disconnect still submits further file batches the request cancellation repair is incomplete.

## requirements

| id | requirement | testable in isolation |
|---|---|---|
| R1 | CPU provider selection disables ort automatic device selection; explicit accelerator choices remain available | yes |
| R2 | corrected CPU vectors use a distinct identity and separate profile directory, preserving every legacy database | yes |
| R3 | client disconnection cancels its admitted request and prevents future embedding batches from that request | yes |
| R4 | identical captured inputs produce recorded RSS, CPU time, latency, error counts, and per-input payloads across baseline and corrected process | yes |
| R5 | live daemon runs the built artifact, its memory and user query are observed, and the requested small index finishes without resuming the entire archive export job | yes |

## architecture

Vendor the pinned fastembed 7.5.0 source with its license and provenance; change its common ONNX session builder to select AutoDevicePolicy::PreferCPU before explicit provider registration. This is the shared construction point used by downloaded and local snapshot models; manually configured accelerator providers retain precedence. It avoids an inaccessible custom-provider dispatch constructor and unsafe access to private session internals.

Give CPU embeddings a cpu-explicit identity. Settings::store_config uses a profile subdirectory below each root directory, so the existing legacy index.db is preserved and cannot be reset by opening a different backend. The old executable and databases remain rollback artifacts.

Each connection owns request tasks. Disconnect aborts those tasks; admission workers select between reply.closed() and execute(), cancelling the async index pipeline when its caller disappears. Already admitted native model calls finish their current batch; workers skip queued embedding jobs whose reply receiver has closed. Watcher indexing remains daemon-owned.

Keep the already implemented lazy startup and idle model expiry. Do not change model, truncation length, ranking, or chunking to manufacture a cost gain.

### decisions

| decision | chosen | why not the alternative |
|---|---|---|
| provider seam | patch the pinned fastembed session construction | installed fastembed has no auto-device API and ort custom dispatch creation is private |
| data migration | distinct profile store directory | changing pinned identity in the existing database would delete its derived rows |
| request lifetime | cancel through owned futures and existing oneshot closure notification | detached work survives clients and serializes later requests behind an unwanted archive job |
| rollout | restart and index only the pending owner-message scope plus registered memory notes | re-running the entire export archive would recreate the excessive workload |

## acceptance criteria

Oracles: each correctness check uses an independent source: AC2 compares hand-authored legacy bytes and a literal legacy CPU identity, AC3/AC4 use model-owned channel endpoints and caller-owned futures, and AC5/AC6 compare query paths against independently authored filesystem fixtures. Named incumbents are direct Rust ownership (drop an owning future/model without a daemon), direct filesystem access (write/read the legacy bytes without Settings), and rg --files (enumerate expected paths without semantic search). AC0 runs these incumbents through the exact same lifetime, preserved-bytes and fixture-membership assertions used by AC2–AC6; closed/live channels and absent fixture controls exercise both directions. AC1 runs an existing upstream regression fixture rather than a new correctness claim about policy; AC5 records actual native backend observations. Baseline executable is the preserved pre-change ss/ssd pair under /private/tmp/ss-idle-rollback-20261102. Process cost, backend samples, live executable digest and completion counts are reported as measured observations, not correctness verdicts.

| id | discharges | command | expected |
|---|---|---|---|
| AC0 | R2, R3 | `cargo test -p ss --lib cost_oracle_ -- --nocapture` | 1 test passed, 0 failed; direct-ownership, direct-filesystem and rg-fixture incumbents pass shared assertions and negative controls are rejected |
| AC1 | R1 | `cargo test -p fastembed --lib common::tests::init_session_builder_shares_one_default_environment -- --nocapture` | 1 existing upstream regression test passed, 0 failed; native backend observations are captured by AC5 |
| AC2 | R2 | `cargo test -p ss --lib cost_profile_ -- --nocapture` | 2 tests passed, 0 failed; hand-authored legacy bytes preserved and CPU identity separated from legacy cpu |
| AC3 | R3 | `cargo test -p ss --lib cost_cancel_ -- --nocapture` | 2 tests passed, 0 failed; independent model-channel gates prove admitted request stops and abandoned embedding jobs do not invoke the model |
| AC4 | R3 | `cargo test -p ss --test arena cost_disconnect_ -- --nocapture` | 1 test passed, 0 failed; client disconnect stops future file work and a subsequent scoped index returns authored fixture rows |
| AC5 | R1, R4 | `python specs/search-resource-cost/compare.py` | 2 executable conditions, 2 input sizes, 3 repetitions each; every completed query identifies the authored fixture, rg incumbent accepted, empty control rejected; RSS/CPU/latency/error observations and native backend samples captured per process; corrected CPU heap has 0 CoreML inference-engine objects, baseline has more than 0; 1 separately selected CoreML control produces an actual CoreML native allocation stack and the fixture query payload |
| AC6 | R5 | `python specs/search-resource-cost/verify_live.py` | 1 live executable identity matching the built digest, 2 completed scoped indexes, at least 1 actual query payload identifying a source file, 1 recorded post-idle process sample; legacy databases preserved |

## out of scope

- indexing every archived checkout or changing ranking to reduce inference cost
- publishing dependencies or committing changes without owner authorization

## risks

| risk | likelihood | what it costs | what we do about it |
|---|---|---|---|
| provider changes floating point vectors | expected | incompatible old vector semantics | isolated profile stores with explicit CPU identity |
| native inference cannot be interrupted mid-call | certain | current batch finishes after disconnect | cancel future batches and skip orphaned queued jobs |
| dependency vendoring increases review surface | certain | extra source files | pinned provenance and a small explicit upstream patch |
| cold model reload costs latency | expected | first query after expiry | capture both cold and warm observations; keep configurable idle interval |

## context

- /Users/brianbruggeman/repos/mine/ss/ss/src/embed.rs
- /Users/brianbruggeman/repos/mine/ss/ss/src/daemon.rs
- /Users/brianbruggeman/repos/mine/ss/ss/src/settings.rs
- /Users/brianbruggeman/repos/mine/ss/specs/idle-model-release/provider-source.jsonl
- /Users/brianbruggeman/repos/mine/ss/specs/idle-model-release/allocation-findings.md

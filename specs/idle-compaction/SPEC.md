# idle-compaction

status: audited
implementation: present, uncommitted
evidence: see TASKS.md and command/process artifacts
owner: brian
created: 2026-11-02

## problem

The ss daemon must compact stale cached vector segments and ask the native allocator to reclaim free pages after foreground, watcher, and native embedding work become quiet, with per-pass telemetry and recorded idle RSS plus subsequent real query latency.

## refutation condition

If compaction changes the authored live row/vector payload, starts without a matching quiet activity/native-work snapshot, repeats continuously without intervening work, or the measured allocator pass cannot reduce retained memory on the captured workload, that behavior cannot be reported as idle memory reclamation.

## requirements

| id | requirement | testable in isolation |
|---|---|---|
| R1 | idle vector compaction uses the existing merge primitive, preserves exact live IDs/vector payloads, and skips already compact live lists | yes |
| R2 | idle maintenance starts once per eligible quiet-work epoch after matching activity/native-work snapshots show no active work; new work arriving after the start remains admitted; SS_IDLE_COMPACT_SECS configures the quiet interval and zero disables it | yes |
| R3 | on macOS the pass calls the SDK malloc_zone_pressure_relief API off the async runtime, after segment merge temporaries are dropped, and logs the returned bytes as an allocator hint observation | yes |
| R4 | real-process comparison captures model-drop, compaction and pressure-relief events, before/after RSS and CPU samples, and subsequent query payload/latency; final live daemon runs the built artifact | yes |

## architecture

Reuse vector_cache::compact and native allocator pressure relief; do not evict live rows or clear every cache to manufacture a reduction. Expose an idle compaction entry that selects only multi-segment or tombstoned lists and respects any existing compaction-in-progress guard. Existing swap semantics keep concurrent readers' held views valid.

Track actual request/watcher activity and native embedding calls with owner-scoped atomic counters. The existing idle monitor checks the quiet interval, active work, and native work; it runs one blocking maintenance task per activity epoch. Recheck the captured activity/native epoch inside the blocking task before starting; a changed/busy snapshot skips without consuming the epoch. Work arriving after this start remains admitted and uses immutable views while compaction finishes. No global gate may hold foreground requests behind the merge. A new work epoch makes a later pass eligible. Probe/status requests do not perpetually postpone reclamation. Configuration defaults to a quiet interval after normal model expiry; zero disables maintenance.

The macOS SDK declares malloc_zone_pressure_relief(NULL, 0) as asking all allocator zones for reclaimable pages and returning released bytes. Bind that C function in a small platform module, retaining owned/live allocations. Unsupported platforms return an explicit no-op result. The maintenance task merges stale segments, drops old local snapshots, invokes pressure relief, and emits structured telemetry for the actual pass.

### decisions

| decision | chosen | why not the alternative |
|---|---|---|
| cache shape | existing segment merge | clearing every live cache could add large cold-query latency |
| OS memory | native documented pressure-relief API | dropping Rust values alone left resident allocator pages in earlier process captures |
| scheduling | once after quiet work, off async worker threads | frequent timers would consume idle CPU and block query progress |
| accounting | actual API return plus process RSS | allocator hint bytes and RSS are different accounting views |

## acceptance criteria

Independent oracles: hand-authored live rows/vectors and SQLite rows, direct Rust channel/future ownership for active-work gates, and rg --files for actual fixture membership. The vector-payload predicate is shared between the direct authored-row incumbent and actual maintenance result, and rejects wrong/missing rows. Ownership gates are exercised by actual caller-owned blocking work and overlapping leases in AC2. AC3 uses one fixture-membership assertion for rg output, actual query paths and an empty-path negative control. Native RSS and returned allocator bytes are observations, not row-correctness verdicts.

| id | discharges | command | expected |
|---|---|---|---|
| AC0 | R1 | `cargo test -p ss --lib idle_compaction_oracle_ -- --nocapture` | 1 test passed, 0 failed; direct authored-row incumbent passes the shared vector-payload predicate and a wrong or missing authored row is rejected; filesystem/channel controls are independent ancillary checks |
| AC1 | R1 | `cargo test -p ss --lib idle_compaction_vectors_ -- --nocapture` | 2 tests passed, 0 failed; exact authored live vector IDs/values preserved and already compact live list left intact |
| AC2 | R2 | `cargo test -p ss --lib idle_compaction_gate_ -- --nocapture` | 2 tests passed, 0 failed; actual active ownership prevents the pass, and one quiet epoch permits one pass then new work permits another |
| AC3 | R3, R4 | `python specs/idle-compaction/probe.py` | 2 maintenance settings times 3 process repetitions; 0 fixture misses; actual model-drop/maintenance/API-result telemetry captured, raw RSS/CPU and subsequent query latency recorded; rg incumbent accepted and empty control rejected |
| AC4 | R4 | `python specs/search-resource-cost/verify_live.py` | 1 live executable identity matching built digest, 2 completed scoped indexes, at least 1 real query payload, 1 actual idle maintenance event; legacy database fingerprints preserved |

## out of scope

- replacing the allocator, changing ranking/chunking/model, or claiming defragmentation of live allocations
- clearing persisted indexes or adding periodic maintenance while foreground work is active

## risks

| risk | likelihood | what it costs | what we do about it |
|---|---|---|---|
| allocator cannot return all free pages | expected | some RSS remains | report actual API result and RSS; preserve their distinct meaning |
| compaction briefly needs old and new segments | expected | transient memory | skip compact lists and record peak memory; use existing guarded merge |
| new request arrives during merge | possible | concurrent read pressure | existing immutable snapshots and short swap lock preserve readers; measure following query |
| macOS-specific API | certain | platform-specific implementation | read local SDK signature, isolate FFI, explicit unsupported-platform behavior |

## context

- /Users/brianbruggeman/repos/mine/ss/ss/src/store/vector_cache.rs
- /Users/brianbruggeman/repos/mine/ss/ss/src/daemon.rs
- /Users/brianbruggeman/repos/mine/ss/ss/src/embed.rs
- /Users/brianbruggeman/repos/mine/ss/specs/search-resource-cost/outputs/compare.jsonl
- /Applications/Xcode.app/Contents/Developer/Platforms/MacOSX.platform/Developer/SDKs/MacOSX.sdk/usr/include/malloc/malloc.h

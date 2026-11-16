# idle-model-release -- slices

| # | slice | discharges | validation command | expected | done | note |
|---|---|---|---|---|---|---|
| 1 | model expiry, daemon configuration, lifecycle regression fixtures | AC1, AC2, AC3, AC4, AC5, AC6 | `cargo test -p ss --lib idle_ -- --nocapture` | 8 tests passed, 0 failed | [x] | also run python specs/idle-model-release/probe.py for AC6 |
| 2 | isolate process-global vector-cache cap mutation | AC7 | `cargo test -p ss --test vector_cache -- --nocapture` | 9 tests passed, 0 failed | [x] | cap mutation runs in a child process; the parent verifies its assertions and test count |

## resume

Last landed slice: none; all requested changes remain uncommitted
Next action: none within the requested cost reduction and idle-compaction work
Open question, if any: none

## struck

- none

## recorded artifacts

- final workspace and release commands: `../idle-compaction/logs/final-*`
- corrected CPU comparison: `../search-resource-cost/outputs/compare.jsonl`
- final idle comparison: `../idle-compaction/probe.jsonl:10-16`
- live executable, indexes, queries, compaction, and legacy fingerprints: `../search-resource-cost/live.jsonl`

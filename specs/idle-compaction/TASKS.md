# idle-compaction -- slices

| # | slice | discharges | validation command | expected | done | note |
|---|---|---|---|---|---|---|
| 1 | idle compaction, native pressure relief, ownership gates and fixtures | AC0, AC1, AC2 | `cargo test -p ss --lib idle_compaction_ -- --nocapture` | 5 tests passed, 0 failed | [x] | reuse immutable vector merge and SDK allocator API |
| 2 | real-process reclamation comparison and final live check | AC3, AC4 | `python specs/idle-compaction/probe.py` and `python specs/search-resource-cost/verify_live.py` | 6 isolated process repetitions plus 1 actual live maintenance event | [x] | preserve old indexes and record actual post-compaction query |

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

# search-resource-cost -- slices

| # | slice | discharges | validation command | expected | done | note |
|---|---|---|---|---|---|---|
| 1 | CPU policy and profile storage isolation | AC0, AC1, AC2 | `cargo test -p ss --lib cost_profile_ -- --nocapture` and `cargo test -p ss --lib cost_oracle_ -- --nocapture` and `cargo test -p fastembed --lib common::tests::init_session_builder_shares_one_default_environment -- --nocapture` | 2 profile fixtures, 1 oracle fixture, 1 existing upstream fixture passed | [x] | pinned fastembed policy test also required |
| 2 | request cancellation and embed job abandonment | AC3, AC4 | `cargo test -p ss cost_ -- --nocapture` | 6 application fixtures passed across slices | [x] | channel-owned native calls finish their current batch |
| 3 | same-input measurement and live rollout | AC5, AC6 | `python specs/search-resource-cost/compare.py` and `python specs/search-resource-cost/verify_live.py` | 12 per-condition process repetitions, 2 completed live scoped indexes, 1 query payload | [x] | verify executable digest and preserve legacy databases |

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

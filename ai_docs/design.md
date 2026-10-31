# ss design

`ss` = semantic code search. `ssd` is the shared daemon and admin tool (`ssd serve|status|stop|watch add|rm|list`);
`ss <terms>` and `ss index <path>` are thin clients that start `ssd` on demand. No dependency on ragd.

## assumption: "arena"
One arena = one `ssd` owning one set of indexes. Every client call is a short, stateless request admitted into it:
many connections on one endpoint, requests tagged with an id, reads never block on writes (SQLite WAL, a connection
per operation), bounded FIFO admission with an explicit `busy` past the queue depth, one watch registry for all
callers, a crashed client leaves nothing behind (the request still runs to completion; nothing is cancelled).

## what is mirrored (and what is not there)
- rg-pool at the read revision is a skeleton: every lib is <= 9 lines (`rg-pool-core/src/lib.rs` is a doc only,
  `rg-pool/src/main.rs:1-10` an empty clap parser). No daemon, IPC, watcher or pool exists to copy. Mirrored:
  workspace + lint shape (`rg-pool/Cargo.toml:1-16`), tier gate (`scripts/gate.sh`, `x86_64-unknown-none`), toolchain pin.
- proxima daemon precedent: `tools/proxima-cli/src/daemon.rs:80` (`#[proxima::main(runtime = "tokio")]`) and `:106-120`
  (`Exporter::fan([std, file])` + drain thread: console AND file, never OTLP-only).
- admission: proxima's own `SinkFront<Job>` (bounded FIFO + `FailMode`, the engine under telemetry's `OverflowPolicy`)
  drained by `cap` worker tasks; no ss admission type. `CreditGate` wakes all waiters and has no depth bound. See `backpressure.md`.
  `Pipe` is `pipe/primitives.rs:91`; composition is `PipeExt::and_then` (`pipe/ext.rs:48`).
- runtime: prime has no Unix-socket or named-pipe adapter (`proxima/ai_docs/windows-surface.jsonl`, `net-adapters`), so
  the daemon runs on the tokio fallback; chunker, fusion and codec are runtime-free and tested on prime.

## process model
- State dir `<cache_dir>/ss` (override `SS_HOME`): `ssd.lock`, `ssd.sock` (unix), `state.json` (roots + watches),
  `logs/`, `roots/<blake3(root)[..16]>/index.db` (SQLite: vectors, chunks, FTS5, file table).
- Client: connect; on failure spawn `ssd serve --setsid` detached (stdio null; unix: the child calls `setsid` first;
  Windows: `DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW`), then retry connect (25 ms) until
  `spawn_timeout_ms` (15 s), re-spawning each second since a loser exits at once; one request retry.
- IPC (`ss-ipc`, platform-coupled, isolated so Windows can be cross-checked): unix socket in a 0700 state dir;
  Windows named pipe `\\.\pipe\ss-<fnv1a(home)>` with owner-only DACL `D:P(A;;GA;;;OW)`, remote clients rejected.
- Single instance: exclusive `File::try_lock` on `ssd.lock` for the process life; the loser exits 0.
- Idle exit: no watches AND no request for `idle_secs` (600). `status` and `stop` do not count as activity.

## protocol (`ss-proto`, no_std + alloc, sans-IO)
JSON lines of `Frame<T>{id, body}`. `Request`: `Search{query,path,limit}`, `Index{path}`, `WatchAdd/Rm{path}`,
`WatchList`, `Status`, `Stop`. `Response`: `Hits`, `Indexed`, `Watches`, `Status`, `Ok`, `Error{Busy|NotIndexed|
BadRequest|Internal}`. `LineDecoder::push(&[u8])` / `next_frame()` do the framing; the crate performs no IO.

## pipe decomposition (each stage `Pipe`, error `IndexError`)
- source (`In = ()`): `Walk` -> `Vec<PathBuf>` (`ignore`, gitignore-aware); watcher batches enter as the same list.
- transform `ReadFile`: `PathBuf -> Flow<SourceFile>` (mtime+size, then content hash; skips binary/oversize/vanished).
- transform `Chunk`: `Flow<SourceFile> -> Flow<FileChunks>` (tree-sitter rust/python/ts/js/go, 40-line windows
  otherwise; key = blake3(chunk text)).
- transform `Embed`: `Flow<FileChunks> -> Flow<EmbeddedFile>`: asks the store which keys exist (hash-skip) and embeds
  only unseen ones through `EmbedPool`, itself a pipe `EmbedRequest -> Vec<Vec<f32>>` (fastembed or feature hashing).
- sink-shaped `StoreFile`: `Flow<EmbeddedFile> -> FileReport` (one transaction; the report feeds counters and tests).
- Per file: `ReadFile.and_then(Chunk).and_then(Embed).and_then(StoreFile)`; `Flow::Done` short-circuits unchanged files.
- Query: embed (query lane) + exact cosine scan + FTS5 BM25 -> RRF (k = 60). `Router: Request -> Response` is a pipe;
  Search runs through a `SinkFront` lane (cap 8, depth 64, `drop_newest`), index through another (cap 2, depth 8, `drop_oldest`). The embed pool has `embed_workers` low-priority
  index threads and `embed_query_workers` query threads that never take index work: a worker holds its model for a whole
  index batch (seconds), so a shared "query first" pool still made every search wait for a running batch (measured
  2.1-2.4 s vs 16 ms, `ss/examples/query_load.rs`).

## watch
`ssd watch add` persists to `state.json`, starts the watcher, and its first batch is a full mtime+hash rescan; later
events are debounced (200 ms), filtered (hidden, root `.gitignore`/`.ignore`, state dir) and fed to `index_changed`.
A repeated or nested add is a no-op (the ancestor covers it); a parent absorbs child watches. Watcher error/overflow, or
`rescan_secs` (300) of silence, forces a full rescan: events are hints, the rescan is truth. Backend `native` or `poll`
(`SS_WATCH_BACKEND`): macOS FSEvents was observed late or silent under load, hence the safety rescan and `poll` in most
tests. The walk honors nested `.gitignore` files; the event filter does not.

## telemetry
`ssd serve` installs the default recorder with console (`Exporter::std()`: logs only; `StdSplitPipe` drops spans and
metrics, `proxima-telemetry/src/pipes.rs:1531,1606`) AND a rotating JSON-lines file under `logs/` (spans, logs, metrics).
Stages carry `#[proxima::instrument]`; each index run adds `chunks_seen/embedded/skipped` counters plus one structured
`index run complete` log (the file formatter writes counters without names, `pipes.rs:1988-1991`).

## abandoned
Per-call embedding in the CLI; an embedder trait (it is a pipe); `Mutex<TextEmbedding>` (worker threads own models);
sqlite-vec (exact scan suffices); `CreditGate` and a bespoke `Admit` wrapper for admission. Unverifiable here: Windows runtime behavior.

## embedding profile and store identity
`SS_EMBED_PROVIDER` (`cpu` default, `coreml` = CPU+GPU, `coreml_all` = CPU+GPU+ANE; macOS only, other platforms run the CPU provider)
and `SS_EMBED_MAX_LENGTH` (256) feed `EmbedProfile`. The store pins `bge-small-en-v1.5+<provider>+len<max_length>`
(`EmbedProfile::store_id`); opening a shard under a different id wipes vectors, chunks and files so vectors from two
profiles are never mixed. Measured on the 329-chunk frozen corpus `ss/tests/data/corpus` with 44 gold queries
(`ss/tests/data/queries.tsv`, `ss/examples/embed_eval.rs`): CoreML vectors match CPU to 3.4e-7 max abs and rank identically but the
cold index was 3.3% faster (gpu) or 7.2% slower (all), so CPU stays the default; 128 tokens halves the index time and
lowers hit@3 (hybrid 0.636 -> 0.523), so 256 stays. Prefixing the embedded text with `path: symbol` raised hybrid hit@3
0.636 -> 0.750 but lowered hybrid hit@5 0.795 -> 0.773 (11 of 44 queries ranked worse), so it was not kept.

## embedded text format variants (not adopted)
Rule fixed before running: keep a variant only if hybrid hit@3 or MRR improves and no hybrid or vector-only hit@1, hit@3, hit@5 or MRR drops;
chosen on the dev half (gold chunk start line odd, 26 queries), then required not to regress on the held-out half (even, 18 queries).
CPU, max_length 256, 329-chunk frozen corpus, 44 queries, embedded text only (stored text and FTS stay plain). Hybrid hit@1/hit@3/hit@5/MRR:

| variant | dev | held-out | all 44 |
|---|---|---|---|
| plain (baseline) | .385 .577 .731 .527 | .444 .722 .889 .625 | .409 .636 .795 .567 |
| `path: symbol` before text | .462 .769 .769 .618 pass | .389 .722 .778 .581 fail | .432 .750 .773 .603 fail |
| a: path before text | .462 .731 .769 .613 pass | .278 .667 .778 .523 fail | .386 .705 .773 .576 fail |
| b: symbol before text | .500 .731 .769 .638 pass | .444 .722 .778 .608 fail | .477 .727 .773 .626 fail |
| c: `path: symbol` after text | .346 .577 .692 .507 fail | .611 .778 .944 .736 pass | .455 .659 .795 .601 pass |
| d: `// path` `// symbol` before text | .462 .731 .769 .622 pass | .500 .778 .833 .646 fail | .477 .750 .795 .632 pass |
| e: dir+stem words, symbol words | .462 .731 .808 .621 pass | .444 .722 .722 .576 fail | .455 .727 .773 .603 fail |
| f1: symbol words only | .500 .731 .808 .635 pass | .389 .667 .833 .571 fail | .455 .705 .818 .609 pass |
| f2: file stem + symbol words | .462 .731 .846 .622 pass | .444 .667 .722 .587 fail | .455 .705 .795 .608 pass |

Best dev hybrid MRR among dev-passing variants is b, which drops held-out hybrid hit@5 (.889 -> .778) and MRR, so no variant is adopted and the
embedded text stays the plain chunk. Every dev-passing variant fails on the held-out half and the only held-out passer (c) fails on dev,
so the single-set passes (c, d, f1, f2 on all 44) do not carry across the split. The walker query (`walk files respecting gitignore`,
held-out) moves hybrid 5 -> 2..4 under every variant but c (5) and vector 3 -> 1..2 under every variant.

## rg baseline versus ss (metric definitions, fixed before running)
Harness: `ss/tests/data/rg_baseline.py`. Same 44 queries (`ss/tests/data/queries.tsv`), same frozen corpus, same chunks (those `embed_eval chunks` prints). A rank is the 1-based position of the gold in the ranking; a gold absent from the ranking has rank infinity (hit 0, reciprocal rank 0). hit@k = share of queries with rank <= k; MRR = mean of 1/rank. Splits by the tsv `kind` column (`ident` vs `desc`); counts are asserted (N == 44, every gold chunk present in the chunk list), and the split sizes are whatever the tsv says.
- terms: query lowercased, split on non-alphanumeric/underscore runs, an identifier token (contains `_` or an interior capital) additionally split into its words, stopwords from a fixed list in the script dropped, duplicates removed.
- R1 term-OR: one `rg -i -n --json -e t1 -e t2 ...` over the corpus; each match line maps to the chunk whose line range contains it; chunks rank by number of distinct query terms matched, then total match count, then path/start-line order. Chunks with no match are not ranked.
- R2 identifier-aware: R1 where each term expands to its snake_case, camelCase, PascalCase and joined forms (`walk files` terms `walk`, `files` -> `walk_files|walkFiles|WalkFiles|walkfiles` for adjacent term pairs and for the single words), plus the raw word. A pair form is credited to both of its terms (adjacent terms in the filtered list); a chunk matches a term if any of the term's variants or a pair form containing it matches; the score is the number of distinct original terms matched.
- R3 file-level: files rank by the same scores aggregated over the file (distinct terms matched anywhere in the file, then total matches, then path order), for R1 and R2. The gold counts as a hit at k if its file is in the top k files. ss file-level: the file of each of the top ranked chunks of the pipeline, deduplicated in rank order (from the harness run's top 50 per pipeline; the first rank at which a chunk of the gold file appears).
- ss numbers come from `embed_eval run cpu256 1 <out>` then `report` at HEAD (plain-text embedding, CPU, max_length 256), hybrid, vector-only and FTS-only, with the depth cap of 50.
- wins/losses: queries where rg gold rank <= 3 and ss hybrid rank > 3, and the reverse, listed with both ranks.
- timing: `uptime` recorded before each measurement; rg wall time per query is the median of 20 process runs over the corpus; ss through a running daemon is the wall time of the `ss` client (end to end) with p50/p95; cold start with no daemon and no index; index time with the real model.
Added after the first run (post hoc, not part of the pre-registered arms): R1 and R2 are identical under `-i` substring matching, because a joined or snake form of two terms contains both terms as substrings and so adds no distinct-term credit. R1w and R2w repeat R1 and R2 with every pattern wrapped in `\b...\b` (whole-word match, so `walk_files` no longer credits `walk`), where the identifier forms can change a ranking. The win and loss lists use R1/R2 only. ss file-level metrics from the harness are exact only where the top-5 chunk lists hold enough distinct files; otherwise the script prints a lower and an upper bound, and `rg_baseline.py clihits` recomputes hybrid chunk and file metrics exactly from the `ss --json -n 50` output as a cross-check.

## graph output: measures fixed before running (see `graph.md`)
Corpus `ss/tests/data/corpus` (329 chunks), 44 gold queries, CPU embedder max_length 256, `uptime` recorded before each run. Every count is asserted (N == 44, or N == the sample size); N == 0 is a failure.
- M0 ranking unchanged: `ss/tests/ranks.rs` pins the hybrid top ten of all 44 queries (hash embedder, serial indexing) from before the change; `embed_eval run cpu256` before and after must give identical `hybrid_rank` and `hybrid_top5` for all 44 (real model).
- M1 tokens: ceil(chars/4) of stdout of one `ss` call per gold query at `-n 5` and `-n 10`, for old `--lines`, new default, new `--budget 400`; median and p95 (nearest rank), N = 44 per arm. No-symbol chunks print 2 preview lines.
- M2 answerability: the gold chunk's symbol is its primary node (first non-file, non-module node starting inside the chunk). Structured arm: the top-5 stdout contains the symbol name AND its printed signature AND its doc line (doc counted only for symbols that have one; the count of such symbols is reported). Old arm: the visible first 3 lines of the top-5 hits contain the signature (whitespace-collapsed substring). Reported as counts of 44.
- M3 edge precision: `calls`+`imports` edges of the corpus with resolved in {exact, ambiguous}, 60 sampled uniformly with seed 20261030, each judged by reading the call site (correct = the site invokes/imports a definition among the edge's candidates; for exact, the sole candidate). Precision for exact and ambiguous separately, plus the 10 worst false edges. Languages: only those with real code available; fixtures-only languages are not sampled.
- M4 index cost: `embed_eval run cpu256` x 3 on the corpus at commit 273d569 versus the final tree: cold index seconds, `index.db` bytes.
- M5 `ss graph` latency: 100 sequential warm calls through a running daemon, wall time per client process, p50/p95.
- M6 one-call sufficiency: follow-up targets of a gold query = names of the gold symbol's callees and callers (resolved exact or ambiguous) and container siblings, from the graph. Sufficient = the single response (`-n 5 --budget 800`, and old `--lines -n 5`) contains >= 80% of those names, each on a line that also carries a `file:line`-shaped token. Also reported: median response tokens versus the estimated rg chain = (1 + number of missing targets) rg calls x the median tokens of `rg -n <name>` over the corpus (measured here: no mining report exists), and ss calls versus rg calls per query. Recall is also split by category (callees, callers, siblings). Targets are uncapped even though output lists are capped at 5.
- M7 (separate experiment, only if M0-M6 are done) graph-aware ranking: kept only if hybrid hit@3 or MRR improves and no hybrid or vector-only hit@1/hit@3/hit@5/MRR drops on the 44 queries, chosen on dev (odd gold start line) and not regressing on held-out (even).

## graph output: results (measures above; CPU embedder, 329-chunk frozen corpus, 44 queries)
Harness `ss/examples/graph_eval.rs` (in-process, same render functions as the CLI, paths relative to the corpus root), `graph_rank_eval.rs`, `embed_eval`. `uptime` load averages are listed per run; the machine was shared and loaded (15 to 48), so timings carry that noise.
- M0 ranking: `ss/tests/ranks.rs` (hash embedder) passes on the final tree. Real model: `embed_eval run cpu256` at 273d569 (3 runs) and at the final tree (3 runs): `hybrid_rank`, `vector_rank`, `fts_rank`, `hybrid_top5` and `vector_top5` equal for all 44 queries in all 6 runs (0 differences).
- M1 tokens (ceil(chars/4), N = 44 per arm): -n 5 old `--lines` median 222 p95 274; new default 488 / 673; new `--budget 400` 358 / 392. -n 10 old 449 / 506; new default 1028 / 1295; `--budget 400` 359 / 386. The default is 2.2x the old size. Share of new default -n 5 characters by line kind: header 27.5%, members 30.0%, sig 15.7%, calls 10.8%, tests 5.7%, called by 5.5%, doc 4.7%.
- M2 (gold symbol = primary node of the gold chunk; all 44 chunks have one): name AND signature in the top-5 output: new 35 of 44, old first-3-lines 26 of 44. 35 is the number of queries whose gold chunk is in the hybrid top 5 (hit@5 0.795). Doc line: 20 gold symbols have one; shown by new in 17, by old in 17; new name+signature+doc (doc only where one exists) 35 of 44.
- M6 (`-n 5 --budget 800`, targets uncapped, 645 distinct targets over 44 queries): queries with >= 80% of targets present: new 20 of 44 (name anywhere) and 20 (name on a line with `:line`), old 0 of 44 (names anywhere) and 0 (with a line). Restricted to the 35 queries whose gold symbol is shown: new 19, old 0. Target-level: new has 349 of 645 with a line (362 by name), old 88 by name and 2 with a line. Recall by category, new with a line: callees 62 of 72, callers 45 of 55, container siblings 284 of 563 (siblings are 563 of 645 targets; the list cap is 5). rg size measured here (no mining report existed): `rg -nw <name>` over the corpus, median 121 tokens (117 names; absolute paths included). Median per query: one new response 488 tokens, old response 222, new plus one rg per missing target 854, pure rg chain (1 + all targets) 1452. Per-query table in `graph_eval tokens` output; ss calls are 1 for every query, rg calls after new are the missing targets (median 3, max 20 over 44 queries), pure rg calls are 1 + targets.
- M3 edge precision (Rust corpus, 428 resolved call/import edges: 174 exact calls, 106 exact imports, 148 ambiguous calls; sample 60 uniform, seed 20261030): exact 40 of 40 correct, ambiguous 12 of 20 correct (8 false). All 8 false edges are receiver-unknown method calls whose name is also a method in the index: `.load(Ordering)` -> `Registry::load`/`Settings::load` (daemon.rs:107), `thread::Builder::new()...spawn` -> `EmbedPool::spawn` (embed.rs:192, spawn.rs:26 via `Command::spawn`), `Builder...name` -> `Endpoint::name` (embed.rs:192, telemetry.rs:33, endpoint.rs:62), `terms.is_empty()` -> `Batch::is_empty` (query.rs:41), `units.push` -> `LineDecoder::push` (chunk.rs:130). Only 8 false edges exist in the sample, not 10. Python (real code, the `dotenv` package, 22 files, 28 resolved edges exist so the sample is all 28 rather than 60): exact 25 of 25 correct, ambiguous 3 of 3 correct (a test-local `TC` class defined in three test files; the true one is among the candidates); 32 nodes and 141 edges of all kinds. ts, js and go were not sampled (fixtures only), tsx has no fixture.
- M4: cold index with the real CPU embedder via `embed_eval`: at 273d569 43.2 s (load 31.7), 66.5 s (30.2), 48.7 s (24.4); at the final tree 62.3 s (29.7), 57.2 s (47.7), 30.6 s (36.2); the spread inside each side is larger than the gap between them. Through the CLI (`ss index`, one run each, daemon spawn included): 28.8 s before (load 30.9), 24.0 s after (24.1). `index.db`: 1,937,408 bytes before, 2,465,792 after (+27.3%), 444 nodes and 2,422 edges beside 329 chunks.
- M5 `ss graph` through the warm daemon (client process wall, N = 100 each, load 16.8): `rrf_fuse` p50 6.8 ms p95 8.3 ms; `--depth 2` p50 6.9 / p95 8.1; `new` (many definitions) p50 28.2 / p95 40.9. Reference: structured search p50 22.6 / p95 25.7, `--lines` search p50 20.6 / p95 25.0.
- M7 graph-aware ranking (`SS_GRAPH_BOOST`, off by default; a third RRF list of the chunks in the fused top 20 whose symbol is an exact caller, callee or container of another chunk there): dev hybrid hit@1/3/5/MRR .385/.577/.731/.527 -> .308/.500/.846/.473; held-out .444/.722/.889/.625 -> .389/.833/.944/.606; all 44 .409/.636/.795/.567 -> .341/.636/.886/.528. Vector-only is unchanged by construction (.364/.591/.591/.497). 25 of 44 gold ranks moved. The rule is not met (dev hit@3 and MRR fall, hit@1 falls on all three splits), so the boost is not adopted and stays off.

Two defects found while measuring and fixed: `Store::open` ran `PRAGMA journal_mode=WAL` before `busy_timeout` and sqlite does not run the busy handler for a concurrent WAL switch (16 threads opening a fresh shard failed in round 14 of 20; `store.rs` tests `many_connections_opening_a_fresh_shard_at_once_all_succeed`), and `replace_file` used a deferred transaction: indexing the `dotenv` tree with the hash embedder failed `database is locked` in 2 of 3 runs before taking the write lock up front (`TransactionBehavior::Immediate`) and 0 of 4 after. The regression tests `many_writers_replacing_different_files_at_once_all_commit` and `a_tree_of_many_tiny_files_indexes_without_a_busy_database` pass with either behavior, so they do not reproduce the second failure; its mechanism is not established.

## default output budget and exact search: rule fixed before running
Goal: the default `ss <query>` costs no more than the old `--lines` form and still answers most follow-ups in one call. Corpus `ss/tests/data/corpus` (329 chunks), 44 gold queries, CPU embedder max_length 256, `uptime` recorded per run. Tokens = ceil(chars/4) of stdout, median and p95 (nearest rank), N = 44 per arm; every count is asserted, N == 0 is a failure. Old `--lines` reference (measured above): `-n 5` median 222 p95 274; `-n 10` median 449 p95 506.

Adoption rule for a new default (all three must hold, for the same arm):
1. median tokens at `-n 5` <= 222 and at `-n 10` <= 449; p95 at `-n 5` <= 342 and at `-n 10` <= 632 (old p95 + 25%, floored).
2. share of the 44 queries with >= 80% of the CALLER + CALLEE + TEST follow-up targets of the gold symbol present is >= the same count for the current structured default (`-n 5`, no budget, computed by the same harness first). Targets: names of the gold symbol's callees and callers (resolved exact or ambiguous) and of the tests that call it; container siblings are excluded from this set and reported separately (name anywhere, and name with a `:line`). Present = the name appears on a line that also holds a `:<digit>` token. Denominator = queries whose gold chunk has a primary symbol and >= 1 such target. Measured at `-n 5`.
3. M0: hybrid, vector and FTS ranks identical for all 44 queries (`ss/tests/ranks.rs` plus `embed_eval run cpu256` before and after).

Arms (grid fixed now, all at `-n 5` and `-n 10`; `A0` = current structured default, unbudgeted, verbose):
- (a) greedy-tier budget: tier 1 every hit's header + signature (+ doc), tier 2 calls / called by / tests with `file:line`, tier 3 members as names only (capped 5 then `+N more`, container kinds only), related last; within a tier hits in rank order; an item that does not fit is skipped and later smaller items may still fit. Budgets: flat 200, 250, 300, 400 tokens, and per-hit 40*n and 45*n tokens (200/400 and 225/450 at n = 5/10).
- (b) members of hits below rank 3 move to a last tier after every other section.
- (c) grouped paths: paths relative to the root, hits grouped by file in order of their best rank, the file printed once and `:start-end` per hit.
- arms are every budget x {a, a+b, a+c, a+b+c} = 24, plus A0. The footer `cut: N hits, N links, N members (use --budget T)` is counted inside the budget.
- Choice: among arms that pass 1-3, the highest rule-2 count; ties by sibling-by-name recall, then lower median. If none passes, the rule failed: the best Pareto points (tokens vs rule-2 count) are tabulated and the failure stated.
- `--full` restores the verbose form (A0); `--budget N` overrides the default budget.

Exact search (`ss --exact <regex>`, `ss -F <literal>`): a lexical scan of the files `Walk` lists (the indexer's own walker) with the `regex` crate, line-oriented, no rg process; output grouped by file, at most `--per-file` matches per file (default 5) then `+N more`, flood-capped at the budget by the same tiers (tier 1 the first match of every file, tier 2 further matches), with a `cut:` line inside the budget. Measure: 20 patterns written before running into `ss/tests/data/exact_queries.tsv` (kind: regex or literal; 5 are expected to match > 40 lines in the corpus); per pattern the tokens of `ss --exact` at the default budget versus `rg -n --no-heading <pattern>` run from the corpus root (relative paths, so favourable to rg), median / p95, plus the match-line count of the scan versus `rg -c` summed over files (parity), and cuts. Default budget for exact output = the chosen default budget.

## default output budget and exact search: results (rule above; CPU embedder, 329-chunk corpus, 44 queries)
`uptime` load averages 16.7 to 81.8 (shared machine). Harness `graph_eval arms` (`ss/examples/graph_eval_arms/mod.rs`, in-process, same render functions as the CLI), `ss/tests/data/exact_eval.py`.
- Rule: **no arm met rule 2, so no new default is adopted**; the default of `ss <query>` stays the verbose structured form (nothing was changed there). Old `--lines` reference measured in the same run: `-n 5` 222 / 274, `-n 10` 446 / 506 (the earlier 449 for `-n 10` differs by 3: the real-model hit set varies slightly between runs; the registered 449 is what the rule used).
- Rule 2 baseline (A0, current default, `-n 5`, unbudgeted, 43 queries with a primary symbol and >= 1 caller/callee/test target): **36 of 43** with >= 80% of targets present (128 of 151 target names with a line); siblings 293 of 559 by name, 280 with a line. A0 tokens 488 / 673 (`-n 5`), 1028 / 1295 (`-n 10`).
- Rule 3 (M0): `embed_eval run cpu256` at f63a7f9 versus this tree: `hybrid_rank`, `vector_rank`, `fts_rank`, `hybrid_top5`, `vector_top5` differ for 0 of 44 queries each; `ss/tests/ranks.rs` passes.
- Registered grid (24 arms, `-n 5` median / p95, `-n 10`, rule-2 count of 43): rule 1 met only by flat 200 and per-hit 40n / 45n; their rule-2 counts are 15 to 17 (flat 200 a+c 17, 40n a+c 17, 45n a+c 17; a and a+b 15 or 16). Flat 250: 237 / 242, 26 to 28 (rule 1 fails, median > 222); flat 300: 283 / 291, 32; flat 400: 348 / 392, 34, rule 1 fails. Move (b) changed no number in any arm (members is the last tier and no budget reached it with room to spare); move (c) saved 0 to 5 tokens median and added 0 to 2 sufficiency counts; flat 200 cut 37 hits in total over the 44 `-n 5` responses (31 with (c)), 45n cut 13 (11 with (c)).
- Post hoc arms (chosen after seeing the above, so not part of the pre-registered choice): header without container and score ("lean"), signature and doc only for the top `k` hits (`sig k`), grouped paths, flat budgets 200 to 300. Best points, `-n 5` median / p95, `-n 10` median / p95, rule 2 count of 43, dev (odd gold start, 25) and held-out (even, 18) counts, siblings with a line (of 559), hits cut over 44 queries:

| arm | n5 | n10 | rule 1 | rule 2 | held-out / dev | siblings w/ line | hits cut |
|---|---|---|---|---|---|---|---|
| A0 full | 488 / 673 | 1028 / 1295 | fails | 36 | 16 of 18 / 20 of 25 (36 total) | 280 | 0 |
| 200 a+c (registered) | 187 / 191 | 188 / 192 | meets | 17 | 8 of 18 / 9 of 25 | 66 | 31 |
| 200 lean sig1 c | 175 / 190 | 189 / 192 | meets | 33 | 15 / 18 | 109 | 0 |
| 222 lean sig3 c | 206 / 213 | 211 / 215 | meets | 33 | 15 / 18 | 100 | 0 |
| 240 lean sig1 c | 203 / 227 | 228 / 231 | meets | 34 | 15 / 19 | 145 | 0 |
| 250 lean sig1 c (shipped as `--compact`) | 206 / 240 | 236 / 241 | meets | 34 | 15 / 19 | 156 | 0 |
| 260 lean sig1 c | 208 / 250 | 248 / 253 | meets | 34 | 15 / 19 | 156 | 0 |
No arm reached 36. The two missing queries are not explained; the arms keep at most 5 entries per edge list like A0, and drop the container and score on every hit and the signature on hits 2 to 5. The post hoc points meet rules 1 and 3 and miss rule 2 by 2 (34 vs 36); they are not claimed as passing.
- Shipped: `ss --compact [--budget N]` (default 250) is that 250 post hoc point, opt-in; `ss --budget N` keeps trimming the verbose form by hit order; `--full` was not added because the verbose form is still the default.
- Exact search, 20 patterns of `ss/tests/data/exact_queries.tsv` (9 match > 40 lines in the corpus, 2 match none), default budget 300 (chosen post hoc: no default budget was adopted for the structured form), tokens of stdout: `rg -n --no-heading` median 475 p95 6026 (sum 34088); `ss --exact` default median 285 p95 296 (sum 4485); `--budget 1000` median 384 p95 986. Matching-line counts equal `rg -c` summed over files for 20 of 20 patterns. For the 9 patterns above 40 lines rg median 2883 tokens, ss 295. Every default reply is <= 300 tokens; 12 of 20 carry a `cut:` line (e.g. `fn `: 293 matches, 296 tokens, `cut: 9 files (46 matches), 47 lines (use --budget 600)`). Small answers cost the same or more than rg (`pub fn \w+_files\b` 124 vs 118, `busy_timeout` 33 vs 29) because of the `N matches in F files` line and the `path (N)` header.

## Vector cache version capture: kill criteria (registered before measuring)

An append-only arena plus per-version visibility set replaces the rebuild-per-generation cache only if all hold, measured in release with getrusage CPU time over 1,000 iterations:

1. capturing a version (arena Arc + visibility Arc, everything beyond the scan) is at most 20 us p99
2. end-to-end vector_search CPU at about 19,000 chunks, during a reindex with 32 clients, is at most 10 ms p99
3. a write's publish (append rows + build the visibility set) is at most 2 ms per file at 19,000 live rows
4. visibility set memory is at most 1 bit per row per live version held

If any fails, ship only single-flight rebuild plus the stale bound. No process is spawned on the query path.

## Latency budgets (registered before the first run)

Owner requirement: responses in tens to a few hundred ms. Harness: `ss/examples/latency_budget.rs`, `cargo run --release --example latency_budget -- <embedder> <copies>` (hash default, about 19,000 chunks). The machine is shared (load average 50 to 150), so every row reports wall time and CPU time (getrusage user+sys of the waited-for child processes) separately and labeled; where wall is load noise the gate is CPU time. Exit 0 only if every budget holds; exit 1 lists each failed budget with its number.

| id | scenario | gate |
|---|---|---|
| B1 | idle warm query, 200 sequential `ss` client calls | wall p50 <= 100 ms, wall p99 <= 500 ms, CPU p99 <= 20 ms (CPU is the client process only: the daemon exposes no per-query CPU counter in `ssd status`, so server CPU is not measured) |
| B2 | first query after daemon readiness (stop, start, wait ready, query once) | wall <= 300 ms |
| B3 | first query after a write (touch one file, wait for the index run, query once) | wall <= 300 ms |
| B4 | 32 concurrent clients while 200+ new files index | wall p50 <= 150 ms, wall p99 <= 500 ms, every client returns > 0 hits |
| B5 | cold client call, no daemon (spawn + ready + first query) | hash: wall <= 1,000 ms; fastembed: recorded, not gated (model load included) |

Baseline at 57913b0 is recorded below after the first runs.

### Baseline at 57913b0 (hash embedder, 20 copies = 26,940 chunks, 3 release runs, load average 124 to 141)

| budget | run 1 | run 2 | run 3 | limit |
|---|---|---|---|---|
| B1 p50 / p99 wall ms | 25.5 / 105.8 | 32.4 / 101.1 | 23.4 / 79.7 | 100 / 500 |
| B1 p99 client cpu ms | 8.2 | 9.0 | 8.8 | 20 |
| B2 first query after ready, wall ms | 1367.8 | 1098.0 | 866.4 | 300 |
| B3 first query after write, wall ms | 1231.6 | 782.4 | 686.9 | 300 |
| B4 p50 / p99 wall ms | 13653 / 15413 | 16465 / 16967 | 13170 / 13791 | 150 / 500 |
| B5 cold call, wall ms | 819.8 | 1075.0 | 1216.1 | 1000 |

B1 passes; B2, B3, B4 fail in all runs; B5 passes once of three. CPU per query is small (3.9 to 9.0 ms client), so the failures are wall-clock stalls, not compute.

## Immutable segments replace the generation counter: kill criteria (registered before measuring)

The `gen` meta row and everything keyed on it (vector_cache snapshots, single-flight build, `vector_cache_stale_ms`, `IndexRun` stale serving, prebuild on run end) are replaced by content-addressed immutable segments: a live list `Arc<[Arc<Segment>]>` plus a tombstone set, swapped atomically; a query clones the list pointer (identity is the version, no number). A write commits to SQLite, then publishes one new segment and its tombstones in ONE swap. Ship only if all hold (release builds, getrusage CPU time):

- K1: capturing the list pointer on the query path is at most 5 us p99 CPU (1,000 iterations)
- K2: end-to-end `vector_search` CPU p99 is at most 10 ms on the ~27k-chunk index (20 tagged copies) with 4 concurrent writers appending segments
- K3: publish cost for one file's write (new segment + tombstones + swap) is at most 2 ms p99
- K4: compaction of 32 segments at 27k rows runs off the query path and K2 still holds while it runs
- K5: all 44 gold queries (frozen corpus, `ss/tests/data`) return IDENTICAL ids and order to the SQLite scan, before and after a replace_file of one file and a remove_file

If any fails, report the failing number and commit nothing but this section.

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

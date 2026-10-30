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

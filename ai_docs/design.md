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
- admission: `proxima-primitives/src/pipe/credit_gate.rs:31` `CreditGate` wakes all waiters (`:57`, `notify_waiters`)
  and has no depth bound: not FIFO, cannot answer `busy`. ss adds one wrapper pipe, `Admit`, over a FIFO semaphore.
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
  `Admit` wraps search (cap 8, queue 64) and index (cap 2, queue 8). The embed pool has N low-priority threads and a
  query lane drained first, so an index flood cannot starve queries.

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
sqlite-vec (exact scan suffices); `CreditGate` for admission. Unverifiable here: Windows runtime behavior.

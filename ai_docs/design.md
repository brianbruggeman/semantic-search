# ss design

`ss` = semantic code search. Two binaries, one shared daemon. `ssd` is the daemon and admin tool
(`ssd serve|status|stop|watch add|rm|list <path>`); `ss <terms>` and `ss index <path>` are thin clients.
Distinct from ragd: no dependency on it.

## assumption: "arena"
One arena = one `ssd` owning one set of indexes. Every client call is a short, independent, stateless
request admitted into it. Many connections multiplex on one endpoint, requests carry an id, reads never block
on writes (SQLite WAL, a connection per operation), admission is bounded and FIFO with an explicit `busy`
beyond the queue depth, one watch registry serves all callers, a crashed client leaves nothing behind.

## what is mirrored (and what is not there)
- rg-pool at the read revision is a skeleton: every lib is <= 9 lines (`rg-pool-core/src/lib.rs` is a doc only,
  `rg-pool/src/main.rs:1-10` is an empty clap parser). There is no daemon, IPC, watcher or pool to copy. Mirrored
  from it: workspace + lint shape (`rg-pool/Cargo.toml:1-16`), tier build gate (`scripts/gate.sh`, `x86_64-unknown-none`
  for the tier-1 crates), pinned toolchain (`rust-toolchain.toml`).
- proxima precedent for a daemon binary: `tools/proxima-cli/src/daemon.rs:80` (`#[proxima::main(runtime = "tokio")]`)
  and `:106-120` (`Exporter::fan([std, file])` + a drain thread, i.e. console AND file, never OTLP-only). ss mirrors both.
- admission: `proxima-primitives/src/pipe/credit_gate.rs:31` (`CreditGate`) is a capped gate but wakes all waiters
  (`notify_waiters`, `:57`) and has no depth bound, so it is neither FIFO nor able to answer `busy`. ss adds one
  wrapper pipe, `Admit`, over a FIFO tokio semaphore. `Pipe` is `proxima-primitives/src/pipe/primitives.rs:91`,
  composition is `PipeExt::and_then` (`pipe/ext.rs:48`).
- runtime: prime has no Unix-socket or named-pipe adapter (`proxima/ai_docs/windows-surface.jsonl`, id `net-adapters`),
  so the daemon runs on the tokio fallback. Pure stages (chunker, fusion, codec) are runtime-free and tested on prime.

## process model
- State dir: `<cache_dir>/ss` (override `SS_HOME`): `ssd.lock`, `ssd.sock` (unix), `state.json`, `logs/`,
  `roots/<blake3(root)[..16]>/index.db` (SQLite: vectors, chunks, FTS5, file table).
- Client: connect; on failure spawn `ssd serve` detached (stdio null; unix: the child calls `setsid` first thing;
  Windows: `DETACHED_PROCESS | CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP`), then poll connect with a bounded
  timeout (default 15 s, 25 ms backoff) and retry the request once.
- IPC per OS via `interprocess::local_socket` in crate `ss-ipc` (platform-coupled, isolated so Windows can be
  cross-checked): unix = socket file in a 0700 state dir; Windows = named pipe `\\.\pipe\ss-<blake3(user+home)[..12]>`
  created with an owner-only DACL (`D:P(A;;GA;;;OW)`) and remote clients rejected.
- Single-instance guard: exclusive `File::try_lock` on `ssd.lock` held for process life. The loser exits 0 after
  logging "already running". Unix also unlinks a stale socket only after winning the lock.
- Idle exit: no watches registered AND no client for `idle_secs` (default 600). Registered watches keep it up.

## protocol (crate `ss-proto`, no_std + alloc, sans-IO)
JSON lines. `Frame<T> { id: u64, body: T }`. `Request`: `Search{query, path, limit}`, `Index{path}`,
`WatchAdd{path}`, `WatchRm{path}`, `WatchList`, `Status`, `Stop`. `Response`: `Hits`, `Indexed(stats)`, `Watches`,
`Status`, `Ok`, `Error{kind: Busy|NotIndexed|BadRequest|Internal, message}`. `LineDecoder::push(&[u8])` yields
complete frames; no IO in the crate. Responses may arrive out of order; the id pairs them.

## pipe decomposition (every stage is a `Pipe`, `In -> Result<Out, IndexError>`)
- source (`In = ()`): `Walk` -> `Vec<PathBuf>` (`ignore::WalkBuilder`, gitignore-aware); watcher events enter as
  the same `Vec<PathBuf>`.
- transform `ReadFile`: `PathBuf -> Option<SourceFile>` (`None` = binary/oversize/unreadable, skipped).
- transform `Chunk`: `SourceFile -> FileChunks` (tree-sitter rust/python/ts/js/go, line-window fallback; key =
  blake3(chunk text)).
- transform `Embed`: `FileChunks -> EmbeddedFile`; asks the store which keys exist (hash-skip), embeds only unseen
  ones. The embedder is itself a pipe `Vec<String> -> Vec<Vec<f32>>`: fastembed (real), or a hashing embedder.
- transform-with-effect `Store`: `EmbeddedFile -> FileReport{seen, embedded, skipped}` (sink-shaped; returns the
  report so counters and tests read it).
- Per-file pipeline = `ReadFile.and_then(Chunk).and_then(Embed).and_then(Store)`. The indexer drives it per path and
  deletes vanished files/chunks. Query path: `Embed(query)` + cosine scan + FTS5 BM25 -> RRF (k = 60).
- Admission: `Admit<P>` wraps the search pipe (cap 8, queue 64) and the index pipe (cap 2, queue 8); the embed pool
  has N low-priority threads and two lanes (query lane drained first) so an index flood cannot starve queries.

## watch
`ssd watch add` persists to `state.json`, starts the watcher, then its first batch is a full mtime+hash rescan; later
events are debounced (200 ms), filtered (hidden, root `.gitignore`/`.ignore`, the state dir) and fed to the same
pipeline (`index_changed`). Adding an already-watched or nested path is a no-op (nested paths are covered by the
ancestor; a parent absorbs child watches). Watcher error/overflow, or `rescan_secs` (default 300) of event silence,
triggers a full rescan: events are hints, the rescan is truth. Backend is `native` (OS events) or `poll` (stat
polling) via `SS_WATCH_BACKEND`; macOS FSEvents was observed to deliver late or not at all under load, which is why
the safety rescan exists and why most watch tests use `poll`. Nested `.gitignore` files are honored by the walk
(rescan) but not by the event filter.

## designs abandoned
Per-invocation embedding in the CLI (violates no-heavy-work); a trait for the embedder (it is a pipe); one
`Mutex<TextEmbedding>` (lock where an owner belongs: worker threads own models); sqlite-vec (exact cosine scan is
required and sufficient); `CreditGate` for admission (not FIFO, no shed).

## unverifiable here
Windows runtime behavior: named pipe DACL, DETACHED_PROCESS spawn, `File::try_lock` on Windows.

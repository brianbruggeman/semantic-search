# SCIP: compiler-resolved definitions and references

The code graph (`ss/src/graph/*`) resolves call and import edges by name only. A receiver-unknown method call such as
`.load(Ordering)` therefore links to any local `Registry::load`. SCIP indexes carry the compiler's answer: every
occurrence names the exact symbol it refers to.

## Tables (per-root store, `ss/src/store.rs`)

- `scip_symbols(id, symbol, display_name, kind, signature_doc)`
- `scip_occurrences(file, start_line, start_col, end_line, end_col, symbol_id, role, enclosing_start, enclosing_end)`
- `scip_files(file, content_hash)` with blake3 of the file at the time the index was generated.

`GRAPH_VERSION` is part of the pinned store identity; it moved to 2 so older shards rebuild.

## Freshness policy

SCIP rows describe the source at generation time, not now. A file's SCIP rows are used only while the current blake3 of
the file equals `scip_files.content_hash`. Otherwise the graph falls back to name-resolved edges and tags them `stale`
(SCIP exists, hash differs) or leaves them untagged (no SCIP rows). Files that vanish lose their rows on reload and on
`remove_file`.

## Generation

Never on the query path. Generation is a background step: `rust-analyzer scip . --exclude-vendored-libraries --output
index.scip`, then `ss scip load index.scip`. It is minutes of CPU on a loaded machine, so it is explicit, not automatic.

## Other languages

The loader reads the SCIP protobuf only, so any indexer works unchanged: `scip-typescript`, `scip-python`, `scip-go`
write the same format; symbol strings differ, the tables do not.

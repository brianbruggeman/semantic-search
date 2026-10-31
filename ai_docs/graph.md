# graph: structured search output (design note)

Goal: fewer wasted tokens. One `ss` call should return the symbol, its signature, its first doc line, and the
neighbours an agent would otherwise `rg` for (callers, callees, container members, tests) in one budgeted response.

## nodes and edges
- nodes: `file`, `module` (Rust `mod`), `struct`, `enum`, `trait` (Rust trait, TS interface, Go interface), `type` (class,
  alias), `fn`, `method` (a `fn` inside a type/impl/trait/class, or a Go receiver method), `const`.
- node fields: kind, name, `qpath` (`module::Type::method`; `module` = file stem, or the directory for `mod/lib/main/
  __init__/index` files and for Go), signature (head up to the body, whitespace collapsed, 160 chars), first doc line,
  file, line range, `chunk_key` (key of the chunk whose range holds the node's first line).
- edges: `contains` (file or container -> child), `imports` (file -> imported name), `calls` (symbol -> callee name),
  `implements` (type -> trait/interface), `extends` (class/interface -> base), `tests` (a test symbol -> callee name).
  Edge fields: kind, src (`qpath`, or the file path for imports), dst name as written, qualifier as written, call-site line.

## extraction (tree-sitter node kinds walked per language; no type resolution)
- rust: `function_item`/`function_signature_item`, `struct_item`/`union_item`, `enum_item`, `trait_item`, `type_item`,
  `mod_item`, `const_item`/`static_item`, `impl_item` (`type`, optional `trait` -> implements), `use_declaration` leaves
  (`use_list`, `scoped_use_list`, `use_as_clause`, `use_wildcard`), `call_expression` (`identifier`, `scoped_identifier`,
  `field_expression`, `generic_function`). Doc = first `///` line; test = a preceding `#[..test..]` attribute.
- python: `function_definition`, `class_definition` (`superclasses` -> extends), UPPER_CASE module `assignment` -> const,
  `import_statement`, `import_from_statement`, `call`. Doc = first docstring line; test = name starts `test`.
- ts/tsx/js: function/generator/class/abstract-class/interface/enum/type-alias declarations, `method_definition`,
  `const f = () => ..` bindings, UPPER_CASE consts, `import_statement` clauses, `call_expression`, `new_expression`,
  `class_heritage` (`extends_clause`, `implements_clause`), `extends_type_clause`. Calls inside `it/test/describe`
  callbacks are `tests` edges. Doc = first line of the preceding `/** */` comment.
- go: `function_declaration`, `method_declaration` (receiver type), `type_spec` (struct/interface/other), `const_spec`,
  `import_spec`, `call_expression` (`identifier`, `selector_expression`). `Test*` in `_test.go` are tests. No implements
  edges: interface satisfaction is structural and needs types. Doc = contiguous `//` lines above.
- implemented as a recursive walk over those node kinds, not `Query` S-expressions: one classifier per language returns an
  `Item` and one walker owns scope, so the languages share the qpath, edge-dedup and containment logic.

## honesty rule
`calls`, `imports`, `tests` edges are NAME-RESOLVED ONLY. The row stores the name as written plus its qualifier. At read time
the candidates are the nodes of that name, narrowed by how the call was written: bare `f()` -> fn/struct/type/enum; receiver
`x.f()` (type unknown, qualifier `.`) or `self.f()` -> methods (`self`: same file); path `T::f()`/`mod.f()` -> fn or method
whose `qpath` has the qualifier's last segment. `exact` = one candidate, `ambiguous` = several, `unresolved` = none (std,
other crates, locals). A receiver-unknown call is never `exact`: one definition does not prove it is the target. Text output
lists exact and ambiguous (marked `?`); unresolved edges are not listed, they are counted in `hidden`. Nothing is stored
resolved, so nothing goes stale when another file changes.

## pipe decomposition
- `Extract`: transform `Flow<FileChunks> -> Flow<FileChunks>` between `Chunk` and `Embed`; it reads the file text that
  `Chunk` now keeps, fills `FileChunks.graph`, drops the text. `StoreFile` (the sink) writes graph rows in the same
  transaction as the chunks. `ReadFile` already skips unchanged files by hash, so graph rows are recomputed exactly when
  the content hash changed. Pipeline: `ReadFile.and_then(Chunk).and_then(Extract).and_then(Embed).and_then(StoreFile)`.
- queries are `Router` work like search: `Request::Query`/`Request::Graph` -> `Response::Records`/`Response::Graph`.
- new types and the two questions (pipe? caller can do what before-impossible?): `NodeKind`/`EdgeKind`/`Resolution`
  enums, `GraphNode`/`GraphEdge`/`FileGraph` rows: payload of the `Extract` pipe, not behaviour; callers get symbols and
  relations instead of lines. `Extract` is a pipe. Wire `Symbol`/`EdgeRef`/`Record`/`Facet`/`Related`/`Neighborhood`:
  serde payloads of existing `Response`; callers get one reply with name, signature, doc, neighbours. `render` is free
  functions over those payloads (budget, text, json), no type.

## store schema (per-root SQLite, same `index.db`)
- `nodes(id, file, kind, name, qpath, sig, doc, start_line, end_line, chunk_key BLOB)`, indexes on file, name, chunk_key.
- `edges(id, file, kind, src, dst, qual, line)`, indexes on file, dst, (file, src).
- `replace_file` deletes and reinserts a file's rows in its transaction; `remove_file` deletes them. Edges store dst names,
  so an edge to a symbol that vanished stays a true statement about the source text and reads as `unresolved`; edges whose
  src file vanished are deleted with the file.
- identity: the meta pin becomes `<embed profile id>+graph3`; opening an older index wipes vectors, chunks, files, graph and
  rebuilds instead of mixing.

## output
`ss <q> [<q2> ..] [-q Q ..] [-n N] [--budget T] [--json] [--lines]`: bare words form one query; if any word has a space each
word is a query (facet); `-q` adds facets. One call answers all facets; `related:` lists symbols two hits share. `--lines`
keeps `path:start-end  score` + 3 lines (old `Request::Search`). `ss graph <symbol|path:line> [--depth 1|2] [--json]`.
Caps: 5 per list then `+N more`; `--budget` keeps hits in rank order (rank 1 of every facet first), always the top hit,
and prints `(+N hits cut by --budget T)`; a token is ceil(chars/4) of stdout.
Wire: `Request::Query{queries,path,limit}` -> `Response::Findings`, `Request::Graph{target,path,depth}` -> `Response::Graph`;
old `Search`/`Hits` unchanged; replies carry `schema` (1). `--json` document:
`{schema, facets:[{query, records:[Record]}], related:{items:[{name,path,line,relation,mentions}],more}, cut, budget?}`;
`Record{rank, score, path, start_line, end_line, chunk_key(hex), preview, symbol:{kind,name,qpath,signature,doc,path,
start_line,end_line,chunk_key}|null, container, members|calls|called_by|tests: Capped}`; `Capped{items,more,hidden}`;
`Member{kind,name,signature,path,line}`; `EdgeRef{name,path,line,resolved:exact|ambiguous|unresolved,depth}` (caller: call
site; callee: definition, or the first candidate when ambiguous; tests: the test fn). `ss graph --json`: `{schema,
neighborhoods:{items:[{symbol, members, callers, callees, importers, tests}],more}}`, depth-2 entries have `depth: 2`.

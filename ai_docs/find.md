# `ss find`: one deterministic front door

Goal: the agent should not choose between read, search and `ss`. Evidence: a learned read/search/ss router did not
match agent behaviour on realistic input (AUC 0.39 to 0.48) and 33 of 44 gold queries are "both or neither", so no
learned router. `ss find QUERY...` classifies the query by fixed rules, does the useful combination, and names the
rule in the first output line.

## Pipe question and call sites

No new pipe or runtime type: `find` is pure functions over `exact::scan`, `read::outline_file`, `render::tokens`.
Only the semantic leg and the SCIP graph need a store or daemon, and those live in `bin/ss.rs`, which hands their
results in as plain data (`semantic: &[String]`, `graph: Option<String>`).

- before: the agent picks `rg`, `Read`, `ss outline` or `ss QUERY`, then reads files to find the right one.
- after: `ss find QUERY...` returns at most 5 files, each `path:start-end  purpose`, up to 3 lines, and a `next:`
  line with the exact follow-up (`ss section FILE NAME`, `ss graph SYM`).

## Classification (first rule that matches)

1. `PATH`: contains `/` or ends in a known extension, and the file exists under the root: the `ss outline` output.
2. `IDENT`: one token `[A-Za-z_][A-Za-z0-9_]*`: definitions first (`fn|struct|enum|trait|impl|type|const|class|def|
   function|interface X`, whole word), then other occurrences counted per file; callers/callees from the SCIP store
   when it has rows (tag `resolved-fresh`), else skipped with a note.
3. `REGEX`: any of `\.*+?()[]{}|^$`: the `--exact` scan, grouped by file, most matching lines first.
4. `PHRASE`: the rest. Terms are the R1 method of `tests/data/rg_baseline.py` (stopwords dropped, identifiers split on
   underscore and case, the identifier itself kept); files rank by distinct terms matched (plus the path-name bonus below), then total matches. When
   a daemon is already running (never spawned) its hybrid file ranking is merged with reciprocal rank fusion
   (k = 60) at file level.

Provenance tags per file: `exact`, `term-match`, `semantic`, `resolved-fresh`, `syntactic` (outline only).

## Boundary

A separate component (Clef) decides whether the caller's knowledge is sufficient, that is, whether to acquire evidence
at all. `ss` never decides whether to search, and the model never chooses between `rg`, `Read` and `ss`: `ss find` is
the deterministic evidence acquirer. It gives no tool-choice advice beyond the `next:` follow-up commands.

## Output

Budget 500 tokens (chars / 4, `--budget N`), `--json` gives the same fields. Files are dropped from the tail and
lines from the tail of each file; the top file always prints; `cut:` gives what was dropped. Purpose is the first
`//!` line, markdown heading or Python docstring line within the first 30 lines, else `-`.
The second line is a sufficiency signal a gate can read without more calls: `found: 3 files, 7 matches; definition:
yes (1)` (IDENT only: `no`, `yes (1)` or `ambiguous (N)`), `found: 0` when nothing; the header brackets say whether
results are `exact`, `term-match` and/or `semantic`.
Exit 0 when anything is found, 1 when nothing, 2 on error (bad regex, missing root).

## Abandoned

A learned router (above); scoring phrases by embedding only (needs the daemon, fails cold); spawning the daemon from
`find` (a front door must not start a process).

A query term that names a path component (`queue`, `personas`, `judge`, `reviewer`) marks the file as an owner of that
topic. Terms and components shorter than 4 letters never match (`PATH_MIN`); otherwise one must be a prefix of the other
and their lengths differ by at most 3 (`PATH_SUFFIX`), so `personas` meets `persona` but `orchestrator` does not meet
`orch`. A match scores +3 per term (`PATH_WEIGHT`), a file with no content match still qualifies, and in fusion each term
counts as one extra first place. A term equal to the file stem (`reviewer.md` for `reviewer`) is the answer rather than
a neighbour: it adds a further +9 (`PATH_WEIGHT * STEM_LISTS`, 3 lists) and 3 more first places in fusion. A term
naming more than a quarter of all files (`trial` in a corpus that is mostly `trial/`; limit is `max(1, files / 4)`) is
ignored, since it points at everything. Evidence on a 407-file corpus: path terms off, the top three were quoting files
in `trial/`; on, `improvements/queue/*`, `personas/*` and `trial/judge-report-*` lead. A per-directory cap changed no top
five, so none was added.

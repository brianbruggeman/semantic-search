# Directory map: `ss map [DIR] [--budget N] [--depth D] [--json]`

Goal: models should not have to work hard to find things. From the owner's transcripts the same spec and card docs
are read in full by dozens of threads, the first hop of an episode carries over half of all episode tokens, and a
one-line purpose per file lifted retrieval about as much as changing the embedding model. `ss map` is the structure
first answer: every directory and file of a tree with size and a one-line purpose, inside a token budget, with no
daemon and no index.

## Pipe question and call sites

No new pipe or type with behaviour: `map` is pure functions over text plus one directory walk (`walk::list_files`,
the gitignore-aware walker `ss --exact` also uses). `MapFile`, `MapEntry` and `Map` are plain rows.

- before: `ls -R` or `find`, then `head` of N files: many calls, and the purpose of a file is only known after reading it.
- after: one `ss map DIR` prints every top-level entry first, then expands directories, and says how many files have no purpose.

## Output

```
README.md  166  ~4651  actors draft
trial/  96 files  ~131516
catalog/  10 files  ~15287
trial/blind18/  18 files  ~29302
trial/s-C3.md  74  ~2059  Parallel group 1
catalog/experts.md  24  ~528  expert catalog
cut: 127 entries not shown; --budget N, --depth D, or ss map SUBDIR
purpose coverage: 124 of 146 files have one (84%)
```

- Directory line `dir/  N files  ~T[  purpose]`: N and T count every file beneath it (recursive). File line
  `path  lines  ~tokens[  purpose]`; paths are relative to DIR, tokens are `ceil(bytes / 4)`, no purpose means no text.
- Selection is breadth first. Level 1 prints every top-level entry (each directory and each root file), sorted by
  (readme/index stem first, tokens descending, path). Each further level gives every shown directory one child line,
  then shares the leftover room by directory token mass, the unused quota of one directory carried to the next.
  Children use the same sort. Lines print in tree order: a directory, then what was chosen beneath it.
- Budget default 600 tokens (chars / 4 of the output); the first entry always prints. `cut:` counts the files and
  directories not shown, whether dropped by the budget or by `--depth`.
- `--depth D` is the number of directory levels shown below DIR: 1 = top-level entries only, 2 adds their children.
  Default `max(1, budget / 250)`; an explicit 0 is treated as 1.
- Coverage counts every real purpose over all files, shown or cut, so it does not depend on the budget or depth.
- Skipped: gitignored and hidden entries (`walk::list_files`), files over 1 MiB, files with a NUL byte or invalid
  UTF-8. A directory holding no mappable file (empty, or all skipped) has no entry.

## Purpose rules, in order

1. Cargo.toml `[package]` / pyproject.toml `[project]` `description`.
2. Markdown (`.md`, `.markdown`): first H1 outside code fences; else front matter `description:` or `name:`; else the
   first prose line of at least 4 words that is not a list item, table row, heading, quote, HTML, indented code or fence.
3. Other files: leading `//!` lines (Rust), leading module docstring (Python), leading `/* */` block comment.
4. Other files: the first doc line of the first top-level symbol from the graph extractor (`read::structure`).
5. none.

The text is the first sentence of the first paragraph (cut at `. ` or a blank line), at most 100 chars. A purpose
counts only when it has at least 4 chars and an alphabetic char; bare `1`, `-` or `2024` is no purpose. A directory's
purpose comes only from a readme or index file directly inside it.

## JSON schema (stable)

```
{"root": "DIR",
 "entries": [{"path": "trial", "is_dir": true, "lines": 6264, "files": 96, "tokens": 131516, "purpose": null},
             {"path": "README.md", "is_dir": false, "lines": 166, "files": 1, "tokens": 4651, "purpose": "..." | null}],
 "cut": 127, "coverage": {"with": 124, "total": 146, "percent": 84}}
```

`entries` holds what survived the budget and depth, in the printed order. For a directory `files`, `lines` and
`tokens` are the recursive totals beneath it, so they exceed the entries shown under it; for a file `files` is 1.

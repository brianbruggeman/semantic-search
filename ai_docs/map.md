# Directory map: `ss map [DIR] [--budget N] [--depth D] [--json]`

Goal: models should not have to work hard to find things. From the owner's transcripts the same spec and card docs
are read in full by dozens of threads, the first hop of an episode carries over half of all episode tokens, and a
one-line purpose per file lifted retrieval about as much as changing the embedding model. `ss map` is the structure
first answer: every directory and file of a tree with size and a one-line purpose, inside a token budget, with no
daemon and no index.

## Pipe question and call sites

No new pipe or type with behaviour: `map` is pure functions over text plus one directory walk (`walk::list_files`,
the gitignore-aware walker `ss --exact` also uses). `MapFile`, `MapDir` and `Map` are plain rows.

- before: `ls -R` or `find`, then `head` of N files: many calls, and the purpose of a file is only known after reading it.
- after: one `ss map DIR` prints directories by size, one line per file, and says how many files have no purpose.

## Output

```
src/ (12 files, 3410 tokens)
src/walk.rs  80  ~210  source: () -> Vec<PathBuf>; gitignore-aware listing of regular files under root
...
cut: 31 entries not shown; --budget N, --depth D, or ss map SUBDIR
purpose coverage: 38 of 52 files have one (73%)
```

- Directory line `dir/ (N files, T tokens)`: N and T count the files directly in that directory (the root is `./`).
  Directories are ordered by T descending (ties by path); files inside by tokens descending.
- File line `path  lines  ~tokens  purpose`; path is relative to DIR, tokens are `ceil(bytes / 4)`, purpose is `-`
  when none was found.
- Budget default 600 tokens (chars / 4 of the output); the first directory and file always print. `cut:` counts the
  directory and file lines dropped by the budget or by `--depth`.
- `--depth D` keeps directories with at most D path components below DIR (0 = only files directly in DIR).
- Coverage counts every file within `--depth`, shown or cut, so it does not depend on the budget.
- Skipped: gitignored and hidden entries (`walk::list_files`), files over 1 MiB, files with a NUL byte or invalid UTF-8.

## Purpose rules, in order

1. Cargo.toml `[package]` / pyproject.toml `[project]` `description`.
2. Markdown, reStructuredText, txt: first heading, then `: ` and the first non-empty line under it.
3. Leading `//!` lines (Rust), leading module docstring (Python), leading `/* */` block comment.
4. The first doc line of the first top-level symbol from the graph extractor (`read::structure`).
5. `-`.

The text is the first sentence of the first paragraph (cut at `. ` or a blank line), at most 100 chars.

## JSON schema (stable)

```
{"root": "DIR", "dirs": [{"path": "src", "files": 12, "tokens": 3410}],
 "files": [{"path": "src/walk.rs", "dir": "src", "lines": 80, "tokens": 210, "purpose": "..." | null}],
 "cut": 31, "coverage": {"with": 38, "total": 52, "percent": 73}}
```

`dirs` and `files` hold only what survived the budget and depth, in the printed order. `files` counts in `dirs` are
the shown ones.

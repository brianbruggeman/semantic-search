# History view: `ss show` and `ss diff`

Goal: reduce tokens an agent spends reading `git show` / `git diff`. From the owner's transcripts these are 15% of
all tool-result tokens (`show`: 5,576 calls, 5.66M tokens, median 680, p95 3,164, 14.9% floods; `diff`: 8,714 calls,
4.06M tokens, median 155, p95 2,104).

## Pipe question and call sites

No new pipe or type with behaviour: `history` is pure functions over text, like `render`. The only I/O is
`git` (numstat, patch, `git show <rev>:<path>`), then the graph extractor (`graph::extract_tree`) runs on the old and
new text of each changed supported file.

- before: `git show <rev>` returns the whole patch, N tokens, unbounded.
- after: `ss show <rev>` returns the commit subject, per file `path +A -D`, and the touched symbols
  (`M fn name (+a -d)`, `A fn name`, `D fn name`) in at most 400 tokens, then `ss show <rev> --symbol NAME` returns
  only that symbol's hunks.

## Rules

- Tokens are `ceil(chars / 4)` of the bytes written, same as every other `ss` budget.
- Each changed line (new side for `+`, old side for `-`) is counted against the innermost enclosing symbol; a symbol
  is `A` if absent from the old text, `D` if absent from the new text, else `M`.
- Files ordered by `added + deleted`; unsupported or binary files get a stat-only line.
- Dropped files or symbols end the reply with a `cut:` line naming `--file PATH`, `--symbol NAME`, `--full`.
- Drill-down (`--file`, `--symbol`, `--full`) uses `-U3` context unless `--context N`, default budget 1500 tokens.
- The first file is always listed so a tiny budget still names something.

## Known limits

- Merge commits: `git show` prints a combined diff; the parser reads only `diff --git` sections, so a merge lists
  only its numstat-less stat (usually empty).
- `--no-renames` is forced: a rename is a delete plus an add.
- Untracked files are not in `ss diff`, as with git.

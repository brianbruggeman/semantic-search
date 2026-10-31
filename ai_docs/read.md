# Structure-first reads: `ss outline` and `ss section`

Goal: reduce tokens an agent spends reading whole files. From the owner's transcripts, Read results are 22.3% of all
tool-result tokens; full reads of docs (md/mdx/rst/txt) are 9.45% of all tokens, 66% of them in reads over 8,000
tokens and 65.7% never touched again; full source reads over 1,500 tokens are 2.3% of all, 67% never touched again;
the same spec and cards docs are read in full by many threads.

## Pipe question and call sites

No new pipe or type with behaviour: `read` is pure functions over text, like `history` and `render`. The only I/O is
one `read_to_string`; source symbols come from the graph extractor (`graph::extract_tree`), so the supported languages
are the extractor's (rust, python, ts, tsx, js, go). `Entry` is a plain row (depth, `start-end`, label, name, gist).

- before: `Read file` returns every line, N tokens, unbounded.
- after: `ss outline file` returns the heading tree or symbol tree with `start-end` ranges in at most 400 tokens,
  then `ss section file NAME` (or `--line N`) returns that one range, then a ranged `Read file offset limit`
  can go further.

## Rules

- Tokens are `ceil(chars / 4)` of the bytes written, same as every other `ss` budget.
- Markdown (`md`, `mdx`, `markdown`): ATX headings, fenced code skipped, YAML frontmatter skipped. reStructuredText
  and `txt`: underlined titles, level by first appearance of the adornment character.
- A section runs from its heading to the line before the next heading of the same or higher level, trailing blank
  lines excluded; a parent's range contains its children.
- Outline line: `{indent}{start}-{end} {label} -- {first non-empty line or first doc line, 100 chars}`. Symbol labels
  are `{kind} {signature}` with `[Owner]` before the signature for members.
- Over budget the outline drops the deepest level first (whole levels, so the shape stays), and only at top level
  does it drop trailing entries; the first entry always prints. A `cut:` line gives the number of entries dropped.
  `--depth D` shows entries nested fewer than D levels.
- Any other text file: total line count, first 5 and last 5 lines.
- Section matching: a name equal (case-insensitive) to an entry's name, or to the last `::` segment of a symbol's
  name, wins; otherwise a case-insensitive substring. One match prints; none or several fail (exit 2) with up to 5
  close matches as `start-end label`. `--line N` picks the innermost entry whose range holds N.
- Section output: `path:start-end label`, then `N: text` lines; `--context N` adds N lines each side. Default budget
  1200 tokens; a cut prints `cut:` with the line to resume at.
- `ss -h` prints a block of at most 120 tokens; `ss --help` is clap's full text. In exact mode `-h` is still
  `--no-filename` (rewritten by `grep_argv` before clap sees it).

## Known limits

- Impl blocks are not entries: members show under the position of their first appearance with `[Owner]` and do not
  nest under the type's own line.
- RST overline-and-underline titles are read as underline-only; a `txt` file with ruled lines under short text lines
  yields spurious headings.
- Setext Markdown headings (`Title` over `====`) are not recognized.

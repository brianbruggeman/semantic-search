## Code search with ss

- Run `ss map` once at session start to see the repo layout.
- Before grep, rg, find or Read sweeps, run `ss find "<question>"` for ranked evidence.
- rg flags also work through `ss` (for example `ss -i -l -e pattern`; `-n` is `--limit`, grep line numbers are `--line-number`).
- Use `ss outline <file>` and `ss section <file> <heading-or-symbol>` instead of reading whole files.
- `ss` starts its `ssd` daemon on demand; there is no MCP server.

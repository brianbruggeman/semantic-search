# ss in coding agents

Examples only; not tested against each tool.

## Install

```sh
cargo install --git https://github.com/brianbruggeman/semantic-search ss
```

Installs two binaries: `ss` and `ssd` (`ss/src/bin/ss.rs`, `ss/src/bin/ssd.rs`). The repo is private, so cargo needs git auth (ssh agent or a credential helper; set `CARGO_NET_GIT_FETCH_WITH_CLI=true` to use your git config). `ss` starts `ssd` on demand; `ss map`, `outline`, `section` and `--exact` searches need no daemon. `ss` has no MCP server.

Shared instruction text for every rules-file route below: `examples/agents/ss-instructions.md`.

## Claude Code

Facts from https://code.claude.com/docs/en/hooks.md: PreToolUse stdin carries `tool_name` and `tool_input` (Bash: `tool_input.command`); exit 2 blocks the call and stderr is the reason; SessionStart stdout on exit 0 is added to context. Hooks run in Git Bash on Windows (PowerShell if Git Bash is absent); a hook may set `"shell": "powershell"`. Windows also has a separate `PowerShell` tool; add it to the matcher to cover it.

Copy `examples/hooks/ss-nudge.sh` (or `.ps1`) to `~/.claude/hooks/` (`%USERPROFILE%\.claude\hooks\`), then merge `examples/hooks/settings.example.json` into `~/.claude/settings.json`.

macOS / Linux (`~/.claude/settings.json`):

```json
{
  "hooks": {
    "SessionStart": [{ "hooks": [{ "type": "command", "command": "ss map --budget 300" }] }],
    "PreToolUse": [{
      "matcher": "Grep|Bash",
      "hooks": [{ "type": "command", "command": "sh ~/.claude/hooks/ss-nudge.sh" }]
    }]
  }
}
```

Windows (`%USERPROFILE%\.claude\settings.json`):

```json
{
  "hooks": {
    "SessionStart": [{ "hooks": [{ "type": "command", "command": "ss map --budget 300" }] }],
    "PreToolUse": [{
      "matcher": "Grep|Bash|PowerShell",
      "hooks": [{
        "type": "command",
        "shell": "powershell",
        "command": "& \"$env:USERPROFILE\\.claude\\hooks\\ss-nudge.ps1\""
      }]
    }]
  }
}
```

The leading `&` is required in PowerShell shell form. Exec form also works: `"command": "powershell.exe", "args": ["-NoProfile", "-ExecutionPolicy", "Bypass", "-File", "C:\\Users\\you\\.claude\\hooks\\ss-nudge.ps1"]`. Set `SS_NUDGE_OFF=1` to silence the nudge.

Unverified: Grep `tool_input` field names (the nudge only reads `tool_name`); `%USERPROFILE%` expansion in `command` (not documented, hence `$env:USERPROFILE`); no `pwsh` on the authoring mac, so the `.ps1` was never run. The `.sh` was piped sample payloads on macOS (Grep and `rg` and `find` exit 2, `ls` exit 0, `SS_NUDGE_OFF` exit 0, no `ss` on PATH exit 0).

## Rules-file route

For every agent below without a hook example, install the shared text:

```sh
cp examples/agents/ss-instructions.md <target>      # mac/linux
copy examples\agents\ss-instructions.md <target>    # windows (cmd)
```

If the target exists, append instead (`cat examples/agents/ss-instructions.md >> <target>`). Paths are `~` on mac/linux and `%USERPROFILE%` on Windows unless noted. Project-level files live in the repo root on all OSes.

## Codex CLI

- Global: `~/.codex/AGENTS.md` (`%USERPROFILE%\.codex\AGENTS.md`); project: `AGENTS.md`.
- Route: rules file. Target: `AGENTS.md`.

## opencode

- Global: `~/.config/opencode/AGENTS.md` (`%USERPROFILE%\.config\opencode\AGENTS.md`); project: `AGENTS.md`.
- Plugin hook, `~/.config/opencode/plugin/ss-nudge.js`:

```js
export const SsNudge = async ({ $ }) => ({
  "tool.execute.before": async (input, output) => {
    if (process.env.SS_NUDGE_OFF) return
    const cmd = output.args?.command ?? ""
    if (input.tool === "grep" || /^(rg |grep -r|find )/.test(cmd)) {
      throw new Error('use `ss map` for layout and `ss find "<question>"` for ranked evidence (rg flags also work through `ss`)')
    }
  },
})
```

## openclaw

- Workspace `AGENTS.md` (default workspace `~/.openclaw/workspace/`, `%USERPROFILE%\.openclaw\workspace\`). Route: rules file. Exact paths are from memory.

## Cursor

- Project: `.cursor/rules/ss.mdc`; Windows identical. Needs frontmatter:

```
---
description: Code search with ss
alwaysApply: true
---
```

followed by the shared snippet. Cursor also has hooks (`~/.cursor/hooks.json`) that this doc does not cover.

## Gemini CLI

- Global: `~/.gemini/GEMINI.md` (`%USERPROFILE%\.gemini\GEMINI.md`); project: `GEMINI.md`. Route: rules file.

## GitHub Copilot (CLI and VS Code)

- Project: `.github/copilot-instructions.md` (same on all OSes); Copilot CLI also reads `AGENTS.md`. Route: rules file.

## Windsurf

- Project: `.windsurf/rules/ss.md`; global: `~/.codeium/windsurf/memories/global_rules.md` (`%USERPROFILE%\.codeium\windsurf\memories\global_rules.md`). Route: rules file.

## Cline / Roo / Kilo

- Cline: `.clinerules/ss.md`. Roo Code: `.roo/rules/ss.md`. Kilo Code: `.kilocode/rules/ss.md`. Same on all OSes. Route: rules file.

## Aider

- Copy the snippet to `CONVENTIONS.md` and load it in `.aider.conf.yml` (`~/.aider.conf.yml`, `%USERPROFILE%\.aider.conf.yml`, or repo root):

```yaml
read: CONVENTIONS.md
```

## Zed

- Project: `.rules` in the repo root (also reads `AGENTS.md`). Route: rules file.

## Continue

- Project: `.continue/rules/ss.md`; global: `~/.continue/rules/ss.md` (`%USERPROFILE%\.continue\rules\ss.md`). Route: rules file.

## Amp

- Global: `~/.config/amp/AGENTS.md` (`%USERPROFILE%\.config\amp\AGENTS.md`); project: `AGENTS.md`. Route: rules file.

## Goose

- Project: `.goosehints`; global: `~/.config/goose/.goosehints` (`%APPDATA%\Block\goose\config\.goosehints` on Windows). Route: rules file.

## Unverified

Everything outside the Claude Code section is written from memory, not checked against official docs or run. Hook points that exist but are not covered: Codex, Cursor, Gemini CLI, Copilot, Windsurf, Cline.

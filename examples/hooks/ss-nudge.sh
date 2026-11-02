#!/bin/sh
# PreToolUse hook: nudge Grep and rg/grep -r/find Bash calls toward ss.
# exit 2 blocks the call and shows stderr to Claude; exit 0 lets it run.
[ -n "${SS_NUDGE_OFF:-}" ] && exit 0
command -v ss >/dev/null 2>&1 || exit 0

input=$(cat)
msg='use `ss map` for layout and `ss find "<question>"` for ranked evidence (rg flags also work through `ss`)'

case $input in
  *'"tool_name":"Grep"'* | *'"tool_name": "Grep"'*) echo "$msg" >&2; exit 2 ;;
esac

case $input in
  *'"tool_name":"Bash"'* | *'"tool_name": "Bash"'*) ;;
  *) exit 0 ;;
esac

cmd=$(printf '%s' "$input" | grep -o '"command"[[:space:]]*:[[:space:]]*"\([^"\\]\|\\.\)*' | head -n 1)
cmd=${cmd#*:}
cmd=${cmd#*\"}

case $cmd in
  'rg '* | 'grep -r'* | 'find '*) echo "$msg" >&2; exit 2 ;;
esac
exit 0

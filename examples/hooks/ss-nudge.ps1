# PreToolUse hook: nudge Grep and rg/grep -r/find Bash calls toward ss.
# exit 2 blocks the call and shows stderr to Claude; exit 0 lets it run.
if ($env:SS_NUDGE_OFF) { exit 0 }
if (-not (Get-Command ss -ErrorAction SilentlyContinue)) { exit 0 }

$payload = [Console]::In.ReadToEnd() | ConvertFrom-Json
$msg = 'use `ss map` for layout and `ss find "<question>"` for ranked evidence (rg flags also work through `ss`)'

$hit = $false
if ($payload.tool_name -eq 'Grep') { $hit = $true }
elseif ($payload.tool_name -eq 'Bash') {
    $cmd = [string]$payload.tool_input.command
    if ($cmd -match '^(rg |grep -r|find )') { $hit = $true }
}

if ($hit) {
    [Console]::Error.WriteLine($msg)
    exit 2
}
exit 0

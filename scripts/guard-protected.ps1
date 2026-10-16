# PreToolUse guard for the rust-engineer subagent.
# Reads the hook JSON from stdin. Exit 2 (with a message on stderr) blocks the tool call.
# Blocks Write/Edit on protected files, and PowerShell commands that look like they modify them.

$ErrorActionPreference = 'Stop'

$raw = [Console]::In.ReadToEnd()
if ([string]::IsNullOrWhiteSpace($raw)) { exit 0 }

try { $payload = $raw | ConvertFrom-Json } catch { exit 0 }

$tool = [string]$payload.tool_name
$toolInput = $payload.tool_input

# Protected files, matched against the project-relative path (forward slashes, lower case).
$protectedFilePatterns = @(
    '^cargo\.toml$',                    # workspace root manifest only
    '(^|/)rust-toolchain(\.toml)?$',
    '(^|/)\.?clippy\.toml$',
    '(^|/)plan\.md$',
    '(^|/)tests/architecture(\.rs|/)',  # architecture test
    '(^|/)\.claude/',
    '^scripts/guard-protected\.ps1$'
)

# Names used to spot protected targets inside shell commands.
$protectedCommandNames = @(
    'architecture.rs', 'PLAN.md', 'rust-toolchain', 'clippy.toml', '.claude', 'guard-protected'
)
$rootCargo = '(^|[\s"''\\/])Cargo\.toml'   # refined below: root manifest only

function Get-RelativePath([string]$path) {
    $root = $env:CLAUDE_PROJECT_DIR
    if ([string]::IsNullOrWhiteSpace($root)) { $root = (Get-Location).Path }
    $full = [System.IO.Path]::GetFullPath($(if ([System.IO.Path]::IsPathRooted($path)) { $path } else { Join-Path $root $path }))
    $rootFull = [System.IO.Path]::GetFullPath($root).TrimEnd('\', '/')
    if ($full.StartsWith($rootFull, [System.StringComparison]::OrdinalIgnoreCase)) {
        $full = $full.Substring($rootFull.Length).TrimStart('\', '/')
    }
    return ($full -replace '\\', '/').ToLowerInvariant()
}

function Block([string]$why) {
    [Console]::Error.WriteLine("Blocked by project guard: $why This file is protected for the rust-engineer agent. Stop and ask the user to make or approve this change.")
    exit 2
}

if ($tool -in @('Write', 'Edit', 'NotebookEdit')) {
    $target = [string]$toolInput.file_path
    if ([string]::IsNullOrWhiteSpace($target)) { exit 0 }
    $rel = Get-RelativePath $target
    foreach ($p in $protectedFilePatterns) {
        if ($rel -match $p) { Block "'$rel' matches a protected path." }
    }
    exit 0
}

if ($tool -eq 'PowerShell') {
    $cmd = [string]$toolInput.command
    if ([string]::IsNullOrWhiteSpace($cmd)) { exit 0 }

    $writeVerbs = '(Set-Content|Add-Content|Out-File|Clear-Content|Remove-Item|Move-Item|Copy-Item|Rename-Item|New-Item|WriteAllText|WriteAllLines|AppendAllText|\b(rm|del|erase|mv|move|ren|cp|copy|sc|ac|sed|tee)\b|>>?|git\s+(checkout|restore|reset|stash|apply|rm|mv)|rustup\s+(override|default|toolchain))'
    if ($cmd -notmatch $writeVerbs) { exit 0 }

    foreach ($n in $protectedCommandNames) {
        if ($cmd.IndexOf($n, [System.StringComparison]::OrdinalIgnoreCase) -ge 0) {
            Block "the command appears to modify a protected path ('$n')."
        }
    }
    # Root workspace Cargo.toml: block when the command names a Cargo.toml with no crate directory prefix.
    if ($cmd -match '(^|[\s"''=,;(])(\.[\\/])?Cargo\.toml($|[\s"'',;)])') {
        Block "the command appears to modify the workspace root Cargo.toml."
    }
    exit 0
}

exit 0


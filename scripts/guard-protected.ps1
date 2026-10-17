# PreToolUse guard for the rust-engineer subagent.
# Reads the hook JSON from stdin. Exit 2 (with a message on stderr) blocks the tool call.
#
# Blocks Write/Edit/NotebookEdit on protected files, and PowerShell commands that
# write to them. A protected name only counts when it is used as a PATH argument
# of a write verb or as a redirect target; text after a pipe, inside a commit
# message, or in a read-only command is fine. This is a tripwire for honest
# mistakes by a weaker model, not a sandbox: a determined agent can still build
# a path at runtime. scripts\check.ps1 re-verifies the protected files by hash.

$ErrorActionPreference = 'Stop'

$raw = [Console]::In.ReadToEnd()
# Fail closed: a hook payload we cannot read must not let a tool call through.
if ([string]::IsNullOrWhiteSpace($raw)) {
    [Console]::Error.WriteLine('Blocked by project guard: empty hook input. Stop and ask the user.')
    exit 2
}
try { $payload = $raw | ConvertFrom-Json } catch {
    [Console]::Error.WriteLine('Blocked by project guard: unparseable hook input. Stop and ask the user.')
    exit 2
}
if ($null -eq $payload -or [string]::IsNullOrWhiteSpace([string]$payload.tool_name)) {
    [Console]::Error.WriteLine('Blocked by project guard: hook input has no tool_name. Stop and ask the user.')
    exit 2
}

$tool = [string]$payload.tool_name
$toolInput = $payload.tool_input

# Protected files, matched against the project-relative path (forward slashes, lower case).
$protectedFilePatterns = @(
    '^cargo\.toml$',                         # workspace root manifest only (exact path)
    '(^|/)rust-toolchain(\.toml)?$',
    '(^|/)\.?clippy\.toml$',
    '(^|/)plan\.md$',
    '(^|/)tests/architecture(\.rs|/)',       # architecture test
    '(^|/)\.claude(/|$)',
    '^scripts/(guard-protected|update-protected|test-guard)\.ps1$',
    '^scripts/check\.ps1$',
    '^scripts/protected\.sha256$',
    '(^|/)\.cargo(/|$)',
    '(^|/)\.gitattributes$'
)

# Concrete protected paths, used to evaluate wildcards and directory removals.
$protectedConcrete = @(
    'cargo.toml', 'rust-toolchain.toml', 'rag-core/clippy.toml', 'plan.md',
    'rag-core/tests/architecture.rs', 'scripts/check.ps1', 'scripts/guard-protected.ps1',
    'scripts/update-protected.ps1', 'scripts/test-guard.ps1', 'scripts/protected.sha256',
    '.cargo/config.toml', '.gitattributes',
    '.claude/settings.json', '.claude/settings.local.json', '.claude/agents/rust-engineer.md'
)
$testDirs = @('rag-core/tests', 'rag-infra/tests', 'rag-cli/tests')

$removeVerbs = @('remove-item', 'ri', 'rm', 'rmdir', 'rd', 'del', 'erase')
$writeVerbs = $removeVerbs + @(
    'set-content', 'sc', 'add-content', 'ac', 'out-file', 'clear-content', 'clc',
    'move-item', 'mi', 'mv', 'move', 'copy-item', 'cpi', 'cp', 'copy', 'cpy',
    'rename-item', 'rni', 'ren', 'new-item', 'ni', 'set-item', 'si', 'set-itemproperty',
    'sv', 'set-variable', 'tee', 'tee-object', 'sed', 'export-csv', 'export-clixml',
    'expand-archive', 'compress-archive', 'set-acl', 'attrib', 'icacls', 'takeown',
    'xcopy', 'robocopy', 'invoke-webrequest', 'iwr', 'invoke-restmethod', 'irm', 'curl', 'wget'
)

function Block([string]$why) {
    [Console]::Error.WriteLine("Blocked by project guard: $why This is protected for the rust-engineer agent. Stop and ask the user to make or approve this change.")
    exit 2
}

# --- path resolution -------------------------------------------------------

function Get-LongPath([string]$full) {
    # Expands 8.3 short names (PLAN~1.MD) by walking the existing path.
    if ($full -notmatch '~') { return $full }
    $parts = $full -split '[\\/]'
    $acc = $parts[0] + '\'
    for ($i = 1; $i -lt $parts.Count; $i++) {
        if ($parts[$i] -eq '') { continue }
        $next = $null
        try {
            $next = [System.IO.Directory]::GetFileSystemEntries($acc, $parts[$i]) | Select-Object -First 1
        } catch { $next = $null }
        if ($null -eq $next) { $acc = Join-Path $acc ($parts[$i..($parts.Count - 1)] -join '\'); break }
        $acc = $next
    }
    return $acc.TrimEnd('\')
}

$rootRaw = $env:CLAUDE_PROJECT_DIR
if ([string]::IsNullOrWhiteSpace($rootRaw)) { $rootRaw = (Get-Location).Path }
$rootFull = Get-LongPath ([System.IO.Path]::GetFullPath($rootRaw).TrimEnd('\', '/'))

# Returns @{ Status = 'ok'|'outside'|'bad'; Rel = '<lower/forward/slash relative path>' }.
function Resolve-Rel([string]$path, [string]$base) {
    $p = $path.Trim()
    if ($p -match '^[\\/]{2}[?.][\\/]') { return @{ Status = 'bad'; Rel = '' } }   # \\?\ and \\.\ forms
    $p = $p -replace '(?<!^[A-Za-z]):[^\\/]*', ''                                     # NTFS alternate data streams
    try {
        if ([System.IO.Path]::IsPathRooted($p)) { $full = [System.IO.Path]::GetFullPath($p) }
        else { $full = [System.IO.Path]::GetFullPath((Join-Path $base $p)) }
    } catch { return @{ Status = 'bad'; Rel = '' } }
    $full = Get-LongPath $full
    if (Test-Path -LiteralPath $full) {
        $item = Get-Item -LiteralPath $full -Force -ErrorAction SilentlyContinue
        if ($null -ne $item -and $item.LinkType -and $item.Target) {
            try { $full = Get-LongPath ([System.IO.Path]::GetFullPath([string]@($item.Target)[0])) } catch { }
        }
    }
    $full = $full.TrimEnd('\', '/')
    if ($full.Equals($rootFull, [System.StringComparison]::OrdinalIgnoreCase)) {
        return @{ Status = 'ok'; Rel = '' }
    }
    if ($full.StartsWith($rootFull + '\', [System.StringComparison]::OrdinalIgnoreCase)) {
        $rel = $full.Substring($rootFull.Length + 1) -replace '\\', '/'
        return @{ Status = 'ok'; Rel = $rel.ToLowerInvariant() }
    }
    return @{ Status = 'outside'; Rel = '' }
}

function Test-ProtectedRel([string]$rel) {
    foreach ($pat in $protectedFilePatterns) { if ($rel -match $pat) { return $true } }
    return $false
}

function Test-RemoveBlockedRel([string]$rel) {
    if (Test-ProtectedRel $rel) { return $true }
    if ($rel -match '^(rag-core|rag-infra|rag-cli)/tests(/|$)') { return $true }
    # Removing an ancestor directory removes protected content (root, rag-core, scripts, .claude, tests).
    foreach ($c in ($protectedConcrete + $testDirs)) {
        if ($rel -eq '' -or $c.StartsWith($rel + '/')) { return $true }
    }
    return $false
}

function Test-WildcardBlocked([string]$relPattern, [bool]$isRemove) {
    foreach ($c in $protectedConcrete) { if ($c -like $relPattern) { return $true } }
    if ($isRemove) {
        foreach ($c in ($testDirs + @('rag-core/tests/x.rs', 'rag-infra/tests/x.rs', 'rag-cli/tests/x.rs'))) {
            if ($c -like $relPattern) { return $true }
        }
        foreach ($d in @('rag-core', 'rag-infra', 'rag-cli', 'scripts', '.claude', '.cargo')) {
            if ($d -like $relPattern) { return $true }
        }
    }
    return $false
}

# Checks one path-ish token. Blocks (exits) when it is a protected target.
function Test-Target([string]$token, [string]$cwd, [bool]$isRemove, [string]$context) {
    $t = $token.Trim().Trim('"', "'")
    if ($t -match '^-[A-Za-z]+:(.+)$') { $t = $Matches[1] }
    if ($t -eq '' -or $t.StartsWith('-')) { return }
    $t = $t -replace '(?<!^[A-Za-z]):[^\\/]*', ''   # drop alternate data stream suffixes before the `$` skip below
    $t = $t.Replace('$env:CLAUDE_PROJECT_DIR', $rootFull).Replace('${env:CLAUDE_PROJECT_DIR}', $rootFull)
    $t = $t -replace '^\$PWD', $cwd
    if ($t.Contains('$')) { return }
    $isWild = $t -match '[*?]'
    $r = Resolve-Rel $t $cwd
    if ($r.Status -eq 'bad') { Block "$context uses an unsupported path form ('$t')." }
    if ($r.Status -eq 'outside') { return }
    if ($isWild) {
        if (Test-WildcardBlocked $r.Rel $isRemove) { Block "$context uses a wildcard ('$t') that covers protected files." }
        return
    }
    if ($isRemove) {
        if (Test-RemoveBlockedRel $r.Rel) { Block "$context would delete a protected path or a tests directory ('$t')." }
    } elseif (Test-ProtectedRel $r.Rel) {
        Block "$context targets a protected path ('$t')."
    }
}

# --- file tools ------------------------------------------------------------

if ($tool -in @('Write', 'Edit', 'NotebookEdit')) {
    $target = [string]$toolInput.file_path
    if ([string]::IsNullOrWhiteSpace($target)) { $target = [string]$toolInput.notebook_path }
    if ([string]::IsNullOrWhiteSpace($target)) { $target = [string]$toolInput.path }
    if ([string]::IsNullOrWhiteSpace($target)) { exit 0 }
    $r = Resolve-Rel $target $rootFull
    if ($r.Status -eq 'bad') { Block "'$target' uses an unsupported path form." }
    if ($r.Status -eq 'ok' -and (Test-ProtectedRel $r.Rel)) { Block "'$($r.Rel)' matches a protected path." }
    exit 0
}

# --- PowerShell tool -------------------------------------------------------

function Invoke-CommandCheck([string]$cmd, [string]$cwd, [int]$depth) {
    if ($depth -gt 3) { Block 'nested shell invocations are too deep to inspect.' }

    # Normalise: drop backtick escapes, here-strings, commit/value strings, harmless redirects, comments.
    $c = $cmd -replace '`', ''
    $c = [regex]::Replace($c, "@'[\s\S]*?'@", "@''")
    $c = [regex]::Replace($c, '@"[\s\S]*?"@', '@""')
    $c = [regex]::Replace($c, '(?i)(\s)(-m|-am|--message|-message|-value|-encoding)(\s+|:)("(?:[^"\\]|\\.)*"|''[^'']*'')', '$1$2 ""')
    $c = [regex]::Replace($c, '(?i)(\*|\d)?>>?\s*(&\d|\$null\b|nul\b)', ' ')
    $c = [regex]::Replace($c, '(?m)(^|\s)#[^\r\n]*', '$1')
    $c = $c -replace '&&|\|\|', ';' -replace '[{}]', ';'

    $segments = $c -split '[;|\r\n]+'
    foreach ($seg in $segments) {
        $s = $seg.Trim()
        if ($s -eq '') { continue }

        # Redirect targets (> and >>, with optional stream number).
        foreach ($m in [regex]::Matches($s, '(?<![-=<])\d?>>?\s*([^\s;|&<>]+)')) {
            Test-Target $m.Groups[1].Value $cwd $false 'a redirect'
        }

        $tokens = @($s -split '[\s,()=;"''@]+' | Where-Object { $_ -ne '' })
        if ($tokens.Count -eq 0) { continue }

        # Strip call operators to find the command word.
        $i = 0
        while ($i -lt $tokens.Count -and $tokens[$i] -in @('&', '.')) { $i++ }
        $word = if ($i -lt $tokens.Count) { $tokens[$i] } else { '' }
        $lowerWord = $word.ToLowerInvariant()

        # Directory changes update the simulated cwd.
        if ($lowerWord -in @('cd', 'chdir', 'sl', 'set-location', 'pushd', 'push-location', 'popd', 'pop-location')) {
            $arg = $tokens | Select-Object -Skip ($i + 1) | Where-Object { $_ -notmatch '^-(Path|LiteralPath|PassThru)?$' } | Select-Object -First 1
            if ($null -eq $arg -or $arg.Contains('$') -or $arg -eq '-' -or $lowerWord -in @('popd', 'pop-location')) { $cwd = $rootFull }
            else {
                $d = Resolve-Rel $arg.Trim('"', "'") $cwd
                if ($d.Status -eq 'ok') { $cwd = if ($d.Rel -eq '') { $rootFull } else { Join-Path $rootFull ($d.Rel -replace '/', '\') } }
                else { $cwd = $rootFull }
            }
            continue
        }

        # Dynamic code and script execution (check.ps1 is the only allowed script).
        if ($s -match '(?i)(^|[^\w-])(invoke-expression|iex)(\s|$)') { Block 'Invoke-Expression/iex runs code the guard cannot inspect.' }
        if ($s -match '(?i)-(encodedcommand|enc|ec)\b') { Block 'encoded commands cannot be inspected.' }
        if ($s -match '^\s*&\s*[$(]') { Block 'the call operator on a variable or expression runs code the guard cannot inspect.' }

        if ($lowerWord -match '^(powershell|pwsh)(\.exe)?$' -or $lowerWord -match '^(cmd)(\.exe)?$') {
            $idx = 0
            for ($k = $i + 1; $k -lt $tokens.Count; $k++) {
                if ($tokens[$k] -match '(?i)^-(file|f)$' -and $k + 1 -lt $tokens.Count) { $script = $tokens[$k + 1]; $idx = $k + 1; break }
                if ($tokens[$k] -match '(?i)^(-command|-c|/c|/k)$') {
                    $rest = ($tokens | Select-Object -Skip ($k + 1)) -join ' '
                    Invoke-CommandCheck $rest $cwd ($depth + 1)
                    $idx = -1; break
                }
            }
            if ($idx -gt 0) { Assert-ScriptAllowed $script $cwd }
            continue
        }
        if ($word -match '(?i)\.(ps1|bat|cmd)$') { Assert-ScriptAllowed $word $cwd; continue }

        # git subcommands.
        if ($lowerWord -in @('git', 'git.exe')) {
            $sub = $null
            for ($k = $i + 1; $k -lt $tokens.Count; $k++) {
                if ($tokens[$k] -in @('-C', '-c')) { $k++; continue }
                if ($tokens[$k].StartsWith('-')) { continue }
                $sub = $tokens[$k].ToLowerInvariant(); break
            }
            if ($sub -in @('apply', 'am', 'restore', 'stash')) { Block "'git $sub' can overwrite protected files." }
            if ($sub -eq 'reset' -and $s -match '(?i)--hard') { Block "'git reset --hard' can overwrite protected files." }
            if ($sub -eq 'checkout') {
                if ($s -match '\s--(\s|$)' -or $tokens -contains '.') { Block "'git checkout -- <path>' can overwrite protected files." }
                foreach ($tk in ($tokens | Select-Object -Skip ($i + 2))) { Test-Target $tk $cwd $false 'git checkout' }
            }
            if ($sub -in @('rm', 'mv')) {
                foreach ($tk in ($tokens | Select-Object -Skip ($i + 2))) { Test-Target $tk $cwd ($sub -eq 'rm') "git $sub" }
            }
            continue
        }
        if ($lowerWord -eq 'rustup' -and ($tokens | Select-Object -Skip ($i + 1) -First 1) -in @('override', 'default', 'toolchain', 'self', 'update')) {
            Block 'changing the Rust toolchain is not allowed.'
        }

        # Write verbs: every path-like argument is a potential target.
        $hasVerb = $false
        $isRemove = $false
        foreach ($tk in $tokens) {
            $l = $tk.ToLowerInvariant()
            if ($l -in $writeVerbs) { $hasVerb = $true }
            if ($l -in $removeVerbs) { $isRemove = $true }
        }
        if ($s -match '(?i)::(WriteAll\w*|AppendAll\w*|Copy|Move|Delete|Create\w*|Replace|Open\w*|SetAttributes)\b|StreamWriter|FileStream') {
            $hasVerb = $true
            if ($s -match '(?i)::Delete\b') { $isRemove = $true }
        }
        if ($hasVerb) {
            foreach ($tk in $tokens) {
                if ($tk.ToLowerInvariant() -in $writeVerbs) { continue }
                Test-Target $tk $cwd $isRemove 'the command'
            }
        }
    }
}

function Assert-ScriptAllowed([string]$script, [string]$cwd) {
    $r = Resolve-Rel $script.Trim('"', "'") $cwd
    if ($r.Status -eq 'ok' -and $r.Rel -eq 'scripts/check.ps1') { return }
    Block "running a script file ('$script') is not allowed; only scripts\check.ps1 may be run."
}

if ($tool -eq 'PowerShell') {
    $cmd = [string]$toolInput.command
    if ([string]::IsNullOrWhiteSpace($cmd)) { exit 0 }
    $cwd = [string]$payload.cwd
    if ([string]::IsNullOrWhiteSpace($cwd)) { $cwd = $rootFull }
    Invoke-CommandCheck $cmd $cwd 0
}

exit 0

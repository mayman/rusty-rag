# Quality gate for rag. Run from anywhere: scripts\check.ps1
# Verifies protected files against scripts\protected.sha256, checks lint opt-in,
# then runs test, fmt --check, and clippy -D warnings. Exits non-zero if any step fails.

# Native tools write progress to stderr; that must not abort the gate.
$ErrorActionPreference = 'Continue'

# PATH: cargo\bin first, then the existing process PATH, then the registry values
# (shells can hold a stale PATH, but we never drop what the caller already had).
$seen = @{}
$entries = @()
$sources = @((Join-Path $env:USERPROFILE '.cargo\bin'), $env:Path,
    [Environment]::GetEnvironmentVariable('Path', 'Machine'), [Environment]::GetEnvironmentVariable('Path', 'User'))
foreach ($source in $sources) {
    foreach ($dir in ([Environment]::ExpandEnvironmentVariables([string]$source) -split ';')) {
        $d = $dir.Trim()
        if ($d -ne '' -and -not $seen.ContainsKey($d.TrimEnd('\').ToLowerInvariant())) {
            $seen[$d.TrimEnd('\').ToLowerInvariant()] = $true
            $entries += $d
        }
    }
}
$env:Path = $entries -join ';'

if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    Write-Host 'FATAL: cargo was not found on PATH or in %USERPROFILE%\.cargo\bin.'
    Write-Host 'Install the Rust toolchain (rustup) and the MSVC Build Tools, then retry. This script installs nothing.'
    exit 127
}

$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

# Minimum number of tests the architecture test binary must report as passed.
$minArchitectureTests = 15

$results = @()
function Add-Result([string]$name, [int]$code) { $script:results += [pscustomobject]@{ Name = $name; Code = $code } }

function Get-LfHash([string]$path) {
    $latin1 = [System.Text.Encoding]::GetEncoding(28591)
    $text = $latin1.GetString([System.IO.File]::ReadAllBytes($path)).Replace("`r`n", "`n")
    $sha = [System.Security.Cryptography.SHA256]::Create()
    try { return ([BitConverter]::ToString($sha.ComputeHash($latin1.GetBytes($text))) -replace '-', '').ToLowerInvariant() }
    finally { $sha.Dispose() }
}

# --- protected files ---------------------------------------------------------
Write-Host ''
Write-Host '=== protected files'
$requiredProtected = @('Cargo.toml', 'rust-toolchain.toml', 'rag-core/clippy.toml', 'rag-core/tests/architecture.rs',
    'scripts/check.ps1', 'scripts/guard-protected.ps1', '.gitattributes')
$problems = @()
$manifestPath = Join-Path $PSScriptRoot 'protected.sha256'
if (-not (Test-Path -LiteralPath $manifestPath)) {
    $problems += 'scripts/protected.sha256 is missing: run scripts/update-protected.ps1 after the user approves'
} else {
    $expected = @{}
    foreach ($line in (Get-Content -LiteralPath $manifestPath)) {
        if ($line -match '^([0-9a-f]{64})\s+(\S+)$') { $expected[$Matches[2]] = $Matches[1] }
    }
    $mustHave = @($requiredProtected)
    if (Test-Path -LiteralPath (Join-Path $root '.cargo\config.toml')) { $mustHave += '.cargo/config.toml' }
    foreach ($rel in $mustHave) {
        if (-not $expected.ContainsKey($rel)) { $problems += "protected file not in manifest: $rel (run scripts/update-protected.ps1 after the user approves)"; continue }
        $full = Join-Path $root ($rel -replace '/', '\')
        if (-not (Test-Path -LiteralPath $full)) { $problems += "protected file missing: $rel"; continue }
        if ((Get-LfHash $full) -ne $expected[$rel]) {
            $problems += "protected file changed: $rel - run scripts/update-protected.ps1 after the user approves"
        }
    }
}
$problems | ForEach-Object { Write-Host "  $_" }
Add-Result 'protected' $(if ($problems.Count -eq 0) { 0 } else { 1 })

# --- lint opt-in -------------------------------------------------------------
Write-Host ''
Write-Host '=== lints opt-in and lint-allow scan'
$problems = @()
foreach ($crate in 'rag-core', 'rag-infra', 'rag-cli') {
    $manifest = Join-Path $root "$crate\Cargo.toml"
    $text = if (Test-Path -LiteralPath $manifest) { Get-Content -LiteralPath $manifest -Raw } else { '' }
    $optIn = ($text -match '(?ms)^\[lints\]\s*\r?\n(?:(?!\[)[^\r\n]*\r?\n)*?\s*workspace\s*=\s*true') -or ($text -match '(?m)^lints\.workspace\s*=\s*true')
    if (-not $optIn) { $problems += "$crate/Cargo.toml lacks a [lints] section with workspace = true" }
}
foreach ($crate in 'rag-core', 'rag-infra') {
    $hits = Get-ChildItem -LiteralPath (Join-Path $root "$crate\src") -Recurse -Filter *.rs -ErrorAction SilentlyContinue |
        Select-String -Pattern 'allow\([^)]*(print_stdout|print_stderr|clippy::exit|unsafe_code)'
    foreach ($h in $hits) { $problems += "$($h.Path):$($h.LineNumber) relaxes a workspace deny lint" }
}
$problems | ForEach-Object { Write-Host "  $_" }
Add-Result 'lints' $(if ($problems.Count -eq 0) { 0 } else { 1 })

# --- deleted tests -----------------------------------------------------------
Write-Host ''
Write-Host '=== deleted tests (git)'
$problems = @()
if (Get-Command git -ErrorAction SilentlyContinue) {
    & git rev-parse --verify --quiet HEAD *> $null
    if ($LASTEXITCODE -eq 0) {
        $deleted = & git diff --name-only --diff-filter=D HEAD 2>$null
        foreach ($f in $deleted) { if ($f -match '(^|/)tests/') { $problems += "deleted test file: $f" } }
    } else {
        Write-Host '  no commits yet; deleted-test check skipped'
    }
} else {
    Write-Host '  git not available; deleted-test check skipped'
}
$problems | ForEach-Object { Write-Host "  $_" }
Add-Result 'git-tests' $(if ($problems.Count -eq 0) { 0 } else { 1 })

# --- cargo steps -------------------------------------------------------------
$steps = @(
    @{ Name = 'test';   Args = @('test', '--workspace', '--all-features', '--locked') },
    @{ Name = 'fmt';    Args = @('fmt', '--all', '--', '--check') },
    @{ Name = 'clippy'; Args = @('clippy', '--workspace', '--all-targets', '--all-features', '--locked', '--', '-D', 'warnings') }
)

foreach ($step in $steps) {
    Write-Host ''
    Write-Host ('=== cargo ' + ($step.Args -join ' '))
    $lines = & cargo @($step.Args) 2>&1 | ForEach-Object { $line = [string]$_; Write-Host $line; $line }
    $code = $LASTEXITCODE
    Write-Host ('=== exit code: ' + $code)
    Add-Result $step.Name $code

    if ($step.Name -eq 'test') {
        # The architecture test must really have run, and passed, with real tests in it.
        $passed = 0
        $ran = $false
        for ($i = 0; $i -lt $lines.Count; $i++) {
            if ($lines[$i] -match 'Running\s+tests[\\/]architecture\.rs') {
                for ($j = $i + 1; $j -lt $lines.Count -and $j -lt $i + 200; $j++) {
                    if ($lines[$j] -cmatch 'Running\s') { break }
                    if ($lines[$j] -match 'test result: ok\. (\d+) passed') { $ran = $true; $passed = [int]$Matches[1]; break }
                }
            }
        }
        $archCode = if ($ran -and $passed -ge $minArchitectureTests) { 0 } else { 1 }
        Write-Host ('=== architecture test binary ran: {0}, passed: {1} (minimum {2})' -f $ran, $passed, $minArchitectureTests)
        Add-Result 'architecture' $archCode
    }
}

Write-Host ''
Write-Host '=== summary'
foreach ($r in $results) {
    $status = if ($r.Code -eq 0) { 'PASS' } else { 'FAIL' }
    Write-Host ('{0,-13} {1} (exit {2})' -f $r.Name, $status, $r.Code)
}

if ($results | Where-Object { $_.Code -ne 0 }) { exit 1 }
exit 0

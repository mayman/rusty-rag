# Quality gate for rag. Run from anywhere: scripts\check.ps1
# Rebuilds PATH from the registry (shells can hold a stale PATH), then runs
# test, fmt --check, and clippy -D warnings. Exits non-zero if any step fails.
$ErrorActionPreference = 'Stop'

$machine = [Environment]::GetEnvironmentVariable('Path', 'Machine')
$user = [Environment]::GetEnvironmentVariable('Path', 'User')
$env:Path = (Join-Path $env:USERPROFILE '.cargo\bin') + ';' + $machine + ';' + $user

if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    Write-Host 'FATAL: cargo was not found on PATH or in %USERPROFILE%\.cargo\bin.'
    Write-Host 'Install the Rust toolchain (rustup) and the MSVC Build Tools, then retry. This script installs nothing.'
    exit 127
}

Set-Location (Split-Path -Parent $PSScriptRoot)

$steps = @(
    @{ Name = 'test';   Args = @('test', '--workspace') },
    @{ Name = 'fmt';    Args = @('fmt', '--all', '--', '--check') },
    @{ Name = 'clippy'; Args = @('clippy', '--workspace', '--all-targets', '--', '-D', 'warnings') }
)

$results = @()
foreach ($step in $steps) {
    Write-Host ''
    Write-Host ('=== cargo ' + ($step.Args -join ' '))
    & cargo @($step.Args)
    $code = $LASTEXITCODE
    Write-Host ('=== exit code: ' + $code)
    $results += [pscustomobject]@{ Name = $step.Name; Code = $code }
}

Write-Host ''
Write-Host '=== summary'
foreach ($r in $results) {
    $status = if ($r.Code -eq 0) { 'PASS' } else { 'FAIL' }
    Write-Host ('{0,-7} {1} (exit {2})' -f $r.Name, $status, $r.Code)
}

if ($results | Where-Object { $_.Code -ne 0 }) { exit 1 }
exit 0
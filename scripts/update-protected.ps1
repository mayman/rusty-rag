# Regenerates scripts\protected.sha256, the manifest that scripts\check.ps1 verifies.
# Run this ONLY after the user has approved a change to a protected file.
# Hashes are taken over LF-normalised content so autocrlf cannot cause mismatches.
$ErrorActionPreference = 'Stop'

$root = Split-Path -Parent $PSScriptRoot
$protected = @(
    'Cargo.toml',
    'rust-toolchain.toml',
    'rag-core/clippy.toml',
    'rag-core/tests/architecture.rs',
    'scripts/check.ps1',
    'scripts/guard-protected.ps1',
    '.cargo/config.toml',      # only when present
    '.gitattributes'
)

function Get-LfHash([string]$path) {
    $latin1 = [System.Text.Encoding]::GetEncoding(28591)
    $text = $latin1.GetString([System.IO.File]::ReadAllBytes($path)).Replace("`r`n", "`n")
    $sha = [System.Security.Cryptography.SHA256]::Create()
    try { return ([BitConverter]::ToString($sha.ComputeHash($latin1.GetBytes($text))) -replace '-', '').ToLowerInvariant() }
    finally { $sha.Dispose() }
}

$lines = @()
foreach ($rel in $protected) {
    $full = Join-Path $root ($rel -replace '/', '\')
    if (-not (Test-Path -LiteralPath $full)) {
        if ($rel -eq '.cargo/config.toml') { continue }
        Write-Host "FATAL: protected file is missing: $rel"
        exit 1
    }
    $lines += ('{0}  {1}' -f (Get-LfHash $full), $rel)
}

$manifest = Join-Path $PSScriptRoot 'protected.sha256'
[System.IO.File]::WriteAllText($manifest, (($lines -join "`n") + "`n"), (New-Object System.Text.UTF8Encoding($false)))
Write-Host ('Wrote {0} ({1} entries)' -f $manifest, $lines.Count)
$lines | ForEach-Object { Write-Host $_ }
exit 0

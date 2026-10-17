# Pipe-tests scripts\guard-protected.ps1. Pester-free: prints PASS/FAIL per case
# and exits non-zero if any case fails.   Usage: scripts\test-guard.ps1 [-Guard <path>]
param([string]$Guard = (Join-Path $PSScriptRoot 'guard-protected.ps1'))

$ErrorActionPreference = 'Continue'
$root = Split-Path -Parent $PSScriptRoot
$env:CLAUDE_PROJECT_DIR = $root
$ps = (Get-Command powershell.exe).Source

$cases = New-Object System.Collections.Generic.List[object]
function Add-Case([string]$name, [string]$tool, $toolInput, [string]$expect, [string]$cwd = '') {
    $cases.Add([pscustomobject]@{ Name = $name; Tool = $tool; Input = $toolInput; Expect = $expect; Cwd = $cwd })
}
function W([string]$name, [string]$path, [string]$expect, [string]$tool = 'Write') {
    Add-Case $name $tool @{ file_path = $path; content = 'x' } $expect
}
function P([string]$name, [string]$command, [string]$expect, [string]$cwd = '') {
    Add-Case $name 'PowerShell' @{ command = $command } $expect $cwd
}

# --- the 12 original cases (reconstructed: protected Write/Edit/PowerShell blocked, normal edits allowed)
W  'orig01 Write PLAN.md' "$root\PLAN.md" block
W  'orig02 Edit architecture.rs' "$root\rag-core\tests\architecture.rs" block Edit
W  'orig03 Write domain source' "$root\rag-core\src\domain\foo.rs" allow
W  'orig04 Write root Cargo.toml' "$root\Cargo.toml" block
W  'orig05 Write rag-core Cargo.toml' "$root\rag-core\Cargo.toml" allow
W  'orig06 Write rag-infra Cargo.toml' "$root\rag-infra\Cargo.toml" allow
W  'orig07 Write .claude agent file' "$root\.claude\agents\x.md" block
W  'orig08 Write rust-toolchain.toml' "$root\rust-toolchain.toml" block
W  'orig09 Write clippy.toml' "$root\rag-core\clippy.toml" block
P  'orig10 Set-Content PLAN.md' 'Set-Content PLAN.md "x"' block
P  'orig11 cargo test' 'cargo test --workspace' allow
P  'orig12 redirect into architecture.rs' '"x" > rag-core\tests\architecture.rs' block

# --- new protected paths (D)
W  'new protect scripts/check.ps1' "$root\scripts\check.ps1" block
W  'new protect update-protected.ps1' "$root\scripts\update-protected.ps1" block
W  'new protect protected.sha256' "$root\scripts\protected.sha256" block
W  'new protect guard-protected.ps1' "$root\scripts\guard-protected.ps1" block
W  'new protect .cargo/config.toml' "$root\.cargo\config.toml" block
W  'new protect .gitattributes' "$root\.gitattributes" block
W  'new allow other scripts file' "$root\scripts\helper.ps1" allow
W  'new allow rag-core/tests new file' "$root\rag-core\tests\new_test.rs" allow
Add-Case 'new NotebookEdit notebook_path PLAN.md' 'NotebookEdit' @{ notebook_path = "$root\PLAN.md" } block
Add-Case 'new NotebookEdit other notebook' 'NotebookEdit' @{ notebook_path = "$root\notes.ipynb" } allow

# --- path resolution (D)
W  'path sibling dir with same prefix' "$($root)-evil\Cargo.toml" allow
W  'path \\?\ extended form' "\\?\$root\Cargo.toml" block
W  'path alternate data stream' "$root\Cargo.toml:evil" block
W  'path case-insensitive' "$root\cargo.TOML" block
W  'path dot-dot traversal' "$root\rag-core\..\Cargo.toml" block
W  'path relative root manifest' 'Cargo.toml' block
W  'path relative crate manifest' 'rag-core\Cargo.toml' allow
W  'path trailing dots/spaces' "$root\PLAN.md. " block
try {
    $short = (New-Object -ComObject Scripting.FileSystemObject).GetFile("$root\PLAN.md").ShortPath
    if ($short -and $short -ne "$root\PLAN.md") { W 'path 8.3 short name for PLAN.md' $short block }
} catch { }

# --- false blocks that must stay fixed
P  'fix text after pipe: Select-String architecture.rs' 'cargo test 2>&1 | Select-String architecture.rs' allow
P  'fix read PLAN.md then pipe' 'Get-Content PLAN.md | Select-String foo' allow
P  'fix git commit -m mentions PLAN.md' 'git commit -m "update PLAN.md and architecture.rs"' allow
P  'fix git commit here-string message' "git commit -m @'`nfix: PLAN.md > Cargo.toml`nSet-Content clippy.toml`n'@" allow
P  'fix 2>$null' 'cargo clippy 2>$null' allow
P  'fix 2>&1 with write verb elsewhere' 'cargo test 2>&1 | Out-File target\log.txt' allow
P  'fix mention in -Value string' 'Set-Content rag-core\src\x.rs -Value "see PLAN.md and Cargo.toml"' allow
P  'fix read-only script' 'Get-Content scripts\check.ps1' allow
P  'fix cd rag-core then crate Cargo.toml' 'cd rag-core; Set-Content Cargo.toml "x"' allow
P  'fix cwd in rag-core, crate Cargo.toml' 'Set-Content Cargo.toml "x"' allow "$root\rag-core"
P  'fix git status/diff/log' 'git status; git diff --stat; git log -n 3' allow
P  'fix git checkout branch' 'git checkout main' allow
P  'fix Remove-Item target dir' 'Remove-Item target -Recurse -Force' allow
P  'fix Remove-Item source file' 'Remove-Item rag-core\src\domain\old.rs' allow
P  'fix ReadAllText' "[IO.File]::ReadAllText('PLAN.md')" allow

# --- root Cargo.toml still blocked in shell forms
P  'block bare Cargo.toml write' 'Set-Content Cargo.toml "x"' block
P  'block .\Cargo.toml write' 'Set-Content .\Cargo.toml "x"' block
P  'block cd . then Cargo.toml' 'cd .; Set-Content Cargo.toml "x"' block
P  'block absolute root Cargo.toml' "Add-Content $root\Cargo.toml 'x'" block
P  'block cwd=root, bare Cargo.toml' 'Set-Content Cargo.toml "x"' block $root
P  'allow crate manifest write' 'Add-Content rag-core\Cargo.toml "x"' allow

# --- Remove-Item and wildcards
P  'rm file under rag-core/tests' 'Remove-Item rag-core\tests\foo.rs' block
P  'ri -Recurse rag-infra/tests' 'ri rag-infra\tests -Recurse' block
P  'del rag-cli/tests/*' 'del rag-cli\tests\*' block
P  'rmdir rag-core/tests' 'rmdir rag-core\tests' block
P  'Remove-Item rag-core/tests/*' 'Remove-Item rag-core\tests\*' block
P  'Remove-Item *.md (covers PLAN.md)' 'Remove-Item *.md' block
P  'rm Cargo.t* wildcard' 'rm Cargo.t*' block
P  'Remove-Item -Recurse rag-core' 'Remove-Item -Recurse rag-core' block
P  'Set-Content wildcard over architecture' 'Set-Content rag-core\tests\arch*.rs "x"' block

# --- git
P  'git apply' 'git apply fix.patch' block
P  'git am' 'git am 0001.patch' block
P  'git checkout -- Cargo.toml' 'git checkout -- Cargo.toml' block
P  'git checkout HEAD -- PLAN.md' 'git checkout HEAD -- PLAN.md' block
P  'git restore .' 'git restore .' block
P  'git restore --staged x' 'git restore --staged PLAN.md' block
P  'git stash' 'git stash' block
P  'git stash pop' 'git stash pop' block
P  'git reset --hard' 'git reset --hard HEAD~1' block

# --- script and dynamic execution
P  'call operator on own script' '& .\x.ps1' block
P  'run own script' '.\x.ps1' block
P  'dot-source own script' '. .\x.ps1' block
P  'powershell -File own script' 'powershell -File x.ps1' block
P  'Invoke-Expression' 'Invoke-Expression "Remove-Item PLAN.md"' block
P  'iex' 'iex $payload' block
P  'powershell -EncodedCommand' 'powershell -EncodedCommand AAAA' block
P  'powershell -Command nested write' 'powershell -Command "Set-Content PLAN.md x"' block
P  'call operator on variable' '& $script' block
P  'allowed: scripts\check.ps1' 'scripts\check.ps1' allow
P  'allowed: & .\scripts\check.ps1' '& .\scripts\check.ps1' allow
P  'allowed: powershell -File scripts\check.ps1' 'powershell -NoProfile -File scripts\check.ps1' allow
P  'allowed: writing a script file (not running it)' 'Set-Content scripts\helper.ps1 "x"' allow

# --- aliases
P  'alias ni' 'ni PLAN.md' block
P  'alias ri' 'ri PLAN.md' block
P  'alias cpi' 'cpi x.txt PLAN.md' block
P  'alias rd' 'rd .claude' block
P  'alias mi' 'mi x.txt Cargo.toml' block
P  'alias rni' 'rni PLAN.md y.md' block
P  'alias sc' 'sc Cargo.toml x' block
P  'alias ac' 'ac PLAN.md x' block
P  'alias sv' 'sv PLAN.md x' block
P  'backtick evasion' 'Set-Content PLA`N.md x' block
P  'redirect >> to clippy.toml' 'echo x >> rag-core\clippy.toml' block
P  'redirect to .gitattributes' 'echo x > .gitattributes' block
P  'redirect to .cargo/config.toml' 'echo x > .cargo\config.toml' block
P  'redirect to other file allowed' 'cargo test > target\out.txt' allow

# --- .NET write methods
P  '[IO.File]::WriteAllText PLAN.md' "[IO.File]::WriteAllText('PLAN.md','x')" block
P  '[System.IO.File]::AppendAllText Cargo.toml' '[System.IO.File]::AppendAllText("Cargo.toml","x")' block
P  '[IO.File]::Copy over clippy.toml' "[IO.File]::Copy('x','rag-core\clippy.toml',`$true)" block
P  '[IO.File]::Delete architecture.rs' "[IO.File]::Delete('rag-core\tests\architecture.rs')" block
P  '[IO.File]::WriteAllText normal file' "[IO.File]::WriteAllText('rag-core\src\x.rs','x')" allow

# --- toolchain
P  'rustup override' 'rustup override set nightly' block

$failed = 0
foreach ($c in $cases) {
    $payload = @{ tool_name = $c.Tool; tool_input = $c.Input }
    if ($c.Cwd) { $payload.cwd = $c.Cwd }
    $json = $payload | ConvertTo-Json -Depth 6 -Compress
    $null = $json | & $ps -NoProfile -ExecutionPolicy Bypass -File $Guard 2>&1
    $code = $LASTEXITCODE
    $actual = if ($code -eq 2) { 'block' } elseif ($code -eq 0) { 'allow' } else { "error($code)" }
    if ($actual -eq $c.Expect) { Write-Host ('PASS  {0}' -f $c.Name) }
    else { Write-Host ('FAIL  {0}  (expected {1}, got {2})' -f $c.Name, $c.Expect, $actual); $failed++ }
}
# --- fail closed on unreadable input (raw stdin, not built from $cases)
$rawCases = @(
    @{ Name = 'fail closed: empty input';         Raw = '' },
    @{ Name = 'fail closed: not JSON';            Raw = 'not json at all' },
    @{ Name = 'fail closed: JSON without tool';   Raw = '{"tool_input":{"file_path":"x"}}' }
)
foreach ($r in $rawCases) {
    $null = $r.Raw | & $ps -NoProfile -ExecutionPolicy Bypass -File $Guard 2>&1
    $code = $LASTEXITCODE
    if ($code -eq 2) { Write-Host ('PASS  {0}' -f $r.Name) }
    else { Write-Host ('FAIL  {0}  (expected block, got exit {1})' -f $r.Name, $code); $failed++ }
}

Write-Host ''
Write-Host ('{0} cases, {1} failed' -f ($cases.Count + $rawCases.Count), $failed)
if ($failed -gt 0) { exit 1 }
exit 0

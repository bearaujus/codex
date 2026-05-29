# Run benchmark targets once to ensure they start successfully.
$ErrorActionPreference = 'Stop'

$RepoRoot = Split-Path -Parent $PSScriptRoot
. (Join-Path $PSScriptRoot 'rust-env.ps1')
Write-Phase 'Preparing benchmark smoke run'
Write-Output "Rust workspace: $(Join-Path $RepoRoot 'codex-rs')"
Enable-SccacheIfAvailable

Push-Location (Join-Path $RepoRoot 'codex-rs')
try {
    Write-Phase 'Running cargo bench smoke command'
    Invoke-CargoWithSccacheFallback `
        -CargoArgs @('bench', '--workspace', '--bench', '*', '--', '--test') `
        -FailureMessage 'cargo bench smoke failed'
}
finally {
    Pop-Location
}
Write-Phase 'Benchmark smoke completed'

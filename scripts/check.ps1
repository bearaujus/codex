# Fast compile check (inner dev loop) — no codegen, no optimized binary.
# Optional crate name narrows scope, e.g. scripts/check.ps1 codex-login
param([string]$Crate)
$ErrorActionPreference = 'Stop'

$RepoRoot = Split-Path -Parent $PSScriptRoot
. (Join-Path $PSScriptRoot 'rust-env.ps1')
Enable-SccacheIfAvailable

Push-Location (Join-Path $RepoRoot 'codex-rs')
try {
    if ($Crate) { cargo check -p $Crate } else { cargo check --workspace }
    if ($LASTEXITCODE -ne 0) { throw "cargo check failed (exit $LASTEXITCODE)" }
}
finally {
    Pop-Location
}

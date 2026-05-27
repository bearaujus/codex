# Clippy with the repo's deny-heavy lints (run before committing — `check` won't
# catch these). Optional crate name narrows scope, e.g. scripts/lint.ps1 codex-login
param([string]$Crate)
$ErrorActionPreference = 'Stop'

$RepoRoot = Split-Path -Parent $PSScriptRoot
. (Join-Path $PSScriptRoot 'rust-env.ps1')
Enable-SccacheIfAvailable

Push-Location (Join-Path $RepoRoot 'codex-rs')
try {
    if ($Crate) { cargo clippy -p $Crate --all-targets } else { cargo clippy --workspace --all-targets }
    if ($LASTEXITCODE -ne 0) { throw "cargo clippy failed (exit $LASTEXITCODE)" }
}
finally {
    Pop-Location
}

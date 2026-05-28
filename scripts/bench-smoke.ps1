# Run benchmark targets once to ensure they start successfully.
$ErrorActionPreference = 'Stop'

$RepoRoot = Split-Path -Parent $PSScriptRoot
. (Join-Path $PSScriptRoot 'rust-env.ps1')
Enable-SccacheIfAvailable

Push-Location (Join-Path $RepoRoot 'codex-rs')
try {
    & just bench-smoke
    if ($LASTEXITCODE -ne 0) { throw "just bench-smoke failed (exit $LASTEXITCODE)" }
}
finally {
    Pop-Location
}

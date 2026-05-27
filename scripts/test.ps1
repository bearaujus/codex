# Run repo-standard tests. Optional crate name narrows scope, e.g. scripts/test.ps1 codex-login
param([string]$Crate)
$ErrorActionPreference = 'Stop'

$RepoRoot = Split-Path -Parent $PSScriptRoot
. (Join-Path $PSScriptRoot 'rust-env.ps1')
Enable-SccacheIfAvailable

Push-Location (Join-Path $RepoRoot 'codex-rs')
try {
    if ($Crate) {
        just test -p $Crate
    }
    else {
        just test
    }
    if ($LASTEXITCODE -ne 0) { throw "just test failed (exit $LASTEXITCODE)" }
}
finally {
    Pop-Location
}

# Run the repo-standard formatter for the Rust workspace.
$ErrorActionPreference = 'Stop'

$RepoRoot = Split-Path -Parent $PSScriptRoot

Push-Location (Join-Path $RepoRoot 'codex-rs')
try {
    just fmt
    if ($LASTEXITCODE -ne 0) { throw "just fmt failed (exit $LASTEXITCODE)" }
}
finally {
    Pop-Location
}

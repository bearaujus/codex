# Build a wasm artifact for the browser-oriented wasm target.
# Narrow with a crate name, e.g. scripts/web-build.ps1 codex-login --release
param(
    [string]$Crate,
    [Parameter(ValueFromRemainingArguments = $true)][string[]]$CargoArgs
)
$ErrorActionPreference = 'Stop'

$RepoRoot = Split-Path -Parent $PSScriptRoot
. (Join-Path $PSScriptRoot 'rust-env.ps1')
Enable-SccacheIfAvailable
Assert-RustupTargetInstalled 'wasm32-unknown-unknown'

Push-Location (Join-Path $RepoRoot 'codex-rs')
try {
    $BuildArgs = @('build', '--target', 'wasm32-unknown-unknown')
    if ($Crate) {
        $BuildArgs += @('-p', $Crate)
    }
    else {
        $BuildArgs += '--workspace'
    }
    if ($CargoArgs) {
        $BuildArgs += $CargoArgs
    }

    & cargo @BuildArgs
    if ($LASTEXITCODE -ne 0) { throw "cargo build for wasm32-unknown-unknown failed (exit $LASTEXITCODE)" }
}
finally {
    Pop-Location
}

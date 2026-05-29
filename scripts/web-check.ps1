# Compile-check Rust code for the browser-oriented wasm target.
# Narrow with a crate name, e.g. scripts/web-check.ps1 codex-login
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
    $CheckArgs = @('check', '--target', 'wasm32-unknown-unknown')
    if ($Crate) {
        $CheckArgs += @('-p', $Crate)
    }
    else {
        $CheckArgs += '--workspace'
    }
    if ($CargoArgs) {
        $CheckArgs += $CargoArgs
    }

    Invoke-CargoWithSccacheFallback -CargoArgs $CheckArgs -FailureMessage 'cargo check for wasm32-unknown-unknown failed'
}
finally {
    Pop-Location
}

# Fast compile check (inner dev loop) — no codegen, no optimized binary.
# Optional crate name narrows scope, e.g. scripts/check.ps1 codex-login
param(
    [string]$Crate,
    [Parameter(ValueFromRemainingArguments = $true)][string[]]$CargoArgs
)
$ErrorActionPreference = 'Stop'

$RepoRoot = Split-Path -Parent $PSScriptRoot
. (Join-Path $PSScriptRoot 'rust-env.ps1')
Write-Phase "Preparing cargo check (crate=$Crate)"
Write-Output "Rust workspace: $(Join-Path $RepoRoot 'codex-rs')"

Push-Location (Join-Path $RepoRoot 'codex-rs')
try {
    $CheckArgs = @('check')
    $WorkspaceWide = -not $Crate
    if ($Crate) {
        $CheckArgs += @('-p', $Crate)
    }
    else {
        $CheckArgs += '--workspace'
    }
    if ($CargoArgs) {
        $CheckArgs += $CargoArgs
    }
    $CheckArgs = Add-DefaultCargoParallelism -CargoArgs $CheckArgs -WorkspaceWide:$WorkspaceWide

    Write-Phase 'Running cargo check'
    Invoke-Cargo -CargoArgs $CheckArgs -FailureMessage 'cargo check failed'
}
finally {
    Pop-Location
}
Write-Phase 'Cargo check completed'

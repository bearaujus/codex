# Clippy with the repo's deny-heavy lints (run before committing — `check` won't
# catch these). Optional crate name narrows scope, e.g. scripts/lint.ps1 codex-login
param(
    [string]$Crate,
    [Parameter(ValueFromRemainingArguments = $true)][string[]]$CargoArgs
)
$ErrorActionPreference = 'Stop'

$RepoRoot = Split-Path -Parent $PSScriptRoot
. (Join-Path $PSScriptRoot 'rust-env.ps1')
Write-Phase "Preparing lint run (crate=$Crate)"
Write-Output "Rust workspace: $(Join-Path $RepoRoot 'codex-rs')"
Enable-SccacheIfAvailable

Push-Location (Join-Path $RepoRoot 'codex-rs')
try {
    $ClippyArgs = @('clippy')
    $WorkspaceWide = -not $Crate
    if ($Crate) {
        $ClippyArgs += @('-p', $Crate)
    }
    else {
        $ClippyArgs += '--workspace'
    }
    $ClippyArgs += '--all-targets'
    if ($CargoArgs) {
        $ClippyArgs += $CargoArgs
    }
    $ClippyArgs = Add-DefaultCargoParallelism -CargoArgs $ClippyArgs -WorkspaceWide:$WorkspaceWide

    Write-Phase 'Running cargo clippy'
    Invoke-CargoWithSccacheFallback -CargoArgs $ClippyArgs -FailureMessage 'cargo clippy failed'
}
finally {
    Pop-Location
}
Write-Phase 'Lint run completed'

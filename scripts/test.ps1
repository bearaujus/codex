# Run local tests. Optional crate name narrows scope, e.g. scripts/test.ps1 codex-login
# Defaults to the lighter-weight `ci-test` cargo profile for a faster local loop.
# Use -Full to include benchmark smoke.
param(
    [Parameter(Position = 0)][string]$Crate,
    [switch]$Full,
    [string]$CargoProfile = 'ci-test',
    [Parameter(Position = 1, ValueFromRemainingArguments = $true)][string[]]$CargoArgs
)
$ErrorActionPreference = 'Stop'

$RepoRoot = Split-Path -Parent $PSScriptRoot
. (Join-Path $PSScriptRoot 'rust-env.ps1')
Write-Phase "Preparing test run (profile=$CargoProfile, full=$Full, crate=$Crate)"
Write-Output "Rust workspace: $(Join-Path $RepoRoot 'codex-rs')"
if (-not $env:RUST_MIN_STACK) {
    $env:RUST_MIN_STACK = '16777216'
    Write-Output 'Enabled RUST_MIN_STACK=16777216 for Windows test stability'
}
Enable-SccacheIfAvailable

Push-Location (Join-Path $RepoRoot 'codex-rs')
try {
    $HasNoRun = $CargoArgs -contains '--no-run'
    $UseCargoTest = $HasNoRun -or (Test-SccacheTemporarilyDisabled)
    $WorkspaceWide = -not $Crate
    if ($UseCargoTest) {
        $Args = @('test')
        $HasExplicitProfile = $CargoArgs -contains '--profile' `
            -or $CargoArgs -contains '--release' `
            -or $CargoArgs -contains '-r' `
            -or ($CargoArgs | Where-Object { $_ -like '--profile=*' })
        if (-not $HasExplicitProfile) {
            $Args += @('--profile', $CargoProfile)
        }
    }
    else {
        $Args = @('nextest', 'run', '--no-fail-fast')
        $HasExplicitProfile = $CargoArgs -contains '--cargo-profile' `
            -or $CargoArgs -contains '--release' `
            -or $CargoArgs -contains '-r' `
            -or ($CargoArgs | Where-Object { $_ -like '--cargo-profile=*' })
        if (-not $HasExplicitProfile) {
            $Args += @('--cargo-profile', $CargoProfile)
        }
    }
    if ($Crate) {
        $Args += @('-p', $Crate)
    }
    if ($CargoArgs) {
        $Args += $CargoArgs
    }
    $Args = Add-DefaultCargoTestVisibility -CargoArgs $Args
    $Args = Add-DefaultNextestVisibility -CargoArgs $Args -WorkspaceWide:$WorkspaceWide
    $Args = Add-DefaultCargoParallelism -CargoArgs $Args -WorkspaceWide:$WorkspaceWide

    if ($UseCargoTest) {
        Write-Phase 'Selected cargo test execution path'
        Invoke-CargoWithSccacheFallback -CargoArgs $Args -FailureMessage 'cargo test failed'
    }
    else {
        Write-Phase 'Selected cargo nextest execution path'
        Invoke-CargoWithSccacheFallback -CargoArgs $Args -FailureMessage 'cargo nextest failed'
    }

    if ($Full -and -not $HasNoRun) {
        Write-Phase 'Running benchmark smoke phase'
        & (Join-Path $PSScriptRoot 'bench-smoke.ps1')
        if ($LASTEXITCODE -ne 0) { throw "bench smoke failed (exit $LASTEXITCODE)" }
    }
}
finally {
    Pop-Location
}
Write-Phase 'Test run completed'

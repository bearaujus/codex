# Run the repo-standard formatter. Optional crate name narrows scope,
# e.g. scripts/fmt.ps1 codex-login --check
param(
    [string]$Crate,
    [Parameter(ValueFromRemainingArguments = $true)][string[]]$RustfmtArgs
)
$ErrorActionPreference = 'Stop'

$RepoRoot = Split-Path -Parent $PSScriptRoot
. (Join-Path $PSScriptRoot 'rust-env.ps1')
Write-Phase "Preparing Rust format run (crate=$Crate)"
Write-Output "Rust workspace: $(Join-Path $RepoRoot 'codex-rs')"

Push-Location (Join-Path $RepoRoot 'codex-rs')
try {
    $FmtArgs = @('fmt')
    if ($Crate) {
        $FmtArgs += @('-p', $Crate)
    }
    else {
        $FmtArgs += '--all'
    }
    if ($RustfmtArgs) {
        $FmtArgs += $RustfmtArgs
    }
    $FmtArgs += @('--', '--config', 'imports_granularity=Item')

    Write-Phase 'Running cargo fmt'
    $SavedNativeErrorPreference = $null
    $SavedErrorActionPreference = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    if (Get-Variable -Name PSNativeCommandUseErrorActionPreference -ErrorAction SilentlyContinue) {
        $SavedNativeErrorPreference = $PSNativeCommandUseErrorActionPreference
        $PSNativeCommandUseErrorActionPreference = $false
    }
    try {
        & cargo @FmtArgs 2>$null
    }
    finally {
        $ErrorActionPreference = $SavedErrorActionPreference
        if ($null -ne $SavedNativeErrorPreference) {
            $PSNativeCommandUseErrorActionPreference = $SavedNativeErrorPreference
        }
    }
    if ($LASTEXITCODE -ne 0) { throw "cargo fmt failed (exit $LASTEXITCODE)" }
}
finally {
    Pop-Location
}
Write-Phase 'Formatting completed'

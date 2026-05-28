# Clippy with the repo's deny-heavy lints (run before committing — `check` won't
# catch these). Optional crate name narrows scope, e.g. scripts/lint.ps1 codex-login
param(
    [string]$Crate,
    [Parameter(ValueFromRemainingArguments = $true)][string[]]$CargoArgs
)
$ErrorActionPreference = 'Stop'

$RepoRoot = Split-Path -Parent $PSScriptRoot
. (Join-Path $PSScriptRoot 'rust-env.ps1')
Enable-SccacheIfAvailable

Push-Location (Join-Path $RepoRoot 'codex-rs')
try {
    $ClippyArgs = @('clippy')
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

    & cargo @ClippyArgs
    if ($LASTEXITCODE -ne 0) { throw "cargo clippy failed (exit $LASTEXITCODE)" }
}
finally {
    Pop-Location
}

# Run local tests. Optional crate name narrows scope, e.g. scripts/test.ps1 codex-login
# Use -Full to include benchmark smoke.
param(
    [string]$Crate,
    [switch]$Full,
    [Parameter(ValueFromRemainingArguments = $true)][string[]]$JustArgs
)
$ErrorActionPreference = 'Stop'

$RepoRoot = Split-Path -Parent $PSScriptRoot
. (Join-Path $PSScriptRoot 'rust-env.ps1')
Enable-SccacheIfAvailable

Push-Location (Join-Path $RepoRoot 'codex-rs')
try {
    $Args = @()
    if ($Full) {
        $Args += 'test'
    }
    else {
        $Args += 'test-local'
    }
    if ($Crate) {
        $Args += @('-p', $Crate)
    }
    if ($JustArgs) {
        $Args += $JustArgs
    }

    & just @Args
    if ($LASTEXITCODE -ne 0) { throw "just test failed (exit $LASTEXITCODE)" }
}
finally {
    Pop-Location
}

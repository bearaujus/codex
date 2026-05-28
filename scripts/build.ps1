# Build the codex binary into <repo>\bin\codex.exe using a local cargo profile.
# The default is `fast` (release minus fat LTO / codegen-units=1 — see
# codex-rs/Cargo.toml).
param(
    [string]$CargoProfile = 'fast',
    [Parameter(ValueFromRemainingArguments = $true)][string[]]$CargoArgs
)
$ErrorActionPreference = 'Stop'

$RepoRoot = Split-Path -Parent $PSScriptRoot
$RsDir    = Join-Path $RepoRoot 'codex-rs'
$BinDir   = Join-Path $RepoRoot 'bin'

. (Join-Path $PSScriptRoot 'rust-env.ps1')
Enable-SccacheIfAvailable
if ($CargoProfile -eq 'fast') {
    Enable-LocalFastProfileIncremental
}

New-Item -ItemType Directory -Force -Path $BinDir | Out-Null

Push-Location $RsDir
try {
    $BuildArgs = @('build', '-p', 'codex-cli', '--profile', $CargoProfile)
    if ($CargoArgs) {
        $BuildArgs += $CargoArgs
    }

    & cargo @BuildArgs
    if ($LASTEXITCODE -ne 0) { throw "cargo build failed (exit $LASTEXITCODE)" }
}
finally {
    Pop-Location
}

$Src = Join-Path $RsDir "target\$CargoProfile\codex.exe"
$Dst = Join-Path $BinDir 'codex.exe'
Copy-Item -Force -Path $Src -Destination $Dst
Write-Output "Built $Dst using profile $CargoProfile"

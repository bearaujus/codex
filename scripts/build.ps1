# Build the codex binary into <repo>\bin\codex.exe using the fast profile
# (release minus fat LTO / codegen-units=1 — see codex-rs/Cargo.toml).
$ErrorActionPreference = 'Stop'

$RepoRoot = Split-Path -Parent $PSScriptRoot
$RsDir    = Join-Path $RepoRoot 'codex-rs'
$BinDir   = Join-Path $RepoRoot 'bin'

New-Item -ItemType Directory -Force -Path $BinDir | Out-Null

Push-Location $RsDir
try {
    cargo build -p codex-cli --profile fast
    if ($LASTEXITCODE -ne 0) { throw "cargo build failed (exit $LASTEXITCODE)" }
}
finally {
    Pop-Location
}

$Src = Join-Path $RsDir 'target\fast\codex.exe'
$Dst = Join-Path $BinDir 'codex.exe'
Copy-Item -Force -Path $Src -Destination $Dst
Write-Output "Built $Dst"

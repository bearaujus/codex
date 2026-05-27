# Build the release codex binary into <repo>\bin\codex.exe
$ErrorActionPreference = 'Stop'

$RepoRoot = Split-Path -Parent $PSScriptRoot
$RsDir    = Join-Path $RepoRoot 'codex-rs'
$BinDir   = Join-Path $RepoRoot 'bin'

New-Item -ItemType Directory -Force -Path $BinDir | Out-Null

Push-Location $RsDir
try {
    cargo build -p codex-cli --release
    if ($LASTEXITCODE -ne 0) { throw "cargo build failed (exit $LASTEXITCODE)" }
}
finally {
    Pop-Location
}

$Src = Join-Path $RsDir 'target\release\codex.exe'
$Dst = Join-Path $BinDir 'codex.exe'
Copy-Item -Force -Path $Src -Destination $Dst
Write-Output "Built $Dst"

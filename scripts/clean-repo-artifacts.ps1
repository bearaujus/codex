# Remove local build artifacts: the cargo `target/` tree (the real disk hog) and
# the repo-local built binary. `cargo clean` removes every profile's artifacts
# (dev, dev-small, ci-test, fast, release, ...), each of which is a full copy of
# the compiled workspace + dependencies, so this is where hundreds of GB live.
$ErrorActionPreference = 'SilentlyContinue'

$RepoRoot = Split-Path -Parent $PSScriptRoot
$RsDir = Join-Path $RepoRoot 'codex-rs'
$Exe = Join-Path $RepoRoot 'bin\codex.exe'
$ProfileStamp = Join-Path $RepoRoot 'bin\codex.profile.txt'

Push-Location $RsDir
try {
    Write-Output "Running cargo clean in $RsDir (removes the entire target/ tree)..."
    & cargo clean
}
finally {
    Pop-Location
}

if (Test-Path $Exe) {
    Remove-Item -Force $Exe
    Write-Output "Removed $Exe"
}
else {
    Write-Output "Nothing to clean ($Exe not present)"
}

if (Test-Path $ProfileStamp) {
    Remove-Item -Force $ProfileStamp
}

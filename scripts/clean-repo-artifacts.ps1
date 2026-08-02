# Remove local build artifacts: the cargo `target/` tree (the real disk hog) and
# the repo-local executable bundle. `cargo clean` removes every profile's artifacts
# (dev, dev-small, ci-test, fast, release, ...), each of which is a full copy of
# the compiled workspace + dependencies, so this is where hundreds of GB live.
$ErrorActionPreference = 'SilentlyContinue'

$RepoRoot = Split-Path -Parent $PSScriptRoot
$RsDir = Join-Path $RepoRoot 'codex-rs'
$Exe = Join-Path $RepoRoot 'bin\codex.exe'
$CodeModeHost = Join-Path $RepoRoot 'bin\codex-code-mode-host.exe'
$CodeModeHostStamp = Join-Path $RepoRoot 'bin\codex-code-mode-host.release.txt'
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

foreach ($Path in @($CodeModeHost, $CodeModeHostStamp)) {
    if (Test-Path $Path) {
        Remove-Item -Force $Path
        Write-Output "Removed $Path"
    }
}

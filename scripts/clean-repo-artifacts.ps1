# Remove the repo-local built binary.
$ErrorActionPreference = 'SilentlyContinue'

$RepoRoot = Split-Path -Parent $PSScriptRoot
$Exe = Join-Path $RepoRoot 'bin\codex.exe'
$ProfileStamp = Join-Path $RepoRoot 'bin\codex.profile.txt'

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

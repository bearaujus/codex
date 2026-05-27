# Build + install the local codex onto the user PATH (%LOCALAPPDATA%\codex\bin).
$ErrorActionPreference = 'Stop'

$RepoRoot   = Split-Path -Parent $PSScriptRoot
$InstallDir = Join-Path $env:LOCALAPPDATA 'codex\bin'

# Build the release binary first.
& (Join-Path $PSScriptRoot 'build.ps1')

$Src = Join-Path $RepoRoot 'bin\codex.exe'
if (-not (Test-Path $Src)) { throw "build did not produce $Src" }

New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
Copy-Item -Force -Path $Src -Destination (Join-Path $InstallDir 'codex.exe')
Write-Output "Installed codex.exe -> $InstallDir"

# Ensure the install dir is on the USER PATH (idempotent; uses the .NET API to
# avoid the setx 1024-char truncation hazard).
$UserPath = [Environment]::GetEnvironmentVariable('Path', 'User')
$Parts = @()
if ($UserPath) { $Parts = $UserPath.Split(';') | Where-Object { $_ -ne '' } }
if ($Parts -notcontains $InstallDir) {
    $NewPath = (@($Parts) + $InstallDir) -join ';'
    [Environment]::SetEnvironmentVariable('Path', $NewPath, 'User')
    Write-Output "Added $InstallDir to your user PATH. Open a NEW shell for it to take effect."
}
else {
    Write-Output "$InstallDir already on your user PATH."
}
Write-Output "Done. Verify in a new shell with: codex --version"

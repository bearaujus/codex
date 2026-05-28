# Build + install the local codex onto the user PATH (%LOCALAPPDATA%\codex\bin).
$ErrorActionPreference = 'Stop'

$RepoRoot   = Split-Path -Parent $PSScriptRoot
$InstallDir = Join-Path $env:LOCALAPPDATA 'codex\bin'

# Build the release-like local binary first.
& (Join-Path $PSScriptRoot 'build.ps1') -CargoProfile fast

$Src = Join-Path $RepoRoot 'bin\codex.exe'
if (-not (Test-Path $Src)) { throw "build did not produce $Src" }

New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
Copy-Item -Force -Path $Src -Destination (Join-Path $InstallDir 'codex.exe')
Write-Output "Installed codex.exe -> $InstallDir"

# Ensure the install dir is first on the USER PATH so the repo build wins over
# npm shims or older installs. Uses the .NET API to avoid the setx 1024-char
# truncation hazard.
$UserPath = [Environment]::GetEnvironmentVariable('Path', 'User')
$Parts = @()
if ($UserPath) { $Parts = $UserPath.Split(';') | Where-Object { $_ -ne '' } }
$NormalizedInstallDir = [IO.Path]::GetFullPath($InstallDir).TrimEnd('\')
$FilteredParts = @(
    $Parts | Where-Object {
        try {
            ([IO.Path]::GetFullPath($_).TrimEnd('\')) -ne $NormalizedInstallDir
        }
        catch {
            $_ -ne $InstallDir
        }
    }
)
$NewPath = (@($InstallDir) + $FilteredParts) -join ';'
if ($NewPath -ne $UserPath) {
    [Environment]::SetEnvironmentVariable('Path', $NewPath, 'User')
    Write-Output "Moved $InstallDir to the front of your user PATH. Open a NEW shell for it to take effect."
}
else {
    Write-Output "$InstallDir is already first on your user PATH."
}
Write-Output "Done. Verify in a new shell with: codex --version"

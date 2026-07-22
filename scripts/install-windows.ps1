# Install the local codex onto the user PATH (%LOCALAPPDATA%\codex\bin).
# Reuses an existing repo-local build by default for a faster inner loop.
param(
    [ValidateSet('dev-small', 'fast')]
    [string]$CargoProfile = 'dev-small',
    [switch]$ForceBuild,
    [string]$InstallDir = (Join-Path $env:LOCALAPPDATA 'codex\bin'),
    [switch]$SkipPathUpdate
)

$ErrorActionPreference = 'Stop'

$RepoRoot = Split-Path -Parent $PSScriptRoot
$RepoBinDir = Join-Path $RepoRoot 'bin'
$RepoBinary = Join-Path $RepoBinDir 'codex.exe'
$RepoBinaryProfile = Join-Path $RepoBinDir 'codex.profile.txt'
$TargetBinary = Join-Path $RepoRoot "codex-rs\target\$CargoProfile\codex.exe"

function Write-RepoBinaryProfile {
    param([Parameter(Mandatory = $true)][string]$Profile)

    New-Item -ItemType Directory -Force -Path $RepoBinDir | Out-Null
    Set-Content -Path $RepoBinaryProfile -Value $Profile -NoNewline
}

function Get-FileSha256 {
    param([Parameter(Mandatory = $true)][string]$Path)

    $Algorithm = [System.Security.Cryptography.SHA256]::Create()
    $Stream = [System.IO.File]::OpenRead($Path)
    try {
        return ($Algorithm.ComputeHash($Stream) | ForEach-Object { $_.ToString('x2') }) -join ''
    }
    finally {
        $Stream.Dispose()
        $Algorithm.Dispose()
    }
}

$RepoBinaryReady = $false
if (-not $ForceBuild -and (Test-Path $RepoBinary) -and (Test-Path $RepoBinaryProfile)) {
    $ExistingProfile = (Get-Content $RepoBinaryProfile -Raw).Trim()
    if ($ExistingProfile -eq $CargoProfile) {
        $RepoBinaryReady = $true
        Write-Output "Reusing existing $RepoBinary built with profile $CargoProfile. Pass -ForceBuild to rebuild."
    }
}

if (-not $RepoBinaryReady -and -not $ForceBuild -and (Test-Path $TargetBinary)) {
    New-Item -ItemType Directory -Force -Path $RepoBinDir | Out-Null
    Copy-Item -Force -Path $TargetBinary -Destination $RepoBinary
    Write-RepoBinaryProfile -Profile $CargoProfile
    $RepoBinaryReady = $true
    Write-Output "Reused existing target\$CargoProfile build. Pass -ForceBuild to rebuild."
}

if (-not $RepoBinaryReady) {
    & (Join-Path $PSScriptRoot 'build.ps1') -CargoProfile $CargoProfile
    if (-not (Test-Path $RepoBinary)) {
        throw "build did not produce $RepoBinary"
    }
}

New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
$InstalledExe = Join-Path $InstallDir 'codex.exe'
if (Test-Path $InstalledExe) {
    $SourceHash = Get-FileSha256 -Path $RepoBinary
    $InstalledHash = Get-FileSha256 -Path $InstalledExe
    if ($SourceHash -eq $InstalledHash) {
        Write-Output "$InstalledExe already matches the repo build. Skipping binary copy."
        $CopySucceeded = $true
    }
    else {
        $CopySucceeded = $false
    }
}
else {
    $CopySucceeded = $false
}

try {
    if (-not $CopySucceeded) {
        Copy-Item -Force -Path $RepoBinary -Destination $InstalledExe
    }
}
catch [System.IO.IOException] {
    throw "Failed to install to $InstalledExe because the destination file is in use. Close running codex processes that use this install path, or rerun with -InstallDir for a side-by-side test install."
}
catch [System.UnauthorizedAccessException] {
    throw "Failed to install to $InstalledExe because Windows denied replacing the destination file. This usually means a running codex.exe is using that install path. Close those processes and rerun, or use -InstallDir for a side-by-side test install."
}
Write-Output "Installed codex.exe -> $InstallDir"

# Ensure the install dir is first on the USER PATH so the repo build wins over
# npm shims or older installs. Uses the .NET API to avoid the setx 1024-char
# truncation hazard.
if ($SkipPathUpdate) {
    Write-Output 'Skipped updating the user PATH.'
}
else {
    $UserPath = [Environment]::GetEnvironmentVariable('Path', 'User')
    $Parts = @()
    if ($UserPath) { $Parts = $UserPath.Split(';') | Where-Object { $_ -ne '' } }
    $NormalizedInstallDir = [IO.Path]::GetFullPath($InstallDir).TrimEnd('\')
    $FilteredParts = @(
        $Parts | Where-Object {
            $Part = $_
            try {
                ([IO.Path]::GetFullPath($Part).TrimEnd('\')) -ne $NormalizedInstallDir
            }
            catch {
                $Part -ne $InstallDir
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
}
Write-Output "Done. Verify in a new shell with: codex --version"

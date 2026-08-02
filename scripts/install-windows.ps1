# Install the repository-local Codex executable bundle on the user PATH.
# The Makefile makes this target depend on build, so installation never starts
# a second Cargo invocation or silently reuses a stale target-directory binary.
param(
    [string]$InstallDir = (Join-Path $env:LOCALAPPDATA 'codex\bin'),
    [switch]$SkipPathUpdate
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$RepoRoot = Split-Path -Parent $PSScriptRoot
$RepoBinDir = Join-Path $RepoRoot 'bin'
$RepoBinary = Join-Path $RepoBinDir 'codex.exe'
$RepoCodeModeHost = Join-Path $RepoBinDir 'codex-code-mode-host.exe'
$RepoCodeModeHostStamp = Join-Path $RepoBinDir 'codex-code-mode-host.release.txt'

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

function Install-Artifact {
    param(
        [Parameter(Mandatory = $true)][string]$Source,
        [Parameter(Mandatory = $true)][string]$Destination
    )

    if (-not (Test-Path -LiteralPath $Source -PathType Leaf)) {
        throw "Required build artifact is missing: $Source. Run make build first."
    }

    $SourcePath = [IO.Path]::GetFullPath($Source)
    $DestinationPath = [IO.Path]::GetFullPath($Destination)
    if ($SourcePath.Equals($DestinationPath, [StringComparison]::OrdinalIgnoreCase)) {
        Write-Output "$Destination already is the repository build artifact."
        return
    }

    $SourceHash = Get-FileSha256 -Path $Source
    if (
        (Test-Path -LiteralPath $Destination -PathType Leaf) -and
        (Get-FileSha256 -Path $Destination) -eq $SourceHash
    ) {
        Write-Output "$Destination already matches the repository build. Skipping copy."
        return
    }

    try {
        Copy-Item -LiteralPath $Source -Destination $Destination -Force
    }
    catch [System.IO.IOException] {
        throw "Failed to install $Destination because it is in use. Close processes using this Codex installation, or rerun with -InstallDir for a side-by-side install."
    }
    catch [System.UnauthorizedAccessException] {
        throw "Windows denied replacing $Destination. Close processes using this Codex installation, or rerun with -InstallDir for a side-by-side install."
    }

    $InstalledHash = Get-FileSha256 -Path $Destination
    if ($InstalledHash -ne $SourceHash) {
        throw "Installed artifact failed checksum verification: $Destination"
    }
}

if (-not (Test-Path -LiteralPath $RepoBinary -PathType Leaf)) {
    throw "Required build artifact is missing: $RepoBinary. Run make build first."
}
if (-not (Test-Path -LiteralPath $RepoCodeModeHost -PathType Leaf)) {
    throw "Required Code Mode host is missing: $RepoCodeModeHost. Run make build first."
}

New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
$InstalledExe = Join-Path $InstallDir 'codex.exe'
$InstalledCodeModeHost = Join-Path $InstallDir 'codex-code-mode-host.exe'
Install-Artifact -Source $RepoBinary -Destination $InstalledExe
Install-Artifact -Source $RepoCodeModeHost -Destination $InstalledCodeModeHost

if (Test-Path -LiteralPath $RepoCodeModeHostStamp -PathType Leaf) {
    Install-Artifact -Source $RepoCodeModeHostStamp -Destination (Join-Path $InstallDir 'codex-code-mode-host.release.txt')
}

Write-Output "Installed codex.exe and codex-code-mode-host.exe -> $InstallDir"

# Ensure the install dir is first on the USER PATH so the repo build wins over
# shims or older installs. Uses the .NET API to avoid the setx truncation hazard.
if ($SkipPathUpdate) {
    Write-Output 'Skipped updating the user PATH.'
}
else {
    $UserPath = [Environment]::GetEnvironmentVariable('Path', 'User')
    $Parts = @()
    if ($UserPath) {
        $Parts = $UserPath.Split(';') | Where-Object { $_ -ne '' }
    }
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

Write-Output 'Done. Verify in a new shell with: codex --version'

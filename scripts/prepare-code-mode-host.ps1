# Download the public OpenAI Code Mode host used by this fork's Windows build.
param(
    [string]$OutputPath = (Join-Path (Split-Path -Parent $PSScriptRoot) 'bin\codex-code-mode-host.exe'),
    [string]$CacheRoot = (Join-Path $env:LOCALAPPDATA 'codex\cache\code-mode-host')
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

$ReleaseVersion = '0.146.0'
$ReleaseTag = "rust-v$ReleaseVersion"
$Architecture = [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture

switch ($Architecture) {
    'X64' {
        $Target = 'x86_64-pc-windows-msvc'
        $ExpectedSha256 = '6ef1de0e04d859f8f4f6d4d64f0f3ceeec28658423d91de160f5e804280d1c36'
    }
    'Arm64' {
        $Target = 'aarch64-pc-windows-msvc'
        $ExpectedSha256 = '886b506c5d995724f426ba730796ab3e9e1fe3291af79e7bea2dfe624f1ff580'
    }
    default {
        throw "The public Code Mode host is not available for Windows architecture $Architecture."
    }
}

$AssetName = "codex-code-mode-host-$Target.exe"
$DownloadUrls = @(
    "https://releases.openai.com/codex/releases/$ReleaseVersion/$AssetName",
    "https://github.com/openai/codex/releases/download/$ReleaseTag/$AssetName"
)
$CacheDir = Join-Path (Join-Path $CacheRoot $ReleaseVersion) $Target
$CachePath = Join-Path $CacheDir $AssetName
$ReleaseStamp = Join-Path (Split-Path -Parent $OutputPath) 'codex-code-mode-host.release.txt'

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

function Test-ExpectedHost {
    param([Parameter(Mandatory = $true)][string]$Path)

    return (
        (Test-Path -LiteralPath $Path -PathType Leaf) -and
        (Get-FileSha256 -Path $Path) -eq $ExpectedSha256
    )
}

function Write-ReleaseStamp {
    $StampDir = Split-Path -Parent $ReleaseStamp
    New-Item -ItemType Directory -Force -Path $StampDir | Out-Null
    Set-Content -LiteralPath $ReleaseStamp -Value @($ReleaseTag, $ExpectedSha256)
}

if (Test-ExpectedHost -Path $OutputPath) {
    Write-ReleaseStamp
    Write-Output "Reusing verified public Code Mode host: $OutputPath ($ReleaseTag)"
    return
}

New-Item -ItemType Directory -Force -Path $CacheDir | Out-Null
$SourcePath = $null
if (Test-ExpectedHost -Path $CachePath) {
    $SourcePath = $CachePath
}
else {
    $InstalledHost = Join-Path $env:LOCALAPPDATA 'codex\bin\codex-code-mode-host.exe'
    if (Test-ExpectedHost -Path $InstalledHost) {
        Write-Output "Seeding the Code Mode host cache from the verified user installation."
        Copy-Item -LiteralPath $InstalledHost -Destination $CachePath -Force
        $SourcePath = $CachePath
    }
}

if (-not $SourcePath) {
    $DownloadPath = "$CachePath.download-$PID"
    Remove-Item -LiteralPath $DownloadPath -Force -ErrorAction SilentlyContinue
    try {
        $LastDownloadError = $null
        foreach ($DownloadUrl in $DownloadUrls) {
            try {
                Write-Output "Downloading public Code Mode host $ReleaseTag from $DownloadUrl"
                Invoke-WebRequest -UseBasicParsing -Uri $DownloadUrl -OutFile $DownloadPath -TimeoutSec 300
                if (-not (Test-ExpectedHost -Path $DownloadPath)) {
                    $ActualSha256 = Get-FileSha256 -Path $DownloadPath
                    throw "Checksum mismatch for $AssetName (expected $ExpectedSha256, got $ActualSha256)."
                }
                Move-Item -LiteralPath $DownloadPath -Destination $CachePath -Force
                $SourcePath = $CachePath
                break
            }
            catch {
                $LastDownloadError = $_
                Remove-Item -LiteralPath $DownloadPath -Force -ErrorAction SilentlyContinue
            }
        }

        if (-not $SourcePath) {
            throw "Could not download and verify $AssetName. Last error: $LastDownloadError"
        }
    }
    finally {
        Remove-Item -LiteralPath $DownloadPath -Force -ErrorAction SilentlyContinue
    }
}

$OutputDir = Split-Path -Parent $OutputPath
New-Item -ItemType Directory -Force -Path $OutputDir | Out-Null
$StagedOutput = "$OutputPath.download-$PID"
Remove-Item -LiteralPath $StagedOutput -Force -ErrorAction SilentlyContinue
try {
    Copy-Item -LiteralPath $SourcePath -Destination $StagedOutput -Force
    if (-not (Test-ExpectedHost -Path $StagedOutput)) {
        throw "The staged public Code Mode host failed checksum verification: $StagedOutput"
    }
    Move-Item -LiteralPath $StagedOutput -Destination $OutputPath -Force
}
finally {
    Remove-Item -LiteralPath $StagedOutput -Force -ErrorAction SilentlyContinue
}

Write-ReleaseStamp
Write-Output "Prepared verified public Code Mode host: $OutputPath ($ReleaseTag)"

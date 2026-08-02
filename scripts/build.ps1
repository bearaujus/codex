# Build this fork's codex binary and prepare the public Code Mode host in
# <repo>\bin. The default profile is the cheapest local edit/build loop.
param(
    [string]$CargoProfile = 'dev-small',
    [Parameter(ValueFromRemainingArguments = $true)][string[]]$CargoArgs
)
$ErrorActionPreference = 'Stop'

$RepoRoot = Split-Path -Parent $PSScriptRoot
$RsDir    = Join-Path $RepoRoot 'codex-rs'
$BinDir   = Join-Path $RepoRoot 'bin'

. (Join-Path $PSScriptRoot 'rust-env.ps1')
Write-Phase "Preparing local build (profile=$CargoProfile)"
Write-Output "Repo root: $RepoRoot"
Write-Output "Rust workspace: $RsDir"
Write-Output "Output bin dir: $BinDir"
if ($CargoProfile -eq 'fast') {
    Enable-LocalFastProfileIncremental
}

New-Item -ItemType Directory -Force -Path $BinDir | Out-Null

Push-Location $RsDir
try {
    $BuildArgs = @('build', '-p', 'codex-cli', '--profile', $CargoProfile)
    if ($CargoArgs) {
        $BuildArgs += $CargoArgs
    }
    $BuildArgs = Add-DefaultCargoParallelism -CargoArgs $BuildArgs -WorkspaceWide:$false

    Invoke-Cargo -CargoArgs $BuildArgs -FailureMessage 'cargo build failed'
}
finally {
    Pop-Location
}

$Src = Join-Path $RsDir "target\$CargoProfile\codex.exe"
$Dst = Join-Path $BinDir 'codex.exe'
$CodeModeHost = Join-Path $BinDir 'codex-code-mode-host.exe'
$ProfileStamp = Join-Path $BinDir 'codex.profile.txt'
Write-Phase "Preparing the public Code Mode host"
& (Join-Path $PSScriptRoot 'prepare-code-mode-host.ps1') -OutputPath $CodeModeHost

Write-Phase "Copying built fork artifact into repo-local bin directory"
Write-Output "Source: $Src"
Write-Output "Destination: $Dst"
Copy-Item -Force -Path $Src -Destination $Dst
Set-Content -Path $ProfileStamp -Value $CargoProfile -NoNewline
Write-Phase "Build completed"
Write-Output "Built $Dst using profile $CargoProfile"
Write-Output "Code Mode host: $CodeModeHost"

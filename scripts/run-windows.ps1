# Build + run the local codex binary, forwarding any args.
# Uses the cheap local profile for a tighter edit/run loop.
$ErrorActionPreference = 'Stop'

$RepoRoot = Split-Path -Parent $PSScriptRoot

& (Join-Path $PSScriptRoot 'build.ps1') -CargoProfile dev-small

$Exe = Join-Path $RepoRoot 'bin\codex.exe'
& $Exe @args

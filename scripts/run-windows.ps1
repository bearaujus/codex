# Build + run the local codex binary, forwarding any args.
$ErrorActionPreference = 'Stop'

$RepoRoot = Split-Path -Parent $PSScriptRoot

& (Join-Path $PSScriptRoot 'build.ps1')

$Exe = Join-Path $RepoRoot 'bin\codex.exe'
& $Exe @args

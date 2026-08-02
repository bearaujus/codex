# Run the already-prepared local codex binary, forwarding any args.
# The Makefile's run target depends on build, so this script stays focused on
# launching the resulting artifact bundle.
$ErrorActionPreference = 'Stop'

$RepoRoot = Split-Path -Parent $PSScriptRoot
$Exe = Join-Path $RepoRoot 'bin\codex.exe'
& $Exe @args

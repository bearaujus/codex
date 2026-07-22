# Shared Rust build environment helpers for local Windows scripts.
#
# These scripts rely on cargo's incremental compilation + the LLD linker
# (configured in codex-rs/.cargo/config.toml) for a fast local edit/build loop.
# There is intentionally no sccache wrapper: it disables incremental compilation
# (the dominant win for repeated single-crate edits) and its server was a
# recurring source of timeouts on Windows.

function Write-Phase {
    param([Parameter(Mandatory = $true)][string]$Message)

    $Timestamp = Get-Date -Format 'HH:mm:ss'
    Write-Output "[$Timestamp] ==> $Message"
}

function Format-CommandArgument {
    param([Parameter(Mandatory = $true)][string]$Argument)

    if ($Argument -match '[\s"]') {
        return '"' + ($Argument -replace '"', '\"') + '"'
    }

    return $Argument
}

function Format-CommandForDisplay {
    param(
        [Parameter(Mandatory = $true)][string]$Executable,
        [Parameter(Mandatory = $true)][string[]]$Arguments
    )

    $RenderedArgs = $Arguments | ForEach-Object { Format-CommandArgument -Argument $_ }
    return (@($Executable) + $RenderedArgs) -join ' '
}

function Test-CargoArgsSpecifyParallelism {
    param([Parameter(Mandatory = $true)][string[]]$CargoArgs)

    for ($i = 0; $i -lt $CargoArgs.Length; $i++) {
        $Arg = $CargoArgs[$i]
        if (
            $Arg -eq '--jobs' `
            -or $Arg -eq '-j' `
            -or $Arg -eq '--build-jobs' `
            -or $Arg -like '--jobs=*' `
            -or $Arg -like '--build-jobs=*'
        ) {
            return $true
        }
    }

    return $false
}

function Test-CargoArgsContainOption {
    param(
        [Parameter(Mandatory = $true)][string[]]$CargoArgs,
        [Parameter(Mandatory = $true)][string[]]$OptionNames
    )

    for ($i = 0; $i -lt $CargoArgs.Length; $i++) {
        $Arg = $CargoArgs[$i]
        foreach ($OptionName in $OptionNames) {
            if ($Arg -eq $OptionName -or $Arg -like "$OptionName=*") {
                return $true
            }
        }
    }

    return $false
}

function Get-DefaultCargoJobs {
    # Default to one rustc job per logical core. With LLD and no sccache there is
    # no reason to leave half the machine idle. Override per-scope via the env
    # vars below, or globally via CARGO_BUILD_JOBS.
    param(
        [Parameter(Mandatory = $true)][string[]]$CargoArgs,
        [Parameter(Mandatory = $true)][bool]$WorkspaceWide
    )

    if ($WorkspaceWide -and $env:CODEX_CARGO_JOBS_WORKSPACE) {
        return $env:CODEX_CARGO_JOBS_WORKSPACE
    }
    if (-not $WorkspaceWide -and $env:CODEX_CARGO_JOBS_SCOPED) {
        return $env:CODEX_CARGO_JOBS_SCOPED
    }

    return [string][Environment]::ProcessorCount
}

function Add-DefaultCargoParallelism {
    param(
        [Parameter(Mandatory = $true)][string[]]$CargoArgs,
        [Parameter(Mandatory = $true)][bool]$WorkspaceWide
    )

    if (Test-Path Env:CARGO_BUILD_JOBS) {
        return $CargoArgs
    }

    if (Test-CargoArgsSpecifyParallelism -CargoArgs $CargoArgs) {
        return $CargoArgs
    }

    $IsNextest = $CargoArgs.Length -ge 2 -and $CargoArgs[0] -eq 'nextest' -and $CargoArgs[1] -eq 'run'
    $ParallelismFlag =
        if ($IsNextest) {
            '--build-jobs'
        }
        else {
            '--jobs'
        }

    $Jobs = Get-DefaultCargoJobs -CargoArgs $CargoArgs -WorkspaceWide:$WorkspaceWide

    $ScopeLabel =
        if ($WorkspaceWide) {
            'workspace-wide'
        }
        else {
            'scoped'
        }
    Write-Host "Using default cargo parallelism: $ParallelismFlag $Jobs ($ScopeLabel Windows run). Override with args or CARGO_BUILD_JOBS."
    if ($IsNextest) {
        $Suffix =
            if ($CargoArgs.Length -gt 2) {
                $CargoArgs[2..($CargoArgs.Length - 1)]
            }
            else {
                @()
            }
        return @($CargoArgs[0], $CargoArgs[1], $ParallelismFlag, $Jobs) + $Suffix
    }

    $Suffix =
        if ($CargoArgs.Length -gt 1) {
            $CargoArgs[1..($CargoArgs.Length - 1)]
        }
        else {
            @()
        }
    return @($CargoArgs[0], $ParallelismFlag, $Jobs) + $Suffix
}

function Add-DefaultNextestVisibility {
    param(
        [Parameter(Mandatory = $true)][string[]]$CargoArgs,
        [Parameter(Mandatory = $true)][bool]$WorkspaceWide
    )

    $IsNextest = $CargoArgs.Length -ge 2 -and $CargoArgs[0] -eq 'nextest' -and $CargoArgs[1] -eq 'run'
    if (-not $IsNextest) {
        return $CargoArgs
    }

    $ExtraArgs = @()
    if (-not (Test-CargoArgsContainOption -CargoArgs $CargoArgs -OptionNames @('--show-progress', '--hide-progress-bar'))) {
        $ExtraArgs += @('--show-progress', 'none')
    }
    if (-not (Test-CargoArgsContainOption -CargoArgs $CargoArgs -OptionNames @('--failure-output'))) {
        $ExtraArgs += @('--failure-output', 'final')
    }
    if (-not (Test-CargoArgsContainOption -CargoArgs $CargoArgs -OptionNames @('--success-output'))) {
        $ExtraArgs += @('--success-output', 'never')
    }
    if (-not (Test-CargoArgsContainOption -CargoArgs $CargoArgs -OptionNames @('--status-level'))) {
        $ExtraArgs += @('--status-level', 'none')
    }
    if (-not (Test-CargoArgsContainOption -CargoArgs $CargoArgs -OptionNames @('--final-status-level'))) {
        $ExtraArgs += @('--final-status-level', 'fail')
    }
    if (-not (Test-CargoArgsContainOption -CargoArgs $CargoArgs -OptionNames @('--cargo-quiet', '--cargo-verbose'))) {
        $ExtraArgs += '--cargo-quiet'
    }

    return $CargoArgs + $ExtraArgs
}

function Add-DefaultCargoTestVisibility {
    param([Parameter(Mandatory = $true)][string[]]$CargoArgs)

    $IsCargoTest = $CargoArgs.Length -ge 1 -and $CargoArgs[0] -eq 'test'
    if (-not $IsCargoTest) {
        return $CargoArgs
    }

    if (Test-CargoArgsContainOption -CargoArgs $CargoArgs -OptionNames @('-q', '--quiet', '-v', '--verbose', '--message-format')) {
        return $CargoArgs
    }

    $Suffix =
        if ($CargoArgs.Length -gt 1) {
            $CargoArgs[1..($CargoArgs.Length - 1)]
        }
        else {
            @()
        }
    return @($CargoArgs[0], '--quiet') + $Suffix
}

function Test-InteractiveCargoProgress {
    param([Parameter(Mandatory = $true)][string[]]$CargoArgs)

    $IsNextest = $CargoArgs.Length -ge 2 -and $CargoArgs[0] -eq 'nextest' -and $CargoArgs[1] -eq 'run'
    if (-not $IsNextest) {
        return $false
    }

    for ($i = 0; $i -lt $CargoArgs.Length; $i++) {
        $Arg = $CargoArgs[$i]
        if ($Arg -eq '--hide-progress-bar' -or $Arg -eq '--show-progress=none') {
            return $false
        }
        if ($Arg -eq '--show-progress' -and $i + 1 -lt $CargoArgs.Length -and $CargoArgs[$i + 1] -eq 'none') {
            return $false
        }
    }

    return $true
}

function Invoke-CargoCommand {
    param(
        [Parameter(Mandatory = $true)][string[]]$CargoArgs
    )

    $PreviousErrorActionPreference = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    $HadTermProgressWhen = Test-Path Env:CARGO_TERM_PROGRESS_WHEN
    $PreviousTermProgressWhen = $env:CARGO_TERM_PROGRESS_WHEN
    $HadTermProgressWidth = Test-Path Env:CARGO_TERM_PROGRESS_WIDTH
    $PreviousTermProgressWidth = $env:CARGO_TERM_PROGRESS_WIDTH
    $UseInteractiveProgress = Test-InteractiveCargoProgress -CargoArgs $CargoArgs
    $Output = [System.Collections.Generic.List[string]]::new()
    try {
        if ($UseInteractiveProgress) {
            $env:CARGO_TERM_PROGRESS_WHEN = 'always'
            if (-not $env:CARGO_TERM_PROGRESS_WIDTH) {
                $env:CARGO_TERM_PROGRESS_WIDTH = '100'
            }
            & cargo @CargoArgs
        }
        else {
            & cargo @CargoArgs 2>&1 | ForEach-Object {
                # PowerShell wraps stderr lines from external processes as ErrorRecord
                # objects when using 2>&1. Convert them back to their message text so
                # the pipeline never emits the bare exception type name.
                $Line = if ($_ -is [System.Management.Automation.ErrorRecord]) {
                    $_.Exception.Message
                } else {
                    [string]$_
                }
                $Output.Add($Line) | Out-Null
                Write-Host $Line
            }
        }
        $ExitCode = $LASTEXITCODE
    }
    finally {
        if ($HadTermProgressWhen) {
            $env:CARGO_TERM_PROGRESS_WHEN = $PreviousTermProgressWhen
        }
        else {
            Remove-Item Env:CARGO_TERM_PROGRESS_WHEN -ErrorAction SilentlyContinue
        }
        if ($HadTermProgressWidth) {
            $env:CARGO_TERM_PROGRESS_WIDTH = $PreviousTermProgressWidth
        }
        else {
            Remove-Item Env:CARGO_TERM_PROGRESS_WIDTH -ErrorAction SilentlyContinue
        }
        $ErrorActionPreference = $PreviousErrorActionPreference
    }

    [pscustomobject]@{
        Output = $Output
        ExitCode = $ExitCode
    }
}

function Invoke-Cargo {
    param(
        [Parameter(Mandatory = $true)][string[]]$CargoArgs,
        [Parameter(Mandatory = $true)][string]$FailureMessage
    )

    # An explicit Cargo CLI override also wins over rustc-wrapper entries in user-level
    # .cargo/config.toml files; clearing environment variables alone does not. Place
    # it after the subcommand so Cargo plugins such as clippy and nextest propagate it
    # to their nested Cargo invocation.
    $ConfigOverride = @('--config', "build.rustc-wrapper=''")
    if ($CargoArgs[0] -eq 'nextest' -and $CargoArgs.Count -gt 1) {
        $RemainingCargoArgs = @()
        if ($CargoArgs.Count -gt 2) {
            $RemainingCargoArgs = $CargoArgs[2..($CargoArgs.Count - 1)]
        }
        $CargoArgs = @($CargoArgs[0], $CargoArgs[1]) + $ConfigOverride + $RemainingCargoArgs
    }
    else {
        $RemainingCargoArgs = @()
        if ($CargoArgs.Count -gt 1) {
            $RemainingCargoArgs = $CargoArgs[1..($CargoArgs.Count - 1)]
        }
        $CargoArgs = @($CargoArgs[0]) + $ConfigOverride + $RemainingCargoArgs
    }
    Write-Phase ("Running " + (Format-CommandForDisplay -Executable 'cargo' -Arguments $CargoArgs))
    $Result = Invoke-CargoCommand -CargoArgs $CargoArgs
    if ($Result.ExitCode -ne 0) {
        throw "$FailureMessage (exit $($Result.ExitCode))"
    }
}

function Enable-LocalFastProfileIncremental {
    if ($env:CARGO_PROFILE_FAST_INCREMENTAL) {
        return
    }

    $env:CARGO_PROFILE_FAST_INCREMENTAL = 'true'
    Write-Output 'Enabled incremental compilation for the local fast profile'
}

function Assert-RustupTargetInstalled {
    param([Parameter(Mandatory = $true)][string]$Target)

    $InstalledTargets = & rustup target list --installed
    if ($LASTEXITCODE -ne 0) {
        throw "rustup target list failed (exit $LASTEXITCODE)"
    }

    if ($InstalledTargets -notcontains $Target) {
        throw "Rust target '$Target' is not installed. Run: rustup target add $Target"
    }
}

function Disable-InheritedRustcWrapper {
    # The user environment may still export RUSTC_WRAPPER=sccache from a previous
    # setup. We deliberately don't use sccache (it disables incremental
    # compilation), so clear any inherited wrapper for this process so cargo runs
    # rustc directly.
    foreach ($Name in @('RUSTC_WRAPPER', 'RUSTC_WORKSPACE_WRAPPER')) {
        if (Test-Path "Env:$Name") {
            Remove-Item "Env:$Name" -ErrorAction SilentlyContinue
            Write-Output "Cleared inherited $Name (sccache is intentionally disabled)"
        }
    }

    # Cargo subcommands such as `cargo clippy` do not reliably propagate a leading
    # inline --config override to their nested Cargo invocation. An explicitly empty
    # config environment value keeps the user-level rustc-wrapper disabled there too.
    $env:CARGO_BUILD_RUSTC_WRAPPER = ''
}

function Get-RustcHostTriple {
    $VersionInfo = & rustc -vV
    foreach ($Line in $VersionInfo) {
        if ($Line -match '^host:\s*(.+)$') {
            return $Matches[1].Trim()
        }
    }
    return $null
}

function Enable-BundledLldLinker {
    # Use the LLD linker bundled with the Rust toolchain instead of the default
    # MSVC link.exe. Linking dominates incremental rebuilds and test-binary builds
    # on Windows, and LLD is typically 2-5x faster.
    #
    # We point cargo at the bundled lld-link.exe via a scoped CARGO_TARGET_*_LINKER
    # env var (process-local) rather than .cargo/config.toml, so this only affects
    # these scripts -- never rust-analyzer or ad-hoc `cargo` runs that may not have
    # the linker resolvable. The self-contained `-Clink-self-contained=+linker`
    # rustflag would be cleaner but is still unstable on the stable toolchain.
    $HostTriple = Get-RustcHostTriple
    if (-not $HostTriple) {
        Write-Warning 'Could not determine rustc host triple; leaving the default linker in place.'
        return
    }

    $LinkerEnvName = 'CARGO_TARGET_' + ($HostTriple.ToUpperInvariant() -replace '[^A-Z0-9]', '_') + '_LINKER'
    if (Test-Path "Env:$LinkerEnvName") {
        return
    }

    # Use the generic rust-lld driver. rustc detects an lld-family linker by path
    # and prepends `-flavor link` (the MSVC flavor); rust-lld.exe accepts that and
    # behaves as lld-link, whereas the pre-specialized gcc-ld\lld-link.exe shim
    # rejects the `-flavor` argument.
    $Sysroot = (& rustc --print sysroot).Trim()
    $RustLld = Join-Path $Sysroot "lib\rustlib\$HostTriple\bin\rust-lld.exe"
    if (-not (Test-Path $RustLld)) {
        Write-Warning "Bundled rust-lld not found at $RustLld; leaving the default MSVC linker in place."
        return
    }

    Set-Item "Env:$LinkerEnvName" $RustLld
    Write-Output "Using bundled LLD linker: $RustLld"
}

# Run once when this helper is dot-sourced so every wrapper script picks up the
# fast, sccache-free, LLD-linked configuration.
Disable-InheritedRustcWrapper
Enable-BundledLldLinker

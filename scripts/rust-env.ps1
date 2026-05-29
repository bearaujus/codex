# Shared Rust build environment helpers for local Windows scripts.

$script:CodexLocalCacheDir =
    if ($env:LOCALAPPDATA) {
        Join-Path $env:LOCALAPPDATA 'codex'
    }
    else {
        Join-Path ([IO.Path]::GetTempPath()) 'codex'
    }
$script:SccacheDisableMarker = Join-Path $script:CodexLocalCacheDir 'sccache-disabled.marker'
$script:SccacheDisableDuration = [TimeSpan]::FromHours(1)

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

function Test-SccacheTemporarilyDisabled {
    if (-not (Test-Path $script:SccacheDisableMarker)) {
        return $false
    }

    $MarkerAge = (Get-Date).ToUniversalTime() - (Get-Item $script:SccacheDisableMarker).LastWriteTimeUtc
    if ($MarkerAge -lt $script:SccacheDisableDuration) {
        return $true
    }

    Remove-Item $script:SccacheDisableMarker -Force -ErrorAction SilentlyContinue
    return $false
}

function Disable-SccacheTemporarily {
    New-Item -ItemType Directory -Force -Path $script:CodexLocalCacheDir | Out-Null
    Set-Content -Path $script:SccacheDisableMarker -Value (Get-Date).ToUniversalTime().ToString('o')
}

function Get-CargoArgsWithoutRustcWrapper {
    param([Parameter(Mandatory = $true)][string[]]$CargoArgs)

    for ($i = 0; $i -lt $CargoArgs.Length; $i++) {
        if ($CargoArgs[$i] -eq '--config' -and $i + 1 -lt $CargoArgs.Length) {
            if ($CargoArgs[$i + 1] -eq "build.rustc-wrapper=''") {
                return $CargoArgs
            }
        }
        elseif ($CargoArgs[$i] -eq "--config=build.rustc-wrapper=''") {
            return $CargoArgs
        }
    }

    return @('--config', "build.rustc-wrapper=''") + $CargoArgs
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

function Get-ScopedCargoJobs {
    if ($env:CODEX_CARGO_JOBS_SCOPED) {
        return $env:CODEX_CARGO_JOBS_SCOPED
    }

    return '8'
}

function Get-WorkspaceCargoJobs {
    if ($env:CODEX_CARGO_JOBS_WORKSPACE) {
        return $env:CODEX_CARGO_JOBS_WORKSPACE
    }

    return '8'
}

function Get-DefaultCargoJobs {
    param(
        [Parameter(Mandatory = $true)][string[]]$CargoArgs,
        [Parameter(Mandatory = $true)][bool]$WorkspaceWide
    )

    if ($WorkspaceWide) {
        return Get-WorkspaceCargoJobs
    }

    return Get-ScopedCargoJobs
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
        [Parameter(Mandatory = $true)][string[]]$CargoArgs,
        [switch]$DisableRustcWrapper
    )

    $PreviousErrorActionPreference = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    $HadRustcWrapper = Test-Path Env:RUSTC_WRAPPER
    $PreviousRustcWrapper = $env:RUSTC_WRAPPER
    $HadCargoBuildRustcWrapper = Test-Path Env:CARGO_BUILD_RUSTC_WRAPPER
    $PreviousCargoBuildRustcWrapper = $env:CARGO_BUILD_RUSTC_WRAPPER
    $HadTermProgressWhen = Test-Path Env:CARGO_TERM_PROGRESS_WHEN
    $PreviousTermProgressWhen = $env:CARGO_TERM_PROGRESS_WHEN
    $HadTermProgressWidth = Test-Path Env:CARGO_TERM_PROGRESS_WIDTH
    $PreviousTermProgressWidth = $env:CARGO_TERM_PROGRESS_WIDTH
    $UseInteractiveProgress = Test-InteractiveCargoProgress -CargoArgs $CargoArgs
    $Output = [System.Collections.Generic.List[string]]::new()
    try {
        if ($DisableRustcWrapper) {
            $env:RUSTC_WRAPPER = ''
            $env:CARGO_BUILD_RUSTC_WRAPPER = ''
            $CargoArgs = Get-CargoArgsWithoutRustcWrapper -CargoArgs $CargoArgs
        }
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
        if ($HadRustcWrapper) {
            $env:RUSTC_WRAPPER = $PreviousRustcWrapper
        }
        else {
            Remove-Item Env:RUSTC_WRAPPER -ErrorAction SilentlyContinue
        }
        if ($HadCargoBuildRustcWrapper) {
            $env:CARGO_BUILD_RUSTC_WRAPPER = $PreviousCargoBuildRustcWrapper
        }
        else {
            Remove-Item Env:CARGO_BUILD_RUSTC_WRAPPER -ErrorAction SilentlyContinue
        }
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

function Enable-SccacheIfAvailable {
    if ($env:RUSTC_WRAPPER -or $env:CARGO_BUILD_RUSTC_WRAPPER) {
        return
    }

    if (Test-SccacheTemporarilyDisabled) {
        Write-Output "Skipping sccache because it recently timed out. Delete $script:SccacheDisableMarker or wait 12h to retry."
        return
    }

    $Sccache = Get-Command sccache -CommandType Application -ErrorAction SilentlyContinue
    if (-not $Sccache) {
        return
    }

    $Wrapper = $Sccache.Source
    $env:RUSTC_WRAPPER = $Wrapper
    $env:CARGO_BUILD_RUSTC_WRAPPER = $Wrapper
    # SCCACHE_NO_DAEMON=1 forces sccache to spin up a fresh one-shot server for
    # every rustc invocation (including cargo's `rustc -vV` probe), which is what
    # was timing out and disabling caching entirely. Use a persistent daemon
    # instead: start it once up front so the first compile connects to a server
    # that is already listening rather than racing its startup.
    Remove-Item Env:SCCACHE_NO_DAEMON -ErrorAction SilentlyContinue
    Start-SccacheServer
    Write-Output "Using sccache wrapper: $Wrapper"
}

function Get-SccacheServerPort {
    if ($env:SCCACHE_SERVER_PORT) {
        return [int]$env:SCCACHE_SERVER_PORT
    }

    return 4226
}

function Test-SccacheServerListening {
    $Port = Get-SccacheServerPort
    $Listener = Get-NetTCPConnection -State Listen -LocalPort $Port -ErrorAction SilentlyContinue
    return [bool]$Listener
}

function Invoke-SccacheControl {
    param(
        [Parameter(Mandatory = $true)][string]$Action,
        [switch]$Quiet
    )

    # sccache control commands write to stderr and exit non-zero in benign cases
    # (e.g. "couldn't connect to server" when none is running, "Address in use"
    # when one already is). Isolate them from the caller's
    # $ErrorActionPreference = 'Stop' so they can never abort the build, and
    # surface their output for visibility (via Write-Host so this helper never
    # pollutes the success/pipeline stream) unless -Quiet is requested.
    $PreviousErrorActionPreference = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try {
        & sccache $Action 2>&1 | ForEach-Object {
            if (-not $Quiet) {
                Write-Host "$_"
            }
        }
    }
    catch {
        if (-not $Quiet) {
            Write-Host "sccache $Action failed (ignored): $($_.Exception.Message)"
        }
    }
    finally {
        $ErrorActionPreference = $PreviousErrorActionPreference
    }
}

function Get-RustcPathForProbe {
    $Rustc = Get-Command rustc -CommandType Application -ErrorAction SilentlyContinue
    if ($Rustc) {
        return $Rustc.Source
    }

    return $null
}

function Test-SccacheCompilePathReady {
    # The decisive readiness check: does the *compile* request path answer? A
    # wedged/zombie sccache server can keep its control port listening (so
    # --show-stats / --stop-server still work) while every `sccache rustc ...`
    # request times out with "Timed out waiting for server startup" -- which is
    # exactly what cargo issues. Mirror cargo by running `sccache rustc -vV`
    # (a fast, special-cased version query) and trust only its exit code.
    $Rustc = Get-RustcPathForProbe
    if (-not $Rustc) {
        # Can't run the authoritative probe; fall back to a liveness signal.
        return (Test-SccacheServerListening)
    }

    $PreviousErrorActionPreference = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try {
        & sccache $Rustc -vV 2>&1 | Out-Null
        return ($LASTEXITCODE -eq 0)
    }
    catch {
        return $false
    }
    finally {
        $ErrorActionPreference = $PreviousErrorActionPreference
    }
}

function Start-SccacheServer {
    # Goal: leave a sccache daemon that actually answers *compile* requests
    # before cargo runs, so cargo's first `sccache rustc -vV` probe connects
    # instead of timing out on a wedged server or racing an auto-spawned one.
    #
    # "Port is listening" is NOT sufficient: a zombie server can listen yet time
    # out every compile request. So readiness is defined by the compile path
    # (Test-SccacheCompilePathReady). If a healthy server is already up, reuse it
    # untouched (and avoid a needless restart race). Otherwise force the clean
    # stop -> start that reliably produces a working server, then re-verify.
    if ((Test-SccacheServerListening) -and (Test-SccacheCompilePathReady)) {
        Write-Output 'Reusing healthy sccache server'
        return
    }

    Write-Output 'Starting a fresh sccache server (no healthy server detected)'
    Invoke-SccacheControl -Action '--stop-server' -Quiet
    # A zombie can ignore --stop-server while still holding the port; clear any
    # stragglers so the fresh --start-server can bind 127.0.0.1:<port>.
    Get-Process sccache -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
    Start-Sleep -Milliseconds 400
    Invoke-SccacheControl -Action '--start-server'

    if (-not (Test-SccacheCompilePathReady)) {
        Write-Warning 'sccache server is not answering compile requests; the build will fall back to no sccache if it times out'
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

function Invoke-CargoWithSccacheFallback {
    param(
        [Parameter(Mandatory = $true)][string[]]$CargoArgs,
        [Parameter(Mandatory = $true)][string]$FailureMessage
    )

    Write-Phase ("Running " + (Format-CommandForDisplay -Executable 'cargo' -Arguments $CargoArgs))
    $Result = Invoke-CargoCommand `
        -CargoArgs $CargoArgs `
        -DisableRustcWrapper:(Test-SccacheTemporarilyDisabled)
    $Output = $Result.Output
    $ExitCode = $Result.ExitCode
    if ($ExitCode -eq 0) {
        return
    }

    $OutputText = $Output | Out-String
    if ($OutputText -match 'sccache: error: Timed out waiting for server startup') {
        Write-Warning 'sccache timed out starting; stopping any stale server and retrying once with sccache'
        # `sccache --stop-server` writes to stderr and exits non-zero when no
        # server is running. Under the caller's $ErrorActionPreference = 'Stop'
        # that stderr line is promoted to a terminating NativeCommandError, which
        # would abort the whole fallback. Isolate it so it can never terminate.
        $PreviousStopErrorActionPreference = $ErrorActionPreference
        $ErrorActionPreference = 'Continue'
        try {
            & sccache --stop-server 2>&1 | ForEach-Object {
                Write-Host "$_"
            }
        }
        catch {
            Write-Host "sccache --stop-server failed (ignored): $($_.Exception.Message)"
        }
        finally {
            $ErrorActionPreference = $PreviousStopErrorActionPreference
        }

        Write-Phase ("Retrying " + (Format-CommandForDisplay -Executable 'cargo' -Arguments $CargoArgs) + ' after sccache reset')
        $RetryResult = Invoke-CargoCommand -CargoArgs $CargoArgs
        $RetryOutput = $RetryResult.Output
        $RetryExitCode = $RetryResult.ExitCode
        if ($RetryExitCode -eq 0) {
            return
        }

        $RetryOutputText = $RetryOutput | Out-String
        if ($RetryOutputText -match 'sccache: error: Timed out waiting for server startup') {
            Write-Warning 'sccache still timed out after reset; retrying cargo without sccache'
            Disable-SccacheTemporarily
            Remove-Item Env:RUSTC_WRAPPER -ErrorAction SilentlyContinue
            Remove-Item Env:CARGO_BUILD_RUSTC_WRAPPER -ErrorAction SilentlyContinue
            Write-Phase ("Retrying " + (Format-CommandForDisplay -Executable 'cargo' -Arguments (Get-CargoArgsWithoutRustcWrapper -CargoArgs $CargoArgs)))
            $NoWrapperResult = Invoke-CargoCommand -CargoArgs $CargoArgs -DisableRustcWrapper
            $NoWrapperOutput = $NoWrapperResult.Output
            $NoWrapperExitCode = $NoWrapperResult.ExitCode
            if ($NoWrapperExitCode -eq 0) {
                return
            }

            $ExitCode = $NoWrapperExitCode
        }
        else {
            $ExitCode = $RetryExitCode
        }
    }

    throw "$FailureMessage (exit $ExitCode)"
}

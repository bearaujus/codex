# Shared Rust build environment helpers for local Windows scripts.

function Enable-SccacheIfAvailable {
    if ($env:RUSTC_WRAPPER -or $env:CARGO_BUILD_RUSTC_WRAPPER) {
        return
    }

    $Sccache = Get-Command sccache -CommandType Application -ErrorAction SilentlyContinue
    if (-not $Sccache) {
        return
    }

    $Wrapper = $Sccache.Source
    $env:RUSTC_WRAPPER = $Wrapper
    $env:CARGO_BUILD_RUSTC_WRAPPER = $Wrapper
    Write-Output "Using sccache wrapper: $Wrapper"
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

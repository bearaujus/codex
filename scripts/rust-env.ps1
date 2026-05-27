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

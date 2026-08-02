# Installing and building this fork

Official OpenAI installers, Homebrew, npm, and upstream GitHub release binaries
install the upstream Codex build. They do not contain this fork's custom
features. Build this repository from source to run the fork.

## Supported repository workflow

The root `Makefile` invokes `powershell.exe` and Windows-specific helper scripts,
so the documented fork workflow currently targets native Windows 10 or 11.
The Rust workspace retains upstream platform-aware code, but this repository
does not provide equivalent fork install helpers for macOS or Linux.

## Prerequisites

- Git
- [Rustup](https://rustup.rs/) and Cargo
- Visual Studio 2022 Build Tools with the C++ workload and a Windows SDK
- GNU Make, available as `make`
- PowerShell 5.1 or newer

The workspace pins Rust in
[`codex-rs/rust-toolchain.toml`](../codex-rs/rust-toolchain.toml), including the
`clippy` and `rustfmt` components. Some native dependencies may also require
CMake and LLVM/Clang.

Install nextest before running normal test targets:

```powershell
cargo install --locked cargo-nextest
```

Contributors who update TUI snapshots also need `cargo-insta`:

```powershell
cargo install --locked cargo-insta
```

## Clone and build

Run all repository automation from the repository root:

```powershell
git clone https://github.com/bearaujus/codex.git
Set-Location codex
make build
```

`make build` compiles this fork's `codex-cli` crate with the lightweight
`dev-small` profile and prepares these repository-local executables:

- `bin\codex.exe`, built from this repository
- `bin\codex-code-mode-host.exe`, downloaded from the pinned public OpenAI
  Codex release and verified with its SHA-256 digest

The split is intentional. The upstream Code Mode host links a pointer-compressed,
sandboxed V8 build published through OpenAI's release pipeline; the normal
`denoland/rusty_v8` release does not publish that Windows archive. Using the
verified public host keeps this fork's normal build from compiling V8 while the
CLI itself still contains all fork changes.

Launch that build directly:

```powershell
.\bin\codex.exe
```

Or rebuild and launch it in one step:

```powershell
make run
```

## Install on the user `PATH`

```powershell
make install
```

The install target depends on `make build`, so Cargo runs once and installation
only copies the completed bundle. It installs both `codex.exe` and
`codex-code-mode-host.exe` to `%LOCALAPPDATA%\codex\bin`, then puts that
directory first on the user `PATH`. Open a new terminal after the first install,
then verify which binary is active:

```powershell
Get-Command codex
codex --version
Get-Item (Join-Path $env:LOCALAPPDATA 'codex\bin\codex-code-mode-host.exe')
```

The public host is pinned and checksum-verified by
[`scripts/prepare-code-mode-host.ps1`](../scripts/prepare-code-mode-host.ps1).
An already verified download is reused from the user cache. Updating the pin
should be done together with an upstream merge and a Code Mode compatibility
check.

## Development loop

Use the root targets instead of invoking `cargo test` directly:

```powershell
make fmt
make check p=codex-tui
make test p=codex-tui
make lint p=codex-tui
```

Prefer `p=<crate>` for checks, tests, and linting. The `fmt`, `check`, `test`,
and `lint` targets also accept `args="<cargo flags>"`. For example:

```powershell
make test p=codex-login args="--no-run"
make lint p=codex-tui args="--features foo"
```

Run lint last after formatting and scoped tests. A workspace-wide build or test
compiles many crates and should be reserved for changes that require that scope.

Use `make clean` to remove the Cargo target directory and the repository-local
executable bundle. The verified public-host download remains in the user cache
so the next build can reuse it.

## Tracing and verbose logging

Codex honors `RUST_LOG`. The TUI stores bounded diagnostics by default; set
`log_dir` when you need a plaintext log:

```powershell
codex -c 'log_dir="./.codex-log"'
```

In a second terminal:

```powershell
Get-Content .\.codex-log\codex-tui.log -Wait
```

The non-interactive `codex exec` mode defaults to `RUST_LOG=error` and prints
messages inline.

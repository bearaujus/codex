> **This is a patched fork of [openai/codex](https://github.com/openai/codex).**
> The build, installation, authentication, and upstream-sync instructions below
> describe this fork.

# Codex CLI

Codex CLI is a coding agent from OpenAI that runs locally on your computer.
This fork keeps the upstream Rust product while carrying custom account-pool,
runtime, and Windows development changes on `main`.

## Branch layout

| Branch | Purpose |
| --- | --- |
| `main` | Fork development and custom patches, with upstream changes merged in |
| `origin/upstream` | Mirror of `openai/codex` `main`, without fork-only commits |

## Fork differences

This tree intentionally differs from the upstream repository:

- ChatGPT user authentication is account-pool only. API key, personal access
  token, Bedrock key, and top-level CLI login surfaces are not supported.
- The npm/pnpm workspace and root JavaScript package are removed.
- The TypeScript and Python SDK packages under `sdk/` are removed.
- The upstream root `justfile` and Bazel workspace are removed. Use the
  repository-root `Makefile` and `scripts/*.ps1`.

Official OpenAI installers, Homebrew, npm, and upstream release binaries install
the upstream build; they do not contain this fork's changes.

## Build and run this fork

The repository automation currently targets native Windows.

Prerequisites:

- Git
- [Rustup](https://rustup.rs/) and Cargo
- Visual Studio 2022 Build Tools with the C++ workload and Windows SDK
- GNU Make
- PowerShell 5.1 or newer

```powershell
git clone https://github.com/bearaujus/codex.git
Set-Location codex
make build
.\bin\codex.exe
```

Use `make run` for the local edit/run loop or `make install` to install
`codex.exe` together with `codex-code-mode-host.exe` under
`%LOCALAPPDATA%\codex\bin` and put that directory first on your user `PATH`.
See [Installing and building](docs/install.md) for setup and verification
details.

## Authentication

Provision one or more ChatGPT accounts through a client that drives the
app-server browser or device-code login flow before running the CLI. The CLI
does not expose a top-level user login command.

The CLI and the companion `codex-accounts` service share
`<CODEX_HOME>/account-pool/accounts.sqlite`. Codex selects an eligible account
at turn boundaries and can fail over after authentication failures or Codex
rate limits.

See [Authentication in this fork](docs/authentication.md) for the supported
flows, account-pool behavior, and the distinction between user authentication,
MCP OAuth, and infrastructure identity.

## Development commands

| Command | Description |
| --- | --- |
| `make build` | Build the fork CLI and prepare its verified public Code Mode host under `bin\` |
| `make run` | Build and run the repository-local binary |
| `make install` | Build once, then install the CLI and Code Mode host to the user `PATH` |
| `make check` | Run a fast Cargo check |
| `make fmt` | Format the Rust workspace |
| `make test` | Run tests with nextest |
| `make lint` | Run Clippy with repository deny lints |
| `make clean` | Remove Cargo targets and the repository-local executable bundle |

The `fmt`, `check`, `test`, and `lint` targets accept `p=<crate>` and
`args="<flags>"`:

```powershell
make fmt p=codex-login args="--check"
make check p=codex-login
make test p=codex-login args="--no-run"
make lint p=codex-tui args="--features foo"
```

For Rust changes, format first, run checks and scoped tests, then run the scoped
lint last.

## Merge upstream into `main`

Configure the OpenAI repository as the `upstream` remote once:

```powershell
git remote add upstream https://github.com/openai/codex.git
```

Refresh the fork's mirror branch without adding fork commits to it:

```powershell
git fetch upstream
git push origin refs/remotes/upstream/main:refs/heads/upstream
git fetch origin
```

Merge the mirrored branch into `main` so both histories remain visible:

```powershell
git switch main
git merge --no-ff origin/upstream
```

Resolve conflicts in favor of the fork's account-pool and root Makefile
architecture while retaining compatible upstream features. Run verification
appropriate to the affected crates, with lint last, before publishing:

```powershell
make fmt
make check p=codex-tui
make test p=codex-tui
make lint p=codex-tui
git push origin main
```

Replace `codex-tui` with each affected crate, or use the workspace-wide target
when the change genuinely requires it.

## Documentation

- [Getting started](docs/getting-started.md)
- [Installing and building](docs/install.md)
- [Authentication](docs/authentication.md)
- [Configuration](docs/config.md)
- [App-server API](codex-rs/app-server/README.md)
- [Official Codex documentation](https://developers.openai.com/codex)

The official documentation describes upstream product features. Use this
repository's installation and authentication documents where the fork differs.

This repository is licensed under the [Apache-2.0 License](LICENSE).

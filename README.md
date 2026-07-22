> **This is a patched fork of [openai/codex](https://github.com/openai/codex).** The sections below document the fork workflow and build system. The original upstream README follows.

---

## Fork Layout

| Branch | Purpose |
|---|---|
| `main` | Your working branch — custom patches on top of upstream |
| `upstream` | Mirrors `openai/codex` main, no custom commits |

## What this fork removes

This tree intentionally drops several upstream packaging surfaces:

- No npm / pnpm workspace (`package.json`, `codex-cli/`, root JS package)
- No TypeScript or Python SDK packages under `sdk/`
- No upstream root `justfile` / Bazel CI workspace; use the repo-root `Makefile` and `scripts/*.ps1` instead

Install and run Codex from this fork with `make build`, `make run`, or `make install` (see below). Upstream `npm install -g @openai/codex` instructions in the preserved README sections do not apply to this repository.

## Prerequisites (Windows)

- [Rust toolchain](https://rustup.rs/) (`rustup`, `cargo`)
- `make` (via [Chocolatey](https://chocolatey.org/): `choco install make`, or [GnuWin32](https://gnuwin32.sourceforge.net/packages/make.htm))
- PowerShell 5.1+

## Make Commands

| Command | Description |
|---|---|
| `make build` | Quick dev build into `./bin` (small profile, fast compile) |
| `make run` | Build + run the local binary |
| `make install` | Build + install to `%LOCALAPPDATA%\codex\bin` (adds to PATH) |
| `make check` | Cargo check only — fastest inner loop, no binary output |
| `make lint` | Clippy with deny lints — run before committing |
| `make fmt` | Format the Rust workspace |
| `make test` | Run tests with nextest |
| `make clean` | `cargo clean` (whole `target/`, all profiles) + remove the repo-local binary |

All commands accept optional `p=<crate>` and `args=<flags>` to narrow scope, e.g.:

```
make check p=codex-login
make test p=codex-login args="--no-run"
make lint p=codex-tui args="--features foo"
```

## Daily Workflow

### Sync upstream into the `upstream` branch

```
git fetch upstream
git checkout upstream
git merge upstream/main --ff-only
git push origin upstream
```

### Rebase your work onto the latest upstream

```
git checkout main
git rebase origin/upstream
git push --force-with-lease origin main
```

### Dev loop

```
make check          # fast compile feedback
make lint           # before committing
make test           # run test suite
make build          # build local binary
make run            # build + run
make install        # install to PATH
```

---

<p align="center"><strong>Codex CLI</strong> is a coding agent from OpenAI that runs locally on your computer.
<p align="center">
  <img src="https://github.com/openai/codex/blob/main/.github/codex-cli-splash.png" alt="Codex CLI splash" width="80%" />
</p>
</br>
If you want Codex in your code editor (VS Code, Cursor, Windsurf), <a href="https://developers.openai.com/codex/ide">install in your IDE.</a>
</br>If you want the desktop app experience, run <code>codex app</code> or visit <a href="https://chatgpt.com/codex?app-landing-page=true">the Codex App page</a>.
</br>If you are looking for the <em>cloud-based agent</em> from OpenAI, <strong>Codex Web</strong>, go to <a href="https://chatgpt.com/codex">chatgpt.com/codex</a>.</p>

---

## Quickstart

### Installing and running Codex CLI

Run the following on Mac or Linux to install Codex CLI:

```shell
curl -fsSL https://chatgpt.com/codex/install.sh | sh
```

Run the following on Windows to install Codex CLI:

```shell
powershell -ExecutionPolicy ByPass -c "irm https://chatgpt.com/codex/install.ps1 | iex"
```

Codex CLI can also be installed via the following package managers:

```shell
# Install using npm
npm install -g @openai/codex
```

```shell
# Install using Homebrew
brew install --cask codex
```

Then simply run `codex` to get started.

<details>
<summary>You can also go to the <a href="https://github.com/openai/codex/releases/latest">latest GitHub Release</a> and download the appropriate binary for your platform.</summary>

Each GitHub Release contains many executables, but in practice, you likely want one of these:

- macOS
  - Apple Silicon/arm64: `codex-aarch64-apple-darwin.tar.gz`
  - x86_64 (older Mac hardware): `codex-x86_64-apple-darwin.tar.gz`
- Linux
  - x86_64: `codex-x86_64-unknown-linux-musl.tar.gz`
  - arm64: `codex-aarch64-unknown-linux-musl.tar.gz`

Each archive contains a single entry with the platform baked into the name (e.g., `codex-x86_64-unknown-linux-musl`), so you likely want to rename it to `codex` after extracting it.

</details>

### Using Codex with your ChatGPT plan

This fork authenticates only through the ChatGPT account pool. Configure ChatGPT
accounts outside the CLI (for example via app-server OAuth or device-code login)
before running `codex`, then use your ChatGPT plan as usual. [Learn more about what's included in your ChatGPT plan](https://help.openai.com/en/articles/11369540-codex-in-chatgpt).

API key, personal access token, and Bedrock API key sign-in are not supported.

## Docs

- [**Codex Documentation**](https://developers.openai.com/codex)
- [**Contributing**](./docs/contributing.md)
- [**Installing & building**](./docs/install.md)
- [**Open source fund**](./docs/open-source-fund.md)

This repository is licensed under the [Apache-2.0 License](LICENSE).

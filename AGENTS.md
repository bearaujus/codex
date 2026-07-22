# Repository instructions

This root `AGENTS.md` is the only source of truth for repository-specific agent instructions. Do not create or rely on nested `AGENTS.md` files or repository-local `.codex` instructions.

## Repository

- Rust lives in `codex-rs/`; workspace crate names use the `codex-` prefix.
- This fork uses Cargo plus the root `Makefile` and `scripts/*.ps1`. Do not reintroduce Bazel, the upstream `justfile`, npm packaging, or removed SDK trees unless explicitly requested.
- Keep behavior portable across Linux, macOS, and Windows unless it is intentionally platform-specific.
- Preserve unrelated user changes and avoid destructive Git or filesystem operations.

## Required workflow

- Prefer `rg`/`rg --files` for searching. Install missing repository tools such as `make`, `cargo-nextest`, or `cargo-insta` when needed.
- Run root `make ...` commands outside the sandbox.
- After Rust edits, run `make fmt` from the repository root.
- Run scoped tests with `make test p=<crate>`; never run `cargo test` directly.
- If common, core, or protocol changes, ask before running the complete `make test` workspace suite.
- Before finalizing a large Rust change, run `make lint p=<crate>`. Keep lint last; do not rerun tests after lint or formatting.
- For user-visible TUI changes, add or update `insta` snapshots, review them, accept intended changes, and leave no `.snap.new` files.
- When the user explicitly asks to push, commit the completed in-scope work after required verification and push it. If the user names target commits, amend the matching changes into those commits instead of adding follow-up commits.

## Rust conventions

- Inline `format!` arguments, collapse nested `if` statements, and use method references instead of redundant closures.
- Avoid opaque boolean or `Option` parameters. If a positional literal is unavoidable, annotate it with the exact `/*parameter_name*/` expected by the argument-comment lint.
- Prefer exhaustive `match` expressions, private modules, small crate APIs, and whole-object equality assertions with `pretty_assertions::assert_eq`.
- New traits need role/implementation documentation. Prefer native RPITIT methods with explicit `Send` futures; do not add `async_trait` or `allow(async_fn_in_trait)` shortcuts.
- Instrument async function definitions with `#[tracing::instrument(...)]`; do not attach spans at call sites when the callee is already instrumented.
- Avoid one-use helpers. Target modules below 500 lines; move new functionality out of files approaching 800 lines, especially central TUI and core orchestration files.
- Use `Path`/`PathBuf` for OS paths and maintain cross-platform tests.

## Architecture and safety

- Resist adding concepts to `codex-core`; prefer an existing focused crate or a new crate when appropriate.
- ChatGPT user auth is account-pool based. Do not restore removed API-key, personal-token, Bedrock-key, or CLI login surfaces without explicit direction.
- Do not modify code related to `CODEX_SANDBOX_NETWORK_DISABLED_ENV_VAR` or `CODEX_SANDBOX_ENV_VAR`.
- For MCP tool mutation/calls, prefer `codex-rs/codex-mcp/src/mcp_connection_manager.rs`. Do not call `reset_client_session` unless incremental checks require it.
- Model-visible context must be incremental, bounded, cache-stable, below 10K tokens per item, and represented by `core/context` types implementing `ContextualUserFragment`. Flag new fragments above 1K tokens as P0 review items.
- Review external compatibility for app-server APIs, raw response events, CLI flags, config loading, and rollout/session resume behavior.

## APIs, schemas, and tests

- Develop app-server API changes in v2 only. Use camelCase wire fields/enum values, matching serde/TS renames, explicit tagged unions, plain string IDs, and Unix-second `*_at` timestamps.
- V2 request optional fields use `#[ts(optional = nullable)]`; v2 payloads must not use `skip_serializing_if` except the established no-params request exception. New list APIs use cursor pagination.
- After `ConfigToml` changes, run `cargo run -p codex-core --bin codex-write-config-schema` from `codex-rs`.
- After app-server protocol changes, update `app-server/README.md`, run `cargo run -p codex-app-server-protocol --bin write_schema_fixtures`, and test `codex-app-server-protocol`.
- Agent logic changes require integration coverage under `core/tests/suite`; prefer `build_with_auto_env()`, structured response helpers, and `wait_for_event`.
- New test modules belong in sibling `*_tests.rs` files via an explicit `#[path = "..."]` attribute. Avoid tests for static values, removed behavior, or test-only production APIs.
- Do not add general product documentation under `docs/`; app-server API documentation is the exception.

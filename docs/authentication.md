# Authentication in this fork

This fork uses ChatGPT account-pool authentication for Codex users. It does not
support API key, personal access token, Bedrock API key, or external ChatGPT
token sign-in, and the top-level CLI user login/logout commands are intentionally
absent.

## Provision ChatGPT accounts

A client integrating `codex-app-server` should provision accounts through the v2
account API:

- `account/read` checks the current account state.
- `account/login/start` with `type: "chatgpt"` starts browser login.
- `account/login/start` with `type: "chatgptDeviceCode"` starts device-code login.
- `account/login/completed` reports whether the attempt succeeded.
- `account/login/cancel` cancels a pending attempt.
- `account/logout` removes the current user authentication.

Completed logins are registered in the shared ChatGPT account pool. See the
[app-server auth endpoints](../codex-rs/app-server/README.md#auth-endpoints) for
the request and response schemas.

Provision at least one account through that app-server flow before launching
`codex`. The CLI consumes account-pool state; it is not the user-auth
provisioning surface.

## Account-pool storage and selection

The CLI and the companion `codex-accounts` service share:

```text
<CODEX_HOME>/account-pool/accounts.sqlite
```

At each turn boundary, Codex chooses from enabled accounts that have usable
authentication and are not on active cooldown. A live turn lease keeps the
chosen account stable during that turn. Authentication failures and Codex 429
responses can move subsequent work to another eligible account.

`codex-accounts` owns background usage polling and token maintenance. Codex
reads the persisted rate-limit snapshot instead of issuing a competing
background `/usage` poll. Explicit backend blocking state, request-side Codex
429s, spend control, and exhausted individual limits remain authoritative.

The account-pool database contains authentication state. Protect
`CODEX_HOME` with the same care as other local credentials and do not commit it
to source control.

## Other authentication surfaces

These similarly named flows do not sign a user into Codex:

- MCP OAuth, including `codex mcp login`, authenticates an individual MCP
  server.
- Agent Identity and provider headers are infrastructure authentication for
  remote execution or managed environments. For example, `codex exec-server`
  can opt into an Agent Identity JWT supplied through `CODEX_ACCESS_TOKEN` with
  `--use-agent-identity-auth`.

See the [exec-server documentation](../codex-rs/exec-server/README.md) for that
infrastructure-only path.

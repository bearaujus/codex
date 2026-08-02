# Core protocol v1

The core protocol types are defined in
[`protocol.rs`](../protocol/src/protocol.rs), and the current thread conduit is
implemented by [`CodexThread`](../core/src/codex_thread.rs).

This document describes the in-process submission/event protocol between a
Codex core thread and its caller. It is not the stable external app-server API;
external clients should use the
[app-server v2 protocol](../app-server/README.md).

## Entities

1. **Model**
   - The model service used by Codex, normally the Responses API.
2. **`CodexThread`**
   - The bidirectional conduit for one Codex conversation.
   - Accepts `Submission` values and emits correlated `Event` values.
3. **Session**
   - Persistent configuration, conversation state, rollout state, and tool
     state owned by a thread.
   - Thread settings can be changed without starting a turn.
4. **Turn**
   - A unit of model-backed work installed for a user message.
   - A later `Op::UserInput` can be admitted into an already-running turn as
     steering input.
   - May contain several model requests and tool iterations before it reaches a
     terminal event.

A UI can be the TUI, CLI, app-server, an editor integration, or another caller
that owns a `CodexThread`. Use a separate thread for independent concurrent
conversations.

## Submission and event queues

`CodexThread` communicates through a submission queue (caller to core) and an
event queue (core to caller).

### `Submission`

A submission contains:

- `id`: a caller-provided identifier used to correlate events.
- `op`: an `Op` payload.
- `client_user_message_id`: an optional client identifier for a user message.
- `trace`: optional W3C trace context.
- `parent_turn_id`: the core-provided parent turn for derived submissions.

`Op` is `non_exhaustive`. Callers must tolerate new variants. Common operations
include:

- `Op::UserInput`: start or steer a turn with input items, optional output
  schema and metadata, additional context, and persistent thread-setting
  overrides.
- `Op::ThreadSettings`: apply persistent settings without starting a turn.
- `Op::Interrupt`: abort the active turn without terminating background
  terminals.
- `Op::ExecApproval` and `Op::PatchApproval`: answer approval requests.
- `Op::UserInputAnswer` and `Op::RequestPermissionsResponse`: answer model tool
  requests that require user input or permissions.
- `Op::ResolveElicitation` and `Op::DynamicToolResponse`: resolve MCP and
  application-provided tool requests.
- `Op::RefreshMcpServers`, `Op::Compact`, `Op::Review`, and
  `Op::RunUserShellCommand`: request thread-level actions.
- `Op::Shutdown`: shut down the thread.

`Op::UserInput` accepts these `UserInput` item types:

- `text` with optional UI-defined text elements.
- `image` and `local_image`, with optional image detail.
- `audio` and `local_audio`.
- `skill` with a skill name and `SKILL.md` path.
- `mention` for an app, connector, plugin, or other structured mention target.

Thread-setting overrides include the working environment, approval and sandbox
policies, permission profile, model, reasoning options, service tier,
collaboration mode, and personality.

### `Event`

An event contains:

- `id`: the submission identifier with which the event is correlated.
- `msg`: an `EventMsg` payload.

Important `EventMsg` lifecycle and presentation variants include:

- `SessionConfigured` and `ThreadSettingsApplied`.
- `TurnStarted`, which supplies the turn ID and available start metadata.
- `AgentMessage` plus text, reasoning, and plan delta events.
- `ExecCommandBegin`, `ExecCommandOutputDelta`, and `ExecCommandEnd`.
- `PatchApplyBegin`, `PatchApplyUpdated`, and `PatchApplyEnd`.
- Approval, permissions, user-input, dynamic-tool, and MCP elicitation requests.
- `TokenCount`, `TurnDiff`, `Warning`, and `Error`.
- `TurnComplete` for a terminal turn result, or `TurnAborted` after an
  interruption.

The v1 wire names for `TurnStarted` and `TurnComplete` remain `task_started` and
`task_complete`. Deserialization also accepts `turn_started` and
`turn_complete` for compatibility.

`TurnCompleteEvent` contains the turn ID, the optional last agent message,
optional terminal error details, and available timing metadata. It does not
carry a Responses API bookmark; callers should use the thread and rollout
interfaces for continuation and resume behavior.

## Turn lifecycle

When no turn is active, the normal lifecycle is:

1. The caller submits `Op::UserInput`.
2. Core emits `EventMsg::TurnStarted`.
3. Core calls the model and emits streaming output and tool lifecycle events.
4. If an action needs a decision, core emits a request event and waits for the
   matching response operation.
5. Model/tool iterations continue until the turn finishes.
6. Core emits `EventMsg::TurnComplete`, `TurnAborted`, or an error-bearing
   terminal result.

`Op::ThreadSettings` can update persistent settings independently; core
acknowledges the applied snapshot with `ThreadSettingsApplied`. User messages
received while work is active are handled by the thread's admission policy
rather than implicitly being treated as a legacy “start a new task” operation.

## Transport

The core `Submission` and `Op` types are primarily in-process Rust interfaces,
not a stable serde wire contract. `Event` values serialize for rollout and
adapter use, but external transports must use the contract owned by their
adapter. In particular, app-server clients should use the generated app-server
v2 schemas instead of serializing core `Op` values directly.

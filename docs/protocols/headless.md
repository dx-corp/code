# Headless Protocol Reference

> **Status:** This document predates the Rust-only runtime migration (#3016, #3017, merged 2026-07-22), which deleted Maestro's TypeScript agent runtime and SDK. Headless protocol dispatch now lives in `packages/tui-rs/src/headless/`. Some file paths below may be stale; they are kept for design context and updated only where a corresponding Rust module was confirmed.


Maestro's headless mode is the embedding-oriented JSON-over-stdio contract used
by native TUIs and external control planes such as EvalOps Chat.

Transport rules:

- stdin carries one JSON object per line into Maestro
- stdout emits one JSON object per line back to the client
- stderr is diagnostics only and is not part of the protocol contract
- startup failures should emit a fatal `error` protocol message on stdout when
  the headless transport has been requested, with human diagnostics on stderr

## Compatibility

The protocol is versioned. The runtime sends the version in `ready` and
`hello_ok`, and clients may send their version in `hello`.

Current version: `2026-08-07`

Source of truth:

- generated constants: [packages/local-host-rs/src/headless/generated_protocol.rs](../../packages/local-host-rs/src/headless/generated_protocol.rs)
- runtime message shapes: [src/cli/headless-protocol.ts](../../src/cli/headless-protocol.ts)
- transport implementation: [packages/local-host-rs/src/headless_server.rs](../../packages/local-host-rs/src/headless_server.rs)
- hosted runner contract: [docs/protocols/hosted-runner-contract.md](./hosted-runner-contract.md)
- hosted runner retention: [docs/protocols/hosted-runner-retention.md](./hosted-runner-retention.md)
- conformance suite: [docs/protocols/headless-conformance.md](./headless-conformance.md)

Compatibility expectations:

- treat unknown fields as additive
- reject unknown message `type` values unless your client intentionally ignores them
- compare `protocol_version` during handshake when you require exact compatibility
- versioned clients must currently match `2026-08-07`; the runtime does not
  downgrade emitted message types for older clients

## Hosted / HeadlessRuntimeService backend

`HeadlessRuntimeService` (web attach, hosted runner, in-process host) owns the
Node-side control plane: connection leases, hello/hello_ok, utility commands,
file watches, server-request bookkeeping, and event replay. The **agent loop**
uses a long-lived piped `maestro-tui --headless` process
(`NativeHeadlessClient`).

| Flag | Effect |
|------|--------|
| `MAESTRO_TUI_BIN` | Override native binary resolution |
| `MAESTRO_NATIVE_MEMORY=0\|false\|off\|no` | Disable automatic memory one-shots |

On native spawn/start failure the service throws. Native protocol messages
are published into the Node broker directly (no TS
`translator.handleAgentEvent`). Automatic memory extraction/consolidation runs
via short-lived native one-shots
([native-memory.ts](../../src/server/native-memory.ts)) when
`MAESTRO_NATIVE_MEMORY` is on (default). Disable with `MAESTRO_NATIVE_MEMORY=0`
for no-ops from [native-memory-noop.ts](../../src/server/native-memory-noop.ts).
Failures warn and continue; never soft-fall back to `createAgent`.

Implementation:

- [src/server/headless-native-bridge.ts](../../src/server/headless-native-bridge.ts) — start + publish bridge
- [src/server/native-headless-client.ts](../../src/server/native-headless-client.ts) — NDJSON client
- [src/server/native-memory.ts](../../src/server/native-memory.ts) — native one-shot auto memory
- [src/server/native-memory-noop.ts](../../src/server/native-memory-noop.ts) — no-op auto memory when disabled
- [src/server/headless-runtime-service.ts](../../src/server/headless-runtime-service.ts) — hosted native headless runtime


## Hosted Remote-Runner Identity

When Maestro runs through `maestro hosted-runner`, the HTTP server exposes a
Platform attach-fencing endpoint:

```http
GET /.well-known/evalops/remote-runner/identity
```

The endpoint returns only runtime identity needed by Platform's headless
gateway:

```json
{
  "protocol_version": "evalops.remote-runner.identity.v1",
  "runner_session_id": "mrs_123",
  "owner_instance_id": "pod_123",
  "ready": true,
  "draining": false
}
```

`runner_session_id` comes from `--runner-session-id`, `MAESTRO_RUNNER_SESSION_ID`,
or `REMOTE_RUNNER_SESSION_ID`. `owner_instance_id` comes from
`--owner-instance-id`, `MAESTRO_REMOTE_RUNNER_OWNER_INSTANCE_ID`, or
`REMOTE_RUNNER_OWNER_INSTANCE_ID`.

This surface intentionally omits workspace, organization, user, token, and
prompt metadata. Platform compares the returned session and owner generation
with the control-plane session before proxying attach traffic. `ready=false` or
`draining=true` means the runtime should not receive new attach requests yet.

## Hosted Remote-Runner Drain

Hosted runner mode also exposes a local drain/snapshot hook for Platform:

```http
POST /.well-known/evalops/remote-runner/drain
```

The request body is optional:

```json
{
  "reason": "ttl_expired",
  "requested_by": "platform",
  "export_paths": ["."]
}
```

The endpoint immediately marks the runtime as draining, stops active headless
runtime work, flushes Maestro session and memory state, validates every
`export_paths` entry stays inside the hosted workspace root, and writes a
snapshot manifest. The manifest directory comes from `--snapshot-root`,
`MAESTRO_REMOTE_RUNNER_SNAPSHOT_ROOT`, or
`REMOTE_RUNNER_SNAPSHOT_ROOT`; the default is
`.maestro/runner-snapshots` under the workspace root.

```json
{
  "protocol_version": "evalops.remote-runner.drain.v1",
  "status": "drained",
  "manifest_path": "/workspace/.maestro/runner-snapshots/mrs_123-2026-04-23T00_00_00_000Z.json",
  "manifest": {
    "protocol_version": "evalops.remote-runner.snapshot-manifest.v1",
    "runner_session_id": "mrs_123",
    "workspace_id": "workspace_123",
    "agent_run_id": "run_123",
    "maestro_session_id": "session_123",
    "reason": "ttl_expired",
    "requested_by": "platform",
    "created_at": "2026-04-23T00:00:00.000Z",
    "workspace_root": "/workspace",
    "runtime": {
      "flush_status": "completed",
      "session_id": "session_123",
      "session_file": "/workspace/.maestro/agent/sessions/session.jsonl",
      "protocol_version": "2026-08-07",
      "cursor": 42
    },
    "workspace_export": {
      "mode": "local_path_contract",
      "paths": [
        {
          "input": ".",
          "path": "/workspace",
          "relative_path": ".",
          "type": "directory"
        }
      ]
    },
    "work_continuity": {
      "protocol_version": "evalops.remote-runner.work-continuity.v1",
      "codex_subagent_schema_version": "evalops.maestro.codex.subagent-workgraph.v1",
      "active_tool_count": 0,
      "tracked_tool_count": 0,
      "pending_request_count": 0,
      "codex_subagent_tool_call_ids": [],
      "codex_subagent_child_run_ids": [],
      "codex_subagent_thread_ids": []
    },
    "platform_evidence": {
      "protocol_version": "evalops.remote-runner.platform-evidence.v1",
      "event_type": "hosted_runner_drain_manifest_recorded",
      "runner_session_id": "mrs_123",
      "workspace_id": "workspace_123",
      "agent_run_id": "run_123",
      "maestro_session_id": "session_123",
      "status": "drained",
      "runtime_flush_status": "completed",
      "manifest_path": "/workspace/.maestro/runner-snapshots/mrs_123-2026-04-23T00_00_00_000Z.json",
      "manifest_protocol_version": "evalops.remote-runner.snapshot-manifest.v1",
      "created_at": "2026-04-23T00:00:00.000Z",
      "work_continuity": {
        "protocol_version": "evalops.remote-runner.work-continuity.v1",
        "codex_subagent_schema_version": "evalops.maestro.codex.subagent-workgraph.v1",
        "active_tool_count": 0,
        "tracked_tool_count": 0,
        "pending_request_count": 0,
        "codex_subagent_tool_call_count": 0,
        "codex_subagent_child_run_count": 0,
        "codex_subagent_thread_count": 0,
        "codex_subagent_edge_count": 0,
        "codex_subagent_tool_call_ids": [],
        "codex_subagent_child_run_ids": [],
        "codex_subagent_thread_ids": []
      },
      "retention": {
        "policy_version": "evalops.remote-runner.retention.v1",
        "control_plane_metadata_visibility": "operator",
        "runtime_snapshot_visibility": "internal",
        "redaction_required_before_external_persistence": [
          "runtime_snapshot",
          "runtime_logs"
        ]
      },
      "evidence_refs": [
        "remote-runner://sessions/mrs_123/drain#manifest",
        "maestro://headless/sessions/session_123#drain",
        "platform-agent-run:run_123"
      ]
    },
    "retention_policy": {
      "policy_version": "evalops.remote-runner.retention.v1",
      "managed_by": "platform",
      "visibility": {
        "control_plane_metadata": "operator",
        "workspace_export": "tenant",
        "runtime_snapshot": "internal",
        "runtime_logs": "operator"
      },
      "redaction": {
        "required_before_external_persistence": [
          "runtime_snapshot",
          "runtime_logs"
        ],
        "forbidden_plaintext": [
          "provider_credentials",
          "tool_secrets",
          "attach_tokens",
          "artifact_access_tokens",
          "raw_environment"
        ]
      }
    },
    "snapshot": {
      "protocolVersion": "2026-08-07",
      "session_id": "session_123",
      "cursor": 42,
      "last_init": null,
      "state": {
        "connection_count": 0,
        "subscriber_count": 0,
        "connections": [],
        "pending_requests": [],
        "pending_approvals": [],
        "pending_client_tools": [],
        "pending_mcp_elicitations": [],
        "pending_user_inputs": [],
        "pending_tool_retries": [],
        "tracked_tools": [],
        "active_tools": [],
        "active_utility_commands": [],
        "active_file_watches": [],
        "is_ready": true,
        "is_responding": false
      }
    }
  }
}
```

`status=interrupted` means Maestro entered draining mode and wrote a manifest,
but runtime flush failed before completion. Platform should treat the manifest
as a partial handoff record, stop sending attach traffic, and decide whether to
retry drain or terminate the pod. Maestro does not upload to GCS or require a
Cloud Storage mount; Platform/deploy own artifact upload, retention, and any
future resume controller behavior. The visibility and redaction rules for those
uploaded artifacts live in [Hosted Runner Retention](./hosted-runner-retention.md).

The runtime flush status is the field that controls restore readiness:
`completed` is attachable, `failed` is an interrupted restore, and `skipped`
means no runtime activity was persisted. Older local manifests that used
`interrupted` for the runtime flush status are treated as `failed`.

New drain manifests also include `runtime_continuity`, a compact handoff record
for Platform and operators. It names the source runner session, owner instance,
source process id, restore manifest path, restore environment key, and the
cursor-scoped evidence refs that prove which headless snapshot should be
restored after process shutdown or replica replacement. `platform_evidence`
mirrors a redacted summary of the same block so AgentRuntime progress records
can be queried without reading the full runtime snapshot.

## Hosted Remote-Runner Restore

When Platform has already restored workspace artifacts and a prior snapshot
manifest into the workspace, the Rust hosted runner can seed its runtime state
from that manifest at startup:

- `MAESTRO_REMOTE_RUNNER_RESTORE_MANIFEST`
- `REMOTE_RUNNER_RESTORE_MANIFEST`

Relative manifest paths resolve under `MAESTRO_WORKSPACE_ROOT`. Startup
validates the manifest protocol and workspace export paths against the current
workspace before binding the HTTP server. A restored runner keeps the new
`runner_session_id` for Platform identity, restores the logical Maestro session
id and cursor from the manifest, returns the restored state from
`GET /api/headless/sessions/:id/state`, and emits an initial SSE reset envelope
with reason `restored_from_snapshot`.

Only manifests with `runtime.flush_status=completed` report `ready=true` and
accept new controller/viewer attachments. `failed` or `skipped` manifests still
preserve the logical session id, cursor, and last snapshot for inspection, but
identity and runtime snapshots stay not-ready with `last_error` populated so
Platform can retry, quarantine, or terminate without silently attaching clients
to a partial restore.

Restore is deliberately local and provider-neutral. Maestro does not download
artifacts, pick a provider object, or decide retention policy; Platform/deploy
must hydrate the workspace and pass the manifest path before the runner starts.

## Handshake

Typical controller flow:

1. Client starts Maestro in headless mode.
2. Client sends `hello`.
3. Maestro replies with `hello_ok`.
4. Maestro emits `ready`.
5. Client optionally sends `init`.
6. Client sends `prompt`.

Minimal hello:

```json
{
  "type": "hello",
      "protocol_version": "2026-08-07",
  "client_info": {
    "name": "evalops-chat",
    "version": "0.1.0"
  },
  "role": "controller"
}
```

Handshake acknowledgement:

The `native_tools` array below is abbreviated; a live response contains one
entry for every native tool in the Maestro registry.

```json
{
  "type": "hello_ok",
      "protocol_version": "2026-08-07",
  "connection_id": "conn_123",
      "client_protocol_version": "2026-08-07",
  "role": "controller",
  "server_capabilities": {
    "server_requests": ["approval", "client_tool", "user_input", "tool_retry"],
    "utility_operations": ["command_exec", "file_search", "file_watch", "file_read"],
    "raw_agent_events": true,
    "connection_roles": ["controller", "viewer"],
    "native_tools": [
      {"name": "bash", "requires_approval": true, "version": "current"}
    ]
  }
}
```

Initial runtime state:

```json
{
  "type": "ready",
      "protocol_version": "2026-08-07",
  "model": "claude-opus-4-6",
  "provider": "anthropic",
  "executor_type": "live",
  "session_id": null
}
```

## Roles, Capabilities, And Notifications

Connection roles:

- `controller`
  - may send prompts, approvals, utility commands, and shutdown
- `viewer`
  - read-only subscriber role

Negotiated client capabilities in `hello.capabilities`:

- `server_requests`
  - supported request classes for approval and control-plane callbacks
- `utility_operations`
  - `command_exec`, `file_search`, `file_watch`, `file_read`
- `raw_agent_events`
  - opt into raw internal agent events

Advertised server capabilities in `hello_ok.server_capabilities`:

- `server_requests`
  - request classes this Maestro runtime can emit when a client advertises support
- `utility_operations`
  - utility operations this Maestro runtime can host for clients
- `raw_agent_events`
  - whether raw internal agent events are available as an opt-in stream
- `connection_roles`
  - connection roles the runtime understands
- `native_tools`
  - the authoritative Maestro-native registry entries, including baseline approval metadata and the active behavior version when the tool is version-managed

The `requires_approval` value in `native_tools` is registry metadata. Argument-aware policy and the action firewall can change the decision for an individual call; clients must use the emitted `tool_call.requires_approval` value for that call and must not recreate a tool allowlist.

Optional notification opt-outs in `hello.opt_out_notifications`:

- `status`
- `heartbeat`
- `connection_info`
- `compaction`

## Client To Maestro Messages

### Run Control

- `hello`
  - handshake and capability negotiation
- `init`
  - runtime configuration such as `system_prompt`, `append_system_prompt`,
    `thinking_level`, `approval_mode`, and optional `history` (array of
    `{role, content}` turns applied before the first prompt; prefer this over
    stuffing multi-turn context into `append_system_prompt`)
- `prompt`
  - starts or continues a run; supports `attachments`
- `interrupt`
  - requests clean cancellation of the active run
- `cancel`
  - alias for `interrupt`
- `shutdown`
  - graceful process termination

### Approval And Callback Responses

- `tool_response`
  - resolves a legacy approval-gated tool call via `call_id`
- `client_tool_result`
  - returns structured content for a client-executed tool
- `server_request_response`
  - resolves a `server_request` using `request_id` and `request_type`

Supported `server_request_response.request_type` values:

- `approval`
- `client_tool`
- `user_input`
- `tool_retry`

`mcp_elicitation` remains in the protobuf schema as a schema-only request type.
The native Maestro runtime does not advertise or resolve it.

### Utility Operations

- `utility_command_start`
- `utility_command_terminate`
- `utility_command_stdin`
- `utility_command_resize`
- `utility_file_search`
- `utility_file_read`
- `utility_file_watch_start`
- `utility_file_watch_stop`

## Maestro To Client Messages

### Session And Connection State

- `hello_ok`
  - handshake acknowledgement
- `ready`
  - runtime-ready event with protocol version, active model/provider, and
    `executor_type`
  - `executor_type=live` means the runtime is backed by an external or local
    model provider; `executor_type=replay` means the session is driven by a
    deterministic scripted scenario and should be visibly badged as replay by
    clients and control planes
- `session_info`
  - current `session_id`, `cwd`, and `git_branch`
- `connection_info`
  - current connection graph, controller lease, and subscriber state
- `compaction`
  - session compaction summary and token counts

### Assistant Response Lifecycle

- `response_start`
- `response_chunk`
  - streamed text or thinking; `is_thinking=true` marks reasoning content
- `response_end`
  - closes one provider response and carries its final usage and execution
    telemetry; it is not a turn terminal
- `turn_completed`
  - positive terminal for a successful full turn
- `turn_interrupted`
  - terminal cancellation or interruption, with a machine-readable reason
- `provider_error`
  - typed terminal provider failure; `kind` is one of `transient_protocol`,
    `output_token_exhaustion`, `incomplete_response`, or
    `provider_declared_failure`
- `response_accepted`
  - durable acknowledgement that a controller response was consumed by the
    native runtime, correlated by `request_id`

`response_end` is the authoritative place to read:

- `usage`
  - `input_tokens`
  - `output_tokens`
  - `cache_read_tokens`
  - `cache_write_tokens`
  - `total_tokens`
  - `total_cost_usd`
  - `model_id`
  - `provider`
- `tools_summary`
  - `tools_used`
  - `calls_succeeded`
  - `calls_failed`
  - `summary_labels`
- `duration_ms`
- `ttft_ms`

A consumer must wait for exactly one explicit turn terminal after any
`response_end` frames. EOF or disconnect before `turn_completed`,
`turn_interrupted`, or `provider_error` is a protocol failure and must not be
converted into an empty successful completion.

### Tool And Server Request Lifecycle

- `tool_call`
- `tool_start`
- `tool_output`
- `tool_end`
- `client_tool_request`
- `server_request`
- `server_request_resolved`

Supported `server_request.resolution` values:

- `approved`
- `denied`
- `completed`
- `failed`
- `answered`
- `retried`
- `skipped`
- `aborted`
- `cancelled`

Supported `server_request.resolved_by` values:

- `user`
- `policy`
- `client`
- `runtime`

Supported `server_request_response.decision_action` values for tool retries:

- `retry`
- `skip`
- `abort`

### Utility Operation Events

- `utility_command_started`
- `utility_command_resized`
- `utility_command_output`
- `utility_command_exited`
- `utility_file_search_results`
- `utility_file_read_result`
- `utility_file_watch_started`
- `utility_file_watch_event`
- `utility_file_watch_stopped`

### Status And Diagnostics

- `status`
  - human-readable runtime status
- `error`
  - recoverable or fatal error classification
- `raw_agent_event`
  - full internal agent event stream when the client negotiated
    `raw_agent_events=true`

Supported `error_type` values:

- `transient`
- `fatal`
- `tool`
- `cancelled`
- `protocol`

## Embedder Notes

- Treat `response_chunk` as append-only.
- Persist `response_end.usage` and `response_end.tools_summary` instead of
  reconstructing totals from streamed chunks.
- Do not record turn success at `response_end`; record it only after
  `turn_completed`. Treat `turn_interrupted` and `provider_error` as failed
  terminals.
- Use `init` instead of shell-interpolating system prompts or approval mode.
- Viewer connections are intentionally limited; use `controller` for active
  orchestration.
- Prefer `server_request` / `server_request_response` for new control-plane
  integrations. `tool_call` / `tool_response` remains for legacy approval
  compatibility.
- Web chat bridge: `src/server/native-headless-event-adapter.ts` maps
  `HeadlessFromAgentMessage` into runtime `AgentEvent` frames so chat-ws can
  forward native headless streams without an in-process TypeScript Agent.
  Prefer negotiated `raw_agent_event` passthrough when available; the adapter
  reconstructs message/tool lifecycle events from protocol messages otherwise.
- Server native approvals fail closed: explicit `auto` remains automatic;
  `prompt` and `fail` map to `fail` until an interactive approval/client-tools
  bridge is available.

## Platform Event Bus

Managed EvalOps deployments can mirror the headless runtime surface onto the
shared platform event bus without enabling user training telemetry. Set
`MAESTRO_EVENT_BUS_URL` or `EVALOPS_NATS_URL` to publish typed CloudEvents to
JetStream subjects that match platform's `maestro.*` event catalog.

The shared publisher lives in `@evalops/ai/telemetry` and currently emits:

- `maestro.sessions.session.started|suspended|resumed|closed`
- `maestro.events.approval_hit`
- `maestro.events.sandbox_violation`
- `maestro.events.firewall_block`
- `maestro.events.tool_call.attempted|completed`
- `maestro.events.prompt_variant.selected`
- `maestro.events.context.learned`
- `maestro.events.skill.invoked|succeeded|failed`
- `maestro.events.eval.scored`

Use `recordMaestroLearnedContext` when a Maestro coding session learns a
durable, evidence-backed fact that future agents should be able to recall. The
event must include a stable `learning_id`, `statement`, claim-family
`dimension`, confidence score/reason, supporting evidence, and the normal
org/user/workspace/session/run correlation. Knowledge projects this event into a
learned-context document Thing plus an agent-authored Fact, so agents can query
it later without treating it as connector source truth.

Use `maestroCorrelationToChronicleMetadata(correlation)` when handing the same
run/session identity to Chronicle or agentd capture. It emits the stable
metadata keys consumed by Platform Chronicle evidence and Knowledge's Chronicle
consumer, including `organization_id`, `user_id`, `workspace_id`,
`maestro_session_id`, `agent_run_id`, `agent_run_step_id`,
`tool_execution_id`, `trace_id`, `task_id`, and `source_issue`.

For end-to-end Platform traceability, managed launchers should set the org and
user identity environment variables before starting Maestro:

- `MAESTRO_EVALOPS_ORG_ID` or `EVALOPS_ORGANIZATION_ID`
- `MAESTRO_EVALOPS_USER_ID`, `EVALOPS_USER_ID`, or `MAESTRO_USER_ID`
- `MAESTRO_EVALOPS_WORKSPACE_ID` or `EVALOPS_WORKSPACE_ID`
- `MAESTRO_SESSION_ID`, `MAESTRO_AGENT_RUN_ID`, and, for tool-level spans,
  `MAESTRO_AGENT_RUN_STEP_ID`

Maestro copies those values into CloudEvent extensions and OpenTelemetry span
attributes (`evalops.organization_id`, `enduser.id`, `evalops.workspace_id`,
`maestro.session_id`, and `maestro.agent_run_id`). Platform `traces` normalizes
those attributes into first-class trace fields, and Knowledge imports the same
event correlation into org/user/session/run/tool graph nodes.

When a session uses an EvalOps managed model provider, Maestro also forwards the
same content-free join keys to `llm-gateway` request metadata. The gateway
metadata includes `agent_id`, `workspace_id`, `objective_id`, `run_id`,
`agent_run_id`, `agent_run_step_id`, `session_id`, `maestro_session_id`,
`trace_id`, `turn_id`, `tool_call_id`, `workload`, and `surface` when those
values are available from stored managed-agent identity or the environment. This
lets Platform's AgentRuntime operating ledger attach model usage to the same run
as tool execution, approval, trace, and timeline evidence without copying raw
prompts or responses into operator surfaces.

For direct Knowledge MCP access through the EvalOps plugin, set
`MAESTRO_PLATFORM_MCP_URL`, `MAESTRO_EVALOPS_AGENT_MCP_URL`, or the manifest
form `MAESTRO_PLATFORM_MCP_MANIFEST_URL`, then grant scopes with
`MAESTRO_CEREBRO_MCP_SCOPES` or `MAESTRO_PLATFORM_MCP_SCOPES`. The URL can be
the public app base URL, the `/mcp` endpoint, or
`/.well-known/evalops/agent-mcp.json`; Maestro normalizes those forms to the
HTTP MCP endpoint. Use `cerebro:read` for recall-only agents and add
`cerebro:assert` only for agents allowed to write explicit learned facts.
Maestro forwards `X-EvalOps-Workspace-Id`, `X-EvalOps-Session-Id`,
`X-EvalOps-Agent-Id`, `X-EvalOps-Agent-Run-Id`, trace/request IDs, and scopes so
Knowledge can attribute every query and assertion to the user/org session.

The publisher conformance fixture used by Platform can be regenerated from the
same shared publisher with
`tsx scripts/generate-maestro-publisher-conformance-fixture.ts`.
The GitHub agent worker also records task session start/close events through
this shared publisher with `MAESTRO_SURFACE=github-agent` and task correlation
attributes, so platform subscribers can join worker runs to issue/PR work.
The Rust Ambient Agent daemon publishes session start/suspend/resume/close
events with `source=maestro.ambient-agent` when the same event-bus NATS
environment is configured.

For Agent Workforce onboarding, Maestro also provides a local
`agent_workforce_native_event.v1` projector over the same runtime event boundary.
It is documented in
[Agent Workforce Native Event Projection](agent-workforce-native-event.md). The
projector records thread, turn, tool, approval, and model-usage envelopes plus a
hash-chain verification primitive, but it remains Maestro runtime-observed
self-report until Platform ingestion and credential join verification are wired.
It also publishes plan-level routing and outcome events:

- `maestro.ambient_agent.routing.selected`
- `maestro.ambient_agent.plan.cost_limited`
- `maestro.ambient_agent.plan.completed`

These include the session correlation block plus repository, upstream event ID,
task type, complexity, selected provider/model/tier, estimated cost, and final
success/cost metadata when available. Maestro web surfaces the same model tier
and Platform bus readiness fields in the fleet dashboard.

`MAESTRO_TELEMETRY` continues to control local training and diagnostic
telemetry. Audit-bus publishing is controlled separately with
`MAESTRO_EVENT_BUS`; set it to `0` or `false` to suppress bus writes even when
managed EvalOps routing is active.

To verify a live JetStream route without relying on best-effort runtime
publishing, run `bun run smoke:event-bus` with `MAESTRO_EVENT_BUS_URL` or
`EVALOPS_NATS_URL` configured. The smoke publishes a single
`maestro.sessions.session.started` CloudEvent and fails on connection or
publish errors.

For the larger remote-attach and control-plane architecture, see the companion
design document: [docs/design/HEADLESS_CONTROL_PLANE.md](../design/HEADLESS_CONTROL_PLANE.md).

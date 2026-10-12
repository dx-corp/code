# Codex Operating Layer

> **Status:** This document predates the Rust-only runtime migration (#3016, #3017, merged 2026-07-22), which deleted Maestro's TypeScript agent runtime and SDK. See `docs/protocols/codex-operating-layer.json`, the machine-readable evidence map this document narrates: redundant TypeScript entries with a surviving Rust equivalent in the same area were removed in this PR; entries with no verified Rust equivalent are tracked in a follow-up issue (see PR description). Some file paths below may be stale; they are kept for design context and updated only where a corresponding Rust module was confirmed.


This protocol keeps the long-horizon Codex work honest. Maestro should not only
route to Codex app-server; it should behave like an audited operating layer for
durable EvalOps agents across TypeScript, Rust, web, headless, and live
operator workflows.

The next horizon is the Codex Mesh: upstream Codex app-server collaboration
primitives become Platform-owned work graph, delegation, remote-runner, and
GitOps runtime state instead of transient local UI events.

The executable evidence map lives at
[`codex-operating-layer.json`](./codex-operating-layer.json). Run it with:

```bash
npm run check:codex-operating-layer
```

## Far-Horizon Success Criteria

- Codex works from a fresh install without silently losing native safety-parser
  behavior.
- Operators can sign in with ChatGPT, inspect status, and run Codex doctor from
  the Maestro CLI.
- TypeScript can run Codex app-server threads with dynamic tools, dynamic-tool
  lifecycle events, approval-gated dynamic callbacks, token usage, subagent
  lifecycle events, and streaming output. Dynamic callback lifecycle events keep
  Platform `toolExecutionId` and `approvalRequestId` joins, including denied
  governance callbacks, so hosted timelines, approvals, and ToolExecution
  governance can be audited without exposing those IDs to the model-facing tool
  result payload.
- Hosted Codex subagent collaboration records Platform AgentRun work items so
  parent/child work can be inspected, resumed, scored, and restored remotely.
  Spawned children also become Platform agent-registry delegations so ownership,
  routing capability, resolution, and evidence refs survive remote execution;
  spawn only opens the delegation, while wait/close or child restore failure
  resolves it. TS and Rust both normalize child run ids and persist subagent
  edge lifecycle state, so spawn/send/resume/wait/close edges survive drain and
  restore. Hosted local task progress also projects todo, background-task, and
  swarm state into deterministic Platform AgentRuntime work items/steps so
  multi-agent coordination is visible without copying raw logs, env, diffs, or
  teammate output.
- Rust can expose the same Codex models through the control plane, bridge
  Codex headless runs, run the hosted remote-runner entrypoint, stream
  SSE/WebSocket events, handle approval requests, and preserve sandbox policy.
- Durable thread metadata covers goals and memory linkage instead of treating
  Codex runs as disposable one-off calls.
- Eval telemetry and trajectory fixtures can score approvals, child runs,
  recovery, and tool execution without scraping human-readable logs.
- Live verification exercises real Codex inference through the installed CLI,
  not only local mocks.

## Prompt-To-Artifact Checklist

| Requirement | Evidence area | Primary artifacts |
| --- | --- | --- |
| installed by default | `default-install` | `package.json`, `test/install/native-dependencies.test.ts` |
| ChatGPT sign-in | `chatgpt-sign-in` | `packages/local-host-rs/src/codex_cli.rs`, `packages/codex-rs/src/codex_app_server.rs` |
| dynamic tools | `dynamic-tools` | `src/codex/compatibility.ts`, `src/agent/providers/codex-app-server.ts`, `test/codex/compatibility.test.ts`, `test/agent/codex-app-server.test.ts` |
| durable threads, goals, memory | `durable-threads-goals-memory` | `src/session/types.ts`, `src/server/run-reconstruction.ts`, `packages/contracts/src/maestro-app-server.ts`, `test/cli/run-command.test.ts`, `test/app-server/session-api.test.ts` |
| approvals and sandbox policy | `approvals-sandbox-policy` | `src/agent/transport.ts`, `test/agent/provider-transport-provider-tools.test.ts`, `packages/runtime-gateway-rs/src/main.rs`, `docs/protocols/pending-requests.md` |
| subagents | `subagents` | `src/agent/providers/codex-app-server.ts`, `test/agent/provider-transport-provider-tools.test.ts` |
| multi-agent work graph | `multi-agent-workgraph` | `docs/protocols/codex-subagent-workgraph-v1.json`, `src/platform/agent-runtime-client.ts`, `src/platform/agent-registry-client.ts`, `src/agent/providers/codex-app-server.ts`, `packages/runtime-gateway-rs/src/main.rs`, `src/server/hosted-agent-runtime-progress.ts`, `test/server/hosted-agent-runtime-progress.test.ts` |
| remote runner continuity | `remote-runner-continuity` | `src/server/handlers/hosted-runner-drain.ts`, `packages/hosted-runner-rs/src/hosted_runner.rs`, `packages/hosted-runner-rs/src/hosted_runner/manifests.rs`, `packages/hosted-runner-rs/src/hosted_runner_cli.rs`, `packages/local-host-rs/src/headless/messages/state.rs`, `test/server/hosted-runner-drain.test.ts` |
| realtime streaming | `realtime-streaming` | `src/server/handlers/runtime-app-server-ws.ts`, `test/server/runtime-app-server-ws.test.ts` |
| TypeScript runtime | `typescript-runtime` | `src/agent/providers/codex-app-server.ts`, `test/agent/codex-app-server.test.ts` |
| Rust runtime | `rust-control-plane` | `packages/runtime-gateway-rs/src/model_catalog.rs`, `packages/runtime-gateway-rs/src/main.rs`, `packages/runtime-gateway-rs/src/tests.rs` |
| eval telemetry | `eval-telemetry` | `docs/protocols/agent-trajectory.md`, `test/fixtures/agent-trajectory/codex-subagent-handoff.timeline.json`, `test/fixtures/agent-trajectory-scenarios/codex-subagent-handoff.json`, `test/telemetry/maestro-publisher-conformance-fixture.test.ts` |
| operator UX and docs | `operator-ux-docs` | `docs/protocols/codex-operating-layer.md`, `docs/MODELS.md` |
| live environment proof | `live-verification` | `scripts/smoke-codex-app-server-live.mjs`, `package.json` |

## Operator Verification Path

Use the fast gate while editing:

```bash
npm run check:codex-operating-layer
npm run check:codex-parity
```

Use the runtime gate before merging changes that affect Codex execution:

```bash
npm test -- test/agent/codex-app-server.test.ts test/agent/provider-transport-provider-tools.test.ts test/codex/app-server-client.test.ts test/codex/compatibility.test.ts test/scripts/codex-operating-layer-conformance.test.ts test/scripts/codex-parity-conformance.test.ts test/server/runtime-app-server-ws.test.ts test/server/hosted-runner-drain.test.ts test/headless/runtime-conformance.test.ts
npm run tui-rs:test -- codex_
npm run tui-rs:test -- hosted_runner
npm run smoke:codex-app-server-live
```

The live Codex smoke is intentionally stricter than a token-presence check. It
uses JSONL output to verify that the final assistant message exactly matches the
token, fails on loop-detector warnings, runs the real inference from an isolated
temporary workspace, and enforces bounded dynamic tool calls with
`MAESTRO_CODEX_LIVE_SMOKE_MAX_TOTAL_TOOL_CALLS` and
`MAESTRO_CODEX_LIVE_SMOKE_MAX_IDENTICAL_TOOL_CALLS`. It also runs real Codex
subagent spawn/wait inference and requires `codexWorkGraph` evidence on both
`codex.subagent.spawnAgent` and `codex.subagent.wait`; the versioned
`codex-subagent-workgraph-v1.json` fixture pins the full
spawn/send/resume/wait/close lifecycle for TypeScript and Rust conformance, and
the remote-runner drain tests require lifecycle-edge continuity without copying
child prompts into manifest metadata.

`maestro run inspect <session-id> --json` also emits a `durability` summary for
the local restore path. The summary is intentionally compact and redacted: it
proves the session file was found, reconstruction produced timeline and
AgentRuntime ledger entries, replay was deterministic, resume-summary and
memory-hash continuity are present when saved, and the dry-run promotion plan has
a stable idempotency key. Published replay smokes assert that summary so release
evidence does not stop at "the CLI printed text."

## Completion Bar

The operating-layer goal is not complete just because this manifest passes. This
manifest is the prompt-to-artifact checklist that prevents drift. Completion
still requires a fresh audit showing every area is covered by real code, tests,
operator documentation, PR review, merge state, and live evidence from the
current environment.

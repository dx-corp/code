# Headless Control Plane Design

> **Status:** This document predates the Rust-only runtime migration (#3016, #3017, merged 2026-07-22), which deleted Maestro's TypeScript agent runtime and SDK. Headless/remote runtime code now lives in `packages/local-host-rs/src/headless/` and `packages/runtime-gateway-rs`. Some file paths below may be stale; they are kept for design context and updated only where a corresponding Rust module was confirmed.


This document describes the next architectural step for Maestro's headless and remote runtime stack. It builds on the current `src/server/headless-runtime-service.ts` and Rust remote transport work, and uses OpenAI Codex's `codex-rs/app-server` as a reference point for stronger lifecycle and protocol discipline.

## Overview

Maestro already supports:

- long-lived headless runtimes with replayable cursors
- HTTP + SSE remote attach
- Rust-side local/remote transport unification
- state replay and reconnect support in `packages/local-host-rs/src/headless`

Those features are useful, but the server is still mostly **runtime-centric**:

- approvals are tracked in a global `approvalStore`
- client tool callbacks are tracked in a separate global `clientToolService`
- SSE subscribers write directly to `ServerResponse`
- session subscription is implicit in an open `/events` request
- there is no connection handshake or capability negotiation

The goal of this design is to evolve that into a small, typed **control plane** with explicit connection, session, subscriber, and callback lifecycles.

## Current Baseline

Current Maestro files and responsibilities:

- `src/server/headless-runtime-service.ts`
  - owns `HeadlessSessionRuntime`
  - maintains a replay buffer and current runtime state
  - publishes translated agent events
- `src/server/handlers/headless-sessions.ts`
  - creates/attaches sessions
  - exposes session snapshot, event stream, and message POSTs
  - writes SSE frames directly to the response
- `src/cli/headless-protocol.ts`
  - defines wire messages and the current state reducer helpers
- `src/server/approval-store.ts`
  - global `requestId -> ActionApprovalService`
- `src/server/client-tools-service.ts`
  - global `toolCallId -> pending client tool resolver`
- `packages/local-host-rs/src/headless/remote_transport.rs`
  - remote HTTP/SSE client for the current headless server contract

This is a good foundation, but it leaves lifecycle gaps:

1. There is no unified abstraction for "the server asked the client for something."
2. Callback resolution is not visible as a first-class event.
3. Slow subscribers are coupled to direct HTTP response writes.
4. Session ownership is inferred from open streams instead of tracked explicitly.
5. Protocol evolution is harder because transport handshake and runtime init are conflated.

## Goals

1. Separate connection lifecycle from session/runtime lifecycle.
2. Introduce one unified `ServerRequest` abstraction for approvals, client tools, and future client callbacks.
3. Make callback resolution explicit with `server_request_resolved`.
4. Isolate the runtime from slow or lagged subscribers with per-subscriber mailboxes.
5. Add explicit subscribe/unsubscribe semantics and session ownership tracking.
6. Preserve compatibility with the current `/api/headless/sessions/*` model during migration.
7. Enable an in-process host that preserves the same contract without HTTP or stdio.

## Non-Goals

- Replacing the current headless protocol in one PR.
- Switching remote transport to WebSocket-first delivery.
- Persisting every live delta into session JSONL immediately.
- Rebuilding web, desktop, IDE, and Rust clients all at once.

## Proposed Architecture

```
┌─────────────────────────────────────────────────────────────────────┐
│                    Headless Control Plane                           │
│                                                                     │
│  ┌──────────────────────┐     ┌──────────────────────────────────┐  │
│  │ Connection Manager   │────▶│ Session Runtime Registry         │  │
│  │ - handshake          │     │ - LiveSessionRuntime             │  │
│  │ - capabilities       │     │ - Agent ownership               │  │
│  │ - viewer/controller  │     │ - replay buffer                 │  │
│  └──────────────────────┘     └──────────────────────────────────┘  │
│              │                              │                        │
│              │                              ▼                        │
│              │                  ┌───────────────────────────────┐    │
│              └─────────────────▶│ ServerRequestManager          │    │
│                                 │ - approvals                   │    │
│                                 │ - client tools               │    │
│                                 │ - request user input         │    │
│                                 │ - auth refresh / future      │    │
│                                 └───────────────────────────────┘    │
│                                                │                     │
│                                                ▼                     │
│                                 ┌───────────────────────────────┐    │
│                                 │ Subscriber Mailboxes          │    │
│                                 │ - bounded queues             │    │
│                                 │ - lagged/reset semantics     │    │
│                                 │ - SSE / in-process adapters  │    │
│                                 └───────────────────────────────┘    │
└─────────────────────────────────────────────────────────────────────┘
```

### Major Components

#### 1. `ConnectionManager`

Tracks transport-level state that should not live inside the session runtime:

- `connectionId`
- authenticated subject
- `clientInfo`
- negotiated protocol version
- capabilities
- controller vs viewer role
- heartbeat / liveness status

This is the right place for capability negotiation and future experimental feature gating.

#### 2. `SessionRuntimeRegistry`

Evolves the current `HeadlessRuntimeService` into a registry of live session runtimes with explicit subscriber tracking.

Each `LiveSessionRuntime` owns:

- `Agent`
- current `HeadlessRuntimeState`
- monotonic cursor
- bounded replay buffer
- pending active turn metadata
- subscriber ids
- controller lease metadata

This remains the semantic source of truth for live runtime state. It should not know about HTTP response objects.

#### 3. `ServerRequestManager`

Unifies flows that currently live in separate global stores:

- approvals
- client tool callbacks
- future `request_user_input`
- future auth refresh or MCP elicitation flows

Each request is scoped to a session, optionally a turn, and one or more target subscribers.

#### 4. `SubscriberMailbox`

Each remote subscriber gets a bounded queue. The runtime publishes once; mailboxes drain independently.

This decouples the runtime from client speed and lets the transport layer decide whether to:

- emit `lagged`
- emit `reset`
- close the subscriber

depending on delivery guarantees for that transport.

#### 5. `InProcessHeadlessHost`

Provides the same control-plane contract without HTTP or stdio. This is the equivalent of Codex app-server's `in_process.rs`.

Immediate uses:

- desktop embedding
- tests
- future Rust TUI local embedding without a separate subprocess

## Protocol Layers

The new control plane should be layered instead of overloading `init`.

### Layer 1: Connection Handshake

Client connects and identifies itself before interacting with a session.

```ts
interface HeadlessHelloRequest {
  type: "hello";
  protocol_version: string;
  client: {
    name: string;
    version?: string;
    platform?: string;
  };
  capabilities?: {
    server_requests?: boolean;
    notification_filters?: boolean;
    remote_exec?: boolean;
    raw_agent_events?: boolean;
  };
  role?: "viewer" | "controller";
}

interface HeadlessHelloResponse {
  type: "hello_ok";
  protocol_version: string;
  connection_id: string;
  capabilities: {
    server_requests: boolean;
    notification_filters: boolean;
    remote_exec: boolean;
    raw_agent_events: boolean;
  };
}
```

`hello` negotiates transport concerns. It does **not** mutate runtime config.

### Layer 2: Session Subscription

```ts
interface HeadlessSubscribeRequest {
  type: "subscribe";
  session_id: string;
  cursor?: number;
  notifications?: {
    status?: boolean;
    heartbeats?: boolean;
    raw_agent_events?: boolean;
  };
}

interface HeadlessUnsubscribeRequest {
  type: "unsubscribe";
  session_id: string;
}
```

This replaces the current "open `/events` and you are implicitly subscribed" model.

### Layer 3: Session Commands

The existing runtime messages remain, but they are clearly session-bound:

- `init`
- `prompt`
- `interrupt`
- `tool_response`
- `cancel`
- `shutdown`

These keep the current semantics while the connection/session split is introduced.

## Event Envelope

All live remote events should be wrapped in a transport-stable envelope.

```ts
type HeadlessSessionEnvelope =
  | {
      type: "snapshot";
      session_id: string;
      cursor: number;
      snapshot: HeadlessRuntimeSnapshot;
    }
  | {
      type: "message";
      session_id: string;
      cursor: number;
      message: HeadlessFromAgentMessage;
    }
  | {
      type: "server_request";
      session_id: string;
      cursor: number;
      request: HeadlessServerRequest;
    }
  | {
      type: "server_request_resolved";
      session_id: string;
      cursor: number;
      request_id: string;
      resolution: HeadlessServerRequestResolution;
    }
  | {
      type: "lagged";
      session_id: string;
      cursor: number;
      skipped: number;
    }
  | {
      type: "reset";
      session_id: string;
      cursor: number;
      reason: "replay_gap" | "subscriber_overflow" | "server_restart";
      snapshot: HeadlessRuntimeSnapshot;
    }
  | {
      type: "heartbeat";
      session_id: string;
      cursor: number;
    };
```

The important addition is `server_request_resolved`. Clients need a way to clear pending approval/input UI before the underlying operation necessarily completes.

## Unified Server Request Model

Current Maestro state is fragmented:

- approvals: `approvalStore`
- client tool calls: `clientToolService`

Both should move behind one typed abstraction.

```ts
type HeadlessServerRequest =
  | {
      type: "approval";
      request_id: string;
      session_id: string;
      turn_id?: string;
      payload: {
        approval_kind: "command" | "write" | "network" | "tool";
        title: string;
        details?: string;
      };
      expires_at?: string;
    }
  | {
      type: "client_tool";
      request_id: string;
      session_id: string;
      turn_id?: string;
      payload: {
        tool_call_id: string;
        tool_name: string;
        args: unknown;
      };
      expires_at?: string;
    }
  | {
      type: "user_input";
      request_id: string;
      session_id: string;
      turn_id?: string;
      payload: {
        prompt: string;
        schema?: unknown;
      };
      expires_at?: string;
    };

type HeadlessServerRequestResolution =
  | { kind: "approved"; message?: string }
  | { kind: "denied"; message?: string }
  | { kind: "responded" }
  | { kind: "cancelled"; reason: "turn_transition" | "disconnect" | "timeout" | "session_closed" };
```

### Lifecycle Rules

1. Requests are always scoped to a session, and optionally a turn.
2. Requests are cancelled when the turn transitions in a way that makes them stale.
3. The server emits `server_request_resolved` when:
   - the client answers
   - the request times out
   - the session/turn transition clears it
4. Resolution is distinct from the final completion of the tool/turn/item.

## Subscriber Mailbox Semantics

Direct `res.write()` from the runtime should be replaced with bounded per-subscriber queues.

### Requirements

- runtime publishing must be O(number of subscribers), not O(network speed)
- one slow subscriber must not stall others
- transports must handle queue overflow predictably
- replay + live delivery ordering must remain stable

### Queue Policy

Each subscriber mailbox has:

- `capacity`
- `pending_count`
- `last_delivered_cursor`
- `overflow_mode`

Recommended overflow behavior:

- HTTP/SSE subscriber:
  - if replayable gap is still within the runtime buffer, emit `lagged`
  - if not, emit `reset` with a fresh snapshot
  - if the write loop is dead, close the subscriber
- in-process subscriber:
  - prefer `WouldBlock` / `lagged`
  - never silently drop `server_request`

### Delivery Rule for Ordering-Sensitive Events

For `server_request` and `server_request_resolved`, transports may optionally await actual mailbox acceptance before proceeding. That keeps resolution events ordered with the request itself.

## Session Ownership and Roles

The current runtime service tracks sessions by scope + session id, but not explicit viewer/controller semantics.

Introduce:

- any number of viewers
- at most one controller lease
- lease expiry on disconnect or heartbeat timeout

Controller-only actions:

- `prompt`
- `interrupt`
- `tool_response`
- `cancel`
- future `command_exec` control

Viewer-only actions:

- `subscribe`
- `state`
- event consumption

This prevents conflicting approval or interrupt decisions from multiple attached clients.

## Compatibility Plan

The migration should preserve the current endpoints while new protocol layers come online.

### Phase 1: Internal Refactor, Same External API

- add `ServerRequestManager`
- route approvals and client tools through it internally
- add `server_request_resolved` events to the replay stream
- introduce subscriber mailboxes behind `/events`

At this phase, `/api/headless/sessions/:id/events` still exists and clients continue to work.

### Phase 2: Handshake and Subscription APIs

Add explicit connection/session operations:

- `POST /api/headless/connections`
- `POST /api/headless/sessions/:id/subscribe`
- `POST /api/headless/sessions/:id/unsubscribe`

Then make SSE a transport for an already-established subscription instead of the subscription itself.

### Phase 3: In-Process Host

Expose the same semantics in-process for desktop/tests and future local embedding.

### Phase 4: Utility Plane

Only after the control plane is stable, add session-adjacent utility operations such as:

- remote exec
- file watch
- fuzzy file search

## File-Level Implementation Plan

### PR 1: Unify server requests

Create:

- `src/server/headless-server-request-manager.ts`

Update:

- `src/server/approval-store.ts`
- `src/server/client-tools-service.ts`
- `src/server/handlers/approval.ts`
- `src/server/handlers/client-tools.ts`
- `src/server/headless-runtime-service.ts`
- `src/cli/headless-protocol.ts`

Outputs:

- new internal `ServerRequest` model
- replayable `server_request` and `server_request_resolved`
- no more separate lifecycle logic for approvals vs client tool callbacks

### PR 2: Subscriber mailboxes

Update:

- `src/server/headless-runtime-service.ts`
- `src/server/handlers/headless-sessions.ts`

Outputs:

- mailbox-backed delivery
- overflow handling
- `lagged` / `reset` semantics

### PR 3: Handshake and roles

Create:

- `src/server/headless-connection-manager.ts`

Update:

- `src/server/routes.ts`
- `src/server/handlers/headless-sessions.ts`
- `packages/local-host-rs/src/headless/remote_transport.rs`

Outputs:

- `hello`
- negotiated capabilities
- viewer/controller role enforcement

### PR 4: Explicit subscribe/unsubscribe

Update:

- `src/server/headless-runtime-service.ts`
- `src/server/handlers/headless-sessions.ts`
- client transports

Outputs:

- explicit subscriber lifecycle
- deterministic unload rules

### PR 5: In-process host

Create:

- `src/server/headless-in-process-host.ts`

Outputs:

- in-process control-plane client
- no HTTP or stdio boundary required for local embedding clients

## Testing Strategy

### TypeScript

Add protocol-level tests for:

- request resolution vs operation completion ordering
- turn-transition cleanup of pending server requests
- lagged subscriber handling
- replay-gap reset behavior
- controller lease enforcement
- subscribe/unsubscribe unload behavior

Likely homes:

- `test/web/headless-sessions.test.ts`
- `test/cli/headless.test.ts`
- new focused tests around request lifecycle and mailbox behavior

### Rust

Add remote transport and supervisor tests for:

- `server_request`
- `server_request_resolved`
- replay after `reset`
- controller/viewer restrictions
- in-process host parity once added

Likely homes:

- `packages/local-host-rs/src/headless/remote_transport.rs`
- `packages/local-host-rs/src/headless/supervisor.rs`
- `packages/local-host-rs/src/headless/session.rs`

## Decision Notes

- Keep SSE + POST as the default remote transport for now. The architecture problem is lifecycle discipline, not lack of WebSockets.
- Do not persist every event into JSONL yet. Use a live replay buffer plus the existing semantic session state.
- Do not try to port Codex app-server endpoint-for-endpoint. Port the control-plane patterns, not the entire API surface.

## References

- `src/server/headless-runtime-service.ts`
- `src/server/handlers/headless-sessions.ts`
- `src/server/approval-store.ts`
- `src/server/client-tools-service.ts`
- `packages/local-host-rs/src/headless/remote_transport.rs`
- `https://github.com/openai/codex/tree/main/codex-rs/app-server`

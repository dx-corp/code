# Hosted Runner Contract

> **Status:** This document predates the Rust-only runtime migration (#3016, #3017, merged 2026-07-22), which deleted Maestro's TypeScript agent runtime and SDK. Hosted runner code now lives in `packages/local-host-rs/src/hosted_runner/` (config, handle, manifests, snapshots, rendezvous, workload identity) and `packages/local-host-rs/src/hosted_runner_cli.rs` (launch resolution and the `hosted-runner` entrypoint). The startup coordinates in [Configuration](#configuration) are executable: `packages/local-host-rs/tests/hosted_runner_startup_contract.rs` runs every case in `packages/local-host-rs/tests/fixtures/hosted-runner-startup-contract.json` through the real CLI resolver, so a documented coordinate that stops being honored fails `cargo test -p maestro-local-host`. Other file paths below may be stale; they are kept for design context and updated only where a corresponding Rust module was confirmed.


Maestro hosted runners are substrate-neutral runtime pods or sandboxes that
Platform can create, attach to, drain, and stop for an EvalOps account. The
contract is deliberately small: every provider must expose the same Maestro
session surface, even if the provider is GKE, GKE Sandbox, Daytona, Modal, or a
future microVM fleet.

This document is the Maestro-side contract. Platform owns policy, account
entitlements, scheduling profiles, billing, artifact upload, and provider
selection. Maestro owns the runtime process, headless protocol behavior,
workspace enforcement, drain manifest, and session state flush.

## Required Shape

A hosted runner instance represents exactly one logical runner session.

- One `runner_session_id` from Platform.
- One workspace root mounted or created for the session.
- One Maestro HTTP/headless attach surface.
- One optional pre-bound Maestro session id.
- One owner generation when Platform uses stale-owner attach fencing.
- One drain/snapshot location under the workspace unless explicitly mounted
  elsewhere by the provider.

The runtime must not expose provider-specific fields to Maestro clients. Public
clients choose product-level profiles such as `maestro-standard`,
`maestro-spot`, or `maestro-secure`; Platform and deploy translate those
profiles into node pools, RuntimeClasses, sandbox options, or microVM details.

## Configuration

The hosted runner entrypoint is `maestro hosted-runner`. Providers may pass
flags or environment variables, but the resolved values are the contract.

| Contract field | Flags and environment | Required |
| --- | --- | --- |
| Runner session id | `--runner-session-id`, `MAESTRO_RUNNER_SESSION_ID` (deprecated `REMOTE_RUNNER_SESSION_ID`) | yes |
| Workspace root | `--workspace-root`, `MAESTRO_WORKSPACE_ROOT` (deprecated `WORKSPACE_ROOT`) | yes |
| Listen address | `--listen`, `--host`, `--port`, `MAESTRO_HOSTED_RUNNER_LISTEN`, `MAESTRO_HOSTED_RUNNER_HOST`, `MAESTRO_HOSTED_RUNNER_PORT` (deprecated `PORT`) | yes |
| Owner generation | `--owner-instance-id`, `MAESTRO_REMOTE_RUNNER_OWNER_INSTANCE_ID` (deprecated `REMOTE_RUNNER_OWNER_INSTANCE_ID`) | required when Platform fences owners |
| Runtime generation | `MAESTRO_PLACEMENT_GENERATION`, `MAESTRO_SANDBOXWICH_PLACEMENT_GENERATION`, `MAESTRO_REMOTE_RUNNER_GENERATION` | required for managed durable threads; the canonical placement variable takes precedence and also enters workload-identity mode, so it must arrive with the full identity set below |
| Snapshot root | `--snapshot-root`, `MAESTRO_REMOTE_RUNNER_SNAPSHOT_ROOT` (deprecated `REMOTE_RUNNER_SNAPSHOT_ROOT`) | optional |
| Restore manifest | `MAESTRO_REMOTE_RUNNER_RESTORE_MANIFEST` (deprecated `REMOTE_RUNNER_RESTORE_MANIFEST`) | optional |
| Workspace id | `--workspace-id`, `MAESTRO_REMOTE_RUNNER_WORKSPACE_ID`, `MAESTRO_WORKSPACE_ID` | optional |
| Agent run id | `--agent-run-id`, `MAESTRO_AGENT_RUN_ID` | optional |
| Existing Maestro session | `--maestro-session-id`, `MAESTRO_SESSION_ID` | optional |
| Attach audience | `--attach-audience`, `MAESTRO_ATTACH_AUDIENCE` | optional |
| Attach bearer token | `MAESTRO_HOSTED_RUNNER_AUTH_TOKEN` or `MAESTRO_HOSTED_RUNNER_AUTH_TOKEN_FILE` (deprecated `MAESTRO_WEB_API_KEY`, `MAESTRO_WEB_API_KEY_FILE`) | required unless workload identity is present or `MAESTRO_WEB_REQUIRE_KEY=0` on a loopback bind; inline and file forms are mutually exclusive |
| Headless model binding | `MAESTRO_MODEL`; managed `evalops/` and `maestro-managed/` models also require `MAESTRO_RESIDENT_CONTRACT_REVISION` pinned to the current resident revision plus non-empty `MAESTRO_EVALOPS_ACCESS_TOKEN`, `MAESTRO_EVALOPS_BASE_URL`, `MAESTRO_EVALOPS_ORG_ID`, `MAESTRO_EVALOPS_WORKSPACE_ID`, and `MAESTRO_EVALOPS_PROVIDER` | yes |
| Headless child | `--agent-cli-path`, `MAESTRO_HEADLESS_CLI_PATH` | optional |
| Launch spec | `--config` or `MAESTRO_HOSTED_LAUNCH_SPEC_FILE` (not both); when present it replaces the flag/environment coordinates above | optional |
| Rendezvous | `MAESTRO_RENDEZVOUS_MODE` (`inbound`, `outbound_shadow`, `outbound`); outbound modes require workload identity | optional |

Deprecated environment keys are the entries of `DEPRECATED_ENV_ALIASES` in
`packages/local-host-rs/src/hosted_runner/config.rs`. The canonical key always
wins when both are set; every deprecated key that carries a value produces one
`hosted-runner environment key ... is deprecated` startup warning, and the
deprecated keys stop being read after the next Maestro minor release. Platform
(`runner-host` and `sandboxwich`) already launches with the canonical keys.

Startup fails closed on a missing or blank runner session id, a missing or
non-directory workspace root, a non-integer runtime generation, a non-loopback
bind without attach authentication, and any partially supplied workload
identity set.

Managed Kubernetes workload-identity mode additionally requires all of the
following values. Supplying only part of the set fails startup; static hosted
runner bearer authentication is forbidden when the set is present.

| Workload identity field | Environment |
| --- | --- |
| Projected pod-bound token | `MAESTRO_KUBERNETES_TOKEN_FILE` |
| Identity HTTPS trust bundle | `MAESTRO_IDENTITY_TLS_CA_FILE` |
| Certificate exchange endpoint | `MAESTRO_IDENTITY_EXCHANGE_URL` |
| Organization binding | `MAESTRO_ORGANIZATION_ID` |
| Workspace binding | `MAESTRO_WORKSPACE_ID` |
| Sandbox binding | `MAESTRO_SANDBOX_ID` |
| Placement generation | `MAESTRO_PLACEMENT_GENERATION` |
| Runner session binding | `MAESTRO_RUNNER_SESSION_ID` |

There is intentionally no file or environment input for a Runner Host client
CA, Maestro server certificate, or private key; setting
`MAESTRO_RUNNER_CLIENT_CA_FILE` rejects startup. The authenticated exchange
response carries the workload CA in memory. Maestro uses that CA only for the
current server chain and Runner Host client verification, pinned to the exact
client URI `spiffe://identity.evalops.dev/service/runner-host`.

Hosted runner startup also sets these runtime defaults before the web server is
imported:

- `MAESTRO_HOSTED_RUNNER_MODE=1`
- `MAESTRO_PROFILE=hosted-runner` unless already set
- `MAESTRO_WEB_REQUIRE_KEY=0`
- `MAESTRO_WEB_REQUIRE_REDIS=0`
- `MAESTRO_WEB_REQUIRE_CSRF=0`
- `MAESTRO_AGENT_DIR=<workspace>/.maestro/agent` unless already set

Those defaults are local runtime defaults, not a public security model.
Platform still owns network access, attach authentication, account policy, and
egress policy.

## Startup And Readiness

A conforming runner must start the Maestro HTTP server only after the workspace
root exists and resolves to a directory. Startup must fail closed when required
identity or workspace fields are missing.

Diagnostics go to stderr or the configured log sink. Protocol stdout must stay
reserved for JSON when the stdio headless transport is used. HTTP handlers must
return structured JSON errors rather than human log text.

Platform should treat the identity endpoint as the readiness gate:

```http
GET /.well-known/evalops/remote-runner/identity
```

The response is intentionally sparse:

```json
{
  "protocol_version": "evalops.remote-runner.identity.v1",
  "runner_session_id": "mrs_123",
  "owner_instance_id": "pod_123",
  "ready": true,
  "draining": false
}
```

`ready=false` or `draining=true` means the gateway must not attach new clients.
If Platform expects an owner generation, it must compare
`owner_instance_id` before proxying attach traffic.

The identity response also exposes the local runtime lease projection:

```json
{
  "runtime_lease": {
    "protocol_version": "evalops.maestro.hosted-runner-lease.v1",
    "state": "bound",
    "generation": 3,
    "maestro_session_id": "maestro-session-123",
    "lease_token_present": true,
    "heartbeat_at": "2026-05-20T04:00:00.000Z",
    "updated_at": "2026-05-20T04:00:01.000Z"
  }
}
```

This projection is intentionally compact. It is the TypeScript runtime's local
fencing contract until Platform owns a durable runner-lease table. Gateways and
operators can distinguish `unbound`, `bound`, and `draining` without reading
process memory or scraping logs.

The Maestro Helm chart defaults to `replicaCount=1` with
`headlessRuntime.routing.mode=single-replica` because the TypeScript
web/headless runtime keeps session ownership, connection leases, event replay,
and utility operation state in-process. A chart render with `replicaCount > 1`
must declare either `headlessRuntime.routing.mode=sticky-session` for ingress
affinity on `/api/headless/*` and `/api/chat/ws`, or
`headlessRuntime.routing.mode=durable-owner` for a gateway backed by durable
runtime-owner records.

## Attach Surface

Managed clients use a thread as the durable public object:

- `POST /api/headless/threads/:threadId/turns`
- `GET /api/headless/threads/:threadId`
- `GET /api/headless/threads/:threadId/events?cursor=:cursor`

`POST .../turns` accepts protocol
`evalops.maestro.thread.v1`, a caller-stable `turnId`, `kind` equal to
`user_message` or `steer`, content, and optional attachments. A retry with the
same turn id and payload returns the existing run without executing it again.
Reusing a turn id with a different payload fails with `409`. While a run is
active, a normal `user_message` also fails with `409`; an explicit `steer`
appends to the same thread and is delivered to the resident agent.

Thread state exposes an active turn, cursor, append-only turn records, and one
of these explicit phases: `idle`, `accepted`, `running`,
`waiting_for_approval`, `waiting_for_input`, `waiting_for_client_tool`,
`waiting_for_retry`, `completed`, `failed`, or `interrupted`.

The event stream returns only envelopes whose cursor is greater than the
requested cursor. A client persists its last applied cursor and deduplicates by
cursor after reconnect. A reset envelope replaces local runtime state only when
the requested cursor has fallen outside the retained replay window.

The runtime keeps a private journal under
`<workspace>/.maestro/hosted-runner/threads`. It durably records accepted
turns, redacted replay envelopes, cursor, and generation. An exclusive
cross-process lock prevents overlapping writers. A replacement may adopt a
journal only at the same or a newer generation; an older generation fails
closed. Non-terminal turns become `interrupted` after process replacement
rather than being executed twice.

The generation header on an append is a fenced assertion checked against the
runtime's canonical generation. It is not authentication. Managed production
traffic is authorized by the workload mTLS boundary and live resident binding;
the guest and browser never receive workload credentials.

The lower-level session surface remains for local clients and compatibility:

- `POST /api/headless/connections`
- `POST /api/headless/sessions/:id/subscribe`
- `GET /api/headless/sessions/:id/events`
- `POST /api/headless/sessions/:id/messages`
- `POST /api/headless/sessions/:id/heartbeat`
- `POST /api/headless/sessions/:id/disconnect`
- `GET /api/headless/sessions/:id/state`

Runtimes may keep `/api/headless/sessions/:id/message` as a compatibility
alias. New managed Runner Host and Platform code must use the thread surface
for user turns; legacy session ids are not the durable product object.

The event stream is replayable by cursor. Clients that fall behind receive a
reset snapshot. This mirrors the reference remote-session pattern: durable
control session plus reconnectable event stream, rather than a single fragile
socket.

Connections negotiate:

- role: `controller` or `viewer`
- server request capabilities: `approval`, `client_tool`, `user_input`,
  `tool_retry`
- utility operations: `command_exec`, `file_search`, `file_read`, `file_watch`
- notification opt-outs: `status`, `heartbeat`, `connection_info`,
  `compaction`

Viewers are read-only. Controllers hold the mutation lease. Controller takeover
must be explicit and visible in heartbeat/subscription snapshots.

Headless session creation must also pass the hosted-runner lease check. A bound
runner only accepts the already-bound Maestro session id; a new or different
session receives `runtime_owned_elsewhere` with the active and requested session
ids plus the lease generation. A draining runner receives `runtime_not_ready`.
Those error reasons are stable so Platform can map them to stale-session,
drain, and retry policies.

## Workspace Rules

The workspace root is the only default file-system authority for hosted utility
operations.

- `utility_file_read` must reject paths outside the workspace root.
- `utility_file_search` must only return workspace-contained paths.
- `utility_file_watch_start` must only watch workspace-contained roots.
- `utility_command_start` must default to the workspace root unless a safe
  workspace-contained cwd is supplied.
- Drain export paths must stay inside the workspace root.

Provider-level file APIs, such as Daytona or Modal filesystem APIs, do not
weaken this contract. They are implementation helpers behind Platform, not
additional Maestro client authority.

## Runtime Lifecycle

The lifecycle is:

1. Platform creates a runner session and selects a provider profile.
2. Provider starts the runtime with the contract configuration.
3. Maestro binds the process to the runner session and workspace root.
4. Platform waits for the identity endpoint to report `ready=true`.
5. Clients attach through Platform's gateway or directly in local development.
6. Platform sends heartbeats and samples usage through its control plane.
7. Platform requests drain before TTL expiry, budget exhaustion, or user stop.
8. Maestro stops active headless work, flushes session state, writes a local
   snapshot manifest, and reports drain status.
9. Platform uploads artifacts if required and terminates the provider instance.
10. If Platform starts a replacement runner from uploaded artifacts, it passes
    the restored local manifest path through the restore-manifest field. Maestro
    validates the manifest against the workspace root, seeds the runtime cursor
    and last snapshot state, emits a `restored_from_snapshot` reset event, and
    accepts fresh controller/viewer attachments only when the runtime flush was
    completed.

Drain uses:

```http
POST /.well-known/evalops/remote-runner/drain
```

The Helm chart's default preStop hook calls this endpoint with the enum-backed
`kubernetes_prestop` reason/requester before sleeping briefly for Kubernetes
termination propagation. If a process receives SIGINT or SIGTERM without that
preStop call, the web server runs the same drain path with the
`process_shutdown` reason and writes the same manifest envelope before closing
connections.

The manifest protocol is
`evalops.remote-runner.snapshot-manifest.v1`. Both Rust-hosted and
TypeScript-hosted drain paths write this same local manifest envelope, including
the runtime flush status, workspace export contract, headless runtime snapshot,
schema-versioned `runtime_continuity` evidence for the source runner session,
source owner instance, source process, restore manifest path, and replay cursor,
schema-versioned `work_continuity` metadata for active/pending
Codex subagent child runs, and
`retention_policy` metadata describing visibility and redaction classes. Managed
drains also include `platform_evidence`, a compact operator-safe record that
Platform can store as AgentRuntime progress before the run is completed or
failed.
Maestro does not upload to GCS, S3, Modal storage, Daytona storage, or any
other provider store. Upload, retention, workspace artifact hydration, and
choosing which manifest should be restored are Platform responsibilities. See
[Hosted Runner Retention](./hosted-runner-retention.md) for the policy rules
that travel with the manifest.

## Rust Hosted Surface

The Rust crate exposes a first hosted-runner library surface through
`maestro_tui::hosted_runner::start_hosted_runner`. It binds a single-session HTTP
runtime for tests and local adapters, exposes the identity/readiness/drain
contract, serves the replayable headless attach endpoints, enforces
workspace-root containment for file, watch, and command utility operations, and
writes the local drain manifest with the requested workspace export paths. It
deliberately keeps provider scheduling and artifact upload out of Rust;
Platform still owns those concerns.

The Rust surface can also start from a previously written snapshot manifest via
`MAESTRO_REMOTE_RUNNER_RESTORE_MANIFEST`, `REMOTE_RUNNER_RESTORE_MANIFEST`, or
`HostedRunnerConfig::with_restore_manifest_path`. Relative paths resolve under
the workspace root. Startup rejects manifests with an unsupported protocol
version or workspace export paths that escape the current workspace. Restore is
a runtime-state seed: it preserves the logical Maestro session id, cursor,
last init, and snapshot state for reconnecting clients, then emits a reset
snapshot with reason `restored_from_snapshot`. Manifests whose
`runtime.flush_status` is `failed` or `skipped` restore into a not-ready
inspection state: identity reports `ready=false`, `/readyz` and attach routes
return `runtime_not_ready`, and the runtime snapshot surfaces the restore
problem in `last_status`, `last_error`, and `last_error_type`. It does not
hydrate files from cloud storage; the provider must mount or download workspace
artifacts before starting Maestro.

The Rust surface now has both an opt-in hosted conformance adapter and a real
hosted-runner CLI wrapper. `maestro-tui hosted-runner` and the
`maestro-hosted-runner` binary parse the same Platform contract names, bind the
Rust hosted HTTP/SSE server, and forward headless traffic through an
`AgentSupervisor` to the configured Maestro headless executable. The wrapper
honors `MAESTRO_HEADLESS_CLI_PATH` or `MAESTRO_AGENT_SCRIPT` when the runtime
image needs to point at a packaged CLI, while leaving the default production
entrypoint as `maestro --headless`.

The required hosted adapter also covers the drain handoff shape: manifest
response, persisted snapshot file, export-path recording, and post-drain
mutation rejection.

## Error Vocabulary

Provider implementations should normalize failures to these categories at the
gateway or attach boundary:

| Error | Retry | Meaning |
| --- | --- | --- |
| `runtime_not_ready` | yes | Provider object exists but identity says not ready or draining. |
| `runtime_proxy_failed` | yes | Gateway could not reach the runtime. |
| `runtime_owned_elsewhere` | no | Identity owner generation does not match the control-plane owner. |
| `runtime_lost` | no | Provider runtime disappeared or became unrecoverable. |
| `runtime_failed` | no | Runtime exited after an infrastructure or startup failure. |
| `runtime_exited` | no | Runtime exited cleanly or after user stop. |
| `access_denied` | no | Attach auth, audience, account, or role is invalid. |
| `workspace_violation` | no | A requested path escapes the hosted workspace root. |
| `unsupported_capability` | no | Client negotiated a capability this runner does not provide. |
| `drain_timeout` | maybe | Drain did not complete before the provider grace window. |

Retryable HTTP failures should use `503` and `Retry-After` where they pass
through HTTP. Permanent authorization and owner failures should not be retried
without a fresh attach token or a new runtime owner.

Hosted runner ownership failures should include a machine-readable
`runtime_owned_elsewhere` code in the JSON error body. TypeScript web-hosted
runners expose it as `error_type` and a `google.rpc.ErrorInfo` detail with
`reason=runtime_owned_elsewhere`, plus routing metadata such as
`runner_session_id`, `owner_instance_id`, `maestro_session_id`, and
`requested_maestro_session_id`. Gateways and clients must key off that code
rather than matching human error text.

## Provider Notes

These notes are intentionally non-contractual. They describe useful provider
primitives without leaking them into Maestro's public runtime shape.

### GKE Standard And Spot

GKE pods are the default self-operated profile because they match Platform's
existing control-plane model: one Kubernetes object per runner session, native
status reconciliation, service routing, resource requests, taints, and
tolerations.

Use stable profile ids at the Platform boundary. Keep node selectors,
tolerations, Spot settings, and node-pool names in deploy/Platform. Kubernetes
recommends node isolation labels that kubelets cannot modify; labels protected
by the `node-restriction.kubernetes.io/` prefix are the right pattern for
security-sensitive scheduling.

### GKE Sandbox

The secure profile can run the same Maestro contract with `RuntimeClass=gvisor`
on a GKE Sandbox node pool. GKE Sandbox applies the `sandbox.gke.io/runtime:
gvisor` label and matching taint to capable nodes, and pods using the `gvisor`
RuntimeClass receive the corresponding scheduling rules.

Use this when defense-in-depth matters more than raw syscall compatibility or
performance. Do not make `gvisor` part of Maestro client input; expose it only
as a resolved profile property.

### GKE Workload Identity Federation

Hosted runners must not receive a long-lived bearer or cloud key. The
Kubernetes provider sets `automountServiceAccountToken: false` and projects one
pod-bound TokenRequest JWT into the Maestro container only. The projection has
an exact EvalOps STS audience and a short expiration with kubelet rotation; it
does not grant generic Google Cloud IAM access and is not mounted into the guest
workspace or helper containers.

Maestro exchanges that projected identity with the configured EvalOps Identity
service using a fresh in-memory P-256 CSR for a certificate valid for at most
five minutes. The issued identity is bound to the typed runtime
resource, including sandbox id, authoritative pod UID, resident generation,
and the current Platform session binding. Issuance and renewal fail closed when
the Kubernetes issuer, audience, subject, pod UID, lease, generation, placement,
or digest-pinned runtime image no longer matches the provider's authoritative
state.

The hosted runner re-reads the kubelet-rotated token and generates a new key for
every renewal. It atomically replaces the TLS configuration before expiry and
closes connections authenticated under the prior generation. Failed renewal
keeps the current certificate only until its recorded expiry, then closes
existing connections and refuses new TLS handshakes until exchange succeeds.
No projected token, private key, CSR, or certificate is persisted or logged.
Runner Host connects through the provider-private endpoint, verifies the exact
dynamic Maestro certificate identity and session binding, and presents its own
short-lived ClientAuth identity. Maestro accepts it only when it chains to the
CA returned by the same Identity exchange and has the exact Runner Host URI.
Attach tokens authorize a user to the Runner Host proxy; they are never
forwarded to Maestro and are not runner credentials.

Provider implementations must cover projected-token rotation, certificate
renewal, stale generation and replaced-pod rejection, wrong audience/issuer/
subject rejection, and revocation after stop or lease expiry. The public
Maestro contract intentionally does not expose Kubernetes service-account
names, cluster issuers, cloud IAM principals, certificate authorities, or
provider DNS names.

### Daytona

Daytona sandboxes provide isolated computers with lifecycle APIs for create,
start, list, stop, archive, recover, resize, delete, labels, resources, and
public HTTP previews. A Daytona provider can map Platform runner metadata to
sandbox labels, start Maestro inside the sandbox, and expose the hosted runner
port through a private or public preview URL.

The Maestro contract stays the same: the sandbox preview points to the same
identity, drain, and headless endpoints.

### Modal

Modal Sandboxes are useful for provider-managed ephemeral environments. The API
has readiness probes, tunnels by container port, connect tokens, process exec,
filesystem APIs, file watching, termination, polling, and directory snapshots.

A Modal provider can use readiness probes for the identity endpoint, tunnels or
connect tokens for attach routing, and `snapshot_directory` as an implementation
detail after Maestro writes its local drain manifest.

### Firecracker

Firecracker can be a future isolation substrate, especially if cold-start and
snapshot restore are important. Its snapshot support can serialize and later
restore a running microVM workload. That is powerful but should stay behind a
provider adapter until Maestro has a runtime identity, secret rotation, network
proxy, workspace mount, and drain/resume story that preserves session
uniqueness after restore.

Do not make clients choose Firecracker directly. Treat it as a backend for a
profile such as `maestro-secure` or `maestro-fast-restore` after Platform owns
the operational model.

## Conformance

Two executable suites back this contract; both run in the Maestro CI Rust lane.

Startup and lifecycle (`cargo test -p maestro-local-host --test hosted_runner_startup_contract`):

- `packages/local-host-rs/tests/fixtures/hosted-runner-startup-contract.json`
  is the accept/reject matrix for the [Configuration](#configuration)
  coordinates. Each case is resolved through
  `hosted_runner_cli::resolve_hosted_runner_launch_config`, the same code path
  as `maestro hosted-runner`, and asserts either the resolved contract fields or
  the fail-closed error. Adding a coordinate means adding a fixture case; the
  suite rejects any exercised coordinate that this document does not name.
  The guard is one-directional: coordinates named here but absent from the
  fixture (for example `MAESTRO_REMOTE_RUNNER_SNAPSHOT_ROOT`,
  `MAESTRO_REMOTE_RUNNER_RESTORE_MANIFEST`, and the split `--host`/`--port`
  flags) are documented, not mechanically enforced.
- The runtime scenario starts the full CLI runtime behind a stub headless child
  and checks the identity endpoint (`ready`, `draining`, sparse fields), a
  drain that writes the snapshot manifest under the workspace root, and the
  post-drain identity transition.

Wire surface (`maestro conformance` and the `rust-hosted-conformance` CI job):
schema-valid snapshots/envelopes, controller/viewer roles, explicit controller
takeover, cursor replay/reset, approval request and response resolution,
workspace-root file-read enforcement, utility command/search/watch lifecycle,
and disconnect cleanup, driven through
`maestro_local_host::hosted_runner_conformance` against the external HTTP/SSE
surface. See [Headless runtime conformance](./headless-conformance.md).

## References

- [Headless protocol reference](./headless.md)
- [Headless runtime conformance](./headless-conformance.md)
- [Hosted runner retention](./hosted-runner-retention.md)
- [Kubernetes node isolation and NodeRestriction](https://kubernetes.io/docs/concepts/scheduling-eviction/assign-pod-node/#node-isolation-restriction)
- [GKE Sandbox with gVisor](https://docs.cloud.google.com/kubernetes-engine/docs/how-to/sandbox-pods)
- [GKE Workload Identity Federation](https://docs.cloud.google.com/kubernetes-engine/docs/how-to/workload-identity)
- [Daytona sandboxes](https://www.daytona.io/docs/en/sandboxes/)
- [Modal sandboxes](https://modal.com/docs/guide/sandboxes)
- [Modal Sandbox reference](https://modal.com/docs/reference/modal.Sandbox)
- [Firecracker snapshot support](https://github.com/firecracker-microvm/firecracker/blob/main/docs/snapshotting/snapshot-support.md)

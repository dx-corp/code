# Safety and Action Approvals

> **Status:** Current Rust runtime. The controls documented here live under
> `packages/tui-rs/src/safety/`, `packages/sandbox-rs/src/lib.rs`, and
> `packages/runtime-gateway-rs/src/auth.rs`.

Audience: operators and contributors configuring approvals and sandboxing.

Nav: [Docs index](README.md) · [Quickstart](QUICKSTART.md)

Deixic Code can execute shell commands and change files on the host. The action
firewall, approval flow, policy checks, and native sandbox are independent
layers. No layer makes an untrusted repository safe by itself.

## Action firewall

`packages/local-host-rs/src/safety/firewall.rs` is the central tool-call checkpoint.
It returns one of:

- `Allow`
- `RequireApproval`
- `Block`

For bash, the firewall combines high-severity dangerous-pattern blocks with
parsed command analysis. The rules cover destructive operations, filesystem
formatting, disk writes, privilege changes, shell metacharacter risk, and
other command shapes. File reads/writes and path-bearing tools also pass
containment and policy checks.

The firewall checks network-tool URLs against the enterprise policy and
requires approval for MCP tools by default. Server-provided MCP annotations
cannot lower that approval requirement.

### Approval modes

Use `--approval-mode` or `MAESTRO_APPROVAL_MODE`:

| Mode | Behavior |
| --- | --- |
| `prompt` | Ask the user for approval; approval-gated work fails when no interactive approver exists. |
| `auto` | Approve approval-gated work automatically. Use only in a trusted, isolated environment. |
| `fail` | Reject approval-gated work immediately. |

High-severity firewall blocks remain blocks; changing approval mode does not
turn them into allowed operations. A request to bypass an active native
sandbox (`bypass_sandbox: true`) is separately approval-gated.

### Bash guard and egress

The bash analyzer is enabled by default. `MAESTRO_BASH_GUARD=1` forces it on;
`MAESTRO_BASH_GUARD=0` disables the additional analysis and leaves the
hard-pattern and path/policy checks in place. Use the latter only for trusted
compatibility cases.

Set `MAESTRO_NO_EGRESS_SHELL=1` to require approval for shell commands
containing common egress primitives such as `curl`, `wget`, `ssh`, `scp`, `nc`,
or `/dev/tcp`. An explicit allowlist or
`MAESTRO_ALLOW_EGRESS_SHELL=1` can override that gate; review such overrides.

### Enterprise policy

The policy loader checks, in order, explicit paths in
`MAESTRO_ENTERPRISE_POLICY_PATH` and `MAESTRO_POLICY_PATH`, then
`$MAESTRO_HOME/policy.json` or the legacy Composer home. Policy can constrain:

- tools, commands, dependencies, and models;
- filesystem paths;
- allowed/blocked network hosts and private or localhost addresses; and
- token, session-duration, and concurrent-session limits.

Malformed or unreadable policy fails closed. Keep policy files owner-readable
and validate changes before deploying them.

#### Managed organization policy

Set `MAESTRO_MANAGED_POLICY_PATH` to opt into a signed organization policy
bundle. The bundle uses an Ed25519 signature over a canonical payload and
includes an SHA-256 policy hash, organization/workspace scope, version, expiry,
and an optional kill switch.

Set `MAESTRO_MANAGED_POLICY_PUBLIC_KEY` to the base64url or hex public key.
`MAESTRO_MANAGED_POLICY_STATE_PATH` optionally selects the persistent rollback
watermark; otherwise it is stored beside the managed bundle.
`MAESTRO_MANAGED_POLICY_KEY_ID`, `MAESTRO_ORG_ID`, and
`MAESTRO_WORKSPACE_ID` can pin the signing key and deployment scope. A
configured bundle that is missing, invalid, expired, rolled back, tampered
with, or revoked fails closed. The local policy may only narrow its limits.
Authenticated `GET /api/admin/enterprise-policy/status` and
`POST /api/admin/enterprise-policy/refresh` expose safe status metadata.

#### Managed policy publishing and audit

Keep the private signing key in the organization KMS or HSM. The publisher
service signs the canonical envelope outside Deixic Code and submits it through
the authenticated runtime gateway; Deixic Code never receives private-key material.
Configure `MAESTRO_MANAGED_POLICY_AUDIT_PATH` to select the local JSONL audit
file. By default it is the managed-policy state path with `.audit.jsonl`
appended.

The publisher endpoints are:

- `POST /api/admin/enterprise-policy/publish` with
  `{ "envelope": <ManagedPolicyEnvelope> }` validates the signature, scope,
  expiry, hash, and monotonic version, then atomically activates the bundle.
- `GET /api/admin/enterprise-policy/audit?limit=50` returns the newest
  accepted and rejected publication events, capped at 100 entries.

Publication is authenticated and CSRF-protected like other state-changing
runtime-gateway API calls. A malformed request returns `400`; a rejected
signature, scope, expiry, rollback, or kill-switch-reason validation returns
`409`.
Successful publication returns the safe managed-policy status. A valid
kill-switch envelope is recorded as published, but its status is invalid so
policy-gated actions remain blocked. Accepted publications and
envelope-validation failures are recorded with the authenticated actor,
outcome, safe policy metadata, and bounded failure reason; malformed HTTP
requests are rejected before publication processing. Audit records never
contain the policy signature or private key.

The intended customer flow is:

1. Render a versioned envelope from the organization policy source.
2. Ask the KMS/HSM to sign the canonical payload and attach the signature and
   policy hash.
3. POST the envelope with the publisher service's runtime-gateway credentials.
4. Monitor the returned status and the audit endpoint, while keeping the
   remote KMS/HSM and proxy logs as the authoritative organization audit trail.

The request shape is intentionally redacted here; the publisher fills the
fields from the existing `ManagedPolicyEnvelope` contract and supplies the
KMS/HSM output:

```sh
curl -X POST "$MAESTRO_URL/api/admin/enterprise-policy/publish" \
  -H "Authorization: Bearer <runtime-gateway-credential>" \
  -H "X-Maestro-CSRF: <csrf-token>" \
  -H "Content-Type: application/json" \
  --data @signed-envelope.json
```

```json
{
  "envelope": {
    "schemaVersion": 1,
    "orgId": "org-example",
    "workspaceId": "workspace-example",
    "policyVersion": 42,
    "issuedAt": 0,
    "expiresAt": 0,
    "keyId": "org-policy-key",
    "policy": {},
    "killSwitch": false,
    "policyHash": "<sha256-of-canonical-payload>",
    "signature": "<kms-hsm-signature>"
  }
}
```

Use `GET /api/admin/enterprise-policy/audit?limit=50` for the newest local
events; the endpoint caps reads at 100 records.

## Safe mode

Set `MAESTRO_SAFE_MODE=1` to enable validators after file changes.
Configured LSP diagnostics can block unsafe results. The action firewall and
path containment remain independent; use `/sandbox` in the interactive UI to
inspect the active native policy.

## Native sandbox

`packages/sandbox-rs/src/lib.rs` defines three policies:

| Policy | Effect |
| --- | --- |
| `read-only` | No filesystem writes and no network access for the sandboxed child. |
| `workspace-write` | Writes are limited to the workspace and configured writable roots; network access is explicit in the policy. |
| `danger-full-access` | No native filesystem or network restriction. |

Platform backends:

- macOS: Seatbelt via `/usr/bin/sandbox-exec`;
- Linux: Landlock for filesystem access plus seccomp for network-disabled
  policies;
- other platforms: native sandboxing is unavailable.

The interactive TUI applies the configured policy by default (`workspace-write`
unless the config says otherwise). `MAESTRO_SANDBOX_MODE` overrides it for one
run, and `MAESTRO_INTERNAL_TUI_SANDBOX_DEFAULT=0` is an internal opt-out for
hosts where the native mechanism breaks real sessions.

The sandbox is applied in the child immediately before `exec`. The child
environment is cleared and replaced with the filtered environment passed by
the executor. If the native mechanism is unavailable, the current interactive
path reports the condition rather than silently claiming isolation. Verify the
status in the TUI and use OS/container isolation when enforcement is required.

The `danger-full-access` policy is an explicit escape hatch, not a safe
default. Avoid `bypass_sandbox` and full-access mode for untrusted work.

## Credentials and PII

`packages/tui-rs/src/agent/credential_store.rs` recognizes and redacts common
credential formats in tool arguments and serialized output. The workflow
tracker in `safety/workflow_state.rs` can block unredacted PII before
human-facing tools.

These are pattern-based controls, not a guarantee. Do not paste long-lived
secrets into prompts or use an untrusted repository with a credential that can
modify production systems. Anything intentionally sent to a model provider or
external tool is disclosed to that service.

## Runtime-gateway safety

For a shared or remote web deployment:

```
MAESTRO_PROFILE=prod \
MAESTRO_JWT_JWKS_URL="https://identity.example/.well-known/jwks.json" \
MAESTRO_WEB_CSRF_TOKEN="$(openssl rand -hex 32)" \
maestro serve
```

The runtime gateway requires authentication on non-loopback binds. It accepts
API-key, shared-secret, JWT/JWKS, or trusted-proxy authentication as described
in `docs/THREAT_MODEL.md`. Core session and chat routes in production require
tenant-bearing JWT or trusted-proxy identity; the static process API key alone
does not grant access to tenant data. State-changing API and A2A requests are CSRF
protected when CSRF enforcement is enabled. `MAESTRO_WEB_REQUIRE_KEY=0` is a
loopback-only development switch.

The Rust runtime gateway does not provide general RBAC or SSO. If remote access
depends on a proxy for identity or authorization, make that proxy part of the
deployment threat model.

## Release and extension safety

The installer verifies a release checksum and, when the release publishes the
signed bundle, verifies its Cosign identity before atomically switching the
launcher. Set `MAESTRO_REQUIRE_SIGNED_INSTALL=1` to reject unsigned legacy
artifacts.

MCP servers are separate trust boundaries with broad tool access. Pin and
review them before enabling them. The `maestro-execpolicy` crate is currently a
dependency-light parsing/migration leaf and is not the live approval path.

## Recommended operating posture

- Use `prompt` or `fail` approval for untrusted work.
- Prefer `read-only` or `workspace-write` native sandboxing.
- Set `MAESTRO_NO_EGRESS_SHELL=1` for sensitive repositories.
- Use `MAESTRO_PROFILE=prod` for shared runtime-gateway deployments.
- Keep provider and MCP credentials short-lived and least-privileged.
- Run `maestro setup` after installation and `maestro doctor --live` when
  diagnosing a deployment; do not paste diagnostic secrets into tickets.

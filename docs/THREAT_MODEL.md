# Deixic Code Threat Model

> **Status:** Current Rust runtime. This document is checked against the
> implementation in `packages/tui-rs/`, `packages/runtime-gateway-rs/`, and
> `packages/execpolicy-rs/`. It describes the controls that exist today; it
> does not claim that prompt injection, secret exposure, or arbitrary code
> execution is solved.

Audience: security reviewers and operators deploying Deixic Code in sensitive
environments.

Nav: [Docs index](README.md) · [Safety](SAFETY.md) · [Enterprise](ENTERPRISE.md)

## Scope and trust boundaries

Deixic Code runs a local Rust agent and tool executor. The web/runtime-gateway
process exposes a separate HTTP boundary. Model providers, MCP servers,
websites, files, and tool results are untrusted inputs unless the operator has
chosen to trust them.

```
 User / web client
        │ terminal or authenticated HTTP
        ▼
 Rust agent and runtime gateway
        │
        ├── model/provider APIs (external)
        ├── MCP servers and web tools (external)
        └── safety gateway
                ├── action firewall and enterprise policy
                ├── path containment and sensitive-file checks
                ├── approval / guardian decisions
                └── optional native command sandbox
                        │
                        └── host processes and filesystem
```

The protected assets are source trees, credentials, model context, provider
tokens, runtime-gateway sessions, and the host account. A user approval is an
authorization decision; text found in a file, web page, MCP response, or model
output is not authorization by itself.

## Threats and current controls

### Prompt injection

**Threat:** Instructions embedded in repository files, web content, MCP
responses, or model output attempt to change the task, gain authority, or
exfiltrate data.

**Controls:**

- The agent protocol treats wrapped content as data and does not grant it
  authority to change the user's task.
- `packages/local-host-rs/src/safety/firewall.rs` checks tool calls after the model
  proposes them.
- Dangerous shell patterns are blocked or sent to approval by
  `dangerous_patterns.rs` and `bash_analyzer.rs`.
- Approval-gated operations are surfaced to the user; the guardian in
  `safety/guardian.rs` can add a review decision but cannot lower the hard
  safety ceiling.

**Residual risk:** High for unattended or permissive operation. Prompt
injection is not a complete security boundary: a malicious instruction can
still produce harmful text or a novel action that the user approves.

### Arbitrary command execution

**Threat:** A model, MCP server, or compromised input causes a shell command to
modify the host, escalate privileges, or destroy data.

**Controls:**

- `ActionFirewall::check_bash` applies high-severity pattern blocks before
  command execution and combines them with parsed bash-risk analysis.
- `check_command_policy` and `check_tool_allowed` apply the configured policy
  file from `~/.maestro/policy.json` or an explicit policy path.
- `bypass_sandbox` is itself approval-gated when a native sandbox is active.
- `MAESTRO_NO_EGRESS_SHELL=1` makes shell egress primitives such as `curl`,
  `wget`, `ssh`, `scp`, and `nc` approval-gated.
- `packages/sandbox-rs/src/lib.rs` provides native child-process enforcement:
  Seatbelt on macOS and Landlock plus seccomp on Linux. `read-only` and
  `workspace-write` restrict the child; `danger-full-access` deliberately
  does not.

Native sandbox availability is checked at runtime. A requested sandbox is not
proof that a command was isolated on a host where the enforcement mechanism is
unavailable; the interactive UI reports that condition and the operator must
choose whether to continue.

**Residual risk:** Low to medium with prompt/fail approvals and a working
native sandbox; high with automatic approval, `danger-full-access`, or
untrusted code on an unsupported host.

### Filesystem escape and destructive writes

**Threat:** A tool reads or writes outside the intended workspace, follows a
symlink to a protected path, or uses traversal to reach host data.

**Controls:**

- `packages/local-host-rs/src/safety/path_containment.rs` canonicalizes paths where
  possible and checks traversal, workspace/safe-zone containment, and
  protected system paths.
- The firewall applies these checks to read, write, edit, search, and related
  path-bearing tools.
- Sensitive filenames such as credential and environment files require extra
  approval in `firewall.rs`.
- `EnterprisePolicy.paths` can further restrict paths. Policy-load and
  policy-parse failures fail closed.

**Residual risk:** Medium. Containment is defense in depth; do not treat
workspace access as a substitute for OS isolation when the repository is
hostile.

### Network access and data exfiltration

**Threat:** The agent sends source, credentials, or private-network requests
to an attacker-controlled endpoint.

**Controls:**

- Network tools (`websearch`, `codesearch`, `webfetch`, `web_fetch`, and
  `extract_document`) pass URL checks in `firewall.rs` and `policy.rs`.
- Enterprise network policy supports allowed/blocked hosts and localhost or
  private-IP blocking.
- `MAESTRO_NO_EGRESS_SHELL=1` covers common shell-based egress primitives.
- MCP tools are approval-gated by default; server annotations cannot reduce
  the required approval.
- Native `read-only` sandbox mode blocks network access for its child process;
  `workspace-write` carries an explicit network-access setting.

**Residual risk:** Medium. URL and tool checks do not make external services
trustworthy, and data deliberately sent to a model provider or approved
external tool can no longer be recovered. Review URLs, MCP servers, and
provider data policies.

### Credential and PII exposure

**Threat:** API keys, tokens, passwords, or personal data enter model context,
logs, approval prompts, or human-facing tools.

**Controls:**

- `packages/tui-rs/src/agent/credential_store.rs` redacts recognized
  credential formats in tool arguments and JSON/text output, while preserving
  internal credential references where required.
- Sandboxed and unsandboxed child-command helpers clear the inherited
  environment and pass only the filtered environment constructed by the
  executor.
- `safety/workflow_state.rs` tracks PII capture/redaction state and the
  firewall can block unredacted PII before a human-facing egress tool.
- Sensitive path names are approval-gated by the firewall.

**Residual risk:** High. Pattern-based redaction is not perfect, file contents
can contain secrets that are not recognized, and anything intentionally
submitted to a provider is disclosed to that provider. Use isolated workspaces,
short-lived credentials, and provider-side controls.

### Denial of service

**Threat:** Runaway commands, excessive sessions, or oversized model work
consume host or provider resources.

**Controls:**

- Enterprise policy supports maximum tokens per session, session duration, and
  concurrent sessions through `LimitsPolicy`.
- The runtime gateway and provider bridges use bounded request timeouts in their
  integration paths, and child-process cleanup is exercised by regression
  tests.
- Safe mode can require a plan before mutation and run validators after writes.

**Residual risk:** Medium to high. Deixic Code does not claim a host-wide CPU,
memory, or disk quota for every child process. Use OS/container quotas and
provider limits for multi-tenant or hostile workloads.

### Supply chain and extension risk

**Threat:** A compromised release, dependency, or MCP server gains code
execution or data access.

**Controls:**

- Rust dependencies are pinned in `Cargo.lock` and checked by repository CI.
- The installer verifies release checksums and verifies Cosign signatures when
  the release publishes the signed bundle. Set
  `MAESTRO_REQUIRE_SIGNED_INSTALL=1` to reject unsigned legacy releases.
- MCP servers run outside the agent process and remain a broad trust boundary;
  review and pin them before enabling them.
- `packages/execpolicy-rs/src/lib.rs` is a parser/migration leaf, not the live
  approval gateway. It must not be wired into approvals without a separate
  trust-boundary review.

**Residual risk:** Medium. A valid signature authenticates the publisher, not
the safety of every feature, dependency, or MCP server.

## Runtime-gateway authentication

The HTTP runtime gateway is authenticated by
`packages/runtime-gateway-rs/src/auth.rs`:

- API keys are accepted as `Authorization: Bearer` or
  `x-maestro-api-key` (the legacy `x-composer-api-key` is also accepted), but
  production session and chat routes require tenant-bearing identity rather
  than a static process key alone.
- Identity JWTs are accepted through `MAESTRO_JWT_SECRET` (HS256) or
  `MAESTRO_JWT_JWKS_URL` / `MAESTRO_JWT_JWKS` (RS-family). Claims populate
  `subject`, `organization_id`, `workspace_id`, and `scopes` on `AuthContext`.
  The custom HMAC `MAESTRO_AUTH_SHARED_SECRET` authenticator is gone.
- A trusted proxy can authenticate a subject with
  `MAESTRO_WEB_TRUST_PROXY_AUTH_TOKEN` and an identity header.
- State-changing `/api/` and A2A requests require the configured CSRF token
  when CSRF enforcement is enabled.
- Non-loopback binds require configured authentication. Disabling the key
  requirement is only accepted for loopback development binds.

`AuthContext` carries an optional subject and an unrestricted API-key status.
The managed policy boundary is separate from identity and authorization:

- An explicitly configured bundle is verified locally before it can widen
  capabilities.
- Missing, invalid, expired, rolled-back, tampered, or kill-switched bundles
  block policy-gated actions.
- The publish endpoint accepts only an externally signed envelope. Private
  signing keys remain in the organization's KMS/HSM and are not handled by
  Deixic Code.
- Successful publication replaces the configured bundle atomically and
  advances a persistent version/hash watermark, preventing a stale valid
  bundle from being replayed.
- Publication attempts are recorded in a bounded local JSONL audit file with
  actor, outcome, safe metadata, and failure reason. The KMS/HSM and
  authenticated proxy remain the authoritative organization audit systems.
- Execution receipts record the accepted policy version and hash, but are not
  a tamper-evident audit log.

This repository does not claim a general RBAC, SSO, or tamper-evident audit
implementation in the Rust runtime gateway. The publish and audit endpoints are
authenticated, but organization-level authorization and identity scoping must
be enforced by the caller or a reviewed reverse proxy. An operator who can
modify the configured policy or audit files can still alter local state; use
owner-restricted paths and retain KMS/HSM, proxy, and centralized log records.

## Deployment checklist

For a local or shared deployment:

- Bind the runtime gateway to loopback unless remote access is required.
- For a remote bind, configure a supported tenant-bearing JWT or trusted-proxy
  authenticator for session and chat routes. A static `MAESTRO_WEB_API_KEY` may
  additionally protect non-tenant process routes; use `MAESTRO_PROFILE=prod`
  and configure CSRF for remote access.
- Keep approval mode at prompt or fail for untrusted work. Do not combine
  automatic approval with `danger-full-access`.
- Select `read-only` or `workspace-write` sandboxing where the host supports
  it, and verify `/sandbox` in the TUI.
- Set `MAESTRO_NO_EGRESS_SHELL=1` for sensitive work and review MCP servers.
- Prefer signed releases and set `MAESTRO_REQUIRE_SIGNED_INSTALL=1` when
  unsigned legacy artifacts are unacceptable.
- Use short-lived, least-privilege provider credentials and rotate them after
  suspected exposure.

## Incident response

If a session may have executed an unsafe action:

1. Stop the Deixic Code process and the affected child processes.
2. Revoke or rotate provider, API, proxy, and MCP credentials.
3. Preserve the session, command, policy, and runtime-gateway logs available to
   the deployment without copying secrets into an issue.
4. Review filesystem, network, and provider-side audit records.
5. Report security vulnerabilities to `security@evalops.dev` with a minimal
   reproduction, impact, and affected version.

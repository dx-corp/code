# Deixic Code Enterprise

Updated: 2026-08-17

Audience: operators deploying Deixic Code against EvalOps Platform, or locally with
bring-your-own-key (BYOK) provider credentials.

Nav: [Docs index](README.md) · [Safety](SAFETY.md) · [Threat model](THREAT_MODEL.md)

## Product rule

Deixic Code has two credential modes. A process must be in one of them before a
model turn starts.

| Mode | How it is selected | Inference | Secrets |
| --- | --- | --- | --- |
| Platform | `maestro evalops login` or `MAESTRO_EVALOPS_ACCESS_TOKEN` + `MAESTRO_EVALOPS_ORG_ID` | `llm-gateway` with a `provider_ref` | Org keys live in Platform `keys`. Deixic Code does not unwrap them. |
| BYOK | An explicit local-runtime route, or no identity session plus one usable local connection | Local runtimes or direct vendor APIs | Local keyring, env, file, 1Password, or delegated provider login. |

There is no Deixic Code-owned password, user table, or RBAC implementation. Human
login, org membership, and permission checks belong to Platform `identity`.
Managed provider credentials belong to Platform `keys`. Managed inference
belongs to `llm-gateway`.

Platform mode ignores local vendor keys. It does not fall back to
`ANTHROPIC_API_KEY` or `OPENAI_API_KEY` when `llm-gateway` or `identity` is
unavailable. Explicit `llamacpp/...`, `lmstudio/...`, and `ollama/...` routes
are a separate authless local path: they do not contact Identity or inherit
cached tenant scope. Selecting a local runtime is explicit; a managed route is
never silently redirected to it.

## First-run

```sh
maestro setup
```

The command offers two choices:

1. Sign in to EvalOps (`maestro evalops login` or `maestro setup --platform`)
2. Use your own API key (`maestro connections add` or `maestro setup --byok`)

`maestro doctor` reports `credential_mode`. The process is not ready until that
check passes.

## Platform mode

`identity` at `https://identity.evalops.dev` is the only issuer.

- CLI login is the existing PKCE loopback flow in `packages/local-host-rs/src/init_cli.rs`.
- The stored session carries access token, refresh token, `organization_id`,
  and the selected `provider_ref`.
- Model calls use the `evalops` provider in `packages/ai-rs` and go to
  `https://llm-gateway.evalops.dev/v1`.
- `maestro connections` lists and selects org provider refs. `add` uploads a
  key to `POST /v1/provider-refs` and does not keep the secret locally.
- The runtime gateway verifies identity JWTs against JWKS or
  `MAESTRO_JWT_SECRET`. `AuthContext` carries `subject`, `organization_id`,
  `workspace_id`, `scopes`, and `source`.

Canonical env vars:

```sh
MAESTRO_EVALOPS_ACCESS_TOKEN=
MAESTRO_EVALOPS_ORG_ID=
MAESTRO_EVALOPS_WORKSPACE_ID=
MAESTRO_KEYS_URL=https://keys.evalops.dev
MAESTRO_EVALOPS_BASE_URL=https://llm-gateway.evalops.dev/v1
```

Legacy aliases such as `EVALOPS_TOKEN` and `MAESTRO_ENTERPRISE_ORG_ID` are not
read.

## BYOK mode

No `llm-gateway`. A usable local connection is required. Explicit local-runtime
routes remain available when a stored Identity session is expired or the
network is unavailable; their turns are unbound from organization and workspace
telemetry. Managed setup derived from an unverified stored selector remains
fail-closed and cannot authorize tenant operations.

```sh
maestro connections add anthropic-api-key work --secret-stdin
maestro connections add openai-api-key work --from-1password op://vault/item/key
maestro codex login
```

The runtime gateway stays loopback-open, or uses `MAESTRO_WEB_API_KEY` for a
non-loopback bind. That key authenticates health and non-tenant process routes;
production session and chat routes additionally require tenant-bearing JWT or
trusted-proxy identity. It is not a provider key.

## Runtime-gateway authentication

See [Threat model](THREAT_MODEL.md). Supported authenticators:

- `MAESTRO_WEB_API_KEY` (static process key; non-tenant routes only in production)
- identity JWT (`MAESTRO_JWT_SECRET` or `MAESTRO_JWT_JWKS_URL`)
- trusted-proxy adapter (`MAESTRO_WEB_TRUST_PROXY_AUTH_TOKEN` plus identity headers)
- HMAC session cookie derived from a prior successful auth
- loopback development with no auth configured

`MAESTRO_AUTH_SHARED_SECRET` is removed.

## What this file used to claim

Earlier revisions described a Deixic Code-local password, bcrypt, Drizzle, and
RBAC schema. That stack is not implemented and must not be added. Use
Platform `identity` for org roles and `keys` for provider credentials.

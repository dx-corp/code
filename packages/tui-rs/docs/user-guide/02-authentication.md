# Authentication

Every Maestro session requires an EvalOps Identity account. Provider credentials
(managed inference, Codex subscription auth, and BYOK API keys) are a separate
choice made after that Identity sign-in.

---

## EvalOps Identity (required)

Sign in before starting Maestro. The browser flow lets a new user create an
account when necessary:

```bash
deixic-code evalops login
```

This Identity session is required even when you bring your own provider key or
use a Codex subscription. It does not replace those credentials or expose your
BYOK secret to EvalOps.

---

## Codex login (after EvalOps Identity)

```bash
maestro codex login
```

- Published installs use the packaged `@openai/codex` app-server.
- Source checkouts can also reuse a `codex` binary already on `PATH`.
- If Codex is already signed in, Maestro reports that account instead of starting a second flow.
- Force a refresh: `maestro codex login --force`
- Headless / remote machines: `maestro codex login --device-auth`

Bare `maestro` defaults to `openai-codex/gpt-5.5` when Codex auth is available. For another Codex subscription model, select under the `openai-codex` provider (for example `openai-codex/gpt-5.5`) via `/model` or CLI flags.
Named Codex identities can be bound to a workspace so Maestro never falls back
to another account. Configure `~/.maestro/codex-auth-profiles.json`:

```json
{
  "version": 1,
  "profiles": {
    "work": {
      "codex_home": "/absolute/path/to/work-codex-home",
      "workspace": "/absolute/path/to/workspace"
    }
  }
}
```

Use `--profile work` with `login`, `logout`, `status`, `ready`, or `doctor`, and
set `MAESTRO_CODEX_PROFILE=work` for native runs. A missing profile or workspace
mismatch fails closed. `maestro codex doctor --profile work` reports the selected
identity, auth health, app-server protocol/connectivity, and dynamic-tool
compatibility without printing tokens or the account email.

Check whether the selected Codex profile can accept a prompt:

```bash
maestro codex ready
maestro codex ready --profile work --model openai-codex/gpt-5.6 --json
```

`ready` exits 0 when auth, required app-server methods, required notifications,
dynamic-tool schemas, and the local thread binding are usable. It exits 1 when a
required check fails. Missing `thread/resume` or `turn/steer` support is reported
as optional degraded support and does not fail readiness.

Use `--model MODEL` when checking a non-default Codex model. The binding check
uses the same canonical model id that runtime sends to `thread/start`, so
`maestro codex ready --model openai-codex/gpt-5.5-pro` checks the `gpt-5.5-pro` binding
instead of the default model binding.

Common Codex auth commands:

| Command | Exit / status behavior |
|---------|------------------------|
| `maestro codex login` | Starts browser sign-in. Use `--device-auth` on remote or headless machines. |
| `maestro codex status` | Reports whether the selected profile is signed in. It does not print the account email. |
| `maestro codex ready [--model MODEL]` | Reports auth, app-server compatibility, tool-schema, binding, and optional resume/steering state. |

Common failure modes:

| Failure | Next step |
|---------|-----------|
| `signed_out`, `expired`, or `invalid` auth | Run `maestro codex login` for the selected profile. |
| Missing required app-server capability | Upgrade the installed Codex CLI or packaged Maestro release. |
| Dynamic-tool schema error | Run `maestro codex doctor` and fix the reported tool schema. |
| Binding integrity failure | Re-run `maestro codex ready`; Maestro quarantines the invalid binding record. |

---

## API keys and environment variables (after EvalOps Identity)

After the required Identity sign-in, managed connections are the recommended
BYOK path when developers switch accounts or do not want keys in their shell
environment:

```bash
maestro connections types
maestro connections add openai-api-key openai-work
maestro connections add anthropic-api-key anthropic-work \
  --from-1password 'op://Engineering/Anthropic/credential'
maestro connections status openai-work
```

Literal input is hidden and stored in the OS credential store. References to
environment variables, files, and 1Password stay non-secret metadata in
`~/.maestro/connections.json`. Existing provider environment variables retain
precedence unless `MAESTRO_CONNECTION=<id>` explicitly selects a connection.
That explicit selection also replaces stale provider credential-file or
1Password sources. Keyring rotation advances a persisted generation so leases
issued before the rotation are rejected. Platform-only connection types must
be configured and used through the Platform broker, not the local CLI.

Export provider keys in your shell or `.env`:

```bash
export OPENAI_API_KEY=sk-...
export ANTHROPIC_API_KEY=sk-ant-...
```

Common native TUI env vars:

| Variable | Purpose |
|----------|---------|
| `ANTHROPIC_API_KEY` | Anthropic / Claude |
| `OPENAI_API_KEY` | OpenAI API |
| `MAESTRO_MODEL` | Override default model |

Additional providers (OpenRouter, Azure OpenAI, GitHub Copilot, Groq, xAI, Cerebras, DeepSeek, Moonshot/Kimi, DashScope/Qwen, MiniMax, Z.ai/GLM, managed EvalOps) are documented in [Models](../../../../docs/MODELS.md).

---

## Key files

| Path | Role |
|------|------|
| `~/.maestro/connections.json` | Managed connection metadata and secret references; never literal secrets |
| `~/.maestro/keys.json` | Legacy stored provider keys |
| `~/.maestro/config.json` | Maestro config (including model registry overrides via `MAESTRO_CONFIG`) |
| `~/.maestro/models.json` | Legacy models registry path |

You can also point at a custom models file with `MAESTRO_MODELS_FILE`.

---

## Switching models in-session

```text
/model
/model claude-sonnet-4-5-20250514
/model gpt-4o
```

`/model` with no argument opens the model selector. CLI equivalents:

```bash
maestro --provider anthropic --model claude-sonnet-4-5-20250514
maestro --model gpt-4o
```

---

## Capability profiles

For task-level selection prefer agent profiles (`low`, `medium`, `high`, `ultra`) over ad-hoc model picks. Legacy aliases (`free`, `rush`, `smart`, `custom`, `frontier`) still map through. See [Agent Profiles](../../../../docs/AGENT_PROFILES.md).

---

## Diagnostics

```text
/about
/diag
/status
```

If auth fails, check `maestro --diag` / provider env vars and re-run `maestro codex login` when using Codex.

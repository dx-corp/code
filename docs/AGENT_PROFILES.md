# Agent Profiles

Agent profiles are versioned runtime bundles. A profile selects the primary model invocation, reasoning effort, complementary read-only Oracle, specialist invocations, fallbacks, and task budgets together. This prevents model comparisons from accidentally comparing different prompts, tools, or reasoning settings.

## Capability dial

| Level | Use it for |
| --- | --- |
| `low` | Bounded, obvious, reversible changes |
| `medium` | Ordinary repository work with moderate uncertainty |
| `high` | Ambiguous or cross-cutting work where a miss is expensive |
| `ultra` | Migrations, architecture, and discovery-heavy work |

Existing names remain accepted during migration: `free` and `rush` map to `low`, `smart` and `custom` map to `medium`, and `frontier` maps to `ultra`.

## Project profiles

Place YAML files in `.maestro/agent-profiles/`. A complete profile looks like:

```yaml
id: security-review-v1
version: 1
level: high
description: Cross-provider security review
primary:
  provider: openai-codex
  model: gpt-5.5
  reasoningEffort: high
oracle:
  provider: anthropic
  model: claude-opus-5-5
  reasoningEffort: high
  readOnly: true
specialists:
  reviewer:
    provider: anthropic
    model: claude-opus-5-5
    reasoningEffort: high
fallbackLevels: [medium, low]
budgets:
  maxAttempts: 2
  maxToolCalls: 40
  maxCostUsd: 5
```

Oracle configurations must be read-only and reasoning-capable at runtime. The built-in profiles always choose the Oracle, and the high and ultra reviewer specialists, from a different provider family than the primary model (`cross_provider` in `packages/tui-rs/src/mode_cli.rs`). Runtime reviewer subagent dispatch for the canonical levels still uses the parent provider; see evalops/mono#9321.

## Routing evidence

An assistant response completing without an error is not a verified success. There is no automatic promotion today: the model-candidate lifecycle is not wired into profile defaults, so any promotion is a manual, advisory decision (tracked in evalops/mono#9322). When automatic promotion ships, it must require at least 20 verified outcomes for the workload. Verification results, explicit user acceptance, rejection, and retries can supply outcome evidence; unverified runs remain useful for latency and cost observations only.

Clients can request a profile through `X-Maestro-Agent-Profile` (or the compatibility `X-Composer-Agent-Profile`) and inspect the versioned resolved profile on the routing decision.

## Outcome-calibrated Oracle experiments

Hosts can enable deterministic session-level control/treatment assignment with `MAESTRO_ORACLE_EXPERIMENT_ID`, `MAESTRO_ORACLE_EXPERIMENT_ALLOCATION`, `MAESTRO_ORACLE_EXPERIMENT_CONTROL_VERSION`, and `MAESTRO_ORACLE_EXPERIMENT_TREATMENT_VERSION`. Configuration must be complete and allocation must be between `0` and `1`.

The assigned arm and policy version appear in routing receipts and bounded telemetry attributes. Assignment is immutable for an experiment/session pair; promotion remains advisory and requires verified outcome gates.

---
name: incident-triage
description: Triage production incidents by building a timeline, scoping blast radius, identifying mitigations, and preserving evidence. Use when the user asks about an outage, regression, alert, Sentry issue, or incident follow-up.
license: Complete terms in LICENSE.txt
compatibility: "Maestro skill packages with bounded GitHub MCP tools and executable toolbox entries."
allowed-tools:
  - read
  - search
  - bash
builtin-tools:
  - read
  - search
mode: incident
isolatedContext: true
metadata:
  version: "0.2.0"
  category: evalops-operations
  artifactSchema: evalops.maestro.skill.incident_triage.v1
---

# Incident Triage

Use this skill to turn scattered incident evidence into a concise operator plan.

## Workflow

1. Check Cerebro memory first (see below): query the world model for what the
   company already knows about the entities in the alert before touching raw
   logs or metrics.
2. Confirm the symptom, start time, affected surface, and urgency.
3. Load learned guidelines (see below) and `reference/triage.md` for the detailed timeline and mitigation checklist.
4. Gather only scoped evidence from granted tools and local files. Keep customer data and internal handles out of the normal answer.
5. Build a timeline with known, inferred, and unknown entries separated.
6. Identify the likely owner, immediate mitigation, verification signal, and follow-up issue.
7. End with current state, blast radius, next action, and what evidence was withheld or unavailable.
8. Record what you learned (see below) so the next incident starts from it.

## Cerebro Memory First

Cerebro is the company world model; treat it as the first responder's memory.
Managed EvalOps launches attach its MCP read tools (`cerebro_*`) when the
session carries the `cerebro:read` scope — see "EvalOps Cerebro MCP" in
`docs/MCP_GUIDE.md`. If no `cerebro_*` tools are available in the run, say so,
fall back to raw evidence, and record the miss under withheld or unavailable
evidence.

When the tools are available, run this sequence before any raw log or metric
query:

1. **Extract entities** from the alert or report: service or workload name,
   image digest, domain, actor. Prefer stable identities (deployment, service,
   repo) over ephemeral pod UIDs, and never put tenant IDs, pod UIDs, or
   secrets into queries.
2. **Find prior coverage**: `cerebro_search` (or `cerebro_gather_facts` to get
   matching Things plus their Facts in one pass) for each entity, with
   `include_map=true` to see linked Things.
3. **Load incident history**: `cerebro_get_thing` on each matched Thing for its
   recent Facts, Events, and Evidence — prior incidents, deploys, and alerts
   involving the same entity.
4. **Check open risk**: `cerebro_attention_lifecycle` for attention items
   already tracking the entity.
5. **Recover the last-known resolution**: `cerebro_action_experiences`
   (optionally filtered by `target_system` / `kind`) for lessons and replay
   hints from terminal remediation actions on similar past incidents.
6. **Scope blast radius**: `cerebro_map_thing` with `depth=2` on the affected
   service for dependency and ownership links.
7. **Resolve conflicts**: when Cerebro's Facts disagree with the live alert,
   `cerebro_debug_beliefs` on the Thing shows why Cerebro believes what it
   does.

If Cerebro has no coverage for the entities — no matching Things, or
`cerebro_source_health` reports the relevant sources stale or missing — state
that plainly and fall back to raw logs and metrics as the primary evidence.
Raw evidence is the fallback, not the starting point.

Concrete query patterns per entity type live in `reference/triage.md`.

## Learned Guidelines

This skill accumulates alert-type knowledge across runs in
`~/.maestro/skills/incident-triage/guidelines.md`.

- **At the start of a run**, read that file if it exists and treat its entries as
  priors to verify — which tools, dashboards, owners, and mitigations a given
  alert type needed last time. Confirm they still hold; do not trust them
  blindly.
- **At the end of a run**, append one concise entry mapping the alert type or
  symptom you handled to the tools/interfaces, likely owner, and mitigation that
  actually worked. Keep entries short and free of secrets and customer data.

Programmatic callers should use ordinary UTF-8 file operations on that stable
path: treat a missing file as an empty guideline set, normalize surrounding
whitespace when loading, and append through an atomic temporary-file rename.
No language-specific Maestro runtime helper is required.

The guidelines file is the local, always-available memory; Cerebro is the
shared one. When the run also carries the `cerebro:assert` scope, durable
learnings (root cause, working mitigation, owner) may additionally be recorded
with `cerebro_assert_fact` using a stable dimension, explicit confidence, and
evidence — never tenant IDs, pod UIDs, or secrets.

## Toolbox

Run `toolbox/incident-timeline` to emit the required incident report skeleton.

## MCP Scope

The bundled GitHub MCP config exposes issue, code, workflow, and file lookups for incident context. Live observability systems must be granted by the runtime separately. Cerebro read tools are attached by managed EvalOps launches with the `cerebro:read` scope rather than by this config; the manifest at `/.well-known/evalops/cerebro-mcp.json` lists the available tools and required headers.

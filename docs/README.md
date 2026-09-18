# Deixic Code Documentation Index

Use this index to jump to the right guide quickly and see how the pieces connect.

## Start Here
- [Quickstart](QUICKSTART.md) — installation, environment prep, builds, and the fast path to running the CLI/TUI.
- [TUI User Guide](../packages/tui-rs/docs/user-guide/README.md) — numbered native TUI guide (getting started through worktrees).
- [Contributor Runbook](CONTRIBUTOR_RUNBOOK.md) — day-one flow for contributors (build, lint, verify).
- [Feature Guide](FEATURES.md) — user-facing capabilities across the TUI and CLI with power-user tips.
- [Dynamic workflows](DYNAMIC_WORKFLOWS.md) — run bounded, resumable workflow graphs from the native CLI.
- [Compatibility and migration](DEIXIC_CODE_MIGRATION.md) — canonical names, retained Maestro coordinates, and publication boundaries.

## Core Reference
- [Tools Reference](TOOLS_REFERENCE.md) — authoritative slash command and flag definitions.
- [Coding acceptance](CODING_ACCEPTANCE.md) — task readiness, independent validation, and evidence required for completion.
- [Safety](SAFETY.md) — approvals, sandboxing, and firewall behavior.
- [Agent Safety Boundary](design/AGENT_SAFETY_BOUNDARY.md) — how MCP workspace trust, guarded files, approvals, audit, and sandbox policy compose.
- [Threat Model](THREAT_MODEL.md) — security architecture, trust boundaries, and attack mitigations.
- [Models](MODELS.md) — provider/model registry sources, overrides, defaults, and OpenAI-compat quirks.
- [Sessions](SESSIONS.md) — session formats, storage locations, and management commands.
- [Prompt Queue](PROMPT_QUEUE.md) — queue lifecycle, prioritization, and diagnostics hooks.
- [MCP Guide](MCP_GUIDE.md) — Model Context Protocol setup and usage.
- [Skill Cookbook](cookbook/skills/README.md) — progressive skill package authoring, linting, bundled MCP, and toolbox examples.
- [Headless Protocol Reference](protocols/headless.md) — versioned JSON-over-stdio contract for Chat, TUIs, and other embedders.
- [Codex Parity Conformance](protocols/codex-parity-conformance.md) — compact anchors for Codex-inspired auth, patching, MCP, queue, and hosted-runtime surfaces.
- [RPC Protocol Conformance](protocols/rpc-protocol-conformance.md) — release-gated JSON-over-stdio request, response, and client-correlation contract.
- [Hosted Runner Contract](protocols/hosted-runner-contract.md) — provider-neutral runtime contract for account-scoped remote Deixic Code sessions (the protocol retains Maestro identifiers).
- [Pending Request Contract](protocols/pending-requests.md) — unified session projection for approvals, user input, MCP elicitations, tool retries, and Platform waits.

## Architecture & Patterns
- [Architecture](ARCHITECTURE.md) — system layout, surfaces, edit patterns.
- [TUI Architecture](TUI_ARCHITECTURE.md) — native `maestro-tui` (`packages/tui-rs`).
- [Native TUI parity](NATIVE_TUI_PARITY.md) — feature checklist after TS TUI removal.
- [Architecture Diagram](ARCHITECTURE_DIAGRAM.md) — high-level system layout.
- [VS Code Architecture](VSCODE_ARCHITECTURE.md) — extension-specific architecture and flows.
- [Patterns](patterns/INDEX.md) — implementation patterns (e.g., event suppression, tool error handling).

## Deployment & Operations
- [Enterprise](ENTERPRISE.md) — deployment, configuration, and hardening guidance for controlled environments.
- [CI Version Pins](CI_VERSION_PINS.md) — where Node, Rust dependency, and action versions are pinned.
- [Changelog](../CHANGELOG.md) — release history and notable changes.

When in doubt, start with the Quickstart, skim the Feature Guide, keep the Tools Reference nearby, and use the Contributor Runbook before opening a PR.

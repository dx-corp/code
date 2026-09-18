# Features

Deixic Code is the customer-facing product. The native runtime retains Maestro
protocol, storage, environment, and artifact identifiers for compatibility.

- Native interactive terminal UI and trailing prompts
- Print, exec, JSON output, output schemas, and NDJSON headless protocol
- OpenAI/Codex, Anthropic, Google, Bedrock, Vertex, OpenRouter, and compatible providers
- Built-in filesystem, shell, search, web, browser, document, and delegation tools
- Permission profiles, sandboxing, approvals, hooks, Lua, and WASM extensions
- Persistent sessions, resume/export/import, usage and cost accounting
- Account-scoped hosted Computer handoffs: freeze a selected remote diff/files/artifacts
  for another same-tenant thread, then list or read the immutable package by digest
- Runtime gateway (`maestro serve`) with SSE/WebSocket chat, automations, A2A, hosted runners, telemetry, and model management
- Scripted scenarios, replay, trajectory evaluation, and protocol conformance
- Slack and GitHub adapters backed by the native runtime gateway

All product execution, runtime-gateway, and adapter behavior runs through Rust.

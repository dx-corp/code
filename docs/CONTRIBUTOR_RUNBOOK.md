# Contributor Runbook

Audience: engineers touching code; use as the day-one checklist.  
Nav: [Docs index](README.md) · [Quickstart](QUICKSTART.md) · [Tools Reference](TOOLS_REFERENCE.md) · [Safety](SAFETY.md)

## 0. Clone and install

- Install stable Rust and Node.js 22 or newer.
- `package.json` has zero runtime dependencies and one locked, test-only development dependency. Run `npm ci` before repository and workflow contract tests; Node otherwise runs the native packaging and repository-check scripts under `scripts/`.
- Export provider keys (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, etc.) or place them in `~/.maestro/keys.json`.

## 1. Build and verify

```bash
npm run check
npm run lint
npm test
npm run build
npm run smoke:release-native-only
```

Product code, protocols, adapters, CLI, TUI, and the runtime gateway are Rust.

## 2. Inner loop

- Interactive TUI: `cargo run --manifest-path packages/maestro-rs/Cargo.toml`
- One-shot CLI: `cargo run --manifest-path packages/maestro-rs/Cargo.toml -- exec "summarize this repository"`
- Web runtime gateway: `cargo run --manifest-path packages/maestro-rs/Cargo.toml -- web --port 3000`
- Focused crate test: `cargo test --manifest-path packages/tui-rs/Cargo.toml <test-name>`

## 3. Safety checks

- Approvals/firewall: see `docs/SAFETY.md`; exposed web auto-approval should be paired with Docker or authentication.
- The Rust-only guards run through `npm run check:rust-only-runtime`.

<!--
  Removed: "Run `scripts/guardian.sh --staged` (or `/guardian` in the TUI) before
  commits." The script was deleted wholesale in the Rust-only runtime migration
  (#3016, merged 2026-07-22) with no Rust replacement and no surviving `/guardian`
  TUI command. Re-add once an owner decides whether guardian's pre-commit checks
  get reimplemented natively or are superseded by `cargo clippy`/`cargo fmt --check`.
-->

## 4. Docs and references

- TUI/CLI UX: `docs/FEATURES.md`
- Tool behavior: `docs/TOOLS_REFERENCE.md`
- Native crates: `packages/maestro-rs`, `packages/tui-rs`, `packages/runtime-gateway-rs`, and `packages/ambient-agent-rs`
- Historical design documents are reference material, not current build instructions.

## 5. Pre-PR checklist

- `npm run check`
- `npm run lint`
- `npm test`
- `npm run build`
- Update canonical docs when flags or behavior change.

## 6. Troubleshooting

- Missing keys: `maestro --diag`
- Approval blocks: check `docs/SAFETY.md`
- Runtime gateway: `curl http://localhost:3000/api/health`
- Sessions: see `docs/SESSIONS.md`

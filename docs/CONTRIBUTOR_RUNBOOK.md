# Contributor runbook

Run commands from `products/maestro` in Mono, or the root of the standalone
Code checkout. Start with [Contributing](../CONTRIBUTING.md) for package
selection and the [documentation index](README.md) for runtime guides.

## Setup

Use Mono's [pinned toolchain](../../../mise.toml). Node runs packaging and
repository checks; Rust builds the CLI, TUI, and runtime gateway. Install
locked script dependencies with `npm ci`.

Mono agents run Rust builds and tests on the
[approved build host](../../../docs/development.md#rust-builds-for-agents),
including npm scripts that invoke Cargo.

```sh
npm ci
npm run build
./bin/deixic-code --version
./bin/deixic-code setup
```

`setup` reports authentication/configuration state and the next setup step.
Use [Models](MODELS.md) for supported inference routes and configuration.
Repository checks do not require a provider key; a model-backed coding turn
requires an authorized inference route.

## Development

```sh
cargo run --locked -p maestro -- --help
cargo run --locked -p maestro -- exec "summarize this repository"
cargo run --locked -p maestro -- serve --port 3000
```

The gateway command remains running. From another terminal,
`curl http://127.0.0.1:3000/healthz` checks process health. Read
[Safety](SAFETY.md) before exposing a gateway or changing approvals.

For UI work, use `./dev ui list` and `./dev ui inspect <story>` to find the
owning adapter and its verification commands. See the
[UI contributor workflow](ui-contributor-workflow.md).

## Verify the changed package

```sh
npm run check:rust-only-runtime
cargo fmt --all --check
cargo clippy --locked -p maestro-tui --all-targets -- -D warnings
cargo test --locked -p maestro-tui <test-name>
```

Replace the package and test filter with the affected scope. Select relevant
consumers when shared contracts change. `npm run check`, `npm run lint`, and
`npm test` include workspace-wide Cargo commands; use the Mono build-host
instructions for agent verification. Release checks and packed-install smoke
requirements are described in [Contributing](../CONTRIBUTING.md#verification).

Record the tested commit, commands, results, and omitted checks in the pull
request. Update current documentation when flags or behavior change.
Historical designs do not establish current runtime behavior.

## References

- [Features](FEATURES.md): CLI and TUI behavior.
- [Tools reference](TOOLS_REFERENCE.md): commands and flags.
- [Sessions](SESSIONS.md): persistence and continuation.
- [Architecture](ARCHITECTURE.md): native crate boundaries.

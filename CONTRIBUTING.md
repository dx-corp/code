# Contributing to Deixic Code

Deixic Code's CLI, TUI, and runtime share the Rust workspace in this directory.
The npm package distributes native binaries. The [desktop client](desktop/README.md)
has its own JavaScript dependencies.

Run commands here from `products/maestro` in Mono or from the root of the
standalone Code checkout. Mono agents use the
[approved Rust build host](../../docs/development.md#rust-builds-for-agents)
for Cargo and npm scripts that invoke it.

## Setup

Use the Mono [pinned toolchain](../../mise.toml) when working in this repository.
The Rust toolchain needs `rustfmt` and `clippy`. Install the locked script
dependencies and build the native package:

```bash
npm ci
npm run build
./bin/maestro --version
```

`package.json` has zero runtime dependencies and one locked, test-only development dependency. Run `npm ci` before repository and workflow contract tests; Node otherwise runs the `scripts/*` packaging and repository-check helpers.

## Development

```bash
cargo run -p maestro -- --help
cargo run -p maestro -- exec "summarize this repository"
cargo run -p maestro -- serve --port 3000
```

Use `cargo test -p <package> [test-name]` for focused work. Workspace package names include `maestro`, `maestro-tui`, `maestro-runtime`, `maestro-runtime-contracts`, `maestro-runtime-gateway`, and `ambient-agent`.

### Native UI development

Use the repository-local UI front door instead of remembering Cargo examples
or fixture paths:

```bash
./dev ui                         # live full-flow workbench
./dev ui list                    # registered stories and cases
./dev ui inspect menu-ready      # owner, source, states, and exact commands
./dev ui check menu-ready        # focused render and behavior verification
./dev ui new workspace-picker --adapter shared-menu --check
./dev ui doctor
```

`./dev ui review --story ID` keeps the familiar AI composer and onboarding
flows in the production Rust renderer while narrowing rebuilds to the selected
story. Exported menu and theme receipts replay with `./dev ui replay FILE`.
Run `./dev ui migrate FILE --check` on checked-in receipts. See the
[UI contributor workflow](docs/ui-contributor-workflow.md) for adapters,
compatibility, and pull request evidence.

## Verification

Select the changed crate and relevant consumers for local verification:

```bash
npm run check:rust-only-runtime
cargo fmt --all --check
cargo clippy --locked -p maestro-runtime --all-targets -- -D warnings
cargo test --locked -p maestro-runtime
```

Replace `maestro-runtime` with the affected package. `npm run check`, `npm test`,
and `npm run lint` include workspace-wide Cargo commands; they are not focused
agent checks. [Mono development](../../docs/development.md#rust-builds-for-agents)
owns build capacity and package selection. `npm run release:check` builds a
release binary and runs native release smoke checks.

Release changes must also pass the packed npm install smoke. The installed CLI must resolve the packaged native binary and run without a JavaScript runtime in its child `PATH`.

## Dependencies and versions

The root `Cargo.toml` and `Cargo.lock` own Rust dependency resolution. Put broadly shared dependencies in `[workspace.dependencies]`. `package.json` and `package-lock.json` describe only the npm distribution and repository scripts.

Use `npm run version:patch`, `npm run version:minor`, or `npm run version:major` for version updates, then verify metadata with `npm run metadata:check`.

# Contributing to Deixic Code

Deixic Code is developed as one Rust workspace with a thin npm distribution wrapper.

## Setup

Install current Node and stable Rust with `rustfmt` and `clippy`, then run:

```bash
npm run build
./bin/maestro --version
```

`package.json` has zero runtime dependencies and one locked, test-only development dependency. Run `npm ci` before repository and workflow contract tests; Node otherwise runs the `scripts/*` packaging and repository-check helpers.

## Development

```bash
cargo run -p maestro -- --help
cargo run -p maestro -- exec "summarize this repository"
cargo run -p maestro -- web --port 3000
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

Before opening a pull request, run the checks relevant to the changed surface. For workspace or release changes, run the full set:

```bash
npm run check
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
npm run release:check
```

Release changes must also pass the packed npm install smoke. The installed CLI must resolve the packaged native binary and run without a JavaScript runtime in its child `PATH`.

## Dependencies and versions

The root `Cargo.toml` and `Cargo.lock` own Rust dependency resolution. Put broadly shared dependencies in `[workspace.dependencies]`. `package.json` and `package-lock.json` describe only the npm distribution and repository scripts.

Use `npm run version:patch`, `npm run version:minor`, or `npm run version:major` for version updates, then verify metadata with `npm run metadata:check`.

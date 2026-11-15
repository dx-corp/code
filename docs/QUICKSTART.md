# Quickstart

The CLI and agent runtime are Rust. From Mono, start in `products/maestro`;
use the repository
[pinned toolchain](../../../mise.toml). Agents run Rust builds on the
[approved build host](../../../docs/development.md#rust-builds-for-agents),
including the Cargo command inside `npm run build`. The
[desktop client](../desktop/README.md) has separate setup instructions.

```sh
npm ci
npm run build
./bin/deixic-code --version
./bin/deixic-code setup
./bin/deixic-code
./bin/deixic-code serve --port 3000
```

Run `./bin/deixic-code --build-info` to print the compiled version, Git commit,
repository state (`clean`, `dirty`, or `unknown`), target triple (including architecture),
and Cargo build profile. Repository state includes staged and not staged changes to
tracked files across the repository at build time; untracked files are excluded.
Builds from source archives or without Git report `unknown` for the commit and
repository state.
`--version` retains its single-line output.

For CLI changes, run focused checks on the build host. Select other changed
crates and their consumers as needed; see [Contributing](../CONTRIBUTING.md).

```sh
npm run check:rust-only-runtime
cargo fmt --all --check
cargo clippy --locked -p maestro --all-targets -- -D warnings
cargo test --locked -p maestro
```

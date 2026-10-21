# Quickstart

Install Rust and Node.js for repository development. All product behavior is built from Rust.

```sh
npm install
npm run build
./bin/deixic-code --version
./bin/deixic-code setup
./bin/deixic-code
./bin/deixic-code serve --port 3000
```

Run `./bin/deixic-code --build-info` to print the compiled version, Git commit,
repository state (`clean`, `dirty`, or `unknown`), target triple (including architecture),
and Cargo build profile. Repository state includes staged and unstaged changes to
tracked files across the repository at build time; untracked files are excluded.
Builds from source archives or without Git report `unknown` for the commit and
repository state.
`--version` retains its single-line output.

Run checks with:

```sh
npm run check:rust-only-runtime
npm run lint
npm test
```

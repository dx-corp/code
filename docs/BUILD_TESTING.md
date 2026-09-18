# Building and testing

Maestro builds from the root Rust workspace. All crates share one lockfile and one target directory.

## Fast local loop

```bash
cargo check --workspace --all-targets --locked
cargo test -p maestro-tui test_name
cargo fmt --all --check
```

## Full workspace verification

```bash
npm run check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
npm run build
npm run smoke:release-native-only
```

`npm run build` compiles the optimized `maestro` binary and materializes the native npm package under `vendor/maestro/<platform>-<arch>`.

## Packed-package verification

After `npm run build`:

```bash
tarball=$(npm pack --silent)
node scripts/smoke-packed-cli.js "$tarball"
rm "$tarball"
```

This validates package metadata, installs the tarball in a clean directory, audits it, and runs the installed launcher through npm's `.bin` symlink. `npm run smoke:release-native-only` separately proves the packaged binary runs with Node, npm, npx, and Bun absent from the child `PATH`.

## Release binaries

`node scripts/build-release-binary.mjs --platform <platform>` builds the canonical binary from the root workspace. Supported package platforms are `linux-x64`, `linux-arm64`, `darwin-x64`, and `darwin-arm64`.

## Profiling

Use the `magic-trace` Cargo profile when symbols and frame pointers are needed. See `docs/perf/MAGIC_TRACE.md`.

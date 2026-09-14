#!/usr/bin/env bash
set -euo pipefail

# Advisory line coverage of workspace *libraries*.
#
# #306 spent 76m in `cargo llvm-cov --workspace` (cargo test) and timed out.
# That command compiles every integration-test binary with instrumentation.
# maestro-tui alone has nine `tests/*.rs` binaries plus three bins. Required
# rust-tests already runs those uninstrumented in ~6m. Coverage does not need
# to compile them again.
#
# Keep this job on an isolated target dir. Sharing CARGO_TARGET_DIR with
# lint/test mixes instrumented and plain incremental artifacts and forces a
# full rebuild.
repo_root="$(pwd)"
cache_root="${MAESTRO_CI_CACHE_ROOT:-${RUNNER_TEMP:-${repo_root}/.cache}/maestro-ci}"
tool_root="${cache_root}/cargo-tools"
mkdir -p "$tool_root/bin"
export PATH="$tool_root/bin:$PATH"
export CARGO_TARGET_DIR="${cache_root}/cargo-target-cov"

install_github_crate_bin() {
  local name="$1"
  local url="$2"
  local sha="$3"
  local member="$4"
  local archive dir
  archive="$(mktemp)"
  dir="$(mktemp -d)"
  timeout --signal=TERM --kill-after=10s 2m curl --fail --location --silent --show-error \
    "$url" --output "$archive"
  printf '%s  %s\n' "$sha" "$archive" | sha256sum --check --status
  tar -C "$dir" -xzf "$archive"
  cp "$dir/$member" "$tool_root/bin/$name"
  rm -f "$archive"
  rm -rf "$dir"
}

if ! command -v cargo-llvm-cov >/dev/null 2>&1 || [[ "$(cargo llvm-cov --version)" != "cargo-llvm-cov 0.9.0" ]]; then
  install_github_crate_bin cargo-llvm-cov \
    "https://github.com/taiki-e/cargo-llvm-cov/releases/download/v0.9.0/cargo-llvm-cov-x86_64-unknown-linux-gnu.tar.gz" \
    b068f7c98841aacb9c4f382b4a0c184ae82f49b56a32d442b429b2961c73be15 \
    cargo-llvm-cov
fi

nextest_version="0.9.143"
if ! command -v cargo-nextest >/dev/null 2>&1 || [[ "$(cargo nextest --version)" != *"$nextest_version"* ]]; then
  install_github_crate_bin cargo-nextest \
    "https://github.com/nextest-rs/nextest/releases/download/cargo-nextest-${nextest_version}/cargo-nextest-${nextest_version}-x86_64-unknown-linux-gnu.tar.gz" \
    66786b9abe23920d022a182d1416b1bbc8130dd4872a9553d76985a1708dcd1e \
    cargo-nextest
fi

# --lib: one instrumented test harness per crate. No tests/*.rs, no bins,
# no doctests. nextest for the same parallelism as rust-tests.
# cargo-llvm-cov 0.9.0 rejects combining a deferred report with --no-clean.
# Keep --no-clean so the isolated target dir stays incremental; generate the
# lcov/summary reports in the commands below.
timeout --signal=TERM --kill-after=30s 25m cargo llvm-cov nextest \
  --workspace \
  --lib \
  --locked \
  --no-clean \
  --ignore-run-fail \
  -- \
  --profile ci \
  --no-fail-fast

cargo llvm-cov report --summary-only
mkdir -p coverage-report
cargo llvm-cov report --lcov --output-path coverage-report/lcov.info

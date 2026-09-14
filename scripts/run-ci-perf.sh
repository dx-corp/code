#!/usr/bin/env bash
set -euo pipefail

os="$(uname -s)"
arch="$(uname -m)"
[[ "$os" == Linux ]] && os=linux
[[ "$os" == Darwin ]] && os=macos
[[ "$arch" == arm64 ]] && arch=aarch64
baseline="packages/tui-rs/benches/baselines/${os}-${arch}.json"
if [[ ! -f "$baseline" ]]; then
  echo "no performance baseline for ${os}-${arch}; skipping advisory comparison"
  exit 0
fi
timeout --signal=TERM --kill-after=30s 50m cargo run -p maestro-tui --release --locked --features test-support --bin maestro-perf-bench -- --baseline "$baseline"

#!/usr/bin/env bash
# Native Linux validation for this repository.
# The platform-ci image does not ship ripgrep; find/search tests shell out to
# `rg` with no grep fallback. Install a pinned binary into the job cache when
# it is missing, then run the same check Make uses.
set -euo pipefail

mode="${1:-all}"
if [[ $# -gt 1 || "${mode}" != "all" && "${mode}" != "--contracts" ]]; then
  echo "usage: $0 [--contracts]" >&2
  exit 2
fi

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)"
maestro_root="$(cd "${script_dir}/.." && pwd -P)"
cache_root="${MAESTRO_CI_CACHE_ROOT:-${RUNNER_TEMP:-${maestro_root}/.cache}/maestro-ci}"
tool_root="${cache_root}/ci-tools"
mkdir -p "${tool_root}/bin"
export PATH="${tool_root}/bin:${PATH}"

if ! command -v rg >/dev/null 2>&1; then
  rg_version="14.1.1"
  case "$(uname -s)-$(uname -m)" in
    Linux-x86_64)
      rg_archive="ripgrep-${rg_version}-x86_64-unknown-linux-musl.tar.gz"
      rg_sha256="4cf9f2741e6c465ffdb7c26f38056a59e2a2544b51f7cc128ef28337eeae4d8e"
      ;;
    Linux-aarch64 | Linux-arm64)
      rg_archive="ripgrep-${rg_version}-aarch64-unknown-linux-gnu.tar.gz"
      rg_sha256="c827481c4ff4ea10c9dc7a4022c8de5db34a5737cb74484d62eb94a95841ab2f"
      ;;
    *)
      echo "ci-linux-check: unsupported platform for ripgrep install: $(uname -s)-$(uname -m)" >&2
      exit 1
      ;;
  esac
  archive="$(mktemp)"
  unpack="$(mktemp -d)"
  trap 'rm -f "${archive}"; rm -rf "${unpack}"' EXIT
  curl --fail --location --silent --show-error --max-time 120 --retry 2 \
    "https://github.com/BurntSushi/ripgrep/releases/download/${rg_version}/${rg_archive}" \
    --output "${archive}"
  if command -v sha256sum >/dev/null 2>&1; then
    printf '%s  %s\n' "${rg_sha256}" "${archive}" | sha256sum --check --status
  else
    printf '%s  %s\n' "${rg_sha256}" "${archive}" | shasum -a 256 --check --status
  fi
  tar -xzf "${archive}" -C "${unpack}"
  rg_bin="$(find "${unpack}" -type f -name rg -print -quit)"
  if [[ -z "${rg_bin}" ]]; then
    echo "ci-linux-check: ripgrep archive did not contain an rg binary" >&2
    exit 1
  fi
  cp "${rg_bin}" "${tool_root}/bin/rg"
  chmod +x "${tool_root}/bin/rg"
  rm -f "${archive}"
  rm -rf "${unpack}"
  trap - EXIT
fi

cd "${maestro_root}"
# Keep the public `npm run check` contract intact: it intentionally includes a
# workspace-wide `cargo check` for local/package consumers. In this imported
# component lane, `make check` below immediately runs workspace Clippy and the
# full locked test suite, which compile the same workspace targets again. Run
# the non-Rust contract checks here and avoid paying for that duplicate Cargo
# check before the existing lint/test gate.
npm run check:workspace-contract
npm run check:protocol-manifest
npm run check:runtime-passport
npm run check:rust-only-runtime
npm run check:helm-probes
npm run check:hook-dispatch
npm run check:session-transfer
npm run check:hosted-orb-delegation
npm run check:macos-signature
npm run check:release-channels
python3 scripts/check-ui-consistency.py
python3 scripts/test-check-ui-consistency.py
python3 -m unittest discover -s scripts -p test_boost_outcomes.py
if [[ "${mode}" == "--contracts" ]]; then
  exit 0
fi
make check

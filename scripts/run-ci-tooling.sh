#!/usr/bin/env bash
set -euo pipefail

cache_root="${MAESTRO_CI_CACHE_ROOT:-${RUNNER_TEMP:-$(pwd)/.cache}/maestro-ci}"
tool_root="${cache_root}/ci-tools"
mkdir -p "$tool_root/bin"
export PATH="$tool_root/bin:$PATH"

if ! actionlint --version >/dev/null 2>&1; then
  case "$(uname -s)-$(uname -m)" in
    Linux-x86_64) actionlint_platform="linux_amd64"; actionlint_sha256="233b280d05e100837f4af1433c7b40a5dcb306e3aa68fb4f17f8a7f45a7df7b4" ;;
    Linux-aarch64 | Linux-arm64) actionlint_platform="linux_arm64"; actionlint_sha256="6b82a3b8c808bf1bcd39a95aced22fc1a026eef08ede410f81e274af8deadbbc" ;;
    Darwin-x86_64) actionlint_platform="darwin_amd64"; actionlint_sha256="f89a910e90e536f60df7c504160247db01dd67cab6f08c064c1c397b76c91a79" ;;
    Darwin-arm64 | Darwin-aarch64) actionlint_platform="darwin_arm64"; actionlint_sha256="855e49e823fc68c6371fd6967e359cde11912d8d44fed343283c8e6e943bd789" ;;
    *) echo "unsupported actionlint platform: $(uname -s)-$(uname -m)" >&2; exit 1 ;;
  esac
  actionlint_archive="$(mktemp)"
  actionlint_unpack="$(mktemp -d)"
  curl --fail --location --silent --show-error --max-time 120 --retry 2 \
    "https://github.com/rhysd/actionlint/releases/download/v1.7.9/actionlint_1.7.9_${actionlint_platform}.tar.gz" \
    --output "$actionlint_archive"
  if command -v sha256sum >/dev/null 2>&1; then
    printf '%s  %s\n' "$actionlint_sha256" "$actionlint_archive" | sha256sum --check --status
  else
    printf '%s  %s\n' "$actionlint_sha256" "$actionlint_archive" | shasum -a 256 --check --status
  fi
  tar -xzf "$actionlint_archive" -C "$actionlint_unpack" actionlint
  cp "$actionlint_unpack/actionlint" "$tool_root/bin/actionlint"
  rm -f "$actionlint_archive"
  rm -rf "$actionlint_unpack"
fi

if ! shellcheck --version >/dev/null 2>&1; then
  case "$(uname -s)-$(uname -m)" in
    Linux-x86_64) platform="linux.x86_64" ;;
    Linux-aarch64 | Linux-arm64) platform="linux.aarch64" ;;
    Darwin-x86_64) platform="darwin.x86_64" ;;
    Darwin-arm64 | Darwin-aarch64) platform="darwin.aarch64" ;;
    *) echo "unsupported ShellCheck platform: $(uname -s)-$(uname -m)" >&2; exit 1 ;;
  esac
  archive="$(mktemp)"
  unpack="$(mktemp -d)"
  trap 'rm -f "$archive"; rm -rf "$unpack"' EXIT
  curl --fail --location --silent --show-error --max-time 120 --retry 2 \
    "https://github.com/koalaman/shellcheck/releases/download/v0.11.0/shellcheck-v0.11.0.${platform}.tar.xz" \
    --output "$archive"
  tar -xJf "$archive" -C "$unpack" --strip-components=1
  cp "$unpack/shellcheck" "$tool_root/bin/shellcheck"
fi

if ! zizmor --version >/dev/null 2>&1; then
  case "$(uname -s)-$(uname -m)" in
    Linux-x86_64) target="x86_64-unknown-linux-gnu" ;;
    Linux-aarch64 | Linux-arm64) target="aarch64-unknown-linux-gnu" ;;
    Darwin-x86_64) target="x86_64-apple-darwin" ;;
    Darwin-arm64 | Darwin-aarch64) target="aarch64-apple-darwin" ;;
    *) echo "unsupported zizmor platform: $(uname -s)-$(uname -m)" >&2; exit 1 ;;
  esac
  curl --fail --location --silent --show-error --max-time 120 --retry 2 \
    "https://github.com/zizmorcore/zizmor/releases/download/v1.28.0/zizmor-${target}.tar.gz" \
    | tar -xz -C "$tool_root/bin"
fi

actionlint -pyflakes=
zizmor --offline --min-severity=high --format=plain .github/workflows/ .github/actions/

shell_files="$(mktemp)"
trap 'rm -f "$shell_files"' EXIT
while IFS= read -r file; do
  [[ -f "$file" ]] || continue
  [[ "$file" == packages/jetbrains-plugin/gradlew ]] && continue
  if [[ "$file" == *.sh ]] || head -1 "$file" 2>/dev/null | grep -Eq '^#!.*sh'; then
    printf '%s\n' "$file" >> "$shell_files"
  fi
done < <(git ls-files)
[[ -s "$shell_files" ]] || { echo "shell file discovery returned no files" >&2; exit 1; }
while IFS= read -r file; do
  shellcheck --external-sources --source-path=SCRIPTDIR "$file"
done < "$shell_files"

timeout --signal=TERM --kill-after=10s 5m npm ci --ignore-scripts
node scripts/check-workflow-footguns.mjs
node --test scripts/check-ci-concurrency.test.mjs
if [[ -f .github/workflows/check-release-workflow-contract.test.mjs ]]; then
  node --test .github/workflows/check-release-workflow-contract.test.mjs
  node .github/workflows/check-release-workflow-contract.mjs
fi
for test_file in \
  scripts/check-required-status-checks.test.mjs \
  scripts/check-integration-required-gate.test.mjs \
  scripts/check-review-thread-guard-workflow.test.mjs \
  scripts/check-sync-public-release-mirror-workflow.test.mjs \
  scripts/tag-release-contract.test.mjs; do
  # The public projection intentionally restores public-owned workflows and
  # excludes the internal mirror workflow. Keep the shared tooling lane
  # green there while still running this contract in the internal repository.
  if [[ "$test_file" == "scripts/check-sync-public-release-mirror-workflow.test.mjs" ]] &&
    [[ ! -f .github/workflows/sync-public-release-mirror.yml ]]; then
    continue
  fi
  if [[ -f "$test_file" ]]; then
    node --test "$test_file"
  fi
done

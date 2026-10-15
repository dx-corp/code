#!/usr/bin/env bash
# Point aws-lc-sys at a prebuilt AWS-LC when the host has one for the workspace lockfile version.
#
#   scripts/ci/aws-lc-system-env.sh          # in CI: appends the env to $GITHUB_ENV
#   eval "$(scripts/ci/aws-lc-system-env.sh --print)"   # in a shell
#
# The prefix comes from scripts/dev/build-awslc-prefix.sh (baked into the runner image or host). With no
# matching prefix this does nothing and the crate compiles its bundled copy, so it cannot break a build.
set -euo pipefail
repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd -P)"
root="${AWS_LC_PREFIX_ROOT:-/opt}"
[[ "$(uname -sm)" == "Linux x86_64" ]] || exit 0
lockfile="$repo/rust/Cargo.lock"
[[ -f "$lockfile" ]] || lockfile="$repo/Cargo.lock"
versions="$(awk '/^name = "aws-lc-sys"$/ { getline; gsub(/[^0-9.]/, "", $3); print $3 }' "$lockfile")"
[[ "$(printf '%s\n' "$versions" | grep -c .)" == 1 ]] || exit 0
prefix="$root/aws-lc-sys-$versions"
[[ -f "$prefix/lib/libcrypto.a" && -f "$prefix/share/rust/aws_lc_bindings.rs" ]] || { echo "no prebuilt AWS-LC at $prefix; aws-lc-sys will compile its bundled copy" >&2; exit 0; }
vars=(
  "AWS_LC_SYS_SYSTEM_DIR=$prefix"
  "AWS_LC_SYS_USE_SYSTEM=1"
  "AWS_LC_SYS_STATIC=1"
  "AWS_LC_SYS_SYSTEM_BINDINGS=$prefix/share/rust/aws_lc_bindings.rs"
)
if [[ "${1:-}" == "--print" ]]; then
  printf 'export %s\n' "${vars[@]}"
else
  printf '%s\n' "${vars[@]}" >> "${GITHUB_ENV:?GITHUB_ENV is required}"
  echo "linking prebuilt AWS-LC from $prefix"
fi

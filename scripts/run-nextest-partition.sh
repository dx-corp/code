#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 1 || ! "$1" =~ ^[1-9][0-9]*/[1-9][0-9]*$ ]]; then
  echo "usage: $0 <partition/total>" >&2
  exit 2
fi

partition="$1"
partition_index="${partition%/*}"
partition_total="${partition#*/}"
if (( partition_index > partition_total )); then
  echo "partition index must not exceed total: ${partition}" >&2
  exit 2
fi

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)"
maestro_root="$(cd "${script_dir}/.." && pwd -P)"
cd "${maestro_root}"

trusted_runner_workspace_root="$(pwd -P)"
export MAESTRO_TRUSTED_RUNNER_WORKSPACE_ROOTS="${trusted_runner_workspace_root}"
maestro_test_home="$(mktemp -d)"
trap 'rm -rf -- "${maestro_test_home}"' EXIT
export MAESTRO_HOME="${maestro_test_home}"
export MAESTRO_SUBAGENTS_DIR="${maestro_test_home}/subagents"

cargo nextest run \
  --profile ci \
  --workspace \
  --locked \
  --no-fail-fast \
  --partition "hash:${partition}"

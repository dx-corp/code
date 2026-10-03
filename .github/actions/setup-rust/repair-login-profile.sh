#!/usr/bin/env bash
set -euo pipefail

# Refresh only Rust setup's generated, quoted env sources for this toolchain
# and the two former Maestro repository names. Keep every other profile line.
cache_root="$1"
toolchain="$2"
cargo_home="$3"
active_env="$cargo_home/env"
shift 3

for profile_file in "$@"; do
  [[ -e "$profile_file" ]] || continue
  if [[ -L "$profile_file" || ! -f "$profile_file" ]]; then
    echo '::error::Rust profile repair requires a regular, non-symlink profile.' >&2
    exit 1
  fi
  scratch="$(mktemp "${profile_file}.maestro-rust.XXXXXX")"
  trap 'rm -f "$scratch"' EXIT
  cp -p "$profile_file" "$scratch"
  : > "$scratch"
  while IFS= read -r line || [[ -n "$line" ]]; do
    for namespace in evalops-maestro dx-corp-maestro; do
      old_env="$cache_root/$namespace/$toolchain/home/cargo/env"
      # The dot/source command must be the whole line's command prefix. Do
      # not rewrite comments, arbitrary strings, other repos, or toolchains.
      case "$line" in
        ". \"$old_env\""*|"source \"$old_env\""*)
          line="${line/\"$old_env\"/\"$active_env\"}"
          ;;
      esac
    done
    printf '%s\n' "$line" >> "$scratch"
  done < "$profile_file"
  if ! cmp -s "$profile_file" "$scratch"; then
    mv "$scratch" "$profile_file"
  else
    rm -f "$scratch"
  fi
  trap - EXIT
done

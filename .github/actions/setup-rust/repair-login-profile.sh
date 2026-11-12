#!/usr/bin/env bash
set -euo pipefail

# Retire generated Rust env sources from persistent profiles. Tool homes belong
# to a job; GITHUB_ENV/GITHUB_PATH select the active home without a login hook.
cache_root="$1"
toolchain="$2"
cargo_home="$3"
active_env="$cargo_home/env"
namespace_root="${cargo_home%/"$toolchain"/home/cargo}"
if [[ "$namespace_root" == "$cargo_home" ]]; then
  echo '::error::Rust profile repair requires the selected toolchain home.' >&2
  exit 1
fi
active_namespace="${namespace_root##*/}"
# Match a whole generated command, never a string, expansion or compound command.
source_line='^[[:space:]]*(\.|source)[[:space:]]+"([^"$`\\]+)"[[:space:]]*(#.*)?$'
shift 3

for profile_file in "$@"; do
  [[ -e "$profile_file" || -L "$profile_file" ]] || continue
  if [[ -L "$profile_file" || ! -f "$profile_file" ]]; then
    echo '::error::Rust profile repair requires a regular, non-symlink profile.' >&2
    exit 1
  fi
  scratch="$(mktemp "${profile_file}.maestro-rust.XXXXXX")"
  trap 'rm -f "$scratch"' EXIT
  cp -p "$profile_file" "$scratch"
  : > "$scratch"
  while IFS= read -r line || [[ -n "$line" ]]; do
    if [[ "$line" =~ $source_line ]]; then
      env_path="${BASH_REMATCH[2]}"
      comment="${BASH_REMATCH[3]:-}"
      owned=false
      for namespace in evalops-maestro dx-corp-maestro "$active_namespace"; do
        case "$env_path" in
          "$cache_root/$namespace/$toolchain/home/cargo/env"|/*/maestro-rust/"$namespace/$toolchain/home/cargo/env"|"$active_env")
            owned=true
            ;;
        esac
      done
      if [[ "$owned" == true ]]; then
        [[ -z "$comment" ]] || printf '%s\n' "$comment" >> "$scratch"
        continue
      fi
    fi
    printf '%s\n' "$line" >> "$scratch"
  done < "$profile_file"
  if ! cmp -s "$profile_file" "$scratch"; then
    mv "$scratch" "$profile_file"
  else
    rm -f "$scratch"
  fi
  trap - EXIT
done

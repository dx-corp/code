#!/usr/bin/env bash
# Install a native local build without replacing its macOS Keychain identity.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PREFIX="${MAESTRO_INSTALL_PREFIX:-$HOME/.local/bin}"
PROFILE="${MAESTRO_CARGO_PROFILE:-release}"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-${XDG_CACHE_HOME:-$HOME/.cache}/platform-target}"

fail() { printf 'error: %s\n' "$*" >&2; exit 1; }
command -v node >/dev/null || fail "Node is required for build tooling (not the installed CLI)."
command -v cargo >/dev/null || fail "Cargo is required to build from source."
mono_root="$ROOT/../.."
if [[ -f "$mono_root/scripts/dev/local_build_capacity.py" ]]; then
  make -C "$mono_root" local-build-capacity-check
else
  # The public product checkout does not contain Mono's capacity checker.
  node - "$CARGO_TARGET_DIR" <<'NODE'
const fs = require('node:fs');
const path = require('node:path');
const { execFileSync } = require('node:child_process');
const target = path.resolve(process.argv[2]);
let ancestor = target;
while (!fs.existsSync(ancestor)) ancestor = path.dirname(ancestor);
const disk = fs.statfsSync(ancestor);
if (disk.bavail * disk.bsize < 32 * 1024 ** 3) throw new Error('Source build requires 32 GiB free disk space');
if (fs.existsSync(target)) {
  const kib = Number(execFileSync('du', ['-sk', target], { encoding: 'utf8' }).split(/\s+/)[0]);
  if (!Number.isFinite(kib) || kib > 120 * 1024 ** 2) throw new Error('Shared Cargo target exceeds 120 GiB or could not be measured');
}
NODE
fi
macos=0
if [[ "$(uname -s)" == Darwin ]]; then
  macos=1
  jobs="${CARGO_BUILD_JOBS:-4}"
  [[ "$jobs" =~ ^[1-9][0-9]*$ ]] || fail "CARGO_BUILD_JOBS must be a positive integer."
  if (( jobs > 4 )); then jobs=4; fi
  export CARGO_BUILD_JOBS="$jobs"
  authority="${MAESTRO_RELEASE_DEVELOPER_ID_AUTHORITY:-}"
  identities="$(security find-identity -v -p codesigning)"
  if [[ -z "$authority" ]]; then
    # Prefer the installed identity so adding a second certificate does not
    # silently move existing credentials to a different signing team.
    if [[ -f "$PREFIX/maestro" ]]; then
      authority="$(codesign -dv --verbose=4 "$PREFIX/maestro" 2>&1 | sed -n 's/^Authority=\(Developer ID Application: .*\)$/\1/p' || true)"
    fi
    if [[ -z "$authority" ]]; then
      authority="$(printf '%s\n' "$identities" | sed -n 's/.*"\(Developer ID Application: .*\)".*/\1/p' | sort -u)"
    fi
  fi
  [[ -n "$authority" && "$authority" != *$'\n'* && "$authority" == 'Developer ID Application: '* ]] ||
    fail "Set MAESTRO_RELEASE_DEVELOPER_ID_AUTHORITY to one Developer ID Application identity. Use the signed release installer if no certificate is available."
  printf '%s\n' "$identities" | grep -Fq "\"$authority\"" ||
    fail "Configured Developer ID identity is unavailable; keeping the installed binary."
fi

mkdir -p "$PREFIX"
lock="$PREFIX/.maestro-install.lock"
mkdir "$lock" 2>/dev/null || fail "Another install holds $lock; remove it only after confirming that installer has stopped."
store="$PREFIX/.maestro-local"
mkdir -p "$store"
stage="$(mktemp -d "$store/build.XXXXXX")"
cleanup() {
  # A signal after activation must not remove the now-live build.
  if [[ -n "$stage" && "$(readlink "$store/current" || true)" != "${stage##*/}" ]]; then
    rm -rf "$stage"
  fi
  rm -rf "$lock"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
printf 'Building Deixic Code (%s) from %s\n' "$PROFILE" "$ROOT"
(
  cd "$ROOT"
  if [[ "$macos" == 1 ]]; then
    nice -n 15 cargo build --locked -p maestro --profile "$PROFILE" --message-format=json-render-diagnostics
  else
    cargo build --locked -p maestro --profile "$PROFILE" --message-format=json-render-diagnostics
  fi
) > "$stage/build.jsonl"

# Cargo reports the actual executable for custom targets, shared target
# directories, and the dev profile (whose output directory is named debug).
BIN="$(node - "$stage/build.jsonl" <<'NODE'
const fs = require('node:fs');
const readline = require('node:readline');
(async () => {
  let binary;
  const lines = readline.createInterface({ input: fs.createReadStream(process.argv[2]), crlfDelay: Infinity });
  for await (const line of lines) {
    if (!line) continue;
    const item = JSON.parse(line);
    if (item.reason === 'compiler-artifact' && item.target?.name === 'maestro' &&
        item.target.kind.includes('bin') && item.executable) binary = item.executable;
  }
  if (!binary) throw new Error('Cargo did not report a maestro executable');
  process.stdout.write(binary);
})().catch(error => { console.error(error.message); process.exitCode = 1; });
NODE
)"
[[ -x "$BIN" ]] || fail "Cargo's reported executable is missing: $BIN"
rm "$stage/build.jsonl"
install -m 755 "$BIN" "$stage/maestro"
if [[ "$macos" == 1 ]]; then
  # Keep the release identifier: changing it invalidates existing Keychain ACLs.
  codesign --force --identifier maestro --options runtime --timestamp --sign "$authority" "$stage/maestro"
  codesign --verify --strict --verbose=2 "$stage/maestro"
  details="$(codesign -dv --verbose=4 "$stage/maestro" 2>&1)"
  printf '%s\n' "$details" | grep -Fxq 'Identifier=maestro' || fail "Signing identifier changed"
  printf '%s\n' "$details" | grep -Fxq "Authority=$authority" || fail "Signing authority changed"
  team="$(printf '%s\n' "$details" | sed -n 's/^TeamIdentifier=//p')"
  [[ "$team" =~ ^[A-Z0-9]{10}$ ]] || fail "Missing signing team"
  [[ -z "${MAESTRO_RELEASE_DEVELOPER_ID_TEAM_IDENTIFIER:-}" || "$team" == "$MAESTRO_RELEASE_DEVELOPER_ID_TEAM_IDENTIFIER" ]] ||
    fail "Signing team does not match the configured release identity"
fi

# Smoke the signed candidate with disposable credentials before touching commands.
(
  cd "$ROOT"
  node scripts/smoke-release-native-only.mjs "$stage/maestro"
)
for name in maestro-tui deixic-code; do ln -s maestro "$stage/$name"; done

# rename(2) replaces a symlink itself on both macOS and Linux; mv can instead
# follow a directory symlink. Migrate legacy commands preserving their bytes,
# then activate all three managed aliases through one current-pointer rename.
node - "$PREFIX" "$stage" <<'NODE'
const fs = require('node:fs');
const path = require('node:path');
const [prefix, stage] = process.argv.slice(2);
const store = path.join(prefix, '.maestro-local');
const current = path.join(store, 'current');
const names = ['maestro', 'deixic-code', 'maestro-tui'];
const exists = file => { try { return fs.lstatSync(file); } catch (e) { if (e.code === 'ENOENT') return; throw e; } };
for (const name of names) {
  const file = path.join(prefix, name);
  const stat = exists(file);
  if (stat && !stat.isFile() && !stat.isSymbolicLink()) throw new Error(`Cannot replace ${file}: expected a file or symlink`);
}
const replaceLink = (target, file) => {
  const temporary = path.join(stage, 'activation-link');
  fs.symlinkSync(target, temporary);
  fs.renameSync(temporary, file);
};
const managed = names.every(name => {
  const file = path.join(prefix, name);
  return exists(file)?.isSymbolicLink() && fs.readlinkSync(file) === `.maestro-local/current/${name}`;
});
if (!managed) {
  const previous = fs.mkdtempSync(path.join(store, 'previous.'));
  for (const name of names) {
    const file = path.join(prefix, name);
    // Missing aliases are introduced at activation; existing aliases retain
    // their individual previous contents during the first migration.
    fs.copyFileSync(fs.existsSync(file) ? file : path.join(stage, name), path.join(previous, name));
    fs.chmodSync(path.join(previous, name), 0o755);
  }
  replaceLink(path.basename(previous), current);
  for (const name of names) replaceLink(`.maestro-local/current/${name}`, path.join(prefix, name));
}
replaceLink(path.basename(stage), current);
NODE
stage="" # The activated build is retained, along with previous installations.
printf 'Installed %s/deixic-code (aliases: maestro, maestro-tui).\n' "$PREFIX"
printf 'Put %s first on PATH, then run deixic-code doctor.\n' "$PREFIX"

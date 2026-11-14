#!/usr/bin/env bash
# Explicit macOS acceptance check with a disposable keychain and real signatures.
set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "${MAESTRO_KEYCHAIN_SMOKE_PRODUCT_ROOT:-$SCRIPT_DIR/..}" && pwd)"
[[ "$(uname -s)" == Darwin ]] || { echo 'Requires macOS' >&2; exit 1; }
: "${MAESTRO_RELEASE_DEVELOPER_ID_AUTHORITY:?Set the Developer ID Application identity used by releases}"
binary="${1:?Pass the exact signed release binary for the installer smoke}"
[[ -f "$binary" && -x "$binary" ]] || { echo 'Release binary is missing' >&2; exit 1; }
binary="$(cd "$(dirname "$binary")" && pwd)/$(basename "$binary")"
fixture="$(mktemp -d)"
cleanup() {
  local delete_status=0
  if [[ -e "$fixture/probe.keychain-db" || -e "$fixture/probe.keychain" ]]; then
    "$fixture/probe-v1" delete "$fixture/probe.keychain" || delete_status=$?
  fi
  rm -rf "$fixture"
  return "$delete_status"
}
trap cleanup EXIT
mkdir -p "$fixture/tools"
# Exercise the unchanged product installer and its mandatory CLI smoke using
# the actual release binary, without compiling the runtime a second time.
cat > "$fixture/tools/cargo" <<'CARGO'
#!/usr/bin/env bash
node -e 'console.log(JSON.stringify({reason:"compiler-artifact",target:{name:"maestro",kind:["bin"]},executable:process.env.MAESTRO_SMOKE_EXECUTABLE}))'
CARGO
chmod 755 "$fixture/tools/cargo"
for version in 1 2; do
  mkdir -p "$fixture/v$version"
  PATH="$fixture/tools:$PATH" MAESTRO_SMOKE_EXECUTABLE="$binary" \
    MAESTRO_INSTALL_PREFIX="$fixture/v$version" bash "$ROOT/scripts/install-native-local.sh"
  node "$ROOT/scripts/check-macos-release-signature.mjs" "$fixture/v$version/maestro"
  # These distinct C probes exercise only the real disposable Keychain APIs.
  # They deliberately do not implement Code's --version/headless/serve protocol.
  clang -Wno-deprecated-declarations -DBUILD_VERSION="\"$version\"" \
    "$SCRIPT_DIR/fixtures/keychain-upgrade.c" -framework Security -framework CoreFoundation \
    -o "$fixture/probe-v$version"
  codesign --force --identifier maestro --options runtime --timestamp \
    --sign "$MAESTRO_RELEASE_DEVELOPER_ID_AUTHORITY" "$fixture/probe-v$version"
  node "$ROOT/scripts/check-macos-release-signature.mjs" "$fixture/probe-v$version"
done
"$fixture/probe-v1" create "$fixture/probe.keychain"
"$fixture/probe-v1" read "$fixture/probe.keychain"
"$fixture/probe-v2" read "$fixture/probe.keychain"
# A changed identifier must fail without prompting, proving this test exercises
# the item's code-signing ACL rather than an unrestricted credential.
cp "$fixture/probe-v2" "$fixture/different-identity"
codesign --force --identifier maestro-keychain-negative-control --options runtime --timestamp \
  --sign "$MAESTRO_RELEASE_DEVELOPER_ID_AUTHORITY" "$fixture/different-identity"
if negative_output="$("$fixture/different-identity" read "$fixture/probe.keychain")"; then
  echo 'Different signing identity unexpectedly accessed the credential' >&2
  exit 1
fi
printf '%s\n' "$negative_output"
# errSecAuthFailed or errSecInteractionNotAllowed; unrelated failures do not count.
[[ "$negative_output" == *'status=-25293' || "$negative_output" == *'status=-25308' ]] || exit 1
printf 'Keychain grant survived a changed binary and path; different identity was refused without UI.\n'

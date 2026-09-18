#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
for cmd in curl mktemp mkdir printf tar python3 find dirname chmod awk sleep cat wc tr rm cp ln; do
  command -v "$cmd" >/dev/null || {
    printf 'Required command not found: %s\n' "$cmd" >&2
    exit 1
  }
done

fail() {
  printf 'Install fixture failed: %s\n' "$*" >&2
  exit 1
}

wait_for_server_log_entry() {
  local start_line="$1"
  local expected="$2"
  for _ in {1..20}; do
    if tail -n +$((start_line + 1)) "$fixture/server.log" | grep -Fq "$expected"; then
      return 0
    fi
    sleep 0.1
  done
  return 1
}

hash_file() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{print $1}'
  else
    shasum -a 256 "$1" | awk '{print $1}'
  fi
}

fixture="$(mktemp -d 2>/dev/null || mktemp -d -t maestro-install-fixture)"
server_pid=""
cleanup() {
  if [[ -n "$server_pid" ]]; then
    kill "$server_pid" 2>/dev/null || true
    wait "$server_pid" 2>/dev/null || true
  fi
  rm -rf "$fixture"
}
trap cleanup EXIT

platform_os="$(uname -s)"
platform_arch="$(uname -m)"
case "$platform_os:$platform_arch" in
  Darwin:arm64) platform=darwin-arm64 ;;
  Darwin:x86_64|Darwin:amd64) platform=darwin-x64 ;;
  Linux:aarch64|Linux:arm64) platform=linux-arm64 ;;
  Linux:x86_64|Linux:amd64) platform=linux-x64 ;;
  *) fail "unsupported test host: $platform_os/$platform_arch" ;;
esac

asset="maestro-$platform"
release_dir="$fixture/v0.0.1"
manifest_failure_release_dir="$fixture/v0.0.2"
legacy_release_dir="$fixture/v0.0.8"
unsigned_legacy_release_dir="$fixture/v0.0.9"
mkdir -p "$release_dir" "$manifest_failure_release_dir" "$legacy_release_dir" "$unsigned_legacy_release_dir" "$fixture/home"

write_fixture_binary() {
  local version="$1"
  local target_dir="${2:-$release_dir}"
  {
    printf '%s\n' '#!/bin/sh'
	    # shellcheck disable=SC2016
	    printf 'if [ "$1" = "--version" ]; then printf "maestro %s\\n"; else printf "fixture binary\\n"; fi\n' "$version"
  } > "$target_dir/$asset"
  chmod 755 "$target_dir/$asset"
}

write_manifest() {
  local target_dir="${1:-$release_dir}"
  local binary_digest
  binary_digest="$(hash_file "$target_dir/$asset")"
  {
    printf '%s  %s\n' "$binary_digest" "$asset"
  } > "$target_dir/SHA256SUMS"
}

write_fixture_binary "0.0.1"
write_manifest
cp "$release_dir/$asset" "$manifest_failure_release_dir/$asset"
write_fixture_binary "0.0.8" "$legacy_release_dir"
write_manifest "$legacy_release_dir"
write_fixture_binary "0.0.9" "$unsigned_legacy_release_dir"

preview_release_dir="$fixture/v0.0.3-beta.1"
mkdir -p "$preview_release_dir"
write_fixture_binary "0.0.3-beta.1"
write_manifest
cp "$release_dir/$asset" "$preview_release_dir/$asset"
{
  printf '%s  %s\n' "$(hash_file "$preview_release_dir/$asset")" "$asset"
} > "$preview_release_dir/SHA256SUMS"
alpha_release_dir="$fixture/v0.0.4-alpha.1"
mkdir -p "$alpha_release_dir"
write_fixture_binary "0.0.4-alpha.1" "$alpha_release_dir"
write_manifest "$alpha_release_dir"
write_fixture_binary "0.0.1"
write_manifest

port_file="$fixture/port"
python3 - "$fixture" > "$port_file" 2>"$fixture/server.log" <<'PY' &
import http.server
import json
import os
import sys
from urllib.parse import parse_qs, urlsplit

os.chdir(sys.argv[1])

def preview_manifest(channel, version, port):
    return {
        "schemaVersion": "evalops.maestro.release-channel.v1",
        "channel": channel,
        "keyId": "preview-2026-08-912a0dab",
        "version": version,
        "releaseTag": f"v{version}",
        "releaseUrl": f"http://127.0.0.1:{port}/v{version}",
        "metadataUrl": None,
        "metadataSha256": None,
        "sourceSha": "a" * 40,
        "issuedAtMs": 1,
        "releaseNotes": None,
        "releaseReceipt": None,
        "signature": "fixture-signature",
    }

def stable_manifest(port, version="0.0.1", release_url=None):
    manifest = preview_manifest("stable", version, port)
    manifest["keyId"] = "stable-2026-08-0c3df2ac"
    if release_url is not None:
        manifest["releaseUrl"] = release_url
    return manifest

def beta_manifest(port):
    return preview_manifest("beta", "0.0.3-beta.1", port)

def alpha_manifest(port):
    return preview_manifest("alpha", "0.0.4-alpha.1", port)

class Handler(http.server.SimpleHTTPRequestHandler):
    def do_GET(self):
        request_path = urlsplit(self.path).path
        query = parse_qs(urlsplit(self.path).query)
        if request_path == "/v0.0.2/SHA256SUMS":
            self.send_response(500)
            self.end_headers()
            self.wfile.write(b"simulated manifest failure")
            return
        if request_path == "/channels/beta/manifest.json":
            port = self.server.server_address[1]
            body = json.dumps(beta_manifest(port)).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            return
        if request_path == "/channels/beta/noncanonical":
            port = self.server.server_address[1]
            manifest = beta_manifest(port)
            manifest["version"] = "01.0.3-beta.1"
            manifest["releaseTag"] = "v01.0.3-beta.1"
            body = json.dumps(manifest).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            return
        if request_path == "/channels/alpha/manifest.json":
            port = self.server.server_address[1]
            body = json.dumps(alpha_manifest(port)).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            return
        if request_path == "/v0.0.1/channel-manifest.json":
            port = self.server.server_address[1]
            body = json.dumps(stable_manifest(port)).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            return
        if request_path == "/v0.0.5/channel-manifest.json":
            port = self.server.server_address[1]
            manifest = stable_manifest(port, "0.0.5")
            manifest["releaseUrl"] = f"http://127.0.0.1:{port}/v0.0.1"
            body = json.dumps(manifest).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            return
        if request_path == "/v0.0.6/channel-manifest.json":
            port = self.server.server_address[1]
            manifest = stable_manifest(port, "0.0.6")
            manifest["releaseTag"] = "v0.0.5"
            body = json.dumps(manifest).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            return
        if request_path == "/v0.0.7/channel-manifest.json":
            port = self.server.server_address[1]
            manifest = stable_manifest(port, "0.0.7")
            manifest["signature"] = ""
            body = json.dumps(manifest).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            return
        if request_path == "/v0.0.3-beta.1/channel-manifest.json":
            port = self.server.server_address[1]
            body = json.dumps(beta_manifest(port)).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            return
        if request_path == "/v0.0.4-alpha.1/channel-manifest.json":
            port = self.server.server_address[1]
            body = json.dumps(alpha_manifest(port)).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            return
        if request_path in {"/github/releases", "/github/fallback/releases"}:
            port = self.server.server_address[1]
            page = int(query.get("page", ["1"])[0])
            if page == 1:
                releases = [
                    {
                        "tag_name": f"v9.9.{index}-beta.1",
                        "draft": True,
                        "prerelease": True,
                        "assets": [],
                    }
                    for index in range(99)
                ]
                releases.append({
                    "tag_name": "v99.99.99-beta.99",
                    "draft": False,
                    "prerelease": True,
                    "assets": [],
                })
            else:
                releases = [
                    {
                        "tag_name": "v0.0.1",
                        "draft": False,
                        "prerelease": False,
                        "assets": [{"name": "channel-manifest.json"}],
                    },
                    {
                        "tag_name": "v0.0.3-beta.1",
                        "draft": False,
                        "prerelease": True,
                        "assets": [{"name": "channel-manifest.json"}],
                    },
                    {
                        "tag_name": "v0.0.4-alpha.1",
                        "draft": False,
                        "prerelease": True,
                        "assets": [{"name": "channel-manifest.json"}],
                    },
                ]
            body = json.dumps(releases).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            return
        if request_path == "/github/empty":
            body = b"[]"
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            return
        if request_path == "/github/rate-limited":
            self.send_response(429)
            self.end_headers()
            self.wfile.write(b"rate limited")
            return
        if request_path == "/github/latest/unavailable":
            self.send_response(503)
            self.end_headers()
            self.wfile.write(b"temporarily unavailable")
            return
        if request_path == "/github/latest/channel-manifest.json":
            port = self.server.server_address[1]
            body = json.dumps(stable_manifest(port)).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            return
        if "maestro-beta-channel" in request_path or "maestro-alpha-channel" in request_path:
            self.send_response(404)
            self.end_headers()
            self.wfile.write(b"deleted channel alias")
            return
        super().do_GET()

server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
print(server.server_port, flush=True)
server.serve_forever()
PY
server_pid="$!"

for attempt in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20; do
	[[ -s "$port_file" ]] && break
	if (( attempt == 20 )); then
		fail "fixture HTTP server did not start: $(cat "$fixture/server.log")"
	fi
	sleep 0.1
done
read -r port < "$port_file"

install_dir="$fixture/bin"
data_dir="$fixture/data"
release_url="http://127.0.0.1:$port/v0.0.1"
run_install() {
  HOME="$fixture/home" \
  MAESTRO_INSTALL_DIR="$install_dir" \
  MAESTRO_DATA_DIR="$data_dir" \
  MAESTRO_INSTALL_VERSION="0.0.1" \
  MAESTRO_INSTALL_CHANNEL="stable" \
  MAESTRO_RELEASE_BASE_URL="$release_url" \
  MAESTRO_ALLOW_UNSIGNED_INSTALL=1 \
  bash "$ROOT/scripts/install.sh"
}

# Replacing a source install must preserve both old entrypoints for rollback.
mkdir -p "$install_dir"
printf '%s\n' '#!/bin/sh' 'export MAESTRO_INSTALL_METHOD=source' 'exit 0' > "$install_dir/maestro"
chmod +x "$install_dir/maestro"
ln -s maestro "$install_dir/deixic-code"
cp "$install_dir/maestro" "$fixture/previous-source-launcher"

mkdir -p "$fixture/shadow"
printf '%s\n' '#!/bin/sh' 'exit 0' > "$fixture/shadow/maestro"
chmod +x "$fixture/shadow/maestro"
PATH="$fixture/shadow:$PATH" run_install > "$fixture/first-install.log" 2>&1 ||
  fail "first install failed: $(cat "$fixture/first-install.log")"
[[ -x "$install_dir/maestro" ]] || fail "launcher was not installed"
[[ -x "$install_dir/deixic-code" ]] || fail "canonical launcher was not installed"
[[ "$("$install_dir/deixic-code" --version)" == "maestro 0.0.1" ]] ||
  fail "canonical launcher did not execute the compatibility binary"
grep -q 'Installed Deixic Code 0.0.1' "$fixture/first-install.log" ||
  fail "installer did not report the canonical product name"

grep -Fq "PATH selects $fixture/shadow/maestro" "$fixture/first-install.log" || fail "shadowing executable was not reported"
backup_dir="$(find "$data_dir/releases/0.0.1" -type d -name previous-launchers | head -n 1)"
[[ -n "$backup_dir" ]] || fail "previous launchers were not retained"
cmp "$fixture/previous-source-launcher" "$backup_dir/maestro" || fail "source launcher backup changed"
[[ -L "$backup_dir/deixic-code" ]] || fail "canonical symlink backup was not preserved"
[[ "$("$install_dir/maestro")" == 'fixture binary' ]] || fail "compatibility launcher does not start default entrypoint"
[[ "$("$install_dir/deixic-code")" == 'fixture binary' ]] || fail "canonical launcher does not start default entrypoint"

progress_install_dir="$fixture/progress-bin"
progress_data_dir="$fixture/progress-data"
HOME="$fixture/home" \
MAESTRO_INSTALL_DIR="$progress_install_dir" \
MAESTRO_DATA_DIR="$progress_data_dir" \
MAESTRO_INSTALL_VERSION="0.0.1" \
MAESTRO_INSTALL_CHANNEL="stable" \
MAESTRO_RELEASE_BASE_URL="$release_url" \
MAESTRO_ALLOW_UNSIGNED_INSTALL=1 \
MAESTRO_UPDATE_PROGRESS=1 \
bash "$ROOT/scripts/install.sh" > "$fixture/progress-install.log" 2>&1 ||
  fail "progress install failed: $(cat "$fixture/progress-install.log")"
progress_steps="$(grep -E '^\[[123]/3\]' "$fixture/progress-install.log")"
expected_progress_steps="$(printf '%s\n' \
  '[1/3] Downloading version 0.0.1...' \
  '[2/3] Verifying checksum...' \
  '[3/3] Installing update...')"
[[ "$progress_steps" == "$expected_progress_steps" ]] ||
  fail "installer progress stages were missing or out of order: $(cat "$fixture/progress-install.log")"
if grep -q 'Installed Deixic Code' "$fixture/progress-install.log"; then
  fail "embedded update progress included the standalone installer summary"
fi

no_python_path="$fixture/no-python-bin"
mkdir "$no_python_path"
# Keep both Python and OpenSSL out of this PATH: the standalone installer must
# use only its documented portable shell, base64, and checksum tool baseline.
for command_name in bash gzip uname curl mktemp chmod mkdir tar rm cp mv awk dirname basename date base64 tr wc; do
  ln -s "$(command -v "$command_name")" "$no_python_path/$command_name"
done
if command -v sha256sum >/dev/null 2>&1; then
  ln -s "$(command -v sha256sum)" "$no_python_path/sha256sum"
else
  ln -s "$(command -v shasum)" "$no_python_path/shasum"
fi
no_python_install_dir="$fixture/no-python-bin-install"
no_python_data_dir="$fixture/no-python-bin-data"
env HOME="$fixture/home" PATH="$no_python_path" MAESTRO_INSTALL_DIR="$no_python_install_dir" MAESTRO_DATA_DIR="$no_python_data_dir" MAESTRO_INSTALL_CHANNEL="stable" MAESTRO_RELEASE_API_URL="http://127.0.0.1:$port/github/releases" MAESTRO_RELEASE_DOWNLOAD_BASE="http://127.0.0.1:$port" MAESTRO_ALLOW_UNSIGNED_INSTALL=1 bash "$ROOT/scripts/install.sh" > "$fixture/no-python-install.log" 2>&1 ||
  fail "standalone install required python3: $(cat "$fixture/no-python-install.log")"
[[ "$("$no_python_install_dir/maestro" --version)" == "maestro 0.0.1" ]] ||
  fail "no-python standalone install did not select the stable release"

legacy_install_dir="$fixture/legacy-bin"
legacy_data_dir="$fixture/legacy-data"
legacy_release_url="http://127.0.0.1:$port/v0.0.8"
HOME="$fixture/home" \
MAESTRO_INSTALL_DIR="$legacy_install_dir" \
MAESTRO_DATA_DIR="$legacy_data_dir" \
MAESTRO_INSTALL_VERSION="0.0.8" \
MAESTRO_RELEASE_BASE_URL="$legacy_release_url" \
MAESTRO_ALLOW_UNSIGNED_INSTALL=1 \
bash "$ROOT/scripts/install.sh" > "$fixture/legacy-install.log" 2>&1 ||
  fail "explicit pinned legacy release without a channel manifest was rejected: $(cat "$fixture/legacy-install.log")"
[[ "$("$legacy_install_dir/maestro" --version)" == "maestro 0.0.8" ]] ||
  fail "explicit pinned legacy release installed the wrong binary"
grep -q 'using legacy artifact verification' "$fixture/legacy-install.log" ||
  fail "legacy pinned install did not report its unsigned compatibility path"

if HOME="$fixture/home" \
  MAESTRO_INSTALL_DIR="$fixture/legacy-strict-bin" \
  MAESTRO_DATA_DIR="$fixture/legacy-strict-data" \
  MAESTRO_INSTALL_VERSION="0.0.8" \
  MAESTRO_RELEASE_BASE_URL="$legacy_release_url" \
  MAESTRO_REQUIRE_SIGNED_INSTALL=1 \
  bash "$ROOT/scripts/install.sh" > "$fixture/legacy-strict-install.log" 2>&1; then
  fail "strict signing accepted a pinned release without a channel manifest"
fi
grep -q 'Pinned release has no channel manifest' "$fixture/legacy-strict-install.log" ||
  fail "strict signing did not reject a pinned release without a channel manifest"

unsigned_legacy_install_dir="$fixture/unsigned-legacy-bin"
unsigned_legacy_data_dir="$fixture/unsigned-legacy-data"
unsigned_legacy_release_url="http://127.0.0.1:$port/v0.0.9"
HOME="$fixture/home" \
MAESTRO_INSTALL_DIR="$unsigned_legacy_install_dir" \
MAESTRO_DATA_DIR="$unsigned_legacy_data_dir" \
MAESTRO_INSTALL_VERSION="0.0.9" \
MAESTRO_RELEASE_BASE_URL="$unsigned_legacy_release_url" \
bash "$ROOT/scripts/install.sh" > "$fixture/unsigned-legacy-install.log" 2>&1 ||
  fail "explicit pinned release without channel or checksum manifests was rejected: $(cat "$fixture/unsigned-legacy-install.log")"
[[ "$("$unsigned_legacy_install_dir/maestro" --version)" == "maestro 0.0.9" ]] ||
  fail "unsigned legacy pinned install installed the wrong binary"
if grep -q 'Downloading Cosign' "$fixture/unsigned-legacy-install.log"; then
  fail "legacy release without signed metadata bootstrapped Cosign unnecessarily"
fi

if HOME="$fixture/home" \
  MAESTRO_INSTALL_DIR="$fixture/mismatched-channel-bin" \
  MAESTRO_DATA_DIR="$fixture/mismatched-channel-data" \
  MAESTRO_INSTALL_VERSION="0.0.1" \
  MAESTRO_INSTALL_CHANNEL="beta" \
  MAESTRO_RELEASE_BASE_URL="$release_url" \
  MAESTRO_ALLOW_UNSIGNED_INSTALL=1 \
  bash "$ROOT/scripts/install.sh" > "$fixture/mismatched-channel.log" 2>&1; then
  fail "installer accepted a stable version for the beta channel"
fi
grep -q 'beta channel requires a beta prerelease version' "$fixture/mismatched-channel.log" ||
  fail "installer did not reject a stable version for the beta channel"

grep -q '^export MAESTRO_INSTALL_METHOD=release$' "$install_dir/maestro" ||
  fail "launcher did not identify the signed release install method"
grep -q 'Downloading channel manifest' "$fixture/first-install.log" ||
  fail "stable install did not verify its channel manifest"
grep -q '^export MAESTRO_INSTALL_DIR=' "$install_dir/maestro" ||
  fail "launcher did not retain its install directory"
grep -q '^export MAESTRO_DATA_DIR=' "$install_dir/maestro" ||
  fail "launcher did not retain its data directory"
grep -q '^export MAESTRO_UPDATE_CHANNEL=' "$install_dir/maestro" ||
  fail "launcher did not retain its update channel"
[[ "$(MAESTRO_UPDATE_CHANNEL='' "$install_dir/maestro" --version)" == "maestro 0.0.1" ]] ||
  fail "launcher with a persisted stable channel did not execute"
grep -q '^export MAESTRO_STARTUP_UPDATE_STATE=' "$install_dir/maestro" ||
  fail "launcher did not retain startup update metadata"
grep -q '^export MAESTRO_VERSION=' "$install_dir/maestro" ||
  fail "launcher did not retain its installed version"
[[ "$("$install_dir/maestro" --version)" == "maestro 0.0.1" ]] ||
  fail "launcher did not execute the first release"
[[ "$("$install_dir/deixic-code" --version)" == "maestro 0.0.1" ]] ||
  fail "canonical launcher did not execute the first release"
release_binary="$(find "$data_dir/releases" -type f -path '*/bin/maestro' -print -quit)"
[[ -n "$release_binary" ]] || fail "versioned release binary was not staged"
release_root="$(dirname "$(dirname "$release_binary")")"
[[ -f "$release_root/install-receipt.json" ]] || fail "install receipt was not staged beside the binary"
python3 - "$release_root/install-receipt.json" <<'PY'
import json
import sys

receipt = json.load(open(sys.argv[1], encoding="utf-8"))
assert receipt["schemaVersion"] == "evalops.maestro.install-receipt.v1"
assert receipt["version"] == "0.0.1"
assert receipt["verified"] is False
assert receipt["verification"]["artifactSha256"].startswith("sha256:")
assert "webSha256" not in receipt["verification"]
assert receipt["verification"]["metadataSha256"] is None
assert receipt["releaseMetadataAsset"] is None
PY
release_dir_name="${release_root##*/}"
case "$release_dir_name" in
  "$platform".??????) ;;
  *) fail "release directory is not uniquely allocated: $release_dir_name" ;;
esac

runtime_version_install_dir="$fixture/runtime-version-bin"
runtime_version_data_dir="$fixture/runtime-version-data"
HOME="$fixture/home" \
MAESTRO_INSTALL_DIR="$runtime_version_install_dir" \
MAESTRO_DATA_DIR="$runtime_version_data_dir" \
MAESTRO_VERSION="9.9.9" \
MAESTRO_RELEASE_BASE_URL="$release_url" \
MAESTRO_ALLOW_UNSIGNED_INSTALL=1 \
bash "$ROOT/scripts/install.sh" > "$fixture/runtime-version-install.log" 2>&1 ||
  fail "runtime version metadata affected install: $(cat "$fixture/runtime-version-install.log")"
[[ "$("$runtime_version_install_dir/maestro" --version)" == "maestro 0.0.1" ]] ||
  fail "runtime version metadata pinned the installed binary"
[[ -d "$runtime_version_data_dir/releases/0.0.1" ]] ||
  fail "runtime version metadata changed the staged release version"
[[ ! -e "$runtime_version_data_dir/releases/9.9.9" ]] ||
  fail "runtime version metadata was interpreted as an installer pin"

if HOME="$fixture/home" \
  MAESTRO_INSTALL_CHANNEL="nightly" \
  bash "$ROOT/scripts/install.sh" > "$fixture/invalid-channel.log" 2>&1; then
  fail "installer accepted an unknown update channel"
fi
grep -q 'MAESTRO_INSTALL_CHANNEL must be stable, beta, or alpha' "$fixture/invalid-channel.log" ||
  fail "installer did not explain the invalid channel"

relative_install_dir="$fixture/relative-bin"
mkdir -p "$fixture/invocation"
(
  cd "$fixture"
  HOME="$fixture/home" \
  MAESTRO_INSTALL_DIR="$relative_install_dir" \
  MAESTRO_DATA_DIR="relative-data" \
  MAESTRO_INSTALL_VERSION="0.0.1" \
  MAESTRO_RELEASE_BASE_URL="$release_url" \
  MAESTRO_ALLOW_UNSIGNED_INSTALL=1 \
  bash "$ROOT/scripts/install.sh" > "$fixture/relative-install.log" 2>&1
) || fail "relative data directory install failed: $(cat "$fixture/relative-install.log")"
relative_version="$(
  cd "$fixture/invocation"
  "$relative_install_dir/maestro" --version
)"
[[ "$relative_version" == "maestro 0.0.1" ]] ||
  fail "launcher with relative data directory failed outside the install directory"

if HOME="$fixture/home" \
  MAESTRO_INSTALL_DIR="$install_dir" \
  MAESTRO_DATA_DIR="$data_dir" \
  MAESTRO_INSTALL_VERSION="0.0.2" \
  MAESTRO_RELEASE_BASE_URL="http://127.0.0.1:$port/v0.0.2" \
  MAESTRO_ALLOW_UNSIGNED_INSTALL=1 \
  bash "$ROOT/scripts/install.sh" > "$fixture/manifest-failure.log" 2>&1; then
  fail "manifest transport failure unexpectedly downgraded to unsigned install"
fi
[[ "$("$install_dir/maestro" --version)" == "maestro 0.0.1" ]] ||
  fail "manifest transport failure changed the active launcher"
release_binary_count="$(find "$data_dir/releases" -type f -path '*/bin/maestro' | wc -l | tr -d ' ')"
[[ "$release_binary_count" == "1" ]] ||
  fail "manifest transport failure published an extra release: $release_binary_count"

if HOME="$fixture/home" \
  MAESTRO_INSTALL_DIR="$install_dir" \
  MAESTRO_DATA_DIR="$data_dir" \
  MAESTRO_INSTALL_VERSION="0.0.1" \
  MAESTRO_RELEASE_BASE_URL="$release_url" \
  MAESTRO_REQUIRE_SIGNED_INSTALL=1 \
  MAESTRO_ALLOW_UNSIGNED_INSTALL=1 \
  bash "$ROOT/scripts/install.sh" > "$fixture/conflicting-signing-flags.log" 2>&1; then
  fail "strict signing accepted the unsigned bypass override"
fi
[[ "$("$install_dir/maestro" --version)" == "maestro 0.0.1" ]] ||
  fail "conflicting signing flags changed the active launcher"

if HOME="$fixture/home" \
  MAESTRO_INSTALL_DIR="$install_dir" \
  MAESTRO_DATA_DIR="$data_dir" \
  MAESTRO_INSTALL_VERSION="0.0.1" \
  MAESTRO_RELEASE_BASE_URL="$release_url" \
  MAESTRO_REQUIRE_SIGNED_INSTALL=1 \
  bash "$ROOT/scripts/install.sh" > "$fixture/strict-install.log" 2>&1; then
  fail "strict mode accepted an unsigned fixture"
fi
[[ "$("$install_dir/maestro" --version)" == "maestro 0.0.1" ]] ||
  fail "strict-mode failure changed the active launcher"

# Corrupt the binary without changing SHA256SUMS. The checksum failure must
# happen before a new release directory or launcher is published.
write_fixture_binary "9.9.9"
if run_install > "$fixture/corrupt-install.log" 2>&1; then
  fail "corrupt binary unexpectedly installed"
fi
[[ "$("$install_dir/maestro" --version)" == "maestro 0.0.1" ]] ||
  fail "checksum failure changed the active launcher"
release_binary_count="$(find "$data_dir/releases" -type f -path '*/bin/maestro' | wc -l | tr -d ' ')"
[[ "$release_binary_count" == "1" ]] ||
  fail "checksum failure published an extra release: $release_binary_count"

pointer_install_dir="$fixture/pointer-bin"
pointer_data_dir="$fixture/pointer-data"
HOME="$fixture/home" \
MAESTRO_INSTALL_DIR="$pointer_install_dir" \
MAESTRO_DATA_DIR="$pointer_data_dir" \
MAESTRO_INSTALL_CHANNEL="beta" \
MAESTRO_CHANNEL_MANIFEST_URL="http://127.0.0.1:$port/channels/beta/manifest.json" \
MAESTRO_ALLOW_UNSIGNED_INSTALL=1 \
bash "$ROOT/scripts/install.sh" > "$fixture/pointer-install.log" 2>&1 ||
  fail "channel pointer install failed: $(cat "$fixture/pointer-install.log")"
[[ "$("$pointer_install_dir/maestro" --version)" == "maestro 0.0.3-beta.1" ]] ||
  fail "channel pointer install did not select the preview release"
grep -q 'Using beta channel pointer' "$fixture/pointer-install.log" ||
  fail "channel pointer install did not report the signed pointer"
grep -q 'maestro-beta-channel' "$fixture/pointer-install.log" &&
  fail "channel pointer install requested the deleted GitHub channel alias"

if HOME="$fixture/home" \
  MAESTRO_INSTALL_DIR="$fixture/noncanonical-bin" \
  MAESTRO_DATA_DIR="$fixture/noncanonical-data" \
  MAESTRO_INSTALL_CHANNEL="beta" \
  MAESTRO_CHANNEL_MANIFEST_URL="http://127.0.0.1:$port/channels/beta/noncanonical" \
  MAESTRO_RELEASE_API_URL="http://127.0.0.1:$port/github/empty" \
  MAESTRO_ALLOW_UNSIGNED_INSTALL=1 \
  bash "$ROOT/scripts/install.sh" > "$fixture/noncanonical.log" 2>&1; then
  fail "installer accepted a non-canonical beta version: $(cat "$fixture/noncanonical.log")"
fi
grep -q 'beta channel requires a matching prerelease version' "$fixture/noncanonical.log" ||
  fail "installer did not reject a non-canonical beta version"

alpha_install_dir="$fixture/alpha-bin"
alpha_data_dir="$fixture/alpha-data"
HOME="$fixture/home" \
MAESTRO_INSTALL_DIR="$alpha_install_dir" \
MAESTRO_DATA_DIR="$alpha_data_dir" \
MAESTRO_INSTALL_CHANNEL="alpha" \
MAESTRO_CHANNEL_MANIFEST_URL="http://127.0.0.1:$port/channels/alpha/manifest.json" \
MAESTRO_ALLOW_UNSIGNED_INSTALL=1 \
bash "$ROOT/scripts/install.sh" > "$fixture/alpha-install.log" 2>&1 ||
  fail "alpha channel pointer install failed: $(cat "$fixture/alpha-install.log")"
[[ "$("$alpha_install_dir/maestro" --version)" == "maestro 0.0.4-alpha.1" ]] ||
  fail "alpha channel pointer install did not select the alpha release"
grep -q 'Using alpha channel pointer' "$fixture/alpha-install.log" ||
  fail "alpha channel pointer install did not report the pointer"

api_install_dir="$fixture/api-bin"
api_data_dir="$fixture/api-data"
HOME="$fixture/home" \
MAESTRO_INSTALL_DIR="$api_install_dir" \
MAESTRO_DATA_DIR="$api_data_dir" \
MAESTRO_INSTALL_CHANNEL="beta" \
MAESTRO_CHANNEL_MANIFEST_URL="http://127.0.0.1:$port/missing-channel-pointer" \
MAESTRO_RELEASE_API_URL="http://127.0.0.1:$port/github/releases" \
MAESTRO_RELEASE_DOWNLOAD_BASE="http://127.0.0.1:$port" \
MAESTRO_ALLOW_UNSIGNED_INSTALL=1 \
bash "$ROOT/scripts/install.sh" > "$fixture/api-install.log" 2>&1 ||
  fail "GitHub channel fallback install failed: $(cat "$fixture/api-install.log")"
[[ "$("$api_install_dir/maestro" --version)" == "maestro 0.0.3-beta.1" ]] ||
  fail "GitHub channel fallback did not select the preview release"
grep -q 'Using GitHub beta release v0.0.3-beta.1' "$fixture/api-install.log" ||
  fail "GitHub channel fallback did not report the immutable tag"

alpha_api_install_dir="$fixture/alpha-api-bin"
alpha_api_data_dir="$fixture/alpha-api-data"
HOME="$fixture/home" \
MAESTRO_INSTALL_DIR="$alpha_api_install_dir" \
MAESTRO_DATA_DIR="$alpha_api_data_dir" \
MAESTRO_INSTALL_CHANNEL="alpha" \
MAESTRO_CHANNEL_MANIFEST_URL="http://127.0.0.1:$port/missing-alpha-pointer" \
MAESTRO_RELEASE_API_URL="http://127.0.0.1:$port/github/releases" \
MAESTRO_RELEASE_DOWNLOAD_BASE="http://127.0.0.1:$port" \
MAESTRO_ALLOW_UNSIGNED_INSTALL=1 \
bash "$ROOT/scripts/install.sh" > "$fixture/alpha-api-install.log" 2>&1 ||
  fail "GitHub alpha channel fallback install failed: $(cat "$fixture/alpha-api-install.log")"
[[ "$("$alpha_api_install_dir/maestro" --version)" == "maestro 0.0.4-alpha.1" ]] ||
  fail "GitHub alpha channel fallback did not select the alpha release"
grep -q 'Using GitHub alpha release v0.0.4-alpha.1' "$fixture/alpha-api-install.log" ||
  fail "GitHub alpha channel fallback did not report the immutable tag"

write_fixture_binary "0.0.1"
write_manifest
stable_api_install_dir="$fixture/stable-api-bin"
stable_api_data_dir="$fixture/stable-api-data"
HOME="$fixture/home" \
MAESTRO_INSTALL_DIR="$stable_api_install_dir" \
MAESTRO_DATA_DIR="$stable_api_data_dir" \
MAESTRO_INSTALL_CHANNEL="stable" \
MAESTRO_RELEASE_API_URL="http://127.0.0.1:$port/github/releases" \
MAESTRO_RELEASE_DOWNLOAD_BASE="http://127.0.0.1:$port" \
MAESTRO_ALLOW_UNSIGNED_INSTALL=1 \
bash "$ROOT/scripts/install.sh" > "$fixture/stable-api-install.log" 2>&1 ||
  fail "GitHub stable channel discovery install failed: $(cat "$fixture/stable-api-install.log")"
[[ "$("$stable_api_install_dir/maestro" --version)" == "maestro 0.0.1" ]] ||
  fail "GitHub stable channel discovery did not select the stable release"
grep -q 'Using GitHub stable release v0.0.1' "$fixture/stable-api-install.log" ||
  fail "GitHub stable channel discovery did not report the immutable tag"

stable_latest_install_dir="$fixture/stable-latest-bin"
stable_latest_data_dir="$fixture/stable-latest-data"
HOME="$fixture/home" \
MAESTRO_INSTALL_DIR="$stable_latest_install_dir" \
MAESTRO_DATA_DIR="$stable_latest_data_dir" \
MAESTRO_INSTALL_CHANNEL="stable" \
MAESTRO_STABLE_LATEST_MANIFEST_URL="http://127.0.0.1:$port/github/latest/channel-manifest.json" \
MAESTRO_RELEASE_API_URL="http://127.0.0.1:$port/github/rate-limited" \
MAESTRO_ALLOW_UNSIGNED_INSTALL=1 \
bash "$ROOT/scripts/install.sh" > "$fixture/stable-latest-install.log" 2>&1 ||
  fail "stable latest-download discovery failed under a rate-limited API: $(cat "$fixture/stable-latest-install.log")"
[[ "$("$stable_latest_install_dir/maestro" --version)" == "maestro 0.0.1" ]] ||
  fail "stable latest-download discovery did not select the stable release"
grep -q 'Using stable GitHub latest release v0.0.1' "$fixture/stable-latest-install.log" ||
  fail "stable latest-download discovery did not report the immutable tag"
if grep -q '/github/rate-limited' "$fixture/server.log"; then
  fail "stable latest-download discovery fell back to the rate-limited Releases API"
fi

stable_latest_fallback_install_dir="$fixture/stable-latest-fallback-bin"
stable_latest_fallback_data_dir="$fixture/stable-latest-fallback-data"
stable_latest_fallback_log_start="$(wc -l < "$fixture/server.log")"
HOME="$fixture/home" \
MAESTRO_INSTALL_DIR="$stable_latest_fallback_install_dir" \
MAESTRO_DATA_DIR="$stable_latest_fallback_data_dir" \
MAESTRO_INSTALL_CHANNEL="stable" \
MAESTRO_STABLE_LATEST_MANIFEST_URL="http://127.0.0.1:$port/github/latest/unavailable" \
MAESTRO_RELEASE_API_URL="http://127.0.0.1:$port/github/fallback/releases" \
MAESTRO_RELEASE_DOWNLOAD_BASE="http://127.0.0.1:$port" \
MAESTRO_ALLOW_UNSIGNED_INSTALL=1 \
bash "$ROOT/scripts/install.sh" > "$fixture/stable-latest-fallback-install.log" 2>&1 ||
  fail "stable latest-download recovery failed after a transient error: $(cat "$fixture/stable-latest-fallback-install.log")"
[[ "$("$stable_latest_fallback_install_dir/maestro" --version)" == "maestro 0.0.1" ]] ||
  fail "stable latest-download recovery did not select the API release"
grep -q 'Warning: stable GitHub latest manifest returned HTTP 503; trying the Releases API.' \
  "$fixture/stable-latest-fallback-install.log" ||
  fail "stable latest-download recovery did not report the API fallback"
if ! wait_for_server_log_entry "$stable_latest_fallback_log_start" 'GET /github/latest/unavailable'; then
  fail "stable latest-download recovery did not request the unavailable endpoint"
fi
if ! wait_for_server_log_entry "$stable_latest_fallback_log_start" 'GET /github/fallback/releases?'; then
  fail "stable latest-download recovery did not request the Releases API"
fi

for invalid_version in 0.0.5 0.0.6 0.0.7; do
  if HOME="$fixture/home" \
    MAESTRO_INSTALL_DIR="$fixture/invalid-$invalid_version-bin" \
    MAESTRO_DATA_DIR="$fixture/invalid-$invalid_version-data" \
    MAESTRO_INSTALL_VERSION="$invalid_version" \
    MAESTRO_INSTALL_CHANNEL="stable" \
    MAESTRO_RELEASE_BASE_URL="http://127.0.0.1:$port/v$invalid_version" \
    MAESTRO_ALLOW_UNSIGNED_INSTALL=1 \
    bash "$ROOT/scripts/install.sh" > "$fixture/invalid-$invalid_version.log" 2>&1; then
    fail "stable installer accepted invalid channel manifest fixture v$invalid_version"
  fi
done
grep -q 'does not describe the selected stable release' "$fixture/invalid-0.0.5.log" ||
  fail "stable installer did not reject a wrong release URL"
grep -q 'Channel manifest verification failed for stable' "$fixture/invalid-0.0.6.log" ||
  fail "stable installer did not reject a wrong release tag"
grep -q 'Channel manifest verification failed for stable' "$fixture/invalid-0.0.7.log" ||
  fail "stable installer did not reject a tampered manifest"

if HOME="$fixture/home" \
  MAESTRO_INSTALL_DIR="$fixture/empty-bin" \
  MAESTRO_DATA_DIR="$fixture/empty-data" \
  MAESTRO_INSTALL_CHANNEL="beta" \
  MAESTRO_CHANNEL_MANIFEST_URL="http://127.0.0.1:$port/missing-channel-pointer" \
  MAESTRO_RELEASE_API_URL="http://127.0.0.1:$port/github/empty" \
  MAESTRO_ALLOW_UNSIGNED_INSTALL=1 \
  bash "$ROOT/scripts/install.sh" > "$fixture/empty-channel.log" 2>&1; then
  fail "installer accepted a preview channel with no published release"
fi
grep -q 'No published beta release' "$fixture/empty-channel.log" ||
  fail "installer did not explain a missing preview release"

printf 'installer fixture passed: checksum failure preserved %s and %s\n' \
  "$install_dir/deixic-code" "$install_dir/maestro"

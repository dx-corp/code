#!/usr/bin/env bash
set -euo pipefail

REPO="${MAESTRO_RELEASE_REPO:-evalops/maestro}"
install_channel="${MAESTRO_INSTALL_CHANNEL:-stable}"
COSIGN_VERSION="2.6.1"
STABLE_CHANNEL_KEY_ID="stable-2026-08-0c3df2ac"
PRERELEASE_CHANNEL_KEY_ID="preview-2026-08-912a0dab"
STABLE_CHANNEL_PUBLIC_KEY="IYgvaSwf2E9DioyEZ6Qcp/QMD1xpsjS0JgYluAAt0pE="
PRERELEASE_CHANNEL_PUBLIC_KEY="4DS+odrY7y1PMg7o4s0jY1FkgcPQb8jjdy0Nst05soA="
# Historical blobs were signed by evalops/maestro-internal and evalops/maestro
# release.yml. Live blobs are signed by Mono's maestro-release.yml. Keep both
# organization aliases valid across the evalops -> dx-corp slug cutover.
COSIGN_IDENTITY_REGEXP='^https://github.com/(evalops|dx-corp)/(maestro-internal/.github/workflows/release\.yml|maestro/.github/workflows/release\.yml|mono/.github/workflows/maestro-release\.yml)@'
COSIGN_OIDC_ISSUER="https://token.actions.githubusercontent.com"

fail() {
  printf 'Error: %s\n' "$*" >&2
  exit 1
}

channel_version_matches() {
  local version="$1"
  local channel="$2"
  case "$channel" in
    stable) [[ "$version" =~ ^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$ ]] ;;
    beta) [[ "$version" =~ ^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)-beta\.[1-9][0-9]*$ ]] ;;
    alpha) [[ "$version" =~ ^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)-alpha\.[1-9][0-9]*$ ]] ;;
    *) return 1 ;;
  esac
}

require_channel_version() {
  local version="${1#v}"
  local channel="$2"
  if channel_version_matches "$version" "$channel"; then
    return 0
  fi
  case "$channel" in
    stable)
      fail "stable channel requires a stable semver version: $version"
      ;;
    beta)
      fail "beta channel requires a beta prerelease version: $version"
      ;;
    alpha)
      fail "alpha channel requires an alpha prerelease version: $version"
      ;;
  esac
}

for cmd in uname curl mktemp chmod mkdir tar rm cp mv awk dirname basename date base64 tr wc; do
  command -v "$cmd" >/dev/null || fail "Required command not found: $cmd"
done
if ! command -v sha256sum >/dev/null 2>&1 && ! command -v shasum >/dev/null 2>&1; then
  fail "Required command not found: sha256sum or shasum"
fi

case "$(uname -s)" in
  Darwin) os=darwin ;;
  Linux) os=linux ;;
  *) fail "Unsupported OS: $(uname -s)" ;;
esac
case "$(uname -m)" in
  x86_64|amd64) arch=x64 ;;
  arm64|aarch64) arch=arm64 ;;
  *) fail "Unsupported architecture: $(uname -m)" ;;
esac

platform="${os}-${arch}"
case "$platform" in
  darwin-arm64|darwin-x64|linux-x64|linux-arm64) ;;
  *) fail "Unsupported platform: $platform" ;;
esac

asset="maestro-${platform}"
metadata_asset="release-metadata.json"
case "$platform" in
  darwin-x64)
    cosign_asset="cosign-darwin-amd64"
    cosign_sha256="f1ed2787cc9648fd3c644fcb279e43f3f55da63b788d69a527aa14ad97ffdca1"
    ;;
  darwin-arm64)
    cosign_asset="cosign-darwin-arm64"
    cosign_sha256="54047052cf46f40a5c3c95a510db276e164ba77e096aea1ca1b733f770359689"
    ;;
  linux-x64)
    cosign_asset="cosign-linux-amd64"
    cosign_sha256="064954c5d8c7e3b28188eee5b1727b31c411550bc5fefd41aa672d3c761d103a"
    ;;
  linux-arm64)
    cosign_asset="cosign-linux-arm64"
    cosign_sha256="56a16480bdd56ec789abaa65924402f6b92c0041f06885995853c05567b76f34"
    ;;
esac

case "$install_channel" in
  stable|beta|alpha) ;;
  *) fail "MAESTRO_INSTALL_CHANNEL must be stable, beta, or alpha" ;;
esac

requested_version="${MAESTRO_INSTALL_VERSION:-}"
requested_version="${requested_version#v}"
if [[ -n "$requested_version" ]]; then
  require_channel_version "$requested_version" "$install_channel"
fi

if [[ -n "${MAESTRO_RELEASE_BASE_URL:-}" ]]; then
  release_url="${MAESTRO_RELEASE_BASE_URL%/}"
elif [[ -n "${MAESTRO_INSTALL_VERSION:-}" ]]; then
  release_url="https://github.com/${REPO}/releases/download/v${requested_version}"
else
  # Every channel resolves to an immutable GitHub release tag so that the
  # signed channel manifest and the downloaded artifacts describe one release.
  # An operator may still provide a legacy signed pointer explicitly for a
  # controlled migration.
  release_url=""
fi

install_dir="${MAESTRO_INSTALL_DIR:-$HOME/.local/bin}"
data_dir="${MAESTRO_DATA_DIR:-${XDG_DATA_HOME:-$HOME/.local/share}/maestro}"
allow_unsigned="${MAESTRO_ALLOW_UNSIGNED_INSTALL:-0}"
require_signed="${MAESTRO_REQUIRE_SIGNED_INSTALL:-0}"
update_progress="${MAESTRO_UPDATE_PROGRESS:-0}"
case "$allow_unsigned" in
  0|false|no|"") ;;
  1|true|yes) ;;
  *) fail "MAESTRO_ALLOW_UNSIGNED_INSTALL must be 0 or 1" ;;
esac
case "$require_signed" in
  0|false|no|"") ;;
  1|true|yes) ;;
  *) fail "MAESTRO_REQUIRE_SIGNED_INSTALL must be 0 or 1" ;;
esac
case "$update_progress" in
  0|1) ;;
  *) fail "MAESTRO_UPDATE_PROGRESS must be 0 or 1" ;;
esac
if { [[ "$allow_unsigned" == "1" || "$allow_unsigned" == "true" || "$allow_unsigned" == "yes" ]]; } &&
  { [[ "$require_signed" == "1" || "$require_signed" == "true" || "$require_signed" == "yes" ]]; }; then
  fail "MAESTRO_REQUIRE_SIGNED_INSTALL cannot be combined with MAESTRO_ALLOW_UNSIGNED_INSTALL"
fi
mkdir -p "$data_dir" || fail "Could not create Maestro data directory: $data_dir"
data_dir="$(cd "$data_dir" 2>/dev/null && pwd -P)" ||
  fail "Could not resolve Maestro data directory: $data_dir"

hash_file() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{print $1}'
  else
    shasum -a 256 "$1" | awk '{print $1}'
  fi
}

receipt_hash_file() {
  printf 'sha256:%s' "$(hash_file "$1")"
}

curl_to() {
  local destination="$1"
  local url="$2"
  local -a options=(
    --fail
    --silent
    --show-error
    --location
    --max-time 180
    --retry 2
    --retry-delay 2
  )
  case "$url" in
    http://127.0.0.1:*|http://localhost:*) ;;
    *) options+=(--proto '=https' --tlsv1.2) ;;
  esac
  curl "${options[@]}" -o "$destination" "$url"
}

fetch_manifest() {
  local destination="$1"
  local url="$2"
  local status
  local -a options=(
    --silent
    --show-error
    --location
    --max-time 180
    --retry 2
    --retry-delay 2
    --write-out '%{http_code}'
  )
  case "$url" in
    http://127.0.0.1:*|http://localhost:*) ;;
    *) options+=(--proto '=https' --tlsv1.2) ;;
  esac
  if ! status="$(curl "${options[@]}" -o "$destination" "$url")"; then
    rm -f "$destination"
    fail "Checksum manifest request failed: $url"
  fi
  case "$status" in
    2??) return 0 ;;
    404)
      rm -f "$destination"
      return 1
      ;;
    *)
      rm -f "$destination"
      fail "Checksum manifest request returned HTTP $status: $url"
      ;;
  esac
}

fetch_channel_manifest() {
  local destination="$1"
  local url="$2"
  local recovery_mode="${3:-fail}"
  local status
  local -a options=(
    --silent
    --show-error
    --location
    --max-time 180
    --retry 2
    --retry-delay 2
    --write-out '%{http_code}'
  )
  case "$url" in
    http://127.0.0.1:*|http://localhost:*) ;;
    *) options+=(--proto '=https' --tlsv1.2) ;;
  esac
  if ! status="$(curl "${options[@]}" -o "$destination" "$url")"; then
    rm -f "$destination"
    if [[ "$recovery_mode" == fallback ]]; then
      printf 'Warning: stable GitHub latest manifest request failed; trying the Releases API.\n' >&2
      return 1
    fi
    fail "Channel manifest request failed: $url"
  fi
  case "$status" in
    2??) return 0 ;;
    404)
      rm -f "$destination"
      return 2
      ;;
    *)
      rm -f "$destination"
      if [[ "$recovery_mode" == fallback ]]; then
        printf 'Warning: stable GitHub latest manifest returned HTTP %s; trying the Releases API.\n' "$status" >&2
        return 1
      fi
      fail "Channel manifest request returned HTTP $status: $url"
      ;;
  esac
}

fetch_optional() {
  local destination="$1"
  local url="$2"
  local status
  local -a options=(
    --silent
    --show-error
    --location
    --max-time 180
    --retry 2
    --retry-delay 2
    --write-out '%{http_code}'
  )
  case "$url" in
    http://127.0.0.1:*|http://localhost:*) ;;
    *) options+=(--proto '=https' --tlsv1.2) ;;
  esac
  if ! status="$(curl "${options[@]}" -o "$destination" "$url")"; then
    rm -f "$destination"
    return 1
  fi
  case "$status" in
    2??) return 0 ;;
    *)
      rm -f "$destination"
      return 1
      ;;
  esac
}

json_tool() {
  local mode="$1"
  local file="$2"
  local target="${3:-}"
  LC_ALL=C awk -v input_file="$file" -v mode="$mode" -v target="$target" '
    function fail(message) {
      print message > "/dev/stderr"
      exit 2
    }
    function new_node(kind_value, raw_value, id) {
      id = ++node_count
      node_kind[id] = kind_value
      node_raw[id] = raw_value
      return id
    }
    function skip_space(character) {
      while (position <= text_length) {
        character = substr(text, position, 1)
        if (character != " " && character != "\t" && character != "\r" && character != "\n") return
        position++
      }
    }
    function parse_string(start, character) {
      skip_space()
      if (substr(text, position, 1) != "\"") fail("expected JSON string")
      start = position++
      while (position <= text_length) {
        character = substr(text, position, 1)
        if (character == "\"") {
          position++
          return new_node("string", substr(text, start, position - start))
        }
        if (character == "\\") {
          position++
          if (position > text_length) fail("unterminated JSON escape")
          if (substr(text, position, 1) == "u") position += 5
          else position++
        } else {
          position++
        }
      }
      fail("unterminated JSON string")
    }
    function parse_number(start, character) {
      start = position
      while (position <= text_length) {
        character = substr(text, position, 1)
        if (character == " " || character == "\t" || character == "\r" || character == "\n" ||
            character == "," || character == "]" || character == "}") break
        position++
      }
      return new_node("number", substr(text, start, position - start))
    }
    function parse_literal(start, character) {
      start = position
      while (position <= text_length) {
        character = substr(text, position, 1)
        if (character == " " || character == "\t" || character == "\r" || character == "\n" ||
            character == "," || character == "]" || character == "}") break
        position++
      }
      return new_node("literal", substr(text, start, position - start))
    }
    function parse_value(character) {
      skip_space()
      character = substr(text, position, 1)
      if (character == "{") return parse_object()
      if (character == "[") return parse_array()
      if (character == "\"") return parse_string()
      if (character == "t" || character == "f" || character == "n") return parse_literal()
      return parse_number()
    }
    function parse_object(object_id, key_id, value_id, character) {
      position++
      object_id = new_node("object", "")
      skip_space()
      if (substr(text, position, 1) == "}") {
        position++
        return object_id
      }
      while (1) {
        key_id = parse_string()
        skip_space()
        if (substr(text, position, 1) != ":") fail("expected JSON object colon")
        position++
        value_id = parse_value()
        object_count[object_id]++
        object_key[object_id, object_count[object_id]] = node_raw[key_id]
        object_child[object_id, object_count[object_id]] = value_id
        skip_space()
        character = substr(text, position, 1)
        if (character == "}") {
          position++
          return object_id
        }
        if (character != ",") fail("expected JSON object separator")
        position++
        skip_space()
      }
    }
    function parse_array(array_id, value_id, character) {
      position++
      array_id = new_node("array", "")
      skip_space()
      if (substr(text, position, 1) == "]") {
        position++
        return array_id
      }
      while (1) {
        value_id = parse_value()
        array_count[array_id]++
        array_child[array_id, array_count[array_id]] = value_id
        skip_space()
        character = substr(text, position, 1)
        if (character == "]") {
          position++
          return array_id
        }
        if (character != ",") fail("expected JSON array separator")
        position++
        skip_space()
      }
    }
    function key_text(raw) {
      return substr(raw, 2, length(raw) - 2)
    }
    function string_text(node_id) {
      return substr(node_raw[node_id], 2, length(node_raw[node_id]) - 2)
    }
    function value_text(node_id) {
      if (node_kind[node_id] == "string") return string_text(node_id)
      if (node_kind[node_id] == "literal" && node_raw[node_id] == "null") return ""
      return node_raw[node_id]
    }
    function object_field(object_id, wanted, i) {
      if (node_kind[object_id] != "object") return 0
      for (i = 1; i <= object_count[object_id]; i++) {
        if (key_text(object_key[object_id, i]) == wanted) return object_child[object_id, i]
      }
      return 0
    }
    function canonical(node_id, i, order_index, swap, left, right, result, child, emitted) {
      if (node_kind[node_id] == "string" || node_kind[node_id] == "number" || node_kind[node_id] == "literal") {
        return node_raw[node_id]
      }
      if (node_kind[node_id] == "array") {
        result = "["
        for (i = 1; i <= array_count[node_id]; i++) {
          if (i > 1) result = result ","
          result = result canonical(array_child[node_id, i])
        }
        return result "]"
      }
      for (i = 1; i <= object_count[node_id]; i++) order[node_id, i] = i
      for (i = 1; i <= object_count[node_id]; i++) {
        for (order_index = i + 1; order_index <= object_count[node_id]; order_index++) {
          left = order[node_id, i]
          right = order[node_id, order_index]
          if (key_text(object_key[node_id, left]) > key_text(object_key[node_id, right])) {
            swap = order[node_id, i]
            order[node_id, i] = order[node_id, order_index]
            order[node_id, order_index] = swap
          }
        }
      }
      result = "{"
      emitted = 0
      for (i = 1; i <= object_count[node_id]; i++) {
        child = order[node_id, i]
        if (canonical_root == node_id && key_text(object_key[node_id, child]) == "signature") continue
        if (emitted++ > 0) result = result ","
        result = result object_key[node_id, child] ":" canonical(object_child[node_id, child])
      }
      return result "}"
    }
    function fast_string_field(object, wanted, start, end, character) {
      start = index(object, "\"" wanted "\"")
      if (!start) return ""
      start += length(wanted) + 2
      while (start <= length(object) && substr(object, start, 1) ~ /[ \t\r\n:]/) start++
      if (substr(object, start, 1) != "\"") return ""
      start++
      end = start
      while (end <= length(object)) {
        character = substr(object, end, 1)
        if (character == "\"") return substr(object, start, end - start)
        if (character == "\\") end++
        end++
      }
      return ""
    }
    function fast_literal_field(object, wanted, start, end, character) {
      start = index(object, "\"" wanted "\"")
      if (!start) return ""
      start += length(wanted) + 2
      while (start <= length(object) && substr(object, start, 1) ~ /[ \t\r\n:]/) start++
      end = start
      while (end <= length(object)) {
        character = substr(object, end, 1)
        if (character == "," || character == "}" || character ~ /[ \t\r\n]/) break
        end++
      }
      return substr(object, start, end - start)
    }
    function fast_version_valid(version, channel) {
      if (channel == "stable") return version ~ /^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$/
      if (channel == "beta") return version ~ /^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)-beta\.[1-9][0-9]*$/
      if (channel == "alpha") return version ~ /^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)-alpha\.[1-9][0-9]*$/
      return 0
    }
    function fast_better(candidate, current, a, b, i) {
      gsub(/-/, ".", candidate)
      gsub(/-/, ".", current)
      split(candidate, a, ".")
      split(current, b, ".")
      for (i = 1; i <= 4; i++) {
        if ((a[i] + 0) != (b[i] + 0)) return (a[i] + 0) > (b[i] + 0)
      }
      return 0
    }
    function fast_release(object, channel, draft, prerelease, tag, normalized) {
      draft = fast_literal_field(object, "draft")
      prerelease = fast_literal_field(object, "prerelease")
      if (draft == "true") return
      if (channel == "stable" && prerelease == "true") return
      if (channel != "stable" && prerelease != "true") return
      if (!match(object, /"name"[ \t\r\n]*:[ \t\r\n]*"channel-manifest\.json"/)) return
      tag = fast_string_field(object, "tag_name")
      normalized = tag
      sub(/^v/, "", normalized)
      if (!fast_version_valid(normalized, channel)) return
      if (fast_best == "" || fast_better(normalized, fast_best)) {
        fast_best = normalized
        fast_best_tag = tag
      }
    }
    function fast_scan(input, channel, want_tag, i, n, character, escaped, in_string, depth, start, object, count) {
      n = length(input)
      for (i = 1; i <= n; i++) {
        character = substr(input, i, 1)
        if (in_string) {
          if (escaped) escaped = 0
          else if (character == "\\") escaped = 1
          else if (character == "\"") in_string = 0
          continue
        }
        if (character == "\"") {
          in_string = 1
          continue
        }
        if (character == "[") {
          depth++
          continue
        }
        if (character == "{") {
          if (depth == 1) start = i
          depth++
          continue
        }
        if (character == "}") {
          depth--
          if (depth == 1 && start) {
            object = substr(input, start, i - start + 1)
            count++
            fast_release(object, channel)
            start = 0
          }
          continue
        }
        if (character == "]") depth--
      }
      if (want_tag) printf "%s", fast_best_tag
      else print count
    }
    BEGIN {
      if (mode == "release-tag" || mode == "length") {
        while ((getline line < input_file) > 0) fast_input = fast_input line "\n"
        close(input_file)
        fast_scan(fast_input, target, mode == "release-tag")
        exit
      }
      while ((getline line < input_file) > 0) text = text line "\n"
      close(input_file)
      text_length = length(text)
      position = 1
      root = parse_value()
      canonical_root = root
      skip_space()
      if (position <= text_length) fail("trailing JSON data")
      if (mode == "field") {
        field_id = object_field(root, target)
        if (!field_id) exit 1
        printf "%s", value_text(field_id)
      } else if (mode == "canonical") {
        printf "%s", canonical(root)
      }
    }
  '
}

json_field() {
  local file="$1"
  local field="$2"
  json_tool field "$file" "$field"
}

base64_decode() {
  local value="$1"
  local destination="$2"
  if printf '%s' "$value" | base64 --decode > "$destination" 2>/dev/null; then
    return 0
  fi
  printf '%s' "$value" | base64 -D > "$destination" 2>/dev/null
}

base64_encode_file() {
  base64 < "$1" | tr -d '\r\n'
}

validate_channel_manifest() {
  local file="$1"
  local expected_channel="$2"
  local expected_key_id
  local expected_public_key
  local schema_version
  local manifest_channel
  local key_id
  local version
  local release_tag
  local release_url
  local metadata_url
  local source_sha
  local metadata_sha
  local signature_text
  local payload="$tmpdir/channel-manifest.payload"
  local signature="$tmpdir/channel-manifest.signature"
  local public_key="$tmpdir/channel-manifest.public-key"
  local bypass_signature=0
  case "$expected_channel" in
    stable)
      expected_key_id="$STABLE_CHANNEL_KEY_ID"
      expected_public_key="$STABLE_CHANNEL_PUBLIC_KEY"
      ;;
    beta|alpha)
      expected_key_id="$PRERELEASE_CHANNEL_KEY_ID"
      expected_public_key="$PRERELEASE_CHANNEL_PUBLIC_KEY"
      ;;
    *) return 1 ;;
  esac

  schema_version="$(json_field "$file" schemaVersion)" || return 1
  manifest_channel="$(json_field "$file" channel)" || return 1
  key_id="$(json_field "$file" keyId)" || return 1
  version="$(json_field "$file" version)" || return 1
  release_tag="$(json_field "$file" releaseTag)" || return 1
  release_url="$(json_field "$file" releaseUrl)" || return 1
  metadata_url="$(json_field "$file" metadataUrl)" || return 1
  source_sha="$(json_field "$file" sourceSha)" || return 1
  metadata_sha="$(json_field "$file" metadataSha256)" || return 1
  signature_text="$(json_field "$file" signature)" || return 1

  [[ "$schema_version" == "evalops.maestro.release-channel.v1" ]] || {
    printf 'Channel manifest: unsupported release channel manifest schema\n' >&2
    return 1
  }
  [[ "$manifest_channel" == "$expected_channel" ]] || {
    printf 'Channel manifest: channel does not match requested %s\n' "$expected_channel" >&2
    return 1
  }
  [[ "$key_id" == "$expected_key_id" ]] || {
    printf 'Channel manifest: key ID does not match the requested channel\n' >&2
    return 1
  }
  channel_version_matches "$version" "$expected_channel" || {
    printf 'Channel manifest: %s channel requires a matching prerelease version\n' "$expected_channel" >&2
    return 1
  }
  [[ "$release_tag" == "v$version" ]] || {
    printf 'Channel manifest: release tag does not match its version\n' >&2
    return 1
  }
  case "$release_url" in
    "https://github.com/${REPO}/releases/download/v${version}") ;;
    http://127.0.0.1:*|http://localhost:*) ;;
    *)
      printf 'Channel manifest: release URL is not the requested GitHub release\n' >&2
      return 1
      ;;
  esac
  if [[ -n "$metadata_url" ]]; then
    case "$metadata_url" in
      https://*|http://127.0.0.1:*|http://localhost:*) ;;
      *)
        printf 'Channel manifest: metadata URL must use HTTPS\n' >&2
        return 1
        ;;
    esac
  fi
  [[ "$source_sha" =~ ^[0-9a-fA-F]{40}$ ]] || {
    printf 'Channel manifest: source SHA is invalid\n' >&2
    return 1
  }
  if [[ -n "$metadata_sha" ]] && [[ ! "$metadata_sha" =~ ^sha256:[0-9a-fA-F]{64}$ ]]; then
    printf 'Channel manifest: metadata digest is invalid\n' >&2
    return 1
  fi
  [[ -n "$signature_text" ]] || {
    printf 'Channel manifest: signature is missing or invalid\n' >&2
    return 1
  }
  case "$allow_unsigned" in
    1|true|yes) bypass_signature=1 ;;
  esac
  if [[ "$bypass_signature" == "1" ]]; then
    printf 'Warning: channel manifest signature verification was explicitly bypassed.\n' >&2
    return 0
  fi
  [[ "$signature_text" =~ ^[A-Za-z0-9+/]+={0,2}$ ]] || {
    printf 'Channel manifest: signature encoding is invalid\n' >&2
    return 1
  }
  json_tool canonical "$file" > "$payload" || return 1
  base64_decode "$signature_text" "$tmpdir/channel-manifest.signature.raw" || {
    printf 'Channel manifest: signature encoding is invalid\n' >&2
    return 1
  }
  printf '%s' "$signature_text" > "$signature"
  base64_decode "MCowBQYDK2VwAyEA$expected_public_key" "$tmpdir/channel-manifest.public-key.der" || return 1
  {
    printf '%s\n' '-----BEGIN PUBLIC KEY-----'
    base64_encode_file "$tmpdir/channel-manifest.public-key.der"
    printf '\n%s\n' '-----END PUBLIC KEY-----'
  } > "$public_key"
  [[ "$(wc -c < "$tmpdir/channel-manifest.signature.raw")" -eq 64 &&
    "$(wc -c < "$tmpdir/channel-manifest.public-key.der")" -eq 44 ]] || {
    printf 'Channel manifest: signature or public key has an invalid length\n' >&2
    return 1
  }
  bootstrap_cosign || return 1
  "$cosign_path" verify-blob \
    --insecure-ignore-tlog \
    --signature-digest-algorithm sha512 \
    --key "$public_key" \
    --signature "$signature" \
    "$payload" >/dev/null 2>&1
}


fetch_github_releases() {
  local destination="$1"
  local api="$2"
  local page=1
  local separator
  local url
  local page_file
  local page_count
  : > "$destination"
  while (( page <= 10 )); do
    if [[ "$api" == *\?* ]]; then
      separator='&'
    else
      separator='?'
    fi
    url="${api}${separator}per_page=100&page=${page}"
    page_file="$tmpdir/github-releases-page-$page.json"
    fetch_optional "$page_file" "$url" || return 1
    json_tool length "$page_file" >/dev/null || return 1
    printf '%s\n' "$page_file" >> "$destination"
    page_count="$(json_tool length "$page_file")"
    if (( page_count < 100 )); then
      return 0
    fi
    page=$((page + 1))
  done
  return 0
}

channel_version_greater() {
  local candidate="${1#v}"
  local current="${2#v}"
  LC_ALL=C awk -v candidate="$candidate" -v current="$current" '
    BEGIN {
      gsub(/-/, ".", candidate)
      gsub(/-/, ".", current)
      split(candidate, a, ".")
      split(current, b, ".")
      for (i = 1; i <= 4; i++) {
        if ((a[i] + 0) != (b[i] + 0)) exit !((a[i] + 0) > (b[i] + 0))
      }
      exit 1
    }
  '
}

latest_channel_tag() {
  local source="$1"
  local channel="$2"
  local page_file
  local candidate
  local best=""
  while IFS= read -r page_file; do
    [[ -n "$page_file" ]] || continue
    candidate="$(json_tool release-tag "$page_file" "$channel" || true)"
    [[ -n "$candidate" ]] || continue
    if [[ -z "$best" ]] || channel_version_greater "$candidate" "$best"; then
      best="$candidate"
    fi
  done < "$source"
  printf '%s' "$best"
}

release_url_allowed() {
  case "$1" in
    https://*) return 0 ;;
    http://127.0.0.1:*|http://localhost:*) return 0 ;;
    *) return 1 ;;
  esac
}

resolve_channel_release_url() {
  local channel="$1"
  local pointer="${MAESTRO_CHANNEL_MANIFEST_URL:-}"
  local pointer_base="${MAESTRO_CHANNEL_POINTER_BASE:-}"
  local api="${MAESTRO_RELEASE_API_URL:-https://api.github.com/repos/${REPO}/releases}"
  local stable_latest_manifest_url="${MAESTRO_STABLE_LATEST_MANIFEST_URL:-}"
  local dest tag url

  if [[ -z "$pointer" && -n "$pointer_base" ]]; then
    pointer="${pointer_base%/}/channels/${channel}/manifest.json"
  fi
  dest="$tmpdir/channel-manifest.json"
  if [[ -n "$pointer" ]] && fetch_optional "$dest" "$pointer"; then
    if validate_channel_manifest "$dest" "$channel"; then
      url="$(json_field "$dest" releaseUrl || true)"
      url="${url%/}"
      if release_url_allowed "$url"; then
        printf 'Using %s channel pointer %s\n' "$channel" "$pointer" >&2
        : > "$tmpdir/channel-manifest-verified"
        printf '%s' "$url"
        return 0
      fi
    fi
    printf 'Warning: ignoring invalid %s channel pointer %s; trying GitHub Releases.\n' \
      "$channel" "$pointer" >&2
    rm -f "$dest"
  fi

  rm -f "$tmpdir/channel-manifest-verified"
  # Stable releases expose a signed manifest through GitHub's latest-download
  # redirect. Use that path before the unauthenticated Releases API so normal
  # standalone installs do not depend on the 60-request-per-hour REST quota.
  # An explicit API override remains authoritative for controlled mirrors;
  # MAESTRO_STABLE_LATEST_MANIFEST_URL is a matching explicit latest endpoint
  # override for those environments and for the installer fixture.
  if [[ "$channel" == stable &&
    ( -z "${MAESTRO_RELEASE_API_URL:-}" || -n "$stable_latest_manifest_url" ) ]]; then
    stable_latest_manifest_url="${stable_latest_manifest_url:-https://github.com/${REPO}/releases/latest/download/channel-manifest.json}"
    if fetch_channel_manifest "$dest" "$stable_latest_manifest_url" fallback; then
      if validate_channel_manifest "$dest" "$channel"; then
        url="$(json_field "$dest" releaseUrl || true)"
        url="${url%/}"
        if release_url_allowed "$url"; then
          tag="$(json_field "$dest" releaseTag || true)"
          printf 'Using stable GitHub latest release %s\n' "$tag" >&2
          : > "$tmpdir/channel-manifest-verified"
          printf '%s' "$url"
          return 0
        fi
      fi
      printf 'Warning: ignoring invalid stable GitHub latest manifest %s; trying the Releases API.\n' \
        "$stable_latest_manifest_url" >&2
      rm -f "$dest"
    fi
  fi

  dest="$tmpdir/github-releases.json"
  if ! fetch_github_releases "$dest" "$api"; then
    fail "No published $channel release pointer at $pointer, and GitHub release listing failed: $api"
  fi
  tag="$(latest_channel_tag "$dest" "$channel" || true)"
  if [[ -z "$tag" ]]; then
    fail "No published $channel release. Omit MAESTRO_INSTALL_CHANNEL for stable, or set MAESTRO_INSTALL_VERSION to a published tag."
  fi
  printf 'Using GitHub %s release %s\n' "$channel" "$tag" >&2
  printf '%s/%s' "${MAESTRO_RELEASE_DOWNLOAD_BASE:-https://github.com/${REPO}/releases/download}" "$tag"
}

download() {
  local url="$1"
  local destination="$2"
  local label="$3"
  if [[ "$update_progress" != "1" ]]; then
    printf 'Downloading %s...\n' "$label" >&2
  fi
  curl_to "$destination" "$url" ||
    fail "Download failed: $url"
}

progress_step() {
  [[ "$update_progress" == "1" ]] || return 0
  printf '[%s/3] %s\n' "$1" "$2" >&2
}

bootstrap_cosign() {
  [[ -x "$cosign_path" ]] && return 0
  download \
    "https://github.com/sigstore/cosign/releases/download/v$COSIGN_VERSION/$cosign_asset" \
    "$cosign_path" \
    "Cosign $COSIGN_VERSION"
  actual_cosign_sha256="$(hash_file "$cosign_path")"
  [[ "$actual_cosign_sha256" == "$cosign_sha256" ]] ||
    fail "Cosign bootstrap checksum mismatch"
  chmod 755 "$cosign_path"
}

verify_manifest_checksum() {
  local manifest="$1"
  local file="$2"
  local name="$3"
  local expected
  expected="$(awk -v name="$name" '$2 == name { value=$1; count++ } END { if (count != 1) exit 1; print value }' "$manifest")" ||
    fail "Checksum manifest does not contain exactly one entry for $name"
  local actual
  actual="$(hash_file "$file")"
  [[ "$actual" == "$expected" ]] ||
    fail "Checksum mismatch for $name"
}

verify_blob_signature() {
  local cosign="$1"
  local subject="$2"
  local bundle="$3"
  "$cosign" verify-blob \
    --bundle "$bundle" \
    --certificate-identity-regexp "$COSIGN_IDENTITY_REGEXP" \
    --certificate-oidc-issuer "$COSIGN_OIDC_ISSUER" \
    "$subject" >/dev/null ||
    fail "Signature verification failed for $(basename "$subject")"
}

shell_quote() {
  printf '%q' "$1"
}

tmpdir="$(mktemp -d 2>/dev/null || mktemp -d -t maestro-install)"
cosign_path="$tmpdir/cosign"
stage=""
launcher_stage=""
canonical_launcher_stage=""
cleanup() {
  rm -rf "$tmpdir"
  if [[ -n "$stage" ]]; then
    rm -rf "$stage"
  fi
  if [[ -n "$launcher_stage" ]]; then
    rm -f "$launcher_stage"
  fi
  if [[ -n "$canonical_launcher_stage" ]]; then
    rm -f "$canonical_launcher_stage"
  fi
}
trap cleanup EXIT

if [[ -z "$release_url" ]]; then
  release_url="$(resolve_channel_release_url "$install_channel")"
fi
progress_step 1 "Downloading version ${requested_version:-latest}..."

channel_manifest_verified=0
channel_manifest_version=""
channel_manifest="$tmpdir/channel-manifest.json"
if [[ -f "$tmpdir/channel-manifest-verified" ]]; then
  channel_manifest_verified=1
elif fetch_channel_manifest "$channel_manifest" "${release_url}/channel-manifest.json"; then
  if [[ "$update_progress" != "1" ]]; then
    printf 'Downloading channel manifest...\n' >&2
  fi
  validate_channel_manifest "$channel_manifest" "$install_channel" ||
    fail "Channel manifest verification failed for $install_channel"
  channel_manifest_verified=1
else
  manifest_status="$?"
  if [[ "$manifest_status" -ne 2 || -z "$requested_version" ]]; then
    fail "Channel manifest is required for an unpinned $install_channel installation"
  fi
  case "$require_signed" in
    1|true|yes)
      fail "Pinned release has no channel manifest; refusing unsigned installation"
      ;;
  esac
  printf 'Warning: pinned release has no channel manifest; using legacy artifact verification.\n' >&2
fi
if [[ "$channel_manifest_verified" == "1" ]]; then
  channel_manifest_version="$(json_field "$channel_manifest" version || true)"
  channel_manifest_release_url="$(json_field "$channel_manifest" releaseUrl || true)"
  channel_manifest_release_url="${channel_manifest_release_url%/}"
  release_url_normalized="${release_url%/}"
  [[ -n "$channel_manifest_version" && "$channel_manifest_release_url" == "$release_url_normalized" ]] ||
    fail "Channel manifest does not describe the selected $install_channel release"
fi

manifest="$tmpdir/SHA256SUMS"
manifest_available=0
manifest_sha256=""
signature_verified=0
metadata_checksum_verified=0
metadata_available=0
binary_checksum_verified=0
if fetch_manifest "$manifest" "${release_url}/SHA256SUMS"; then
  manifest_available=1
  manifest_sha256="$(receipt_hash_file "$manifest")"
else
  if [[ "$require_signed" == "1" || "$require_signed" == "true" || "$require_signed" == "yes" ]]; then
    fail "Release has no SHA256SUMS manifest; refusing unsigned installation"
  fi
  printf 'Warning: release has no signed checksum manifest; installing in legacy unsigned mode.\n' >&2
fi

if [[ "$manifest_available" == "1" && "$allow_unsigned" != "1" && "$allow_unsigned" != "true" && "$allow_unsigned" != "yes" ]]; then
  bootstrap_cosign
  download "${release_url}/SHA256SUMS.cosign.bundle" \
    "$tmpdir/SHA256SUMS.cosign.bundle" "SHA256SUMS signature"
  download "${release_url}/${asset}.cosign.bundle" \
    "$tmpdir/${asset}.cosign.bundle" "${asset} signature"
  verify_blob_signature "$cosign_path" "$manifest" "$tmpdir/SHA256SUMS.cosign.bundle"
  signature_verified=1
else
  if [[ "$manifest_available" == "1" ]]; then
    printf 'Warning: MAESTRO_ALLOW_UNSIGNED_INSTALL is enabled; skipping Cosign signature verification.\n' >&2
  fi
fi

metadata_manifest_entry=0
if [[ "$manifest_available" == "1" ]] &&
  awk -v name="$metadata_asset" '$2 == name { found=1 } END { exit !found }' "$manifest"; then
  metadata_manifest_entry=1
fi
if [[ "$metadata_manifest_entry" == "1" ]]; then
  download "${release_url}/${metadata_asset}" "$tmpdir/$metadata_asset" "$metadata_asset"
  verify_manifest_checksum "$manifest" "$tmpdir/$metadata_asset" "$metadata_asset"
  metadata_checksum_verified=1
  metadata_available=1
fi

if [[ "$manifest_available" == "1" && "$metadata_manifest_entry" == "0" ]]; then
  case "$require_signed" in
    1|true|yes)
      printf 'Warning: signed release has no %s; continuing with artifact verification and omitting optional release metadata.\n' "$metadata_asset" >&2
      ;;
  esac
fi

download "${release_url}/${asset}" "$tmpdir/$asset" "$asset"
progress_step 2 "Verifying checksum..."
if [[ "$manifest_available" == "1" ]]; then
  verify_manifest_checksum "$manifest" "$tmpdir/$asset" "$asset"
  binary_checksum_verified=1
  if [[ "$allow_unsigned" == "1" || "$allow_unsigned" == "true" || "$allow_unsigned" == "yes" ]]; then
    printf 'Checksum manifest verified; signature verification was explicitly bypassed.\n' >&2
  else
    verify_blob_signature "$cosign_path" "$tmpdir/$asset" "$tmpdir/$asset.cosign.bundle"
  fi
fi

progress_step 3 "Installing update..."
chmod 755 "$tmpdir/$asset"

version_output="$("$tmpdir/$asset" --version 2>/dev/null)" ||
  fail "Downloaded Maestro binary could not report its version"
release_version="$(printf '%s\n' "$version_output" | awk 'NF {print $NF; exit}')"
release_version="${release_version#v}"
[[ "$release_version" =~ ^[A-Za-z0-9][A-Za-z0-9._+-]*$ ]] ||
  fail "Invalid release version: $release_version"
if [[ -n "$requested_version" && "$release_version" != "$requested_version" ]]; then
  fail "Downloaded release version $release_version does not match requested version $requested_version"
fi
require_channel_version "$release_version" "$install_channel"
if [[ "$channel_manifest_verified" == "1" && "$channel_manifest_version" != "$release_version" ]]; then
  fail "Channel manifest version $channel_manifest_version does not match downloaded release $release_version"
fi

mkdir -p "$install_dir"
release_root="$data_dir/releases"
mkdir -p "$release_root"
stage="$(mktemp -d "$release_root/.staging.XXXXXX")" ||
  fail "Could not create release staging directory"
mkdir -p "$stage/bin"
cp "$tmpdir/$asset" "$stage/bin/maestro"
chmod 755 "$stage/bin/maestro"

release_version_root="$release_root/$release_version"
mkdir -p "$release_version_root"
staged_version="$("$stage/bin/maestro" --version)" ||
  fail "Staged Maestro binary failed its version check"
release_dir="$(mktemp -d "$release_version_root/${platform}.XXXXXX")" ||
  fail "Could not create release directory"
mv "$stage/bin" "$release_dir/bin"
if [[ "$metadata_available" == "1" ]]; then
  cp "$tmpdir/$metadata_asset" "$release_dir/$metadata_asset"
fi
binary_receipt_sha256="$(receipt_hash_file "$release_dir/bin/maestro")"
metadata_receipt_sha256=""
if [[ "$metadata_available" == "1" ]]; then
  metadata_receipt_sha256="$(receipt_hash_file "$tmpdir/$metadata_asset")"
fi
installed_at_ms="$(( $(date +%s) * 1000 ))"
verified=0
if [[ "$signature_verified" == "1" && "$binary_checksum_verified" == "1" ]]; then
  verified=1
fi
{
  printf '{\n'
  printf '  "schemaVersion": "evalops.maestro.install-receipt.v1",\n'
  printf '  "version": "%s",\n' "$release_version"
  printf '  "platform": "%s",\n' "$platform"
  printf '  "installedAtMs": %s,\n' "$installed_at_ms"
  printf '  "verified": %s,\n' "$([[ "$verified" == "1" ]] && printf true || printf false)"
  printf '  "verification": {\n'
  printf '    "manifestSha256": "%s",\n' "$manifest_sha256"
  printf '    "manifestChecksumVerified": %s,\n' "$([[ "$manifest_available" == "1" ]] && printf true || printf false)"
  printf '    "signatureVerified": %s,\n' "$([[ "$signature_verified" == "1" ]] && printf true || printf false)"
  printf '    "artifactSha256": "%s",\n' "$binary_receipt_sha256"
  printf '    "metadataSha256": '
  if [[ "$metadata_available" == "1" ]]; then
    printf '"%s",\n' "$metadata_receipt_sha256"
  else
    printf 'null,\n'
  fi
  printf '    "metadataChecksumVerified": %s\n' "$([[ "$metadata_checksum_verified" == "1" ]] && printf true || printf false)"
  printf '  },\n'
  printf '  "releaseMetadataAsset": '
  if [[ "$metadata_available" == "1" ]]; then
    printf '"%s"\n' "$metadata_asset"
  else
    printf 'null\n'
  fi
  printf '}\n'
} > "$release_dir/install-receipt.json"
rm -rf "$stage"
stage=""

# Preserve both entrypoints, including source-build launchers and symlinks.
# The release directory is unique, so a later install cannot overwrite this backup.
previous_launchers="$release_dir/previous-launchers"
mkdir -p "$previous_launchers"
for command_name in maestro deixic-code; do
  previous="$install_dir/$command_name"
  if [[ -f "$previous" || -L "$previous" ]]; then
    cp -P "$previous" "$previous_launchers/$command_name"
  elif [[ -e "$previous" ]]; then
    fail "Cannot replace $previous: expected a file or symlink"
  fi
done

launcher_stage="$install_dir/.maestro.install.$$"
release_dir_quoted="$(shell_quote "$release_dir")"
install_dir_quoted="$(shell_quote "$install_dir")"
data_dir_quoted="$(shell_quote "$data_dir")"
release_version_quoted="$(shell_quote "$release_version")"
install_channel_quoted="$(shell_quote "$install_channel")"
{
  printf '%s\n' '#!/usr/bin/env bash' 'set -eu'
  printf 'release_dir=%s\n' "$release_dir_quoted"
	printf 'install_dir=%s\n' "$install_dir_quoted"
	printf 'data_dir=%s\n' "$data_dir_quoted"
	printf 'release_version=%s\n' "$release_version_quoted"
	printf 'install_channel=%s\n' "$install_channel_quoted"
	# These lines are intentionally literal: they are the generated launcher.
	# shellcheck disable=SC2016
	printf '%s\n' \
		'export MAESTRO_INSTALL_METHOD=release' \
		'export MAESTRO_INSTALL_DIR="$install_dir"' \
		'export MAESTRO_DATA_DIR="$data_dir"' \
		'export MAESTRO_UPDATE_CHANNEL="${MAESTRO_UPDATE_CHANNEL:-$install_channel}"' \
		'export MAESTRO_STARTUP_UPDATE_STATE="${MAESTRO_STARTUP_UPDATE_STATE:-$data_dir/startup-update-state.json}"' \
		'export MAESTRO_VERSION="$release_version"'
	# shellcheck disable=SC2016
	printf '%s\n' 'exec "$release_dir/bin/maestro" "$@"'
} > "$launcher_stage"
chmod 755 "$launcher_stage"
mv -f "$launcher_stage" "$install_dir/maestro"
launcher_stage=""

canonical_launcher_stage="$install_dir/.deixic-code.install.$$"
{
  printf '%s\n' '#!/usr/bin/env bash' 'set -eu'
	printf 'install_dir=%s\n' "$install_dir_quoted"
	# The Maestro launcher remains the compatibility owner for update and
	# rollback repointing. Deixic Code is the stable customer entrypoint.
	# shellcheck disable=SC2016
	printf '%s\n' 'exec "$install_dir/maestro" "$@"'
} > "$canonical_launcher_stage"
chmod 755 "$canonical_launcher_stage"
mv -f "$canonical_launcher_stage" "$install_dir/deixic-code"
canonical_launcher_stage=""

for command_name in maestro deixic-code; do
  installed_version="$("$install_dir/$command_name" --version)" ||
    fail "Installed $command_name failed verification; previous launchers are in $previous_launchers"
  [[ "$installed_version" == "$staged_version" ]] ||
    fail "Installed $command_name does not match the staged binary; previous launchers are in $previous_launchers"
done

if [[ "$update_progress" != "1" ]]; then
  printf 'Installed Deixic Code %s to %s\n' "$release_version" "$install_dir/deixic-code" >&2
  printf 'The maestro command remains available as a compatibility alias at %s.\n' "$install_dir/maestro" >&2
  printf 'Release files retained under %s for rollback.\n' "$release_root" >&2
  printf 'Previous launchers retained in %s.\n' "$previous_launchers" >&2
  for command_name in maestro deixic-code; do
    selected="$(command -v "$command_name" || true)"
    if [[ -n "$selected" && "$selected" != "$install_dir/$command_name" ]]; then
      printf 'PATH selects %s. Put %s first on PATH, then restart your shell.\n' "$selected" "$install_dir" >&2
    fi
  done
  if [[ "$platform" == darwin-* ]]; then
    printf 'macOS may request keychain access when switching from a source build. Approve the Deixic Code request to reuse your login.\n' >&2
  fi
  printf 'Run deixic-code to open the TUI; use deixic-code doctor --live to check your login and default model.\n' >&2
  "$install_dir/deixic-code" --version
fi

#!/usr/bin/env bash
# Select the sccache backend and export job-wide compiler-cache env.
# Precedence: GCS on ARC when GCE metadata identity is present AND an
# objects.list probe against the bucket prefix succeeds; else local disk.
# GCP runners never fall through to GitHub's Azure-hosted cache data plane.
# Off-GCP runners use SCCACHE_LOCAL_DIR when it is an absolute writable
# directory (WSL: /var/cache/evalops-sccache, 40G by default). GHA is only
# selected when the caller explicitly permits that fallback.
# Hard-fail only if GCS or local disk was selected and sccache then reports
# a different Cache location.
set -euo pipefail

# Single bucket name; override via the action input / SCCACHE_GCS_BUCKET_NAME.
: "${GITHUB_ENV:?GITHUB_ENV is required}"
: "${GITHUB_WORKSPACE:?GITHUB_WORKSPACE is required}"
SCCACHE_GCS_BUCKET_NAME="${SCCACHE_GCS_BUCKET_NAME:-evalops-prod-ci-sccache}"
SCCACHE_BACKEND="${SCCACHE_BACKEND:-auto}"
SCCACHE_ALLOW_GHA_FALLBACK="${SCCACHE_ALLOW_GHA_FALLBACK:-true}"
SCCACHE_CURL="${SCCACHE_CURL:-curl}"
SCCACHE_BIN="${SCCACHE_BIN:-sccache}"
SCCACHE_METADATA_EMAIL_URL="${SCCACHE_METADATA_EMAIL_URL:-http://169.254.169.254/computeMetadata/v1/instance/service-accounts/default/email}"
SCCACHE_METADATA_TOKEN_URL="${SCCACHE_METADATA_TOKEN_URL:-http://169.254.169.254/computeMetadata/v1/instance/service-accounts/default/token}"

case "${SCCACHE_ALLOW_GHA_FALLBACK}" in
  true | false) ;;
  *)
    echo "::error::setup-sccache allow-gha-fallback must be true or false (got ${SCCACHE_ALLOW_GHA_FALLBACK})" >&2
    exit 1
    ;;
esac

urlencode() {
  # RFC 3986 encode so prefix slashes become %2F in the objects.list query.
  local s="$1"
  local out="" c hex
  local i
  for ((i = 0; i < ${#s}; i++)); do
    c="${s:i:1}"
    case "${c}" in
      [a-zA-Z0-9._~-]) out+="${c}" ;;
      *)
        printf -v hex '%02X' "'${c}"
        out+="%${hex}"
        ;;
    esac
  done
  printf '%s' "${out}"
}

parse_access_token() {
  local json="$1"
  local rest
  rest="${json#*\"access_token\"}"
  if [[ "${rest}" == "${json}" ]]; then
    return 1
  fi
  rest="${rest#*:}"
  rest="${rest#*\"}"
  rest="${rest%%\"*}"
  if [[ -z "${rest}" ]]; then
    return 1
  fi
  printf '%s\n' "${rest}"
}

probe_metadata() {
  local url="$1"
  "${SCCACHE_CURL}" -sf --connect-timeout 2 --max-time 2 \
    -H "Metadata-Flavor: Google" \
    "${url}" || true
}

gce_identity="$(probe_metadata "${SCCACHE_METADATA_EMAIL_URL}")"
gce_identity="${gce_identity//$'\n'/}"

# Persistent self-hosted hosts (the WSL box) export SCCACHE_LOCAL_DIR in the
# runner service environment. Ephemeral ARC pods do not; they keep GCS/GHA.
# Default size cap 40G; override with SCCACHE_CACHE_SIZE on the host.
SCCACHE_LOCAL_DIR="${SCCACHE_LOCAL_DIR:-}"
SCCACHE_CACHE_SIZE="${SCCACHE_CACHE_SIZE:-40G}"

local_disk_ready() {
  local dir="${SCCACHE_LOCAL_DIR}"
  if [[ -z "${dir}" ]]; then
    return 1
  fi
  if [[ "${dir}" != /* ]]; then
    echo "::warning::SCCACHE_LOCAL_DIR must be an absolute path (got ${dir}); ignoring"
    return 1
  fi
  if [[ ! -d "${dir}" ]]; then
    if ! mkdir -p "${dir}"; then
      echo "::warning::SCCACHE_LOCAL_DIR ${dir} could not be created; ignoring"
      return 1
    fi
  fi
  if [[ ! -w "${dir}" ]]; then
    echo "::warning::SCCACHE_LOCAL_DIR ${dir} is not writable; ignoring"
    return 1
  fi
  return 0
}

select_local_or_gha() {
  if local_disk_ready; then
    SCCACHE_BACKEND="local"
  elif [[ "${SCCACHE_ALLOW_GHA_FALLBACK}" == "true" ]]; then
    SCCACHE_BACKEND="gha"
  else
    SCCACHE_LOCAL_DIR="${RUNNER_TEMP:?RUNNER_TEMP is required}/sccache-no-gha-fallback"
    if ! local_disk_ready; then
      echo "::error::sccache ephemeral local fallback ${SCCACHE_LOCAL_DIR} is unavailable; refusing to use GitHub Actions cache" >&2
      exit 1
    fi
    SCCACHE_BACKEND="local"
  fi
}

case "${SCCACHE_BACKEND}" in
  auto)
    if [[ -n "${gce_identity}" ]]; then
      SCCACHE_BACKEND="gcs"
    else
      select_local_or_gha
    fi
    ;;
  gcs | gha | local) ;;
  *)
    echo "::error::setup-sccache backend must be auto, gcs, local, or gha (got ${SCCACHE_BACKEND})" >&2
    exit 1
    ;;
esac

if [[ "${SCCACHE_BACKEND}" == "gha" && "${SCCACHE_ALLOW_GHA_FALLBACK}" != "true" ]]; then
  echo "::error::sccache GitHub Actions cache backend is disabled for this caller" >&2
  exit 1
fi

echo "sccache backend=${SCCACHE_BACKEND} gce_identity=${gce_identity:-none} local_dir=${SCCACHE_LOCAL_DIR:-none}"

cache_source_root="${GITHUB_WORKSPACE}"
if [[ -n "${GITHUB_ACTION_PATH:-}" ]]; then
  action_source_root="$(cd "${GITHUB_ACTION_PATH}/../../.." && pwd)"
  if [[ -f "${action_source_root}/rust-toolchain.toml" || -f "${action_source_root}/mise.toml" ]]; then
    cache_source_root="${action_source_root}"
  fi
fi
rust_toolchain=""
if [[ -f "${cache_source_root}/rust-toolchain.toml" ]]; then
  rust_toolchain="$(
    awk -F'"' '/^channel = / { print $2; exit }' "${cache_source_root}/rust-toolchain.toml"
  )"
fi
if [[ -z "${rust_toolchain}" && -f "${cache_source_root}/mise.toml" ]]; then
  rust_toolchain="$(
    awk -F'"' '/^rust = / { print $2; exit }' "${cache_source_root}/mise.toml"
  )"
fi
if [[ -z "${rust_toolchain}" ]]; then
  echo "::error::unable to resolve rust toolchain from rust-toolchain.toml or mise.toml" >&2
  exit 1
fi

runner_os="${RUNNER_OS:?RUNNER_OS is required}"
gcs_key_prefix="sccache/${runner_os}/${rust_toolchain}/"
encoded_prefix="$(urlencode "${gcs_key_prefix}")"
# objects.list, not buckets.get: the runner GSA is roles/storage.objectUser,
# which grants storage.objects.* and not storage.buckets.get.
: "${SCCACHE_GCS_BUCKET_PROBE_URL:=https://storage.googleapis.com/storage/v1/b/${SCCACHE_GCS_BUCKET_NAME}/o?maxResults=1&prefix=${encoded_prefix}&fields=kind}"

if [[ "${SCCACHE_BACKEND}" == "gcs" ]]; then
  bucket_reason=""
  if [[ -z "${gce_identity}" ]]; then
    bucket_reason="GCE metadata identity is unavailable"
  else
    token_json="$(probe_metadata "${SCCACHE_METADATA_TOKEN_URL}")"
    access_token=""
    if access_token="$(parse_access_token "${token_json}")"; then
      :
    else
      access_token=""
    fi
    if [[ -z "${access_token}" ]]; then
      bucket_reason="metadata-server token is unavailable"
    else
      echo "sccache GCS probe ${SCCACHE_GCS_BUCKET_PROBE_URL}"
      probe_body="$(mktemp)"
      http_code=""
      set +e
      http_code="$("${SCCACHE_CURL}" -sS --connect-timeout 2 --max-time 5 \
        -o "${probe_body}" -w '%{http_code}' \
        -H "Authorization: Bearer ${access_token}" \
        -H "Accept: application/json" \
        "${SCCACHE_GCS_BUCKET_PROBE_URL}")"
      curl_status=$?
      set -e
      if [[ "${curl_status}" -ne 0 || -z "${http_code}" ]]; then
        http_code="000"
      fi
      rm -f "${probe_body}"
      case "${http_code}" in
        200) ;;
        404) bucket_reason="HTTP 404 (bucket missing)" ;;
        403) bucket_reason="HTTP 403 (permission denied)" ;;
        000) bucket_reason="HTTP 000 (probe transport failed)" ;;
        *) bucket_reason="HTTP ${http_code}" ;;
      esac
    fi
  fi
  if [[ -n "${bucket_reason}" ]]; then
    if [[ -n "${gce_identity}" && -z "${SCCACHE_LOCAL_DIR}" ]]; then
      SCCACHE_LOCAL_DIR="${RUNNER_TEMP}/sccache-gcp-fallback"
    fi
    select_local_or_gha
    if [[ "${SCCACHE_BACKEND}" == "local" ]]; then
      echo "::warning::sccache GCS bucket ${SCCACHE_GCS_BUCKET_NAME} probe failed: ${bucket_reason}; falling back to local disk instead of GitHub/Azure"
    else
      echo "::warning::sccache GCS bucket ${SCCACHE_GCS_BUCKET_NAME} probe failed: ${bucket_reason}; falling back to the GitHub Actions cache backend"
    fi
  fi
fi

if [[ "${SCCACHE_BACKEND}" == "local" ]]; then
  if ! local_disk_ready; then
    if [[ -n "${gce_identity}" ]]; then
      SCCACHE_LOCAL_DIR="${RUNNER_TEMP}/sccache-gcp-fallback"
      if ! local_disk_ready; then
        echo "::error::sccache GCP local fallback ${SCCACHE_LOCAL_DIR} is unavailable; refusing to use GitHub/Azure" >&2
        exit 1
      fi
      echo "::warning::sccache local disk cache is not configured; using ephemeral local disk instead of GitHub/Azure"
    else
      select_local_or_gha
      if [[ "${SCCACHE_BACKEND}" == "local" ]]; then
        echo "::warning::sccache persistent local disk cache is not configured; using ephemeral local disk instead of GitHub Actions cache"
      else
        echo "::warning::sccache local disk cache is not configured (set SCCACHE_LOCAL_DIR to an absolute writable path on a persistent host); falling back to the GitHub Actions cache backend"
      fi
    fi
  fi
fi

echo "sccache selected backend=${SCCACHE_BACKEND}"

# Self-hosted machines run several Actions runners against one network
# namespace. Give each job its own Unix socket so concurrent sccache servers
# cannot contend for the default 127.0.0.1:4226 endpoint.
server_key="$(
  printf '%s\n' \
    "${GITHUB_RUN_ID:?GITHUB_RUN_ID is required}" \
    "${GITHUB_RUN_ATTEMPT:?GITHUB_RUN_ATTEMPT is required}" \
    "${GITHUB_JOB:?GITHUB_JOB is required}" \
    "${RUNNER_NAME:?RUNNER_NAME is required}" \
    | cksum | awk '{print $1}'
)"
SCCACHE_SERVER_UDS="${RUNNER_TEMP:?RUNNER_TEMP is required}/sccache-${server_key}.sock"
rm -f "${SCCACHE_SERVER_UDS}"

{
  echo "SCCACHE_IDLE_TIMEOUT=0"
  echo "RUSTC_WRAPPER=sccache"
  echo "SCCACHE_BASEDIRS=${GITHUB_WORKSPACE}"
  echo "SCCACHE_SERVER_UDS=${SCCACHE_SERVER_UDS}"
} >>"${GITHUB_ENV}"
export SCCACHE_IDLE_TIMEOUT=0
export RUSTC_WRAPPER=sccache
export SCCACHE_BASEDIRS="${GITHUB_WORKSPACE}"
export SCCACHE_SERVER_UDS

if [[ "${SCCACHE_BACKEND}" == "gcs" ]]; then
  {
    echo "SCCACHE_GCS_BUCKET=${SCCACHE_GCS_BUCKET_NAME}"
    echo "SCCACHE_GCS_RW_MODE=READ_WRITE"
    echo "SCCACHE_GCS_KEY_PREFIX=${gcs_key_prefix}"
  } >>"${GITHUB_ENV}"
  export SCCACHE_GCS_BUCKET="${SCCACHE_GCS_BUCKET_NAME}"
  export SCCACHE_GCS_RW_MODE=READ_WRITE
  export SCCACHE_GCS_KEY_PREFIX="${gcs_key_prefix}"
  # Do not set SCCACHE_GHA_ENABLED: that would select the GitHub Actions
  # cache backend. mozilla-actions/sccache-action only installs the binary.
  # Do not set SCCACHE_GCS_CREDENTIALS_URL: that is TaskCluster token fetch
  # in sccache 0.15. OpenDAL Gcs loads the GCE metadata server token when
  # no credential_path / token is set (default service account).
elif [[ "${SCCACHE_BACKEND}" == "local" ]]; then
  {
    echo "SCCACHE_DIR=${SCCACHE_LOCAL_DIR}"
    echo "SCCACHE_CACHE_SIZE=${SCCACHE_CACHE_SIZE}"
  } >>"${GITHUB_ENV}"
  export SCCACHE_DIR="${SCCACHE_LOCAL_DIR}"
  export SCCACHE_CACHE_SIZE="${SCCACHE_CACHE_SIZE}"
  # Do not set SCCACHE_GHA_ENABLED or GCS vars: local disk is SCCACHE_DIR.
else
  {
    echo "SCCACHE_GHA_ENABLED=true"
    # GITHUB_ENV persists values across steps. Empty assignments prevent
    # inherited runner or earlier-step GCS settings from overriding the
    # selected GHA backend after this script exits.
    echo "SCCACHE_GCS_BUCKET="
    echo "SCCACHE_GCS_RW_MODE="
    echo "SCCACHE_GCS_KEY_PREFIX="
  } >>"${GITHUB_ENV}"
  export SCCACHE_GHA_ENABLED=true
  # A prior step or runner image may have left GCS vars set. sccache 0.15
  # prefers GCS whenever SCCACHE_GCS_BUCKET is present, even if GHA is
  # enabled, and --show-stats then prints Cache location gcs.
  unset SCCACHE_GCS_BUCKET SCCACHE_GCS_RW_MODE SCCACHE_GCS_KEY_PREFIX
fi

"${SCCACHE_BIN}" --start-server
stats="$("${SCCACHE_BIN}" --show-stats)"
printf '%s\n' "${stats}"

if [[ "${SCCACHE_BACKEND}" == "gcs" ]]; then
  # sccache 0.15 RemoteStorage::location prints OpenDAL scheme/name/root:
  # "gcs, name: <bucket>, prefix: <root>"
  if ! grep -qF "gcs, name: ${SCCACHE_GCS_BUCKET_NAME}" <<<"${stats}"; then
    echo "::error::sccache Cache location is not GCS bucket ${SCCACHE_GCS_BUCKET_NAME} on ARC after a successful bucket probe" >&2
    exit 1
  fi
elif [[ "${SCCACHE_BACKEND}" == "local" ]]; then
  # sccache 0.15 disk cache prints the directory in Cache location.
  if ! grep -qF "${SCCACHE_DIR}" <<<"${stats}"; then
    echo "::error::sccache Cache location is not local disk ${SCCACHE_DIR} after SCCACHE_LOCAL_DIR was accepted" >&2
    exit 1
  fi
fi

#!/usr/bin/env bash
set -euo pipefail

supply_chain_tmp="$(mktemp -d)"
trap 'rm -rf "$supply_chain_tmp"' EXIT
base_lockfile="$supply_chain_tmp/base-Cargo.lock"
base_deny="$supply_chain_tmp/base-deny.toml"
pr_json="$supply_chain_tmp/supply-chain-pr.json"
timeline_json="$supply_chain_tmp/supply-chain-timeline.json"
changed_inputs="$supply_chain_tmp/changed-dependency-inputs"
deny_report="$supply_chain_tmp/deny-report.jsonl"
base_deny_report="$supply_chain_tmp/base-deny-report.jsonl"

cache_root="${MAESTRO_CI_CACHE_ROOT:-${RUNNER_TEMP:-$(pwd)/.cache}/maestro-ci}"
tool_root="${cache_root}/cargo-tools"
mkdir -p "$tool_root"
export CARGO_INSTALL_ROOT="$tool_root"
export PATH="$tool_root/bin:$PATH"

if ! deny_version="$(cargo deny --version 2>/dev/null)" || [[ "$deny_version" != "cargo-deny 0.19.9" ]]; then
  timeout --signal=TERM --kill-after=30s 20m cargo install cargo-deny --version 0.19.9 --locked --force
fi
timeout --signal=TERM --kill-after=10s 2m cargo deny fetch db
node --test scripts/check-new-deps-supply-chain.test.mjs scripts/check-advisory-expiry.test.mjs
node scripts/check-advisory-expiry.mjs

pull_request="${MAESTRO_CI_PULL_REQUEST:-false}"
if [[ "$pull_request" == "false" || -z "$pull_request" ]]; then
  timeout --signal=TERM --kill-after=30s 20m cargo deny check --disable-fetch
  exit 0
fi

base_branch="${MAESTRO_CI_BASE_BRANCH:-${GITHUB_BASE_REF:-main}}"
export MAESTRO_CI_COMMIT="${MAESTRO_CI_COMMIT:-${GITHUB_SHA:-}}"
timeout --signal=TERM --kill-after=10s 2m git fetch --no-tags origin "+refs/heads/$base_branch:refs/remotes/origin/$base_branch"
base_sha="$(git merge-base HEAD "origin/$base_branch")"
git show "$base_sha:Cargo.lock" > "$base_lockfile"
git show "$base_sha:deny.toml" > "$base_deny"
test -s "$base_lockfile"
test -s "$base_deny"

policy_changed=false
if ! git diff --quiet "$base_sha" HEAD -- deny.toml; then
  policy_changed=true
  repo_slug="${GITHUB_REPOSITORY:-}"
  [[ "$repo_slug" == */* ]] || {
    echo "could not derive GitHub repository from GITHUB_REPOSITORY" >&2
    exit 1
  }
  if [[ -n "${GH_TOKEN:-}" ]]; then
    command -v gh >/dev/null 2>&1 || {
      echo "GH_TOKEN is set, but GitHub CLI is unavailable" >&2
      exit 1
    }
    timeout --signal=TERM --kill-after=10s 60s gh api \
      "repos/$repo_slug/pulls/$pull_request" > "$pr_json"
    timeout --signal=TERM --kill-after=10s 60s gh api --paginate --slurp \
      -H "Accept: application/vnd.github+json" \
      "repos/$repo_slug/issues/$pull_request/timeline?per_page=100" \
      > "$timeline_json"
  else
    command -v curl >/dev/null 2>&1 || {
      echo "deny.toml changes require curl when GH_TOKEN is unavailable" >&2
      exit 1
    }
    github_api="https://api.github.com/repos/$repo_slug"
    curl_args=(
      --fail
      --silent
      --show-error
      --location
      -H "Accept: application/vnd.github+json"
      -H "X-GitHub-Api-Version: 2022-11-28"
      -H "User-Agent: evalops-maestro-ci"
    )
    timeout --signal=TERM --kill-after=10s 60s curl "${curl_args[@]}" \
      "$github_api/pulls/$pull_request" > "$pr_json"

    timeline_pages="$supply_chain_tmp/timeline-pages"
    mkdir -p "$timeline_pages"
    timeline_page=1
    while (( timeline_page <= 100 )); do
      printf -v timeline_page_file '%s/timeline-page-%06d.json' \
        "$timeline_pages" "$timeline_page"
      timeout --signal=TERM --kill-after=10s 60s curl "${curl_args[@]}" \
        "$github_api/issues/$pull_request/timeline?per_page=100&page=$timeline_page" \
        > "$timeline_page_file"
      timeline_page_size="$(node --input-type=module \
        -e 'const value = JSON.parse(await import("node:fs").then(({readFileSync}) => readFileSync(process.argv[1], "utf8"))); if (!Array.isArray(value)) process.exit(2); process.stdout.write(String(value.length));' \
        "$timeline_page_file")"
      if (( timeline_page_size < 100 )); then
        break
      fi
      ((timeline_page += 1))
    done
    if (( timeline_page > 100 )); then
      echo "GitHub timeline pagination exceeded 100 pages" >&2
      exit 1
    fi
    SUPPLY_CHAIN_TIMELINE_JSON="$timeline_json" node --input-type=module - \
      "$timeline_pages"/*.json <<'NODE'
      import { readFileSync, writeFileSync } from "node:fs";

      const events = process.argv.slice(2).flatMap((path) => {
        const page = JSON.parse(readFileSync(path, "utf8"));
        if (!Array.isArray(page)) {
          throw new Error(`GitHub timeline page is not an array: ${path}`);
        }
        return page;
      });
      writeFileSync(process.env.SUPPLY_CHAIN_TIMELINE_JSON, JSON.stringify([events]));
NODE
  fi
  export SUPPLY_CHAIN_PR_JSON="$pr_json"
  export SUPPLY_CHAIN_TIMELINE_JSON="$timeline_json"
  node --input-type=module <<'NODE'
    import { readFileSync } from "node:fs";

    const pr = JSON.parse(readFileSync(process.env.SUPPLY_CHAIN_PR_JSON, "utf8"));
    const timeline = JSON.parse(
      readFileSync(process.env.SUPPLY_CHAIN_TIMELINE_JSON, "utf8"),
    ).flat();
    const label = "supply-chain-policy-approved";
    if (pr.head?.sha !== process.env.MAESTRO_CI_COMMIT) {
      throw new Error("CI commit is not the current pull-request head");
    }
    if (!pr.labels?.some((entry) => entry.name === label)) {
      throw new Error(`${label} is not currently applied`);
    }
    const headCommitIndex = timeline.findLastIndex(
      (event) =>
        event.event === "committed" && event.sha === process.env.MAESTRO_CI_COMMIT,
    );
    const approvalIndex = timeline.findLastIndex(
      (event) => event.event === "labeled" && event.label?.name === label,
    );
    if (headCommitIndex < 0 || approvalIndex <= headCommitIndex) {
      throw new Error(`${label} must be applied after the exact PR head commit`);
    }
NODE
fi

git diff --name-only "$base_sha" HEAD \
  | awk '
      /(^|\/)Cargo\.toml$/ ||
      /^package\.json$/ ||
      /^Dockerfile$/ ||
      /^\.github\/workflows\/ghcr-publish\.yml$/ ||
      /^\.github\/workflows\/release\.yml$/ ||
      /^scripts\/build-release-binary\.mjs$/ { print }
    ' > "$changed_inputs"

dependency_input_args=()
if [[ -s "$changed_inputs" ]]; then
  dependency_input_args+=(--dependency-input-changed)
fi

set +e
timeout --signal=TERM --kill-after=10s 2m cargo deny -f json check --disable-fetch \
  2> "$deny_report" >/dev/null
deny_status=$?
set -e
if [[ "$deny_status" -ne 0 && ( "$deny_status" -lt 1 || "$deny_status" -gt 15 ) ]]; then
  exit "$deny_status"
fi
if [[ "$deny_status" -ne 0 ]]; then
  node scripts/check-new-deps-supply-chain.mjs \
    --validate-report "$deny_report" || exit "$deny_status"
  if [[ "$policy_changed" == true ]]; then
    echo "deny.toml changes must leave the current dependency tree compliant" >&2
    exit "$deny_status"
  fi
fi

if [[ "$policy_changed" == true ]]; then
  set +e
  timeout --signal=TERM --kill-after=10s 2m cargo deny -f json check \
    --config "$base_deny" --disable-fetch \
    2> "$base_deny_report" >/dev/null
  base_status=$?
  set -e
  if [[ "$base_status" -ne 0 && ( "$base_status" -lt 1 || "$base_status" -gt 15 ) ]]; then
    exit "$base_status"
  fi
  if [[ "$base_status" -ne 0 ]]; then
    node scripts/check-new-deps-supply-chain.mjs \
      --validate-report "$base_deny_report" || exit "$base_status"
    node scripts/check-new-deps-supply-chain.mjs \
      --report "$base_deny_report" \
      --base-lockfile "$base_lockfile" \
      --head-lockfile Cargo.lock \
      "${dependency_input_args[@]}" \
      --fail-on-preexisting
  fi
fi

node scripts/check-new-deps-supply-chain.mjs \
  --report "$deny_report" \
  --base-lockfile "$base_lockfile" \
  --head-lockfile Cargo.lock \
  "${dependency_input_args[@]}"

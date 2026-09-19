#!/usr/bin/env node

/**
 * Public sync PR check health monitor.
 *
 * The mirror incident behind this check: evalops/maestro#998 (a generated
 * sync PR from sync/public-release-mirror) merged while its `actionlint`
 * and `validate` checks were failing. The sync workflow's own run was green
 * — only the PR's checks were red — and the scheduled-failure-watchdog only
 * watches workflow RUN conclusions, so nobody was alerted. This monitor
 * closes that gap: it looks at the open sync PR's check runs and fails when
 * any of them concluded failing.
 *
 * Semantics:
 * - no open sync PR           -> OK (nothing in flight to watch)
 * - checks pending/in-flight  -> OK (a red conclusion is the signal, not a
 *                                still-running check; the sync workflow
 *                                debounce already handles in-flight resets)
 * - any failing conclusion    -> FAIL, listing the offending check runs
 *
 * FAIL CLOSED: this script is a monitor. Any failure to read the PR list,
 * the PR, or its check runs exits non-zero (see "Monitors fail closed" in
 * AGENTS.md). evalops/maestro is public, so reads work with any token, but
 * GH_TOKEN should still be set for rate-limit headroom.
 */

import { spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";

const DEFAULT_REPO = "dx-corp/code";
const DEFAULT_SYNC_BRANCH = "sync/public-release-mirror";

/**
 * Check-run conclusions treated as "the sync PR is red". Pending statuses
 * (queued/in_progress) and non-blocking conclusions (success, neutral,
 * skipped) never fail this monitor.
 */
export const FAILING_CONCLUSIONS = new Set([
	"failure",
	"cancelled",
	"timed_out",
	"action_required",
	"startup_failure",
	"stale",
]);

export class SyncPrBlindSpotError extends Error {}

function parseArgs(argv) {
	const args = {
		base: "main",
		branch: DEFAULT_SYNC_BRANCH,
		repo: DEFAULT_REPO,
	};
	for (let index = 0; index < argv.length; index += 1) {
		const arg = argv[index];
		switch (arg) {
			case "--base":
				args.base = argv[++index] ?? args.base;
				break;
			case "--branch":
				args.branch = argv[++index] ?? args.branch;
				break;
			case "--repo":
				args.repo = argv[++index] ?? args.repo;
				break;
			default:
				throw new Error(`Unknown argument: ${arg}`);
		}
	}
	return args;
}

export function ghJson(endpoint) {
	const result = spawnSync("gh", ["api", endpoint], {
		encoding: "utf8",
		maxBuffer: 64 * 1024 * 1024,
		timeout: 30000,
	});
	if (result.error) {
		throw new SyncPrBlindSpotError(`failed to run gh: ${result.error.message}`);
	}
	if (result.status !== 0) {
		const detail = (result.stderr || "").trim() || "unknown error";
		throw new SyncPrBlindSpotError(`gh api ${endpoint} failed: ${detail}`);
	}
	try {
		return JSON.parse(result.stdout);
	} catch {
		throw new SyncPrBlindSpotError(`gh api ${endpoint} returned invalid JSON`);
	}
}

/**
 * Pure classification of one sync PR's check runs. Returns the failing runs
 * (empty when the PR is green or merely still in flight).
 *
 * The check-runs API returns every run, including superseded re-runs: a
 * cancelled attempt and its successful retry coexist under the same check
 * name (observed on evalops/maestro#1000). Only the latest run per producer and name —
 * the one GitHub surfaces on the PR — decides.
 */
export function classifyCheckRuns(checkRuns) {
	const latestByName = new Map();
	for (const run of checkRuns) {
		const key = `${run.app?.id ?? "unknown"}:${run.name}`;
		const existing = latestByName.get(key);
		if (!existing || (run.id ?? 0) > (existing.id ?? 0)) {
			latestByName.set(key, run);
		}
	}
	const failing = [];
	for (const run of latestByName.values()) {
		if (run.status !== "completed") continue;
		if (FAILING_CONCLUSIONS.has(run.conclusion)) {
			failing.push(run);
		}
	}
	return { failing };
}

/**
 * Pure evaluation over already-fetched API data: pick the open sync PR (if
 * any) and classify its check runs. Network/JSON failures never reach this
 * function — they raise SyncPrBlindSpotError at the fetch layer, which is
 * what makes the monitor fail closed.
 */
export function evaluateSyncPrChecks({ openPrs, checkRuns }) {
	if (!Array.isArray(openPrs) || !Array.isArray(checkRuns)) throw new SyncPrBlindSpotError("Malformed sync PR/check data");
	const pr = openPrs[0];
	if (!pr) {
		return { state: "no-open-pr", failing: [] };
	}
	const { failing } = classifyCheckRuns(checkRuns);
	if (failing.length === 0) {
		return { state: "ok", pr, failing };
	}
	return { state: "failing", pr, failing };
}

export function readSyncPrHealth(options, fetch = ghJson) {
	const owner = options.repo.split("/")[0];
	const openPrs = fetch(`repos/${options.repo}/pulls?state=open&base=${encodeURIComponent(options.base)}&head=${encodeURIComponent(`${owner}:${options.branch}`)}`);
	if (!Array.isArray(openPrs) || openPrs.length > 1) throw new SyncPrBlindSpotError("Malformed or ambiguous sync PR list");
	const pr = openPrs[0];
	if (!pr) return { openPrs, checkRuns: [], failingStatuses: [] };
	const headSha = pr.head?.sha;
	if (!/^[0-9a-f]{40}$/u.test(headSha ?? "")) throw new SyncPrBlindSpotError("Sync PR reported no valid head SHA");
	const checkRuns = [];
	for (let page = 1; ; page++) {
		if (page > 100) throw new SyncPrBlindSpotError("Check pagination limit exceeded");
		const data = fetch(`repos/${options.repo}/commits/${headSha}/check-runs?per_page=100&page=${page}`);
		if (!Array.isArray(data?.check_runs) || !Number.isInteger(data.total_count) || data.total_count < 0) throw new SyncPrBlindSpotError("Malformed check-runs payload");
		for (const run of data.check_runs) {
			if (!run.name || !Number.isInteger(run.id) || !["queued", "in_progress", "completed", "waiting", "pending", "requested"].includes(run.status)) throw new SyncPrBlindSpotError("Malformed check run");
			if (run.status === "completed" && ![...FAILING_CONCLUSIONS, "success", "neutral", "skipped"].includes(run.conclusion)) throw new SyncPrBlindSpotError("Unknown completed check conclusion");
		}
		checkRuns.push(...data.check_runs);
		if (checkRuns.length === data.total_count) break;
		if (!data.check_runs.length || checkRuns.length > data.total_count) throw new SyncPrBlindSpotError("Incomplete check-runs pagination");
	}
	if (new Set(checkRuns.map(run => run.id)).size !== checkRuns.length) throw new SyncPrBlindSpotError("Duplicate check-run pages");
	// Combined statuses cover non-Actions producers as well as check runs.
	const latest = new Map();
	for (let page = 1; ; page++) {
		if (page > 100) throw new SyncPrBlindSpotError("Status pagination limit exceeded");
		const statuses = fetch(`repos/${options.repo}/commits/${headSha}/statuses?per_page=100&page=${page}`);
		if (!Array.isArray(statuses)) throw new SyncPrBlindSpotError("Malformed commit statuses");
		for (const status of statuses) {
			if (!status.context || !Number.isInteger(status.id) || !["success", "pending", "failure", "error"].includes(status.state)) throw new SyncPrBlindSpotError("Malformed commit status");
			if (!latest.has(status.context) || latest.get(status.context).id < status.id) latest.set(status.context, status);
		}
		if (statuses.length < 100) break;
	}
	return { openPrs, checkRuns, failingStatuses: [...latest.values()].filter(s => ["failure", "error"].includes(s.state)) };
}

function main() {
	const options = parseArgs(process.argv.slice(2));
	const { openPrs, checkRuns, failingStatuses } = readSyncPrHealth(options);
	const pr = openPrs[0];
	if (!pr) { console.log(`No open sync PR on ${options.repo}; nothing to watch.`); return; }
	const headSha = pr.head.sha;

	const { failing } = evaluateSyncPrChecks({ openPrs, checkRuns });
	const pending = checkRuns.filter((run) => run.status !== "completed");
	console.log(
		`Sync PR ${options.repo}#${pr.number} @ ${headSha}: ${checkRuns.length} check run(s), ${pending.length} still in flight, ${failing.length} failing.`,
	);
	if (failing.length === 0 && failingStatuses.length === 0) {
		return;
	}
	for (const status of failingStatuses) console.error(`::error::sync PR status "${status.context}" is ${status.state}: ${status.target_url ?? "no url"}`);
	for (const run of failing) {
		console.error(
			`::error::sync PR check "${run.name}" concluded ${run.conclusion}: ${run.html_url ?? "no url"}`,
		);
	}
	console.error(
		`::error::Open sync PR ${options.repo}#${pr.number} has ${failing.length} failing check run(s) and ${failingStatuses.length} failing commit status(es). This is the evalops/maestro#998 failure mode: the sync workflow run stays green while the PR it maintains is red. Do not merge the sync PR until these are resolved.`,
	);
	process.exitCode = 1;
}

if (process.argv[1] === fileURLToPath(import.meta.url)) {
	try {
		main();
	} catch (error) {
		console.error(
			`::error::${error instanceof Error ? error.message : error}`,
		);
		process.exitCode = 1;
	}
}

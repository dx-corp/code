#!/usr/bin/env node

import { createHash } from "node:crypto";
import { spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";

const DEFAULT_WORKFLOWS = [
	"maestro-sync-public-release-mirror",
	"maestro-model-catalog-freshness",
	"maestro-version-bump",
];
const LABEL = "ci-watchdog";
const LABEL_COLOR = "d93f0b";
const DEFAULT_RUNS = 3;
const MIN_CONSECUTIVE_FAILURES = 2;
const EXCERPT_LINE_LIMIT = 10;
const MAX_REQUESTED_RUNS = 100;
const MAX_RECEIPT_RUNS = 3;
const MAX_EXCERPT_CHARS = 4000;
const WATCHDOG_MARKER_PREFIX = "<!-- maestro-watchdog:";
const FINGERPRINT_MARKER_PREFIX = "<!-- maestro-watchdog-fingerprint:";

export function parseArgs(argv) {
	const args = {
		branch: "",
		dryRun: false,
		repo: process.env.GITHUB_REPOSITORY ?? "",
		runs: DEFAULT_RUNS,
		workflows: [...DEFAULT_WORKFLOWS],
	};

	for (let index = 0; index < argv.length; index += 1) {
		const arg = argv[index];
		switch (arg) {
			case "--branch":
				args.branch = argv[++index] ?? args.branch;
				break;
			case "--dry-run":
				args.dryRun = true;
				break;
			case "--repo":
				args.repo = argv[++index] ?? args.repo;
				break;
			case "--runs":
				args.runs = Number.parseInt(argv[++index] ?? "", 10);
				break;
			case "--workflow": {
				const workflow = argv[++index] ?? "";
				if (!workflow) {
					throw new Error("Missing value for --workflow <name>");
				}
				if (!args.workflows.includes(workflow)) {
					args.workflows.push(workflow);
				}
				break;
			}
			default:
				throw new Error(`Unknown argument: ${arg}`);
		}
	}

	if (
		!Number.isInteger(args.runs) ||
		args.runs < MIN_CONSECUTIVE_FAILURES ||
		args.runs > MAX_REQUESTED_RUNS
	) {
		throw new Error(
			`--runs must be an integer between ${MIN_CONSECUTIVE_FAILURES} and ${MAX_REQUESTED_RUNS}`,
		);
	}

	return args;
}

function warn(message) {
	console.error(`warning: ${message}`);
}

/**
 * A monitored workflow could not be observed at all.
 *
 * The watchdog exists to notice that a workflow is broken. If it cannot read
 * the workflow list, the run list, or the watchdog issues, it knows nothing --
 * and a green watchdog run then actively hides the outage it was built to
 * catch. Every such condition raises this and fails the job.
 */
class BlindSpotError extends Error {}

export function watchdogMarker(workflow) {
	return `${WATCHDOG_MARKER_PREFIX}${workflow} -->`;
}

export function fingerprintMarker(fingerprint) {
	return `${FINGERPRINT_MARKER_PREFIX}${fingerprint} -->`;
}

/**
 * Stable identity for one observed failure streak. The run IDs and attempts
 * make an unchanged API response idempotent while a new failed run produces a
 * new receipt. The CLI caps the input at MAX_REQUESTED_RUNS, so this evidence
 * cannot grow without bound.
 */
export function failureFingerprint(workflow, branch, runs) {
	const canonical = {
		branch,
		runs: runs.map((run) => ({
			conclusion: run?.conclusion ?? null,
			event: run?.event ?? null,
			head_sha: run?.head_sha ?? null,
			id: run?.id ?? null,
			run_attempt: run?.run_attempt ?? null,
		})),
		workflow,
	};
	return `sha256:${createHash("sha256").update(JSON.stringify(canonical)).digest("hex")}`;
}

// spawnSync's 1 MiB default silently becomes an ENOBUFS crash on a large run
// page or a long job log, both of which this script fetches routinely.
const MAX_BUFFER_BYTES = 64 * 1024 * 1024;

function gh(args) {
	const result = spawnSync("gh", ["api", ...args], {
		encoding: "utf8",
		maxBuffer: MAX_BUFFER_BYTES,
	});
	if (result.error) {
		throw result.error;
	}
	if (result.status !== 0) {
		return {
			ok: false,
			stderr: (result.stderr ?? "").trim(),
			stdout: "",
		};
	}
	return { ok: true, stderr: "", stdout: result.stdout ?? "" };
}

function ghJson(args) {
	const result = gh(args);
	if (!result.ok) {
		throw new BlindSpotError(
			`gh api ${args[0]} failed: ${result.stderr || "unknown error"}`,
		);
	}
	try {
		return JSON.parse(result.stdout);
	} catch {
		throw new BlindSpotError(`gh api ${args[0]} returned invalid JSON`);
	}
}

/** Best-effort JSON read for cosmetic data (log excerpts). Never fatal. */
function ghJsonOptional(args) {
	const result = gh(args);
	if (!result.ok) {
		warn(`gh api ${args[0]} failed: ${result.stderr || "unknown error"}`);
		return null;
	}
	try {
		return JSON.parse(result.stdout);
	} catch {
		warn(`gh api ${args[0]} returned invalid JSON`);
		return null;
	}
}

function resolveRepo(repo) {
	if (repo) {
		return repo;
	}
	const result = spawnSync(
		"gh",
		["repo", "view", "--json", "nameWithOwner", "--jq", ".nameWithOwner"],
		{ encoding: "utf8" },
	);
	if (result.status !== 0 || !result.stdout.trim()) {
		throw new Error(
			"Unable to resolve repository; pass --repo <owner/name> or set GITHUB_REPOSITORY",
		);
	}
	return result.stdout.trim();
}

function resolveDefaultBranch(repo) {
	const data = ghJson([`repos/${repo}`]);
	const branch = typeof data?.default_branch === "string" ? data.default_branch : "";
	if (!branch) {
		throw new BlindSpotError(`unable to read default branch for ${repo}`);
	}
	return branch;
}

function resolveWorkflow(repo, workflow) {
	const data = ghJson([`repos/${repo}/actions/workflows?per_page=100`]);
	const workflows = Array.isArray(data.workflows) ? data.workflows : [];
	const match = workflows.find(
		(entry) =>
			entry.name === workflow ||
			(typeof entry.path === "string" && entry.path.endsWith(`/${workflow}.yml`)),
	);
	if (!match) {
		throw new BlindSpotError(`workflow not found in ${repo}: ${workflow}`);
	}
	// A schedule-triggered workflow is auto-disabled after 60 days of repo
	// inactivity, and a disabled workflow stops producing runs entirely. Left
	// unreported that also reads as "nothing failing".
	if (match.state && match.state !== "active") {
		throw new BlindSpotError(
			`workflow ${workflow} is not active in ${repo} (state: ${match.state})`,
		);
	}
	return match;
}

/**
 * Recent completed automatic runs of `workflowId` on `branch`, newest first.
 *
 * The monitored workflows do not share a trigger: `public-mirror-drift-audit`
 * is schedule + dispatch, `sync-public-release-mirror` is push-to-main +
 * dispatch. A hardcoded `event=schedule` filter therefore matched zero runs of
 * the push-triggered one, and the watchdog skipped it -- which is how a 29-run
 * outage went unreported. So the event filter is per-trigger-class rather than
 * per-event-name:
 *
 * - default branch only, and `exclude_pull_requests`, so PR-branch runs of the
 *   same workflow cannot enter the streak
 * - `workflow_dispatch` runs are dropped. A streak is meant to describe the
 *   workflow's own automatic cadence. Manual dispatches are humans poking it,
 *   usually to test a fix, so counting them lets one green re-run erase an
 *   ongoing outage and lets one bad-input dispatch invent an outage.
 *
 * What remains is "the last N times this workflow fired on its own on the
 * default branch", which means the same thing for a schedule and for a push.
 */
const IGNORED_RUN_EVENTS = new Set(["workflow_dispatch"]);
const RUN_PAGE_SIZE = 100;
const MAX_RUN_PAGES = 10;

export function selectAutomaticRuns(pageRuns, count) {
	return (Array.isArray(pageRuns) ? pageRuns : [])
		.filter(
			(run) => run.status === "completed" && !IGNORED_RUN_EVENTS.has(run.event),
		)
		.slice(0, count);
}

function recentRuns(repo, workflowId, count, branch) {
	// Manual dispatches share the run list with automatic runs, so one page
	// can be entirely dispatches even when the workflow fires on its own
	// cadence. Keep paging (bounded) until `count` automatic runs are
	// collected: a burst of manual re-runs must not manufacture a blind spot.
	const collected = [];
	for (let page = 1; page <= MAX_RUN_PAGES; page += 1) {
		const data = ghJson([
			`repos/${repo}/actions/workflows/${workflowId}/runs?branch=${encodeURIComponent(branch)}&exclude_pull_requests=true&per_page=${RUN_PAGE_SIZE}&page=${page}`,
		]);
		const pageRuns = Array.isArray(data.workflow_runs) ? data.workflow_runs : [];
		collected.push(...selectAutomaticRuns(pageRuns, count - collected.length));
		if (collected.length >= count || pageRuns.length < RUN_PAGE_SIZE) {
			break;
		}
	}
	return collected.slice(0, count);
}

function countConsecutiveFailures(runs) {
	let count = 0;
	for (const run of runs) {
		if (run.conclusion !== "failure") {
			break;
		}
		count += 1;
	}
	return count;
}

export function selectCanonicalWatchdogIssue(issues, workflow) {
	const prefix = `[watchdog] ${workflow} `;
	const candidates = issues.filter(
		(issue) =>
			!issue.pull_request &&
			typeof issue.title === "string" &&
			issue.title.startsWith(prefix),
	);
	const open = candidates
		.filter((issue) => issue.state === "open")
		.sort((left, right) => left.number - right.number);
	if (open.length > 0) {
		return open[0];
	}
	const closed = candidates
		.filter((issue) => issue.state === "closed")
		.sort((left, right) => right.number - left.number);
	return closed[0] ?? null;
}

function findWatchdogIssues(repo, workflow) {
	const issues = ghJson([
		`repos/${repo}/issues?state=all&labels=${LABEL}&per_page=100`,
	]);
	if (!Array.isArray(issues)) {
		throw new BlindSpotError(
			`unexpected issue list payload for ${repo}; cannot tell whether a watchdog issue already exists`,
		);
	}
	const prefix = `[watchdog] ${workflow} `;
	const candidates = issues.filter(
		(issue) =>
			!issue.pull_request &&
			typeof issue.title === "string" &&
			issue.title.startsWith(prefix),
	);
	for (const issue of candidates) {
		if (issue.state !== "open" && issue.state !== "closed") {
			throw new BlindSpotError(
				`watchdog issue #${issue.number ?? "unknown"} has unknown state; cannot select a canonical receipt`,
			);
		}
	}
	return candidates;
}

function issueHasFingerprint(repo, issue, fingerprint) {
	if (!Number.isInteger(issue.number)) {
		throw new BlindSpotError(
			"watchdog issue payload omitted its number; cannot deduplicate the receipt",
		);
	}
	const marker = fingerprintMarker(fingerprint);
	if (typeof issue.body === "string" && issue.body.includes(marker)) {
		return true;
	}
	const comments = ghJson([
		`repos/${repo}/issues/${issue.number}/comments?per_page=100`,
	]);
	if (!Array.isArray(comments)) {
		throw new BlindSpotError(
			`unexpected comment payload for watchdog issue #${issue.number}; cannot deduplicate the receipt`,
		);
	}
	return comments.some(
		(comment) =>
			typeof comment.body === "string" && comment.body.includes(marker),
	);
}

function failureExcerpt(repo, run) {
	const jobsUrl = run.jobs_url;
	if (!jobsUrl) {
		return "";
	}
	const jobs = ghJsonOptional([jobsUrl.replace("https://api.github.com/", "")]);
	const failedJob = (jobs?.jobs ?? []).find(
		(job) => job.conclusion === "failure",
	);
	if (!failedJob) {
		return "";
	}
	const log = gh([
		`repos/${repo}/actions/jobs/${failedJob.id}/logs`,
	]);
	if (!log.ok || !log.stdout) {
		return "";
	}
	const lines = log.stdout.split("\n").filter((line) => line.trim());
	const errorLines = lines.filter((line) => /error|fail/i.test(line));
	const excerpt = (errorLines.length > 0 ? errorLines : lines).slice(
		-EXCERPT_LINE_LIMIT,
	);
	return excerpt.join("\n").slice(0, MAX_EXCERPT_CHARS);
}

export function buildFailureBody(workflow, runs, excerpt, branch, fingerprint) {
	const receiptRuns = runs.slice(0, MAX_RECEIPT_RUNS);
	const runLinks = receiptRuns
		.map(
			(run) =>
				`- ${run.html_url} (${run.event ?? "unknown event"}, ${run.created_at ?? "unknown time"})`,
		)
		.join("\n");
	const boundedExcerpt = String(excerpt ?? "").slice(0, MAX_EXCERPT_CHARS);
	const excerptBlock = boundedExcerpt
		? `\n\nFailure excerpt (latest failing run):\n\n\`\`\`\n${boundedExcerpt}\n\`\`\``
		: "";
	return [
		watchdogMarker(workflow),
		fingerprintMarker(fingerprint),
		`Observation fingerprint: ${fingerprint}`,
		`Workflow \`${workflow}\` has failed ${runs.length} consecutive runs on \`${branch}\`.`,
		"",
		`Failing runs (showing ${receiptRuns.length} of ${runs.length}):`,
		runLinks,
		excerptBlock,
		"",
		"Filed by `.github/workflows/scheduled-failure-watchdog.yml`.",
	].join("\n");
}

function ensureLabel(repo, dryRun) {
	const existing = gh([`repos/${repo}/labels/${LABEL}`]);
	if (existing.ok) {
		return;
	}
	if (dryRun) {
		console.log(`[dry-run] would create label ${LABEL}`);
		return;
	}
	const result = gh([
		`repos/${repo}/labels`,
		"-X",
		"POST",
		"-f",
		`name=${LABEL}`,
		"-f",
		`color=${LABEL_COLOR}`,
		"-f",
		"description=Automated scheduled-workflow failure watchdog",
	]);
	if (!result.ok) {
		throw new BlindSpotError(
			`failed to create label ${LABEL}: ${result.stderr}`,
		);
	}
}

function createIssue(repo, title, body, dryRun) {
	if (dryRun) {
		console.log(`[dry-run] would create issue: ${title}`);
		return;
	}
	const result = gh([
		`repos/${repo}/issues`,
		"-X",
		"POST",
		"-f",
		`title=${title}`,
		"-f",
		`body=${body}`,
		"-f",
		`labels[]=${LABEL}`,
	]);
	if (!result.ok) {
		throw new BlindSpotError(
			`failed to create issue "${title}": ${result.stderr}`,
		);
	}
	console.log(`created issue: ${title}`);
}

function commentIssue(repo, issueNumber, body, dryRun) {
	if (dryRun) {
		console.log(`[dry-run] would comment on issue #${issueNumber}`);
		return;
	}
	const result = gh([
		`repos/${repo}/issues/${issueNumber}/comments`,
		"-X",
		"POST",
		"-f",
		`body=${body}`,
	]);
	if (!result.ok) {
		throw new BlindSpotError(
			`failed to comment on issue #${issueNumber}: ${result.stderr}`,
		);
	}
}

function closeIssue(repo, issueNumber, dryRun) {
	if (dryRun) {
		console.log(`[dry-run] would close issue #${issueNumber}`);
		return;
	}
	const result = gh([
		`repos/${repo}/issues/${issueNumber}`,
		"-X",
		"PATCH",
		"-f",
		"state=closed",
	]);
	if (!result.ok) {
		throw new BlindSpotError(
			`failed to close issue #${issueNumber}: ${result.stderr}`,
		);
	}
}

function reopenIssue(repo, issueNumber, dryRun) {
	if (dryRun) {
		console.log(`[dry-run] would reopen issue #${issueNumber}`);
		return;
	}
	const result = gh([
		`repos/${repo}/issues/${issueNumber}`,
		"-X",
		"PATCH",
		"-f",
		"state=open",
	]);
	if (!result.ok) {
		throw new BlindSpotError(
			`failed to reopen issue #${issueNumber}: ${result.stderr}`,
		);
	}
}

function checkWorkflow(repo, workflow, options) {
	const entry = resolveWorkflow(repo, workflow);
	const branch = options.branch;
	const runs = recentRuns(repo, entry.id, options.runs, branch);
	if (runs.length === 0) {
		// The weekly release proposal is intentionally quiet between its first
		// installation and the next Monday 16:00 UTC schedule. Treating that
		// expected pre-cadence silence as a blind spot made every six-hour
		// watchdog run red before the workflow had a chance to fire. Once the
		// first scheduled boundary has passed, an empty run set remains a real
		// blind spot and fails closed.
		if (
			workflow === "maestro-version-bump" &&
			entry.updated_at &&
			Date.now() < nextWeeklyVersionBumpAt(entry.updated_at)
		) {
			console.log(
				`${workflow}: awaiting its first scheduled Monday 16:00 UTC run`,
			);
			return;
		}
		// Not "nothing to report": a monitored workflow with no completed runs
		// on the default branch is unobservable, which is the same blind spot
		// as a failed query.
		throw new BlindSpotError(
			`no completed non-dispatch runs of ${workflow} on ${branch}; the watchdog cannot observe it`,
		);
	}

	const consecutiveFailures = countConsecutiveFailures(runs);
	const issue = selectCanonicalWatchdogIssue(
		findWatchdogIssues(repo, workflow),
		workflow,
	);

	if (consecutiveFailures >= MIN_CONSECUTIVE_FAILURES) {
		const failingRuns = runs.slice(0, consecutiveFailures);
		const title = `[watchdog] ${workflow} failing ${consecutiveFailures} consecutive runs on ${branch}`;
		const fingerprint = failureFingerprint(workflow, branch, failingRuns);
		const body = buildFailureBody(
			workflow,
			failingRuns,
			failureExcerpt(repo, failingRuns[0]),
			branch,
			fingerprint,
		);
		if (issue) {
			if (issue.state === "closed") {
				console.log(
					`${workflow}: still failing (${consecutiveFailures} consecutive); reopening #${issue.number}`,
				);
				reopenIssue(repo, issue.number, options.dryRun);
			}
			if (issueHasFingerprint(repo, issue, fingerprint)) {
				console.log(
					`${workflow}: unchanged failure fingerprint already recorded on #${issue.number}; no new comment`,
				);
				return;
			}
			console.log(
				`${workflow}: new failure fingerprint (${consecutiveFailures} consecutive); commenting on #${issue.number}`,
			);
			commentIssue(repo, issue.number, body, options.dryRun);
		} else {
			console.log(`${workflow}: ${consecutiveFailures} consecutive failures; filing issue`);
			ensureLabel(repo, options.dryRun);
			createIssue(repo, title, body, options.dryRun);
		}
		return;
	}

	if (runs[0].conclusion === "success" && issue) {
		console.log(
			`${workflow}: recovered (latest run on ${branch} succeeded); closing #${issue.number}`,
		);
		commentIssue(
			repo,
			issue.number,
			`Latest run on \`${branch}\` succeeded: ${runs[0].html_url}\n\nClosing this watchdog issue.`,
			options.dryRun,
		);
		closeIssue(repo, issue.number, options.dryRun);
		return;
	}

	console.log(
		`${workflow}: ok (latest conclusion: ${runs[0].conclusion ?? "unknown"})`,
	);
}

function nextWeeklyVersionBumpAt(updatedAt) {
	const updated = new Date(updatedAt);
	if (Number.isNaN(updated.getTime())) {
		return 0;
	}
	const next = new Date(updated);
	const daysUntilMonday = (1 - next.getUTCDay() + 7) % 7 || 7;
	next.setUTCDate(next.getUTCDate() + daysUntilMonday);
	next.setUTCHours(16, 0, 0, 0);
	return next.getTime();
}

function reportBlindSpots(count, total) {
	console.error(
		`::error::The failure watchdog could not observe ${count} of ${total} monitored workflow(s), so it is failing instead of reporting success. A green watchdog run here would hide exactly the outages this job exists to catch. Check the job's \`permissions:\` block first -- listing workflows and reading their runs needs \`actions: read\`.`,
	);
}

function main() {
	const options = parseArgs(process.argv.slice(2));
	const repo = resolveRepo(options.repo);
	options.branch = options.branch || resolveDefaultBranch(repo);
	console.log(
		`Monitoring ${options.workflows.length} workflow(s) in ${repo} on ${options.branch}${options.dryRun ? " (dry-run)" : ""}`,
	);

	// Check every workflow before deciding the exit code so one blind spot
	// does not hide the state of the others.
	let blindSpots = 0;
	for (const workflow of options.workflows) {
		try {
			checkWorkflow(repo, workflow, options);
		} catch (error) {
			if (!(error instanceof BlindSpotError)) {
				throw error;
			}
			blindSpots += 1;
			console.error(
				`::error::watchdog blind spot -- ${workflow}: ${error.message}`,
			);
		}
	}

	if (blindSpots > 0) {
		reportBlindSpots(blindSpots, options.workflows.length);
		return 1;
	}
	return 0;
}


if (process.argv[1] === fileURLToPath(import.meta.url)) {
	try {
		process.exitCode = main();
	} catch (error) {
		if (error instanceof BlindSpotError) {
			// Raised before per-workflow iteration could start (repo lookup, auth).
			console.error(`::error::watchdog blind spot -- ${error.message}`);
			reportBlindSpots(1, 1);
			process.exitCode = 1;
		} else {
			throw error;
		}
	}
}

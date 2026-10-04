#!/usr/bin/env node

/**
 * Required status check coverage guard.
 *
 * The mirror incident behind this check: evalops/maestro#998 (a generated
 * sync PR) merged while its `actionlint` and `validate` checks were failing,
 * because those PR-triggered jobs were not in the branch's required status
 * checks, so branch protection never gated on them. Branch protection is
 * managed manually and drifts; jobs get added to workflows without anyone
 * adding their context to the required set.
 *
 * This is the inverse of scripts/check-required-status-checks.mjs: that one
 * proves every *required* context can report; this one proves every job that
 * can run on pull_request *is* required (or explicitly opted out below).
 *
 * Required contexts are read live from the GitHub branch protection API.
 * Reading protection needs Administration read, so the token must come from
 * GH_TOKEN (an admin-capable app/PAT token), not the default GITHUB_TOKEN.
 *
 * FAIL CLOSED: this script is a monitor — its green run IS the signal. A
 * missing token, a 403/404 from the protection API, or an unreadable
 * workflow tree exits non-zero. There is no non-strict downgrade; see
 * "Monitors fail closed" in AGENTS.md.
 */

import { spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";

import {
	loadWorkflows,
	pullRequestEvents,
} from "./check-required-status-checks.mjs";

/**
 * Jobs that can run on pull_request but are deliberately NOT required status
 * checks, keyed by repository so an opt-out for one repo can never mask
 * drift on the other. Every entry must carry a one-line justification; an
 * uncovered job that is not listed here fails this check.
 *
 * An entry matches the exact base context (job display name or id), the
 * matrix-stripped base ("job (${{ matrix… }})" -> "job"), or the caller
 * segment of a reusable-workflow context — mirroring how GitHub reports
 * check names.
 */
export const COVERAGE_OPT_OUT = {
	"evalops/maestro": [
		// dispatch (evalopsbot-review-request.yml): advisory review-request bot;
		// fails open by design and must not gate PRs.
		"dispatch",
	],
};

export function parseArgs(argv) {
	const args = { targets: [], optOutByRepo: new Map() };
	for (let index = 0; index < argv.length; index += 1) {
		const arg = argv[index];
		switch (arg) {
			case "--target": {
				const value = argv[++index] ?? "";
				args.targets.push(parseTarget(value));
				break;
			}
			case "--opt-out": {
				const value = argv[++index] ?? "";
				const separator = value.indexOf("=");
				if (separator <= 0 || separator === value.length - 1) {
					throw new Error(
						`Invalid --opt-out "${value}"; expected owner/repo=context`,
					);
				}
				const repo = value.slice(0, separator);
				const context = value.slice(separator + 1);
				const entries = args.optOutByRepo.get(repo) ?? [];
				entries.push(context);
				args.optOutByRepo.set(repo, entries);
				break;
			}
			default:
				throw new Error(`Unknown argument: ${arg}`);
		}
	}
	if (args.targets.length === 0) {
		args.targets.push({
			repo: "evalops/maestro",
			branch: "main",
			root: process.cwd(),
		});
	}
	return args;
}

/** `--target owner/repo@branch:root` — root is the checkout to scan. */
function parseTarget(value) {
	const match = /^([^@]+)@([^:]+):(.+)$/.exec(value);
	if (!match) {
		throw new Error(
			`Invalid --target "${value}"; expected owner/repo@branch:root`,
		);
	}
	return { repo: match[1], branch: match[2], root: match[3] };
}

export class CoverageBlindSpotError extends Error {}

export function assertGhOk(result, endpoint) {
	if (result.error) {
		throw new CoverageBlindSpotError(
			`failed to run gh: ${result.error.message}`,
		);
	}
	if (result.status !== 0) {
		const detail = (result.stderr || "").trim() || "unknown error";
		// 403/404 here means the token lacks Administration read on branch
		// protection. A monitor that cannot read the state it watches must
		// fail, loudly — never downgrade this to a warning.
		throw new CoverageBlindSpotError(
			`gh api ${endpoint} failed: ${detail}. This monitor cannot read branch ` +
				"protection (the token needs Administration read on the target " +
				"repository) and is failing closed rather than reporting a pass " +
				"it did not verify.",
		);
	}
	return result.stdout;
}

export function fetchRequiredContexts(repo, branch) {
	const branchPath = `repos/${repo}/branches/${encodeURIComponent(branch)}`;
	const read = (endpoint, jq) => {
		const result = spawnSync("gh", ["api", endpoint, "--jq", jq], {
			encoding: "utf8",
		});
		const stdout = assertGhOk(result, endpoint);
		try {
			return JSON.parse(stdout);
		} catch {
			throw new CoverageBlindSpotError(
				`gh api ${endpoint} returned invalid JSON`,
			);
		}
	};
	return [
		...new Set([
			...read(
				`${branchPath}/protection`,
				"[.required_status_checks.checks[]?.context]",
			),
			...read(
				`repos/${repo}/rules/branches/${encodeURIComponent(branch)}`,
				'[.[] | select(.type == "required_status_checks") | .parameters.required_status_checks[].context]',
			),
		]),
	];
}

/**
 * Every check context a job in this checkout can report on pull_request.
 * The context base is the job display name when set, otherwise the job id —
 * the same convention scripts/check-required-status-checks.mjs documents
 * (matrix jobs suffix " (…)", reusable-workflow callers report as
 * "caller / called").
 */
export function collectPullRequestJobContexts(root) {
	const contexts = [];
	for (const workflow of loadWorkflows(root)) {
		const runsOnPullRequest = [...workflow.triggers.keys()].some((event) =>
			pullRequestEvents.has(event),
		);
		if (!runsOnPullRequest) continue;
		for (const [jobId, job] of workflow.jobs) {
			contexts.push({
				context: job.name || jobId,
				jobId,
				workflowPath: workflow.path,
			});
		}
	}
	return contexts;
}

/**
 * Does `requiredContext` (as reported by branch protection) cover the job
 * whose base context is `jobContext`? Mirrors the matching rules in
 * check-required-status-checks.mjs: exact, matrix-suffixed, or any
 * " / "-separated segment (reusable-workflow "caller / called").
 */
export function contextCovers(requiredContext, jobContext) {
	const normalized = requiredContext.trim();
	if (normalized === jobContext) return true;
	if (normalized.startsWith(`${jobContext} (`)) return true;
	return normalized
		.split(" / ")
		.map((segment) => segment.trim())
		.includes(jobContext);
}

function matchesOptOut(optOutEntry, job) {
	// Matrix job names carry a literal " (${{ matrix… }})" suffix in the
	// workflow source; GitHub reports the rendered " (lane)" variants. An
	// opt-out for the base job covers all of them.
	const baseContext = job.context.replace(/\s+\([^()]*\)\s*$/u, "");
	return (
		optOutEntry === job.context ||
		optOutEntry === baseContext ||
		optOutEntry === job.jobId
	);
}

/**
 * Pure diff: which pull_request job contexts are neither required nor
 * opted out. Extra required contexts (required but not produced by any
 * enumerated job) are reported for information only — proving those can
 * report is scripts/check-required-status-checks.mjs's job.
 */
export function diffCoverage({ jobContexts, requiredContexts, optOut = [] }) {
	const covered = [];
	const optedOut = [];
	const uncovered = [];
	for (const job of jobContexts) {
		if (requiredContexts.some((context) => contextCovers(context, job.context))) {
			covered.push(job);
			continue;
		}
		if (optOut.some((entry) => matchesOptOut(entry, job))) {
			optedOut.push(job);
			continue;
		}
		uncovered.push(job);
	}
	const extraRequired = requiredContexts.filter(
		(context) =>
			!jobContexts.some((job) => contextCovers(context, job.context)),
	);
	return { covered, optedOut, uncovered, extraRequired };
}

function main() {
	const options = parseArgs(process.argv.slice(2));

	if (!process.env.GH_TOKEN) {
		throw new CoverageBlindSpotError(
			"GH_TOKEN is empty. This monitor reads branch protection (which needs " +
				"Administration read) and must not report a pass without a token; " +
				"it fails closed instead.",
		);
	}

	let failed = 0;
	for (const target of options.targets) {
		console.log(
			`\n== ${target.repo}@${target.branch} (workflows from ${target.root})`,
		);
		let jobContexts;
		let requiredContexts;
		try {
			jobContexts = collectPullRequestJobContexts(target.root);
		} catch (error) {
			throw new CoverageBlindSpotError(
				`cannot enumerate workflows under ${target.root}: ${error instanceof Error ? error.message : error}`,
			);
		}
		requiredContexts = fetchRequiredContexts(target.repo, target.branch);

		const { covered, optedOut, uncovered, extraRequired } = diffCoverage({
			jobContexts,
			requiredContexts,
			optOut: [
				...(COVERAGE_OPT_OUT[target.repo] ?? []),
				...(options.optOutByRepo.get(target.repo) ?? []),
			],
		});
		for (const job of covered) {
			console.log(`  required:   ${job.context} (${job.workflowPath})`);
		}
		for (const job of optedOut) {
			console.log(`  opted out:  ${job.context} (${job.workflowPath})`);
		}
		for (const context of extraRequired) {
			console.log(
				`  info: required context "${context}" is not produced by any enumerated job (reportability is checked by check-required-status-checks.mjs)`,
			);
		}
		for (const job of uncovered) {
			console.error(
				`::error::${target.repo}@${target.branch}: pull_request job "${job.context}" (${job.workflowPath} job "${job.jobId}") is not a required status check and is not in the coverage opt-out list. Either add the context to the branch's required status checks, or add a justified entry to COVERAGE_OPT_OUT in scripts/check-required-check-coverage.mjs.`,
			);
		}
		if (uncovered.length > 0) {
			failed += uncovered.length;
		} else {
			console.log(
				`  coverage OK: ${covered.length} required, ${optedOut.length} opted out, 0 uncovered`,
			);
		}
	}

	if (failed > 0) {
		console.error(
			`::error::${failed} pull_request job context(s) are unprotected. This is the evalops/maestro#998 failure mode: a check that runs on PRs but is not required cannot block a merge.`,
		);
		process.exitCode = 1;
	}
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

#!/usr/bin/env node

/**
 * Required status check invariant.
 *
 * Branch protection on main can only gate PRs on checks that actually report.
 * A required context reported by a workflow whose pull_request trigger carries
 * a paths:/paths-ignore: filter can silently never report and wedge every PR
 * that skips the filter (this stalled public sync PRs 868/870). This check
 * fails when a required context:
 *
 * - maps to no job in .github/workflows/*.yml at all, or
 * - is only reported by workflows that never run on pull_request, or
 * - is only reported by pull_request triggers filtered by paths/paths-ignore.
 *
 * Required contexts are read live from the GitHub branch protection API, so
 * protection-rule drift fails the check without a code change.
 *
 * --strict flips the credential/permission failure modes from "warn and
 * skip" to "fail the job": a missing EVALOPS_PR_LENS_TOKEN or a 403/404 from
 * the protection API no longer produces a silent pass. This workflow's sole
 * purpose is proving required checks report; on its home repository it must
 * not be able to go green without actually running. Callers should reserve
 * the soft skip (no --strict) for forks/other repos that can't hold the
 * token or the Administration-read grant it needs.
 */

import { readdirSync, readFileSync } from "node:fs";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { spawnSync } from "node:child_process";

const defaultRoot = fileURLToPath(new URL("..", import.meta.url));

export const pullRequestEvents = new Set([
	"pull_request",
	"pull_request_target",
]);

function parseArgs(argv) {
	const args = {
		branch: "main",
		repo: process.env.GITHUB_REPOSITORY || "evalops/maestro",
		root: process.cwd(),
		strict: false,
	};

	for (let index = 0; index < argv.length; index += 1) {
		const arg = argv[index];
		switch (arg) {
			case "--branch":
				args.branch = argv[++index] ?? args.branch;
				break;
			case "--repo":
				args.repo = argv[++index] ?? args.repo;
				break;
			case "--root":
				args.root = argv[++index] ?? args.root;
				break;
			case "--strict":
				args.strict = true;
				break;
			default:
				throw new Error(`Unknown argument: ${arg}`);
		}
	}

	return args;
}

export function fetchRequiredContexts(
	repo,
	branch,
	{ strict = false, run = spawnSync } = {},
) {
	const endpoint = `repos/${repo}/branches/${encodeURIComponent(branch)}/protection`;
	const rulesEndpoint = `repos/${repo}/rules/branches/${encodeURIComponent(branch)}`;
	const gh = (path, jq) => {
		const result = run("gh", ["api", path, "--jq", jq], { encoding: "utf8" });
		if (result.error) {
			throw new Error(`failed to run gh: ${result.error.message}`);
		}
		return result;
	};
	const unreadable = (detail) => {
		// A token without Administration read on branch protection gets 403/404.
		// In strict mode (the repo whose invariant this is) that must fail the
		// job: the whole point of this check is to prove required checks
		// report, and a credential problem is exactly the kind of thing that
		// must not let it go green silently. Non-strict callers (forks/other
		// repos that can't hold the token or grant) still degrade to a
		// warning + pass.
		if (strict) {
			throw new Error(
				`gh api ${endpoint} failed: ${detail}. The token cannot read branch ` +
					"protection (needs Administration read) on a repository where this " +
					"invariant runs in --strict mode; it must fail closed rather than " +
					"silently skip.",
			);
		}
		console.warn(
			`::warning::INVARIANT NOT VERIFIED: gh api ${endpoint} failed: ${detail}. ` +
				"The token cannot read branch protection (needs Administration read), so " +
				"required contexts could not be enumerated. This job passing does NOT mean " +
				"required checks are reportable.",
		);
		return null;
	};

	const classic = gh(endpoint, "[.required_status_checks.checks[]?.context]");
	let classicContexts = [];
	let classicNotFound = false;
	if (classic.status === 0) {
		classicContexts = JSON.parse(classic.stdout);
	} else {
		const detail = (classic.stderr || "").trim() || "unknown error";
		if (/HTTP 404/.test(detail)) {
			classicNotFound = true;
		} else if (/HTTP 403/.test(detail)) {
			return unreadable(detail);
		} else {
			throw new Error(`gh api ${endpoint} failed: ${detail}`);
		}
	}

	const rules = gh(
		rulesEndpoint,
		'[.[] | select(.type == "required_status_checks") | .parameters.required_status_checks[].context]',
	);
	if (rules.status !== 0) {
		const detail = (rules.stderr || "").trim() || "unknown error";
		throw new Error(`gh api ${rulesEndpoint} failed: ${detail}`);
	}
	const contexts = [
		...new Set([...classicContexts, ...JSON.parse(rules.stdout)]),
	];
	if (classicNotFound && contexts.length === 0) {
		return unreadable("HTTP 404 and no ruleset required_status_checks");
	}
	return contexts;
}

function stripComment(line) {
	let quote = "";
	for (let index = 0; index < line.length; index += 1) {
		const char = line[index];
		if (quote) {
			if (char === quote) quote = "";
			continue;
		}
		if (char === '"' || char === "'") {
			quote = char;
			continue;
		}
		if (char === "#" && (index === 0 || line[index - 1] === " " || line[index - 1] === "\t")) {
			return line.slice(0, index);
		}
	}
	return line;
}

function unquote(value) {
	const trimmed = value.trim();
	if (
		trimmed.length >= 2 &&
		((trimmed.startsWith('"') && trimmed.endsWith('"')) ||
			(trimmed.startsWith("'") && trimmed.endsWith("'")))
	) {
		return trimmed.slice(1, -1);
	}
	return trimmed;
}

const keyPattern = /^([A-Za-z_][\w-]*|"on"|'on'|"[^"]+"|'[^']+')\s*:\s*(.*)$/;
const pathsKeyPattern = /^(["']?)(paths|paths-ignore)\1\s*:/;

/**
 * Split a workflow file into top-level sections. Each section keeps its raw
 * lines so nested blocks (triggers, jobs) can be scanned by indent. This is a
 * structural reader, not a full YAML parser; it relies on the 2-space indent
 * style used by every workflow in this repository.
 */
function topLevelSections(text) {
	const sections = new Map();
	let current = null;
	for (const rawLine of text.split("\n")) {
		const trimmed = rawLine.trim();
		if (!trimmed || trimmed.startsWith("#") || trimmed === "---") continue;
		const line = stripComment(rawLine);
		if (!line.trim()) continue;
		const indent = line.length - line.trimStart().length;
		if (indent === 0) {
			const match = keyPattern.exec(line.trim());
			if (!match) {
				current = null;
				continue;
			}
			const key = unquote(match[1]);
			current = { inline: match[2].trim(), lines: [] };
			sections.set(key, current);
			continue;
		}
		if (current) {
			current.lines.push({ indent, content: line.trim() });
		}
	}
	return sections;
}

/**
 * Map of event name -> { filtered } for each trigger. `filtered` is true when
 * the trigger carries a paths:/paths-ignore: filter, either nested or inline.
 */
function parseTriggers(section) {
	const events = new Map();
	if (!section) return events;

	const inline = section.inline;
	if (inline.startsWith("[") && inline.endsWith("]")) {
		for (const item of inline.slice(1, -1).split(",")) {
			const name = unquote(item);
			if (name) events.set(name, { filtered: false });
		}
		return events;
	}
	if (inline.startsWith("{") && inline.endsWith("}")) {
		for (const entry of inline.slice(1, -1).split(/,(?![^\[]*\])/u)) {
			const match = keyPattern.exec(entry.trim());
			if (!match) continue;
			events.set(unquote(match[1]), {
				filtered: /(^|[{,\s])paths(-ignore)?\s*:/u.test(match[2]),
			});
		}
		return events;
	}
	if (inline) {
		events.set(unquote(inline), { filtered: false });
		return events;
	}

	let currentEvent = null;
	let eventIndent = 0;
	for (const { indent, content } of section.lines) {
		if (currentEvent && indent > eventIndent) {
			if (pathsKeyPattern.test(content)) {
				events.get(currentEvent).filtered = true;
			}
			continue;
		}
		const match = keyPattern.exec(content);
		if (!match) {
			currentEvent = null;
			continue;
		}
		currentEvent = unquote(match[1]);
		eventIndent = indent;
		events.set(currentEvent, {
			filtered: /(^|[{,\s])paths(-ignore)?\s*:/u.test(match[2]),
		});
	}
	return events;
}

/** Map of job id -> display name (job `name:` override, falling back to id). */
function parseJobs(section) {
	const jobs = new Map();
	if (!section) return jobs;

	let currentJob = null;
	let jobIndent = 0;
	let childIndent = null;
	for (const { indent, content } of section.lines) {
		if (currentJob && indent > jobIndent) {
			if (childIndent === null) childIndent = indent;
			if (indent === childIndent && !jobs.get(currentJob).name) {
				const match = /^name\s*:\s*(.+)$/.exec(content);
				if (match) jobs.get(currentJob).name = unquote(match[1]);
			}
			continue;
		}
		const match = /^([A-Za-z_][\w-]*)\s*:\s*$/.exec(content);
		if (!match) {
			currentJob = null;
			childIndent = null;
			continue;
		}
		currentJob = match[1];
		jobIndent = indent;
		childIndent = null;
		jobs.set(currentJob, { name: "" });
	}
	return jobs;
}

export function loadWorkflows(root) {
	const workflowsDir = join(root, ".github/workflows");
	const workflows = [];
	for (const entry of readdirSync(workflowsDir)) {
		if (!/\.ya?ml$/u.test(entry)) continue;
		const relativePath = `.github/workflows/${entry}`;
		const text = readFileSync(join(workflowsDir, entry), "utf8");
		const sections = topLevelSections(text);
		workflows.push({
			path: relativePath,
			name: sections.get("name")?.inline
				? unquote(sections.get("name").inline)
				: entry,
			triggers: parseTriggers(sections.get("on")),
			jobs: parseJobs(sections.get("jobs")),
		});
	}
	return workflows;
}

/**
 * A required context is the check-run name GitHub records for a job: the job
 * display name when set, otherwise the job id. Matrix jobs suffix the context
 * with " (…)" and jobs calling a reusable workflow report as
 * "caller-job / called-job".
 */
function findCandidates(context, workflows) {
	const normalized = context.trim();
	const matrixBase = normalized.replace(/\s+\([^()]*\)\s*$/u, "");
	const segments = normalized.split(" / ").map((segment) => segment.trim());
	const matches = [];
	for (const workflow of workflows) {
		for (const [jobId, job] of workflow.jobs) {
			const displayName = job.name || jobId;
			const names = new Set([jobId, displayName]);
			const matched =
				names.has(normalized) ||
				names.has(matrixBase) ||
				segments.some((segment) => names.has(segment));
			if (matched) {
				matches.push({ workflow, jobId, displayName });
			}
		}
	}
	return matches;
}

export function evaluateRequiredStatusChecks({ contexts, root = defaultRoot }) {
	const workflows = loadWorkflows(root);
	const failures = [];
	const mappings = [];

	for (const context of contexts) {
		const candidates = findCandidates(context, workflows);
		if (candidates.length === 0) {
			failures.push(
				`required context "${context}" does not map to any job in .github/workflows/*.yml; remove it from branch protection or add the missing job`,
			);
			continue;
		}

		const clean = [];
		const filtered = [];
		const notOnPullRequest = [];
		for (const candidate of candidates) {
			const prTriggers = [...candidate.workflow.triggers].filter(([event]) =>
				pullRequestEvents.has(event),
			);
			if (prTriggers.length === 0) {
				notOnPullRequest.push(candidate);
				continue;
			}
			if (prTriggers.some(([, trigger]) => trigger.filtered)) {
				filtered.push(candidate);
				continue;
			}
			clean.push(candidate);
		}

		const reporter = clean[0] ?? filtered[0] ?? notOnPullRequest[0];
		mappings.push(
			`"${context}" -> ${reporter.workflow.path} job "${reporter.jobId}"`,
		);

		if (clean.length > 0) continue;
		if (filtered.length > 0) {
			for (const candidate of filtered) {
				failures.push(
					`required context "${context}" is reported by ${candidate.workflow.path} job "${candidate.jobId}", whose pull_request trigger has a paths:/paths-ignore: filter; the check can silently never report and wedge PRs`,
				);
			}
			continue;
		}
		failures.push(
			`required context "${context}" maps to ${reporter.workflow.path} job "${reporter.jobId}", but that workflow never runs on pull_request`,
		);
	}

	return { failures, mappings };
}

function main() {
	const options = parseArgs(process.argv.slice(2));

	if (!process.env.GH_TOKEN) {
		if (options.strict) {
			console.error(
				"::error::EVALOPS_PR_LENS_TOKEN is empty for a --strict run of the " +
					"required status check invariant. This check's sole purpose is " +
					"proving required checks report; it must not be able to go green " +
					"without a token to query branch protection. Set the secret or " +
					"drop --strict for this caller.",
			);
			process.exitCode = 1;
			return;
		}
		console.warn(
			"::warning::EVALOPS_PR_LENS_TOKEN is unavailable; skipping required status check invariant (non-strict).",
		);
		return;
	}

	const contexts = fetchRequiredContexts(options.repo, options.branch, {
		strict: options.strict,
	});
	if (contexts === null) {
		// Non-strict: protection endpoint unreadable with this token, warned already.
		return;
	}
	if (contexts.length === 0) {
		console.log(
			`No required status checks configured on ${options.repo}@${options.branch}; invariant is vacuous.`,
		);
		return;
	}

	const { failures, mappings } = evaluateRequiredStatusChecks({
		contexts,
		root: options.root,
	});
	for (const mapping of mappings) {
		console.log(mapping);
	}
	if (failures.length === 0) {
		console.log(
			`Required status check invariant passed (${contexts.length}/${contexts.length} contexts map to unfiltered pull_request jobs).`,
		);
		return;
	}
	for (const failure of failures) {
		console.error(`::error::${failure}`);
	}
	process.exitCode = 1;
}

if (process.argv[1] === fileURLToPath(import.meta.url)) {
	try {
		main();
	} catch (error) {
		// Surface as a GitHub annotation, not a Node stack trace: this script's
		// failures are operator-actionable configuration problems.
		console.error(`::error::${error instanceof Error ? error.message : error}`);
		process.exitCode = 1;
	}
}

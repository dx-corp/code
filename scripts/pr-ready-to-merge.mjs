#!/usr/bin/env node

import { execFileSync } from "node:child_process";
import process from "node:process";

const PASSING_CHECK_RUN_CONCLUSIONS = new Set(["SUCCESS", "SKIPPED", "NEUTRAL"]);
const PASSING_STATUS_CONTEXT_STATES = new Set(["SUCCESS"]);

function parseArgs(argv) {
	const args = {
		headSha: "",
		pr: "",
		repo: "",
		strictStatusChecks: false,
	};

	for (let index = 0; index < argv.length; index += 1) {
		const arg = argv[index];
		switch (arg) {
			case "--head-sha":
				args.headSha = argv[++index] ?? "";
				break;
			case "--repo":
				args.repo = argv[++index] ?? "";
				break;
			case "--strict-status-checks":
				args.strictStatusChecks = true;
				break;
			default:
				if (!args.pr) {
					args.pr = arg;
				} else {
					throw new Error(`Unknown argument: ${arg}`);
				}
		}
	}

	if (!args.pr) {
		throw new Error("Usage: node scripts/pr-ready-to-merge.mjs [--repo owner/name] [--head-sha sha] <pr-number-or-url>");
	}

	return args;
}

export function prNumberFromInput(value) {
	const input = String(value).trim();
	const match = input.match(
		/^(?:(\d+)(?:$|[?#])|https?:\/\/[^/]+\/[^/]+\/[^/]+\/pull\/(\d+)(?:$|[/?#]))/,
	);
	if (!match) {
		throw new Error(`Could not parse pull request number from ${value}`);
	}
	return Number(match[1] ?? match[2]);
}

export function parseRepoSpec(repo) {
	const parts = String(repo).trim().split("/").filter(Boolean);
	const [owner, name] = parts.length === 2 ? parts : parts.slice(-2);
	if (!owner || !name || parts.length < 2 || parts.length > 3) {
		throw new Error(`Expected repo as [host/]owner/name, got ${repo}`);
	}
	return {
		host: parts.length === 3 ? parts[0] : "",
		name,
		nameWithOwner: `${owner}/${name}`,
		owner,
	};
}

function ghJson(args) {
	const output = execFileSync("gh", args, { encoding: "utf8", stdio: ["ignore", "pipe", "pipe"] });
	return JSON.parse(output);
}

function ghText(args) {
	return execFileSync("gh", args, { encoding: "utf8", stdio: ["ignore", "pipe", "pipe"] }).trim();
}

function resolveRepo(repo) {
	if (repo) {
		return repo;
	}
	return ghText(["repo", "view", "--json", "nameWithOwner", "--jq", ".nameWithOwner"]);
}

function statusCheckName(node) {
	if (node.__typename === "CheckRun") {
		return node.name;
	}
	if (node.__typename === "StatusContext") {
		return node.context;
	}
	return node.__typename ?? "UNKNOWN";
}

function statusCheckProblem(node) {
	if (node.__typename === "CheckRun") {
		if (node.status !== "COMPLETED") {
			return `${statusCheckName(node)}: ${node.status.toLowerCase()}`;
		}
		if (!PASSING_CHECK_RUN_CONCLUSIONS.has(node.conclusion)) {
			return `${statusCheckName(node)}: ${String(node.conclusion || "UNKNOWN").toLowerCase()}`;
		}
		return null;
	}
	if (node.__typename === "StatusContext") {
		if (!PASSING_STATUS_CONTEXT_STATES.has(node.state)) {
			return `${statusCheckName(node)}: ${String(node.state || "UNKNOWN").toLowerCase()}`;
		}
		return null;
	}
	return `unknown status type: ${node.__typename ?? "UNKNOWN"}`;
}

function statusCheckProblems(pr, requiredStatusChecks, strictStatusChecks) {
	const checks = pr.statusCheckRollup ?? [];
	if (strictStatusChecks) {
		return {
			failures: checks.map(statusCheckProblem).filter(Boolean),
			warnings: [],
		};
	}

	const required = new Set(requiredStatusChecks ?? []);
	if (requiredStatusChecks === null || required.size === 0) {
		const warnings = checks
			.map(statusCheckProblem)
			.filter(Boolean)
			.map((problem) => `non-required check ${problem}`);
		if (requiredStatusChecks === null) {
			warnings.unshift("Required status-check metadata was unavailable; relying on GitHub merge state.");
		}
		return { failures: [], warnings };
	}

	const failures = [];
	const warnings = [];
	for (const name of required) {
		const matches = checks.filter((check) => statusCheckName(check) === name);
		if (matches.length === 0) {
			failures.push(`Required check ${name} is missing.`);
			continue;
		}
		failures.push(...matches.map(statusCheckProblem).filter(Boolean));
	}

	for (const check of checks) {
		if (required.has(statusCheckName(check))) {
			continue;
		}
		const problem = statusCheckProblem(check);
		if (problem) {
			warnings.push(`optional check ${problem}`);
		}
	}

	return { failures, warnings };
}

export function evaluateReadiness({
	pr,
	reviewThreads,
	expectedHeadSha = "",
	requiredStatusChecks = null,
	strictStatusChecks = false,
	bugbotFixedTitles = EMPTY_SET,
}) {
	const failures = [];
	const warnings = [];

	if (pr.state !== "OPEN") {
		failures.push(`PR is ${String(pr.state).toLowerCase()}, not open.`);
	}
	if (pr.isDraft) {
		failures.push("PR is still a draft.");
	}
	if (expectedHeadSha && pr.headRefOid !== expectedHeadSha) {
		failures.push(`PR head is ${pr.headRefOid}, expected ${expectedHeadSha}.`);
	}
	if (pr.mergeable !== "MERGEABLE") {
		failures.push(`PR mergeable state is ${pr.mergeable}.`);
	}
	if (!["CLEAN", "HAS_HOOKS", "UNSTABLE"].includes(pr.mergeStateStatus)) {
		failures.push(`PR merge state is ${pr.mergeStateStatus}.`);
	} else if (pr.mergeStateStatus === "UNSTABLE") {
		warnings.push("PR merge state is UNSTABLE because at least one non-required status is not passing.");
	}

	const unresolvedThreads = reviewThreads.filter((thread) =>
		threadBlocksAfterBugbotDisposition(thread, bugbotFixedTitles),
	);
	for (const thread of unresolvedThreads) {
		const firstComment = thread.comments?.nodes?.[0];
		const location = [thread.path, thread.line].filter(Boolean).join(":");
		failures.push(
			`Unresolved review thread ${thread.id}${location ? ` at ${location}` : ""}${firstComment?.url ? ` (${firstComment.url})` : ""}.`,
		);
	}

	const checkResult = statusCheckProblems(
		pr,
		requiredStatusChecks,
		strictStatusChecks,
	);
	failures.push(...checkResult.failures);
	warnings.push(...checkResult.warnings);

	if ((pr.statusCheckRollup ?? []).length === 0) {
		warnings.push("PR has no status checks in statusCheckRollup.");
	}

	return {
		failures,
		ready: failures.length === 0,
		warnings,
	};
}

export function fetchReviewThreads(owner, repo, number, queryGh = ghJson) {
	const threads = [];
	let cursor = "";

	do {
		const apiArgs = [
			"api",
			"graphql",
			"-f",
			`query=query($owner:String!,$repo:String!,$number:Int!,$after:String){
				repository(owner:$owner,name:$repo){
					pullRequest(number:$number){
						reviewThreads(first:100,after:$after){
							nodes{
								id
								isResolved
								isOutdated
								path
								line
								comments(first:20){nodes{url body author{login}}}
							}
							pageInfo{
								hasNextPage
								endCursor
							}
						}
					}
				}
			}`,
			"-f",
			`owner=${owner}`,
			"-f",
			`repo=${repo}`,
			"-F",
			`number=${number}`,
		];
		if (cursor) {
			apiArgs.push("-f", `after=${cursor}`);
		}

		const data = queryGh(apiArgs);
		const reviewThreads = data.data.repository.pullRequest.reviewThreads;
		threads.push(...reviewThreads.nodes);
		cursor = reviewThreads.pageInfo.hasNextPage ? reviewThreads.pageInfo.endCursor : "";
	} while (cursor);

	return threads;
}

export function fetchIssueComments(owner, repo, number, queryGh = ghJson) {
	const comments = [];
	let cursor = "";

	do {
		const apiArgs = [
			"api",
			"graphql",
			"-f",
			`query=query($owner:String!,$repo:String!,$number:Int!,$after:String){
				repository(owner:$owner,name:$repo){
					pullRequest(number:$number){
						comments(first:100,after:$after){
							nodes{body author{login}}
							pageInfo{hasNextPage endCursor}
						}
					}
				}
			}`,
			"-f",
			`owner=${owner}`,
			"-f",
			`repo=${repo}`,
			"-F",
			`number=${number}`,
		];
		if (cursor) {
			apiArgs.push("-f", `after=${cursor}`);
		}

		const data = queryGh(apiArgs);
		const connection = data.data.repository.pullRequest.comments;
		comments.push(...connection.nodes);
		cursor = connection.pageInfo.hasNextPage ? connection.pageInfo.endCursor : "";
	} while (cursor);

	return comments;
}

const BUGBOT_AUTOFIX_AUTHOR = /^cursor(?:\[bot\])?$/i;
const EMPTY_SET = new Set();

/**
 * A review thread is considered closed by Cursor Bugbot Autofix's
 * false-positive disposition when the most recent comment is Bugbot's own
 * explicit "determined this is a false positive" reply. Autofix posts that
 * disposition as a follow-up comment in the originating thread but does not
 * flip GitHub's `isResolved` flag, which would otherwise deadlock generated
 * mirror PRs on Bugbot's own false positives.
 *
 * The disposition must be the last comment so that a later human reply
 * ("still broken") keeps the thread blocking.
 *
 * @param {{ comments?: { nodes?: Array<{ author?: { login?: unknown }, body?: unknown }> } } | null | undefined} thread
 * @returns {boolean}
 */
export function isBugbotAutofixFalsePositive(thread) {
	const comments = thread?.comments?.nodes ?? [];
	if (comments.length === 0) {
		return false;
	}
	const last = comments[comments.length - 1];
	const author = String(last?.author?.login ?? "");
	const body = String(last?.body ?? "");
	return (
		BUGBOT_AUTOFIX_AUTHOR.test(author) &&
		/Bugbot Autofix/i.test(body) &&
		/false[ -]positive/i.test(body)
	);
}

/**
 * Extracts a review finding's title from the first comment of a thread. Bugbot
 * finding comments begin with a Markdown heading (`### <title>`); the same
 * title appears in Autofix's applied-fix disposition. Returns the trimmed
 * title, or null when the first comment is not a titled finding.
 *
 * @param {{ comments?: { nodes?: Array<{ body?: unknown }> } } | null | undefined} thread
 * @returns {string | null}
 */
export function reviewThreadFindingTitle(thread) {
	const body = String(thread?.comments?.nodes?.[0]?.body ?? "");
	const match = body.match(/^#{1,6}\s+(.+?)\s*$/m);
	return match ? match[1].trim() : null;
}

/**
 * Collects finding titles that Cursor Bugbot Autofix reported as fixed, from a
 * PR's top-level (issue) comments. Autofix posts an applied-fix disposition as
 * a top-level comment marked `<!-- BUGBOT_AUTOFIX_COMMENT -->` that lists one
 * or more `✅ Fixed: **<title>**` entries, where `<title>` matches the original
 * finding's `### <title>`. Returns the set of trimmed titles.
 *
 * @param {Array<{ body?: unknown }> | null | undefined} issueComments
 * @returns {Set<string>}
 */
export function parseBugbotAutofixFixedTitles(issueComments) {
	const titles = new Set();
	for (const comment of issueComments ?? []) {
		const body = String(comment?.body ?? "");
		if (!body.includes("BUGBOT_AUTOFIX_COMMENT")) {
			continue;
		}
		for (const match of body.matchAll(/✅\s*Fixed:\s*\*\*(.+?)\*\*/gi)) {
			const title = match[1].trim();
			if (title) {
				titles.add(title);
			}
		}
	}
	return titles;
}

/**
 * A review thread is considered closed by a Bugbot Autofix applied-fix
 * disposition when the finding's title is among the titles Autofix reported as
 * fixed on the PR. This is matched by title (not by thread id) because the
 * applied-fix disposition is a top-level comment rather than an inline reply.
 *
 * @param {{ comments?: { nodes?: Array<{ body?: unknown }> } } | null | undefined} thread
 * @param {Set<string> | null | undefined} fixedTitles
 * @returns {boolean}
 */
export function isBugbotAutofixResolvedByFix(thread, fixedTitles) {
	if (!fixedTitles || fixedTitles.size === 0) {
		return false;
	}
	const title = reviewThreadFindingTitle(thread);
	return title !== null && fixedTitles.has(title);
}

/**
 * Whether a review thread still blocks readiness after accounting for Cursor
 * Bugbot Autofix dispositions: GitHub resolution, an inline false-positive
 * disposition, or an applied-fix disposition matched by finding title.
 *
 * @param {{ isResolved?: boolean, comments?: { nodes?: unknown[] } }} thread
 * @param {Set<string> | null | undefined} fixedTitles
 * @returns {boolean}
 */
export function threadBlocksAfterBugbotDisposition(thread, fixedTitles) {
	if (thread?.isResolved) {
		return false;
	}
	if (isBugbotAutofixFalsePositive(thread)) {
		return false;
	}
	if (isBugbotAutofixResolvedByFix(thread, fixedTitles ?? EMPTY_SET)) {
		return false;
	}
	return true;
}

function fetchPullRequest(repo, number) {
	const args = [
		"pr",
		"view",
		String(number),
		"--json",
		"baseRefName,headRefOid,isDraft,mergeable,mergeStateStatus,state,statusCheckRollup,url",
	];
	if (repo) {
		args.push("--repo", repo);
	}
	return ghJson(args);
}

export function fetchRequiredStatusChecks(repo, branch, queryGh = ghJson) {
	if (!branch) {
		return null;
	}
	const encoded = encodeURIComponent(branch);
	try {
		let classic = [];
		try {
			const data = queryGh([
				"api",
				`repos/${repo}/branches/${encoded}/protection/required_status_checks`,
			]);
			classic = [
				...(data.contexts ?? []),
				...(data.checks ?? []).map((check) => check.context).filter(Boolean),
			];
		} catch (error) {
			if (!/HTTP 404/.test(`${error?.message ?? ""}${error?.stderr ?? ""}`)) {
				throw error;
			}
		}
		const rules = queryGh(["api", `repos/${repo}/rules/branches/${encoded}`]);
		const ruleset = rules
			.filter((rule) => rule.type === "required_status_checks")
			.flatMap((rule) => rule.parameters?.required_status_checks ?? [])
			.map((check) => check.context)
			.filter(Boolean);
		return Array.from(new Set([...classic, ...ruleset]));
	} catch {
		return null;
	}
}

function main() {
	const args = parseArgs(process.argv.slice(2));
	const number = prNumberFromInput(args.pr);
	const repo = resolveRepo(args.repo);
	const repoSpec = parseRepoSpec(repo);

	const pr = fetchPullRequest(repo, number);
	const reviewThreads = fetchReviewThreads(repoSpec.owner, repoSpec.name, number);
	const bugbotFixedTitles = parseBugbotAutofixFixedTitles(
		fetchIssueComments(repoSpec.owner, repoSpec.name, number),
	);
	const requiredStatusChecks = fetchRequiredStatusChecks(repoSpec.nameWithOwner, pr.baseRefName);
	const result = evaluateReadiness({
		pr,
		reviewThreads,
		expectedHeadSha: args.headSha,
		requiredStatusChecks,
		strictStatusChecks: args.strictStatusChecks,
		bugbotFixedTitles,
	});

	if (result.ready) {
		console.log(`Ready to merge: ${pr.url}`);
		for (const warning of result.warnings) {
			console.log(`warning: ${warning}`);
		}
		return;
	}

	console.error(`Not ready to merge: ${pr.url}`);
	for (const failure of result.failures) {
		console.error(`- ${failure}`);
	}
	for (const warning of result.warnings) {
		console.error(`warning: ${warning}`);
	}
	process.exit(1);
}

if (import.meta.url === `file://${process.argv[1]}`) {
	try {
		main();
	} catch (error) {
		console.error(error instanceof Error ? error.message : String(error));
		process.exit(1);
	}
}

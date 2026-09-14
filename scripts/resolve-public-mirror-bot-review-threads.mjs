#!/usr/bin/env node

import { execFileSync } from "node:child_process";
import process from "node:process";

const DEFAULT_REPO = "evalops/maestro";

// GitHub Apps usually have login suffix "[bot]". Some review apps post
// without that suffix; keep this list aligned with pr-feedback-audit.mjs.
const KNOWN_REVIEW_BOT_LOGINS =
	/^(?:cursor|coderabbitai|chatgpt-codex-connector|devin-ai-integration)\b/iu;

const LIST_REVIEW_THREADS_QUERY = `query($owner:String!,$repo:String!,$number:Int!,$after:String){
	repository(owner:$owner,name:$repo){
		pullRequest(number:$number){
			reviewThreads(first:100,after:$after){
				nodes{
					id
					isResolved
					isOutdated
					path
					line
					comments(first:20){nodes{url body author{__typename login}}}
				}
				pageInfo{
					hasNextPage
					endCursor
				}
			}
		}
	}
}`;

const RESOLVE_REVIEW_THREAD_MUTATION = `mutation($threadId:ID!){
	resolveReviewThread(input:{threadId:$threadId}){
		thread{id isResolved}
	}
}`;

function parseArgs(argv) {
	const args = {
		pr: 0,
		repo: DEFAULT_REPO,
		triageUrl: "",
	};

	for (let index = 0; index < argv.length; index += 1) {
		const arg = argv[index];
		switch (arg) {
			case "--pr":
				args.pr = Number(argv[++index] ?? "");
				break;
			case "--repo":
				args.repo = argv[++index] ?? "";
				break;
			case "--triage-url":
				args.triageUrl = argv[++index] ?? "";
				break;
			default:
				throw new Error(`Unknown argument: ${arg}`);
		}
	}

	if (!Number.isInteger(args.pr) || args.pr <= 0) {
		throw new Error("--pr must be a positive integer");
	}
	if (!args.repo || !args.repo.includes("/")) {
		throw new Error(`Expected --repo owner/name, got ${args.repo}`);
	}

	return args;
}

function ghJson(args) {
	const output = execFileSync("gh", args, {
		encoding: "utf8",
		stdio: ["ignore", "pipe", "pipe"],
	});
	return JSON.parse(output);
}

function graphqlData(payload, label) {
	if (payload?.errors?.length) {
		const messages = payload.errors
			.map((error) => String(error?.message ?? error))
			.join("; ");
		throw new Error(`${label} GraphQL errors: ${messages}`);
	}
	if (!payload?.data) {
		throw new Error(`${label} returned no GraphQL data`);
	}
	return payload.data;
}

export function firstCommentAuthor(thread) {
	return thread?.comments?.nodes?.[0]?.author ?? null;
}

export function isReviewBotAuthor(author) {
	if (!author || typeof author !== "object") {
		return false;
	}
	const typename = String(author.__typename ?? "").trim();
	if (typename === "Bot") {
		return true;
	}
	const login = String(author.login ?? "").trim();
	if (!login) {
		return false;
	}
	if (login.toLowerCase().endsWith("[bot]")) {
		return true;
	}
	return KNOWN_REVIEW_BOT_LOGINS.test(login);
}

export function shouldResolveReviewThread(thread) {
	if (!thread || thread.isResolved) {
		return false;
	}
	if (!thread.id) {
		return false;
	}
	return isReviewBotAuthor(firstCommentAuthor(thread));
}

export function formatSummaryComment({
	resolvedCount,
	humanCount,
	authors,
	triageUrl,
}) {
	const uniqueAuthors = [...new Set(authors.filter(Boolean))];
	const authorNote = uniqueAuthors.length ? ` (${uniqueAuthors.join(", ")})` : "";
	const lines = [
		`Resolved ${resolvedCount} review-bot thread(s) on this generated public mirror PR${authorNote}.`,
		`Left ${humanCount} human-started thread(s) unresolved.`,
		"",
		"Review of mirrored code belongs on the mono source PR.",
	];
	if (triageUrl) {
		lines.push(`See ${triageUrl} for the triage pattern.`);
	}
	return `${lines.join("\n")}\n`;
}

function flagValue(args, name) {
	const prefix = `${name}=`;
	for (let index = 0; index < args.length; index += 1) {
		const arg = args[index];
		if ((arg === "-f" || arg === "-F") && (args[index + 1] ?? "").startsWith(prefix)) {
			return args[index + 1].slice(prefix.length);
		}
		if (arg.startsWith(`-f${prefix}`) || arg.startsWith(`-F${prefix}`)) {
			return arg.slice(arg.indexOf("=") + 1);
		}
	}
	return "";
}

export function isResolveReviewThreadRequest(args) {
	const query = flagValue(args, "query");
	return /\bresolveReviewThread\b/.test(query);
}

export function fetchReviewThreads(owner, repo, number, queryGh) {
	const threads = [];
	let cursor = "";

	do {
		const apiArgs = [
			"api",
			"graphql",
			"-f",
			`query=${LIST_REVIEW_THREADS_QUERY}`,
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

		const data = graphqlData(queryGh(apiArgs), "list review threads");
		const pullRequest = data.repository?.pullRequest;
		if (!pullRequest) {
			throw new Error(`No pull request ${owner}/${repo}#${number}`);
		}
		const reviewThreads = pullRequest.reviewThreads;
		if (!reviewThreads) {
			throw new Error(`Missing reviewThreads on ${owner}/${repo}#${number}`);
		}
		threads.push(...(reviewThreads.nodes ?? []));
		cursor = reviewThreads.pageInfo?.hasNextPage
			? reviewThreads.pageInfo.endCursor
			: "";
	} while (cursor);

	return threads;
}

function resolveReviewThread(queryGh, threadId) {
	const payload = queryGh([
		"api",
		"graphql",
		"-f",
		`query=${RESOLVE_REVIEW_THREAD_MUTATION}`,
		"-f",
		`threadId=${threadId}`,
	]);
	const data = graphqlData(payload, `resolveReviewThread ${threadId}`);
	if (!data.resolveReviewThread?.thread?.isResolved) {
		throw new Error(`resolveReviewThread did not resolve ${threadId}`);
	}
}

export function resolvePublicMirrorBotReviewThreads({
	owner,
	repo,
	number,
	triageUrl = "",
	ghJson: queryGh,
	postComment,
}) {
	const threads = fetchReviewThreads(owner, repo, number, queryGh);
	const toResolve = [];
	const authors = [];
	let humanCount = 0;

	for (const thread of threads) {
		if (thread?.isResolved) {
			continue;
		}
		if (shouldResolveReviewThread(thread)) {
			toResolve.push(thread);
			const login = String(firstCommentAuthor(thread)?.login ?? "").trim();
			if (login && !authors.includes(login)) {
				authors.push(login);
			}
			continue;
		}
		humanCount += 1;
	}

	const resolvedIds = [];
	for (const thread of toResolve) {
		resolveReviewThread(queryGh, thread.id);
		resolvedIds.push(thread.id);
	}

	const commentBody = formatSummaryComment({
		authors,
		humanCount,
		resolvedCount: resolvedIds.length,
		triageUrl,
	});
	postComment(commentBody);

	return {
		commentBody,
		humanCount,
		resolvedIds,
	};
}

function defaultPostComment(repo, number, body) {
	execFileSync(
		"gh",
		[
			"api",
			"--method",
			"POST",
			`repos/${repo}/issues/${number}/comments`,
			"--input",
			"-",
		],
		{
			encoding: "utf8",
			input: JSON.stringify({ body }),
			stdio: ["pipe", "pipe", "pipe"],
		},
	);
}

function main() {
	const args = parseArgs(process.argv.slice(2));
	const [owner, repoName] = args.repo.split("/");
	const result = resolvePublicMirrorBotReviewThreads({
		ghJson,
		number: args.pr,
		owner,
		postComment: (body) => defaultPostComment(args.repo, args.pr, body),
		repo: repoName,
		triageUrl: args.triageUrl,
	});
	console.log(
		`Resolved ${result.resolvedIds.length} review-bot thread(s) on ${args.repo}#${args.pr}; left ${result.humanCount} human-started thread(s) unresolved.`,
	);
}

export {
	KNOWN_REVIEW_BOT_LOGINS,
	LIST_REVIEW_THREADS_QUERY,
	RESOLVE_REVIEW_THREAD_MUTATION,
	flagValue,
};

if (import.meta.url === `file://${process.argv[1]}`) {
	try {
		main();
	} catch (error) {
		console.error(error instanceof Error ? error.message : String(error));
		process.exit(1);
	}
}

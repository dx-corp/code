import assert from "node:assert/strict";
import { existsSync, readFileSync } from "node:fs";
import test from "node:test";
import { fileURLToPath } from "node:url";

import { informationalReviewFeedback } from "./pr-feedback-audit.mjs";

import {
	flagValue,
	formatSummaryComment,
	isResolveReviewThreadRequest,
	isReviewBotAuthor,
	LIST_REVIEW_THREADS_QUERY,
	resolvePublicMirrorBotReviewThreads,
	shouldResolveReviewThread,
} from "./resolve-public-mirror-bot-review-threads.mjs";

const ROOT_SYNC_WORKFLOW = fileURLToPath(
	new URL(
		"../../../.github/workflows/maestro-sync-public-release-mirror.yml",
		import.meta.url,
	),
);

function thread({
	id,
	login,
	typename = "User",
	isResolved = false,
	isOutdated = false,
}) {
	return {
		id,
		isOutdated,
		isResolved,
		comments: {
			nodes: [
				{
					author: login == null ? null : { __typename: typename, login },
					body: `comment on ${id}`,
					url: `https://example.test/${id}`,
				},
			],
		},
	};
}

function page(nodes, { hasNextPage = false, endCursor = "" } = {}) {
	return {
		nodes,
		pageInfo: { endCursor, hasNextPage },
	};
}

function makeGhJson(pages) {
	const resolvedIds = [];
	let listCalls = 0;
	const comments = [];

	const ghJson = (args) => {
		if (isResolveReviewThreadRequest(args)) {
			const threadId = flagValue(args, "threadId");
			resolvedIds.push(threadId);
			return {
				data: {
					resolveReviewThread: {
						thread: { id: threadId, isResolved: true },
					},
				},
			};
		}

		const current = pages[listCalls] ?? page([]);
		listCalls += 1;
		return {
			data: {
				repository: {
					pullRequest: {
						reviewThreads: current,
					},
				},
			},
		};
	};

	return {
		comments,
		ghJson,
		listCalls: () => listCalls,
		postComment: (body) => {
			comments.push(body);
		},
		resolvedIds,
	};
}

test("only exact known review-bot logins qualify for automatic resolution", () => {
	assert.equal(isReviewBotAuthor({ login: "chatgpt-codex-connector" }), true);
	assert.equal(isReviewBotAuthor({ login: "devin-ai-integration" }), true);
	assert.equal(isReviewBotAuthor({ login: "cursor[bot]" }), true);
	assert.equal(isReviewBotAuthor({ login: "coderabbitai" }), true);
	assert.equal(isReviewBotAuthor({ login: "cursor-reviewer" }), false);
	assert.equal(isReviewBotAuthor({ login: "coderabbitai-dev" }), false);
	assert.equal(isReviewBotAuthor({ login: "github-actions[bot]" }), false);
	assert.equal(
		isReviewBotAuthor({ __typename: "Bot", login: "some-review-app" }),
		false,
	);
	assert.equal(isReviewBotAuthor({ login: "alice" }), false);
	assert.equal(isReviewBotAuthor({ __typename: "User", login: "alice" }), false);
	assert.equal(isReviewBotAuthor(null), false);
	assert.equal(isReviewBotAuthor({ login: "" }), false);
});

test("feedback audit does not discard informational-looking human comments", () => {
	assert.equal(informationalReviewFeedback("## Summary", "cursor[bot]"), true);
	assert.equal(informationalReviewFeedback("## Summary", "cursor-reviewer"), false);
	assert.equal(informationalReviewFeedback("**Info:** Please review", "coderabbitai-dev"), false);
});

test("thread query transfers only the first author needed for classification", () => {
	assert.match(
		LIST_REVIEW_THREADS_QUERY,
		/comments\(first:1\)\{nodes\{author\{__typename login\}\}\}/,
	);
	assert.doesNotMatch(LIST_REVIEW_THREADS_QUERY, /\b(?:body|url)\b/);
});

test("resolved threads are never selected even when the author is a bot", () => {
	assert.equal(
		shouldResolveReviewThread(
			thread({ id: "PRRT_resolved", login: "chatgpt-codex-connector", isResolved: true }),
		),
		false,
	);
	assert.equal(
		shouldResolveReviewThread(
			thread({ id: "PRRT_human", login: "alice" }),
		),
		false,
	);
	assert.equal(
		shouldResolveReviewThread(
			thread({
				id: "PRRT_outdated_bot",
				login: "cursor[bot]",
				isOutdated: true,
			}),
		),
		true,
	);
});

test("resolveReviewThread is called only for unresolved bot-started threads", () => {
	const fixtures = [
		thread({ id: "PRRT_bot", login: "chatgpt-codex-connector" }),
		thread({ id: "PRRT_human", login: "alice" }),
		thread({
			id: "PRRT_resolved_bot",
			login: "devin-ai-integration",
			isResolved: true,
		}),
		thread({
			id: "PRRT_outdated_bot",
			login: "cursor[bot]",
			isOutdated: true,
		}),
		thread({
			id: "PRRT_outdated_human",
			login: "bob",
			isOutdated: true,
		}),
		thread({ id: "PRRT_prefixed_human", login: "cursor-reviewer" }),
		thread({ id: "PRRT_devin", login: "devin-ai-integration" }),
		thread({
			id: "PRRT_typename_bot",
			login: "some-review-app",
			typename: "Bot",
		}),
		thread({ id: "PRRT_missing_author", login: null }),
		{
			id: "PRRT_human_then_bot",
			isOutdated: false,
			isResolved: false,
			comments: {
				nodes: [
					{
						author: { __typename: "User", login: "alice" },
						body: "human started this",
						url: "https://example.test/PRRT_human_then_bot",
					},
					{
						author: { login: "chatgpt-codex-connector" },
						body: "bot replied",
						url: "https://example.test/PRRT_human_then_bot-reply",
					},
				],
			},
		},
	];
	const fake = makeGhJson([page(fixtures)]);
	const result = resolvePublicMirrorBotReviewThreads({
		ghJson: fake.ghJson,
		number: 1122,
		owner: "evalops",
		postComment: fake.postComment,
		repo: "maestro",
		triageUrl: "https://example.test/triage/9100",
	});

	assert.deepEqual(result.resolvedIds, [
		"PRRT_bot",
		"PRRT_outdated_bot",
		"PRRT_devin",
	]);
	assert.deepEqual(fake.resolvedIds, result.resolvedIds);
	assert.equal(result.humanCount, 6);
	assert.equal(fake.comments.length, 1);
	assert.match(fake.comments[0], /Resolved 3 review-bot thread\(s\)/);
	assert.match(fake.comments[0], /Left 6 human-started thread\(s\) unresolved/);
	assert.match(fake.comments[0], /Review of mirrored code belongs on the mono source PR/);
	assert.match(fake.comments[0], /https:\/\/example\.test\/triage\/9100/);
	assert.doesNotMatch(fake.comments[0], /PRRT_human/);
});

test("paginated GraphQL thread lists still resolve only bot-started threads", () => {
	const fake = makeGhJson([
		page([thread({ id: "PRRT_human_page1", login: "alice" })], {
			endCursor: "cursor-1",
			hasNextPage: true,
		}),
		page([thread({ id: "PRRT_bot_page2", login: "cursor[bot]" })]),
	]);
	const result = resolvePublicMirrorBotReviewThreads({
		ghJson: fake.ghJson,
		number: 1122,
		owner: "evalops",
		postComment: fake.postComment,
		repo: "maestro",
		triageUrl: "https://example.test/triage/9100",
	});
	assert.deepEqual(result.resolvedIds, ["PRRT_bot_page2"]);
	assert.equal(result.humanCount, 1);
	assert.equal(fake.listCalls(), 2);
});

test("summary comment still posts when no bot threads need resolving", () => {
	const fake = makeGhJson([
		page([thread({ id: "PRRT_human", login: "alice" })]),
	]);
	const result = resolvePublicMirrorBotReviewThreads({
		ghJson: fake.ghJson,
		number: 1122,
		owner: "evalops",
		postComment: fake.postComment,
		repo: "maestro",
		triageUrl: "https://example.test/triage/9100",
	});
	assert.deepEqual(result.resolvedIds, []);
	assert.equal(fake.comments.length, 1);
	assert.equal(
		fake.comments[0],
		formatSummaryComment({
			authors: [],
			humanCount: 1,
			resolvedCount: 0,
			triageUrl: "https://example.test/triage/9100",
		}),
	);
});

test("sync workflow resolves bot threads after opening the PR and before auto-merge", (t) => {
	if (!existsSync(ROOT_SYNC_WORKFLOW)) {
		t.skip("mono-root sync workflow is not in this checkout");
		return;
	}
	const workflow = readFileSync(ROOT_SYNC_WORKFLOW, "utf8");
	const openPr = workflow.indexOf("- name: Open or update public sync PR");
	const resolve = workflow.indexOf(
		"- name: Resolve review-bot threads on public sync PR",
	);
	const autoMerge = workflow.indexOf("- name: Enable auto-merge on public sync PR");
	assert.notEqual(openPr, -1);
	assert.notEqual(resolve, -1);
	assert.notEqual(autoMerge, -1);
	assert.ok(openPr < resolve);
	assert.ok(resolve < autoMerge);
	const resolveBlock = workflow.slice(resolve, autoMerge);
	assert.match(
		resolveBlock,
		/resolve-public-mirror-bot-review-threads\.mjs/,
	);
	assert.match(resolveBlock, /steps\.token\.outputs\.token/);
	assert.match(resolveBlock, /--triage-url /);
	assert.match(resolveBlock, /\/issues\/9100\b/);
	assert.doesNotMatch(resolveBlock, /continue-on-error/);
	assert.match(workflow, /permission-pull-requests:\s*write/);
});

import assert from "node:assert/strict";
import { mkdtempSync, mkdirSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";

import {
	evaluateRequiredStatusChecks,
	fetchRequiredContexts,
} from "./check-required-status-checks.mjs";

function withWorkflows(files, contexts) {
	const root = mkdtempSync(join(tmpdir(), "required-checks-"));
	try {
		const workflowsDir = join(root, ".github/workflows");
		mkdirSync(workflowsDir, { recursive: true });
		for (const [name, text] of Object.entries(files)) {
			writeFileSync(join(workflowsDir, name), text);
		}
		return evaluateRequiredStatusChecks({ contexts, root });
	} finally {
		rmSync(root, { recursive: true, force: true });
	}
}

const cleanWorkflow = `name: lint
on:
  pull_request:
  push:
    branches: [main]
    paths:
      - ".github/workflows/**"
jobs:
  lint:
    runs-on: ubuntu-latest
    steps:
      - run: echo ok
`;

test("required context reported by an unfiltered pull_request job passes", () => {
	const { failures } = withWorkflows({ "lint.yml": cleanWorkflow }, ["lint"]);
	assert.deepEqual(failures, []);
});

test("paths filter on the pull_request trigger fails", () => {
	const workflow = cleanWorkflow.replace(
		"  pull_request:\n",
		'  pull_request:\n    paths:\n      - "docs/**"\n',
	);
	const { failures } = withWorkflows({ "lint.yml": workflow }, ["lint"]);
	assert.equal(failures.length, 1);
	assert.match(failures[0], /paths:\/paths-ignore: filter/u);
	assert.match(failures[0], /"lint"/u);
});

test("paths-ignore filter on the pull_request trigger fails", () => {
	const workflow = cleanWorkflow.replace(
		"  pull_request:\n",
		'  pull_request:\n    paths-ignore:\n      - "docs/**"\n',
	);
	const { failures } = withWorkflows({ "lint.yml": workflow }, ["lint"]);
	assert.equal(failures.length, 1);
	assert.match(failures[0], /paths:\/paths-ignore: filter/u);
});

test("paths filter on push only does not fail", () => {
	const { failures } = withWorkflows({ "lint.yml": cleanWorkflow }, ["lint"]);
	assert.deepEqual(failures, []);
});

test("required context with no matching job fails", () => {
	const { failures } = withWorkflows({ "lint.yml": cleanWorkflow }, ["missing"]);
	assert.equal(failures.length, 1);
	assert.match(failures[0], /does not map to any job/u);
});

test("required context matching a job display name passes", () => {
	const workflow = cleanWorkflow.replace(
		"  lint:\n",
		'  lint:\n    name: Lint Gate\n',
	);
	const { failures } = withWorkflows({ "lint.yml": workflow }, ["Lint Gate"]);
	assert.deepEqual(failures, []);
});

test("reusable workflow caller/callee context maps to the caller job", () => {
	const workflow = `name: guard
on:
  pull_request:
jobs:
  review-gate:
    uses: evalops/.github/.github/workflows/guard.yml@abcdef
`;
	const { failures } = withWorkflows(
		{ "guard.yml": workflow },
		["review-gate / review-gate"],
	);
	assert.deepEqual(failures, []);
});

test("matrix-suffixed context maps to the base job", () => {
	const { failures } = withWorkflows({ "lint.yml": cleanWorkflow }, [
		"lint (ubuntu-latest)",
	]);
	assert.deepEqual(failures, []);
});

test("required context from a workflow that never runs on pull_request fails", () => {
	const workflow = `name: release
on:
  push:
    branches: [main]
jobs:
  release:
    runs-on: ubuntu-latest
    steps:
      - run: echo ok
`;
	const { failures } = withWorkflows({ "release.yml": workflow }, ["release"]);
	assert.equal(failures.length, 1);
	assert.match(failures[0], /never runs on pull_request/u);
});

test("fetchRequiredContexts unions classic protection and ruleset contexts", () => {
	const ok = (stdout) => ({ status: 0, stdout, stderr: "" });
	const run = (_cmd, args) =>
		args[1].includes("/rules/")
			? ok('["validate","platform-ci"]')
			: ok('["validate","legacy"]');
	assert.deepEqual(
		fetchRequiredContexts("dx-corp/mono", "main", { strict: true, run }),
		["validate", "legacy", "platform-ci"],
	);
});

test("fetchRequiredContexts treats classic 404 as empty and reads the ruleset", () => {
	const run = (_cmd, args) =>
		args[1].includes("/rules/")
			? { status: 0, stdout: '["validate","semgrep"]', stderr: "" }
			: { status: 1, stdout: "", stderr: "gh: Not Found (HTTP 404)" };
	assert.deepEqual(
		fetchRequiredContexts("dx-corp/mono", "main", { strict: true, run }),
		["validate", "semgrep"],
	);
});

test("fetchRequiredContexts strict still fails on classic 404 with no ruleset checks", () => {
	const run = (_cmd, args) =>
		args[1].includes("/rules/")
			? { status: 0, stdout: "[]", stderr: "" }
			: { status: 1, stdout: "", stderr: "gh: Not Found (HTTP 404)" };
	assert.throws(
		() => fetchRequiredContexts("dx-corp/mono", "main", { strict: true, run }),
		/fail closed/u,
	);
});

test("fetchRequiredContexts strict still fails on classic 403", () => {
	const run = () => ({ status: 1, stdout: "", stderr: "gh: Forbidden (HTTP 403)" });
	assert.throws(
		() => fetchRequiredContexts("dx-corp/mono", "main", { strict: true, run }),
		/fail closed/u,
	);
});

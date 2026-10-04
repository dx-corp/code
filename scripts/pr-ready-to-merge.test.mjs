import assert from "node:assert/strict";
import { test } from "node:test";

import { fetchRequiredStatusChecks } from "./pr-ready-to-merge.mjs";

const rules = [
	{ type: "pull_request", parameters: {} },
	{
		type: "required_status_checks",
		parameters: {
			required_status_checks: [{ context: "validate" }, { context: "platform-ci" }],
		},
	},
];

function fakeGh({ classic, ruleset }) {
	const calls = [];
	const queryGh = (args) => {
		calls.push(args[1]);
		const result = args[1].includes("/protection/") ? classic : ruleset;
		if (result instanceof Error) {
			throw result;
		}
		return result;
	};
	return { calls, queryGh };
}

test("classic 404 is an empty classic set and the ruleset supplies the contexts", () => {
	const { calls, queryGh } = fakeGh({
		classic: new Error("gh: Required status checks not enabled (HTTP 404)"),
		ruleset: rules,
	});
	assert.deepEqual(fetchRequiredStatusChecks("dx-corp/mono", "main", queryGh), [
		"validate",
		"platform-ci",
	]);
	assert.deepEqual(calls, [
		"repos/dx-corp/mono/branches/main/protection/required_status_checks",
		"repos/dx-corp/mono/rules/branches/main",
	]);
});

test("classic and ruleset contexts are unioned without duplicates", () => {
	const { queryGh } = fakeGh({
		classic: { contexts: ["validate"], checks: [{ context: "legacy" }] },
		ruleset: rules,
	});
	assert.deepEqual(
		fetchRequiredStatusChecks("dx-corp/mono", "main", queryGh).sort(),
		["legacy", "platform-ci", "validate"],
	);
});

test("non-404 classic errors still return null", () => {
	const { queryGh } = fakeGh({
		classic: new Error("gh: Resource not accessible by integration (HTTP 403)"),
		ruleset: rules,
	});
	assert.equal(fetchRequiredStatusChecks("dx-corp/mono", "main", queryGh), null);
});

test("ruleset errors return null", () => {
	const { queryGh } = fakeGh({
		classic: { contexts: ["validate"], checks: [] },
		ruleset: new Error("HTTP 500"),
	});
	assert.equal(fetchRequiredStatusChecks("dx-corp/mono", "main", queryGh), null);
});

test("missing branch returns null", () => {
	assert.equal(fetchRequiredStatusChecks("dx-corp/mono", "", () => []), null);
});

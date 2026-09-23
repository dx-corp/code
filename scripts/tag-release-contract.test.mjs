import assert from "node:assert/strict";
import { existsSync, readFileSync } from "node:fs";
import { test } from "node:test";

const workflow = readFileSync(new URL("../.github/workflows/tag-release.yml", import.meta.url), "utf8");
const hasInternalMirror = existsSync(
	new URL("../.github/workflows/public-release-mirror.yml", import.meta.url),
);

test("tag-release does not cancel in-progress runs", () => {
	assert.doesNotMatch(workflow, /cancel-in-progress: true/);
});

test("tag-release dispatches a public release after tagging", () => {
	assert.match(workflow, /dispatch-public-release:/);
});

test("internal tag-release dispatches public-release-mirror after creating a tag", () => {
	// Public owns .github/workflows/**, so public-release-mirror.yml is absent
	// there and this dispatch job is not part of the public tag-release.yml.
	if (!hasInternalMirror) {
		return;
	}
	const internalWorkflow = readFileSync(
		new URL("../../../.github/workflows/maestro-tag-release.yml", import.meta.url),
		"utf8",
	);
	assert.match(internalWorkflow, /dispatch-public-release-mirror:/);
	assert.match(internalWorkflow, /github\.repository == 'dx-corp\/mono'/);
	assert.match(internalWorkflow, /needs\.tag-current-version\.outputs\.tag_exists != 'true'/);
	assert.match(internalWorkflow, /dispatch maestro-public-release-mirror\.yml/);
	assert.match(internalWorkflow, /dispatch maestro-release\.yml/);
	assert.match(internalWorkflow, /curl --config - --fail-with-body/);
	assert.match(internalWorkflow, /"publish_npm":"false"/);
	assert.match(internalWorkflow, /resolve-maestro-version-commit\.py/);
	assert.match(internalWorkflow, /tag-target-sha: \$\{\{ steps\.tag-target\.outputs\.commit \}\}/);
});

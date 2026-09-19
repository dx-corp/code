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
	assert.match(workflow, /dispatch-public-release-mirror:/);
	assert.match(workflow, /github\.repository == 'evalops\/mono'/);
	assert.match(workflow, /needs\.tag-current-version\.outputs\.tag_exists != 'true'/);
	assert.match(workflow, /gh workflow run public-release-mirror/);
	assert.match(workflow, /--field "publish_npm=false"/);
});

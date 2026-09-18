import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";
import {
	conformanceFixtureCommand,
	dockerConformanceRunArgs,
	validateConformanceExecutionOrder,
	validateConformanceReceipt,
} from "./run-runtime-conformance.mjs";

const fixture = JSON.parse(
	readFileSync("packages/runtime-contracts-rs/fixtures/runtime-conformance-v1.json", "utf8"),
);

function receiptWithOutcome(outcome) {
	return {
		cases: fixture.cases.map((name) => ({
			name,
			tool: "test",
			outcome: name === "wrong_session_rejected" ? outcome : "passed",
		})),
	};
}

test("expected wrong-session rejection is a passing conformance case", () => {
	assert.doesNotThrow(() => validateConformanceReceipt(receiptWithOutcome("passed")));
});

test("unexpected negative-case receipt outcome is rejected", () => {
	assert.throws(
		() => validateConformanceReceipt(receiptWithOutcome("rejected:409")),
		/expected negative cases must be recorded as passed/,
	);
});

test("daemon-local fixture creation is a shell step before file search", () => {
	assert.equal(
		conformanceFixtureCommand(),
		"printf 'runtime conformance fixture\\nreversible test data\\n' > runtime-conformance-fixture.txt; printf runtime-conformance-shell",
	);
	assert.doesNotMatch(conformanceFixtureCommand(), /docker cp|\/tmp\//);

	const executionOrder = [
		"startup_identity_and_readiness",
		"wrong_session_rejected",
		"harmless_shell_command",
		"file_search_and_read",
		"approval_request_and_resolution",
		"idempotent_response_replay",
		"drain_terminal_receipt",
	];
	assert.doesNotThrow(() => validateConformanceExecutionOrder(executionOrder));
	assert.throws(
		() => validateConformanceExecutionOrder(executionOrder.toSpliced(2, 2, "file_search_and_read", "harmless_shell_command")),
		/fixture before file search/,
	);
	assert.throws(
		() => validateConformanceExecutionOrder(executionOrder.toSpliced(2, 1)),
		/cover every runtime case/,
	);
});

test("detached Docker conformance keeps the fixture stdin open", () => {
	const args = dockerConformanceRunArgs({
		containerName: "maestro-runtime-conformance-test",
		dockerImage: "ghcr.io/dx-corp/maestro@sha256:" + "a".repeat(64),
	});

	assert.deepEqual(args.slice(0, 4), ["run", "-d", "-i", "--rm"]);
	assert.deepEqual(args.slice(-4), [
		"--mount",
		"type=tmpfs,destination=/conformance-workspace",
		"ghcr.io/dx-corp/maestro@sha256:" + "a".repeat(64),
		"conformance",
	]);
	assert.equal(args.some((argument) => argument.includes("/tmp/maestro-runtime-conformance-test")), false);
	assert.equal(args.at(-1), "conformance");
});

test("local tagged image pins run by image id so Docker does not pull Docker Hub", () => {
	const digest = "sha256:" + "b".repeat(64);
	const args = dockerConformanceRunArgs({
		containerName: "maestro-runtime-conformance-local",
		dockerImage: `maestro-conformance:123-1@${digest}`,
	});
	assert.equal(args.at(-2), digest);
});

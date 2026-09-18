import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
import { readFileSync } from "node:fs";
import { test } from "node:test";
import { buildRuntimeConformanceIdentity, buildRuntimePassport } from "./generate-runtime-passport.mjs";

const options = {
	artifact_kind: "native_binary",
	artifact_name: "maestro-linux-x64",
	artifact_digest: `sha256:${"a".repeat(64)}`,
	source_sha: "b".repeat(40),
	compatibility_digest: `sha256:${"c".repeat(64)}`,
	launch_spec_version: "evalops.maestro.hosted-launch-spec.v1",
	receipt_version: "evalops.maestro.runtime-receipt.v1",
	rustc: "rustc 1.90.0",
	target: "x86_64-unknown-linux-gnu",
	profiles: ["hosted-http-sse-v1"],
};

test("passport binds the exact artifact and executable conformance suite", () => {
	const passport = buildRuntimePassport(options);
	const contract = JSON.parse(
		readFileSync("packages/runtime-contracts-rs/fixtures/runtime-passport-contract-v1.json", "utf8"),
	);
	assert.equal(passport.schemaVersion, "evalops.maestro.runtime-passport.v1");
	assert.deepEqual(Object.keys(passport).sort(), [...contract.fields].sort());
	assert.ok(contract.artifactKinds.includes(passport.artifact.kind));
	assert.deepEqual(passport.behaviorProfiles, contract.behaviorProfiles);
	assert.equal(passport.artifact.digest, options.artifact_digest);
	assert.match(passport.conformance.fixtureDigest, /^sha256:[0-9a-f]{64}$/);
	assert.match(passport.conformance.suiteDigest, /^sha256:[0-9a-f]{64}$/);
	assert.deepEqual(passport.behaviorProfiles, ["hosted-http-sse-v1"]);
});

test("passport generator rejects an unsupported artifact kind", () => {
	assert.throws(
		() => buildRuntimePassport({ ...options, artifact_kind: "source_tree" }),
		/artifact kind must be native_binary or oci_image/,
	);
});

test("passport generator rejects an unexercised behavior profile", () => {
	assert.throws(
		() => buildRuntimePassport({ ...options, profiles: ["hosted-resident-v1"] }),
		/not exercised by the conformance suite/,
	);
});

test("passport changes when the exact artifact digest changes", () => {
	const first = buildRuntimePassport(options);
	const second = buildRuntimePassport({
		...options,
		artifact_digest: `sha256:${"d".repeat(64)}`,
	});
	assert.notEqual(first.artifact.digest, second.artifact.digest);
	assert.equal(first.conformance.suiteDigest, second.conformance.suiteDigest);
});

test("canonical conformance fixture is present and versioned", () => {
	const fixture = JSON.parse(
		readFileSync("packages/runtime-contracts-rs/fixtures/runtime-conformance-v1.json", "utf8"),
	);
	assert.equal(fixture.schemaVersion, "evalops.maestro.runtime-conformance.v1");
	assert.equal(fixture.profile, "hosted-http-sse-v1");
	assert.ok(fixture.cases.includes("approval_request_and_resolution"));
});

test("passport suite digest includes the executable native fixture source", () => {
	const identity = buildRuntimeConformanceIdentity();
	const nativeFixtureDigest = `sha256:${createHash("sha256")
		.update(readFileSync("packages/local-host-rs/src/hosted_runner_conformance.rs"))
		.digest("hex")}`;
	assert.equal(identity.nativeFixtureDigest, nativeFixtureDigest);
	assert.equal(buildRuntimePassport(options).conformance.suiteDigest, identity.suiteDigest);
});

test("conformance rejects a tag-only OCI reference without an artifact digest", () => {
	let error;
	try {
		execFileSync(
			process.execPath,
			["scripts/run-runtime-conformance.mjs", "--docker-image", "ghcr.io/dx-corp/maestro:review"],
			{ encoding: "utf8", stdio: ["ignore", "pipe", "pipe"] },
		);
	} catch (caught) {
		error = caught;
	}
	assert.ok(error);
	assert.match(`${error.stderr}\n${error.stdout}`, /--docker-image must be pinned/);
});

test("conformance rejects an artifact digest that differs from the pinned OCI image", () => {
	let error;
	try {
		execFileSync(
			process.execPath,
			[
				"scripts/run-runtime-conformance.mjs",
				"--docker-image",
				`ghcr.io/dx-corp/maestro@sha256:${"a".repeat(64)}`,
				"--artifact-digest",
				`sha256:${"b".repeat(64)}`,
			],
			{ encoding: "utf8", stdio: ["ignore", "pipe", "pipe"] },
		);
	} catch (caught) {
		error = caught;
	}
	assert.ok(error);
	assert.match(`${error.stderr}\n${error.stdout}`, /--artifact-digest must match/);
});

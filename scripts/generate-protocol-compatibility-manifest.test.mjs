import assert from "node:assert/strict";
import {
	mkdirSync,
	mkdtempSync,
	existsSync,
	readFileSync,
	rmSync,
	writeFileSync,
} from "node:fs";
import { createHash } from "node:crypto";
import { tmpdir } from "node:os";
import { dirname, resolve } from "node:path";
import { spawnSync } from "node:child_process";
import test from "node:test";
import { fileURLToPath } from "node:url";

import {
	buildCompatibilityManifest,
	canonicalizeForDigest,
	readCanonicalSources,
	renderCompatibilityManifest,
} from "./generate-protocol-compatibility-manifest.mjs";
import {
	CommandFailure,
	assertPublicationIsFresh,
	reserveAndDispatch,
} from "./recovery-publisher-once.mjs";

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const SCRIPT = resolve(ROOT, "scripts/generate-protocol-compatibility-manifest.mjs");
const CHECKED_IN_MANIFEST = resolve(
	ROOT,
	"proto/maestro/v1/protocol-compatibility-manifest.json",
);
const REPOSITORY_GUIDANCE = existsSync(resolve(ROOT, "AGENTS.md"))
	? readFileSync(resolve(ROOT, "AGENTS.md"), "utf8")
	: "";
const IS_PUBLIC_PROJECTION = REPOSITORY_GUIDANCE.includes(
	"This repository is a generated public mirror.",
);

test("checked-in compatibility manifest matches canonical protocol sources", () => {
	assert.equal(
		readFileSync(CHECKED_IN_MANIFEST, "utf8"),
		renderCompatibilityManifest(),
	);
});

test("manifest binds release identity without changing compatibility digest", () => {
	const template = buildCompatibilityManifest();
	const receipt = buildCompatibilityManifest({
		sourceSha: "a".repeat(40),
		buildDigest: `sha256:${"b".repeat(64)}`,
	});

	assert.equal(receipt.compatibilityDigest, template.compatibilityDigest);
	assert.equal(receipt.buildIdentity.sourceSha, "a".repeat(40));
	assert.equal(receipt.buildIdentity.buildDigest, `sha256:${"b".repeat(64)}`);
	assert.match(receipt.buildIdentity.receiptDigest, /^sha256:[0-9a-f]{64}$/);
	const receiptPayload = structuredClone(receipt);
	delete receiptPayload.buildIdentity.receiptDigest;
	assert.equal(
		receipt.buildIdentity.receiptDigest,
		`sha256:${createHash("sha256")
			.update(canonicalizeForDigest(receiptPayload), "utf8")
			.digest("hex")}`,
	);
	assert.equal(
		receipt.compatibility.governedCode.threadProtocolVersion,
		"evalops.maestro.thread.v2",
	);
	assert(receipt.compatibility.headless.messages.toRuntime.includes("steer"));
	assert(
		receipt.compatibility.headless.messages.fromRuntime.includes(
			"codex_compatibility",
		),
	);
	assert(
		receipt.compatibility.headless.capabilities.clientFields.includes(
			"transcript_grade",
		),
	);
	assert(
		receipt.compatibility.headless.capabilities.transcriptGrades.includes("delta"),
	);
	assert(receipt.compatibility.governedCode.toRuntimeMessages.length > 0);
	assert(receipt.compatibility.governedCode.fromRuntimeMessages.length > 0);
});

test("digest canonicalization is independent of object insertion order", () => {
	assert.equal(
		canonicalizeForDigest({ beta: 2, alpha: { delta: 4, charlie: 3 } }),
		canonicalizeForDigest({ alpha: { charlie: 3, delta: 4 }, beta: 2 }),
	);
});

test("serde wire renames and thread validation changes alter compatibility", () => {
	const sources = readCanonicalSources();
	const baseline = buildCompatibilityManifest({ sources });
	const renamed = buildCompatibilityManifest({
		sources: {
			...sources,
			headlessRuntime: sources.headlessRuntime.replace(
				'#[serde(tag = "type", rename_all = "snake_case")]\npub enum ToAgentMessage',
				'#[serde(tag = "type", rename_all = "camelCase")]\npub enum ToAgentMessage',
			),
		},
	});
	const relaxedThread = buildCompatibilityManifest({
		sources: {
			...sources,
			thread: sources.thread.replace(
				'pub(super) const GOVERNED_THREAD_REQUIRED_FIELDS: &[&str] = &["codeMode", "toolGrant"];',
				'pub(super) const GOVERNED_THREAD_REQUIRED_FIELDS: &[&str] = &["codeMode"];',
			),
		},
	});

	assert.notEqual(renamed.compatibilityDigest, baseline.compatibilityDigest);
	assert.notEqual(relaxedThread.compatibilityDigest, baseline.compatibilityDigest);
	assert(renamed.compatibility.headless.messages.toRuntime.includes("governedInit"));
	assert.deepEqual(
		relaxedThread.compatibility.thread.supportedVersions[1].requiredFields,
		["codeMode"],
	);
});

test("hosted thread compatibility matrix changes alter the thread digest", () => {
	const sources = readCanonicalSources();
	const baseline = buildCompatibilityManifest({ sources });
	const changedMatrix = sources.threadCompatibilityMatrix.replace(
		'"current": "evalops.maestro.thread.v2"',
		'"current": "evalops.maestro.thread.v3"',
	);
	assert.notEqual(changedMatrix, sources.threadCompatibilityMatrix);
	const changed = buildCompatibilityManifest({
		sources: { ...sources, threadCompatibilityMatrix: changedMatrix },
	});

	assert.notEqual(
		changed.compatibility.thread.contractDigest,
		baseline.compatibility.thread.contractDigest,
	);
	assert.notEqual(changed.compatibilityDigest, baseline.compatibilityDigest);
});

test("governed thread sources require the compatibility matrix", () => {
	const sources = readCanonicalSources();
	const withoutMatrix = { ...sources };
	delete withoutMatrix.threadCompatibilityMatrix;

	assert.throws(
		() => buildCompatibilityManifest({ sources: withoutMatrix }),
		/governed thread sources require hosted-thread-compatibility-matrix\.json/,
	);
});

test("protobuf field changes alter the compatibility digest", () => {
	const sources = readCanonicalSources();
	const baseline = buildCompatibilityManifest({ sources });
	const mutations = [
		["string name = 1;", "bytes name = 1;"],
		["string name = 1;", "string name = 99;"],
		["string name = 1;", "repeated string name = 1;"],
		["oneof payload {", "oneof replacement {"],
		["SERVER_REQUEST_TYPE_APPROVAL = 1;", "SERVER_REQUEST_TYPE_APPROVAL = 99;"],
	];
	for (const [before, after] of mutations) {
		const mutatedSource = sources.headlessSchema.replace(before, after);
		assert.notEqual(mutatedSource, sources.headlessSchema, `missing fixture ${before}`);
		const changed = buildCompatibilityManifest({
			sources: { ...sources, headlessSchema: mutatedSource },
		});
		assert.notEqual(changed.compatibilityDigest, baseline.compatibilityDigest);
	}
});

test("resident validation changes alter the compatibility digest", () => {
	const sources = readCanonicalSources();
	const baseline = buildCompatibilityManifest({ sources });
	const changed = buildCompatibilityManifest({
		sources: {
			...sources,
			resident: sources.resident.replace(
				"revision.as_deref() != Some(RESIDENT_MODEL_READY_CONTRACT_REVISION)",
				"revision.as_deref() == Some(RESIDENT_MODEL_READY_CONTRACT_REVISION)",
			),
		},
	});
	assert.notEqual(changed.compatibilityDigest, baseline.compatibilityDigest);
});

test("runtime-owned protocol semantics are bound into the compatibility digest", () => {
	const sources = readCanonicalSources();
	const baseline = buildCompatibilityManifest({ sources });
	const changed = buildCompatibilityManifest({
		sources: {
			...sources,
			runtimeFixture: sources.runtimeFixture.replace(
				'"responseEndTerminal": false',
				'"responseEndTerminal": true',
			),
		},
	});

	assert.notEqual(changed.compatibilityDigest, baseline.compatibilityDigest);
	assert.equal(
		baseline.compatibility.runtime.schemaVersion,
		"evalops.maestro.headless-protocol.v1",
	);
	assert.equal(
		baseline.compatibility.runtime.receipt.schemaVersion,
		"evalops.maestro.runtime-receipt.v1",
	);
	assert.match(
		baseline.compatibility.runtime.receipt.sourceDigest,
		/^sha256:[0-9a-f]{64}$/,
	);
	assert.match(
		baseline.compatibility.runtime.receipt.contractDigest,
		/^sha256:[0-9a-f]{64}$/,
	);
});

test("runtime contract digest is bound to the serialized runtime fixture", () => {
	const sources = readCanonicalSources();
	const baseline = buildCompatibilityManifest({ sources });
	const changed = buildCompatibilityManifest({
		sources: {
			...sources,
			runtimeFixture: sources.runtimeFixture.replace(
				'"responseEndTerminal": false',
				'"responseEndTerminal": true',
			),
		},
	});

	assert.notEqual(
		changed.compatibility.runtime.contractDigest,
		baseline.compatibility.runtime.contractDigest,
	);
	assert.notEqual(changed.compatibilityDigest, baseline.compatibilityDigest);

	const testOnlySourceChange = buildCompatibilityManifest({
		sources: {
			...sources,
			runtimeProtocol: sources.runtimeProtocol.replace(
				"checked_in_fixture_matches_typed_contract",
				"changed_test_only_fixture_name",
			),
		},
	});
	assert.equal(
		testOnlySourceChange.compatibility.runtime.contractDigest,
		baseline.compatibility.runtime.contractDigest,
	);
	assert.equal(testOnlySourceChange.compatibilityDigest, baseline.compatibilityDigest);
});

test("runtime contract digest is independent of JSON object key order", () => {
	const sources = readCanonicalSources();
	const baseline = buildCompatibilityManifest({ sources });
	const reverseObjectKeys = (value) => {
		if (Array.isArray(value)) return value.map(reverseObjectKeys);
		if (value !== null && typeof value === "object") {
			return Object.fromEntries(
				Object.entries(value)
					.reverse()
					.map(([key, child]) => [key, reverseObjectKeys(child)]),
			);
		}
		return value;
	};
	const reorderedFixture = JSON.stringify(
		reverseObjectKeys(JSON.parse(sources.runtimeFixture)),
	);
	const reordered = buildCompatibilityManifest({
		sources: { ...sources, runtimeFixture: reorderedFixture },
	});

	assert.equal(
		reordered.compatibility.runtime.contractDigest,
		baseline.compatibility.runtime.contractDigest,
	);
	assert.equal(reordered.compatibilityDigest, baseline.compatibilityDigest);
});

test("runtime receipt fixture is the normalized compatibility projection", () => {
	const sources = readCanonicalSources();
	const baseline = buildCompatibilityManifest({ sources });
	const changedSource = buildCompatibilityManifest({
		sources: {
			...sources,
			runtimeReceipts: sources.runtimeReceipts.replace(
				"evalops.maestro.runtime-receipt.v1",
				"evalops.maestro.runtime-receipt.v2",
			),
		},
	});
	const changedFixture = buildCompatibilityManifest({
		sources: {
			...sources,
			runtimeReceiptFixture: sources.runtimeReceiptFixture.replace(
				'"flushWatermark": 5',
				'"flushWatermark": 6',
			),
		},
	});
	const testOnlySourceChange = buildCompatibilityManifest({
		sources: {
			...sources,
			runtimeReceipts: sources.runtimeReceipts.replace(
				"checked_in_fixture_matches_typed_receipt_contract",
				"renamed_test_only_receipt_fixture",
			),
		},
	});

	assert.notEqual(
		changedSource.compatibility.runtime.receipt.schemaVersion,
		baseline.compatibility.runtime.receipt.schemaVersion,
	);
	assert.notEqual(changedSource.compatibilityDigest, baseline.compatibilityDigest);
	assert.equal(
		changedSource.compatibility.runtime.receipt.sourceDigest,
		baseline.compatibility.runtime.receipt.sourceDigest,
	);
	assert.equal(
		testOnlySourceChange.compatibility.runtime.receipt.sourceDigest,
		baseline.compatibility.runtime.receipt.sourceDigest,
	);
	assert.equal(testOnlySourceChange.compatibilityDigest, baseline.compatibilityDigest);
	assert.notEqual(
		changedFixture.compatibility.runtime.receipt.contractDigest,
		baseline.compatibility.runtime.receipt.contractDigest,
	);
	assert.notEqual(
		changedFixture.compatibility.runtime.receipt.sourceDigest,
		baseline.compatibility.runtime.receipt.sourceDigest,
	);
	assert.notEqual(changedFixture.compatibilityDigest, baseline.compatibilityDigest);
	assert.equal(
		baseline.compatibility.runtime.receipt.sourceDigest,
		baseline.compatibility.runtime.receipt.contractDigest,
	);
	const changedTerminalVariant = buildCompatibilityManifest({
		sources: {
			...sources,
			runtimeReceiptContractFixture: sources.runtimeReceiptContractFixture.replace(
				'"non_fatal"',
				'"nonfatal"',
			),
		},
	});
	assert.notEqual(
		changedTerminalVariant.compatibility.runtime.receipt.contractDigest,
		baseline.compatibility.runtime.receipt.contractDigest,
	);
	assert.notEqual(changedTerminalVariant.compatibilityDigest, baseline.compatibilityDigest);
	const changedValidationLimit = buildCompatibilityManifest({
		sources: {
			...sources,
			runtimeReceiptContractFixture: sources.runtimeReceiptContractFixture.replace(
				'"maxStringBytes": 256',
				'"maxStringBytes": 128',
			),
		},
	});
	assert.notEqual(
		changedValidationLimit.compatibility.runtime.receipt.contractDigest,
		baseline.compatibility.runtime.receipt.contractDigest,
	);
	const changedKindLifecycle = buildCompatibilityManifest({
		sources: {
			...sources,
			runtimeReceiptContractFixture: sources.runtimeReceiptContractFixture.replace(
				'"terminal": ["active"]',
				'"terminal": ["drained"]',
			),
		},
	});
	assert.notEqual(
		changedKindLifecycle.compatibility.runtime.receipt.contractDigest,
		baseline.compatibility.runtime.receipt.contractDigest,
	);
	assert.notEqual(changedKindLifecycle.compatibilityDigest, baseline.compatibilityDigest);
});

test("check mode rejects a stale manifest", () => {
	const directory = mkdtempSync(resolve(tmpdir(), "maestro-protocol-manifest-"));
	const output = resolve(directory, "manifest.json");
	try {
		writeFileSync(output, "{}\n");
		const result = spawnSync(process.execPath, [SCRIPT, "--check", "--out", output], {
			cwd: ROOT,
			encoding: "utf8",
		});
		assert.notEqual(result.status, 0);
		assert.match(result.stderr, /is stale/);
	} finally {
		rmSync(directory, { recursive: true, force: true });
	}
});

test("release environment hooks write an artifact-bound receipt", () => {
	const directory = mkdtempSync(resolve(tmpdir(), "maestro-protocol-receipt-"));
	const output = resolve(directory, "receipt.json");
	try {
		const sourceSha = "c".repeat(40);
		const buildDigest = `sha256:${"d".repeat(64)}`;
		const result = spawnSync(process.execPath, [SCRIPT, "--out", output], {
			cwd: ROOT,
			encoding: "utf8",
			env: {
				...process.env,
				MAESTRO_SOURCE_SHA: sourceSha,
				MAESTRO_BUILD_DIGEST: buildDigest,
			},
		});
		assert.equal(result.status, 0, result.stderr);
		const receipt = JSON.parse(readFileSync(output, "utf8"));
		assert.equal(receipt.buildIdentity.sourceSha, sourceSha);
		assert.equal(receipt.buildIdentity.buildDigest, buildDigest);
		assert.match(receipt.buildIdentity.receiptDigest, /^sha256:[0-9a-f]{64}$/);
	} finally {
		rmSync(directory, { recursive: true, force: true });
	}
});

test("source-root receipt generation supports the legacy v1-only recovery source", () => {
	const directory = mkdtempSync(resolve(tmpdir(), "maestro-legacy-protocol-receipt-"));
	const sourceRoot = resolve(directory, "source");
	const output = resolve(directory, "receipt.json");
	const sources = readCanonicalSources();
	const sourcePaths = {
		headlessSchema: "proto/maestro/v1/headless.proto",
		headlessGenerated: "packages/local-host-rs/src/headless/generated_protocol.rs",
		headlessRuntime: "packages/local-host-rs/src/headless/messages.rs",
		runtimeProtocol: "packages/runtime-contracts-rs/src/protocol.rs",
		runtimeFixture: "packages/runtime-contracts-rs/fixtures/headless-protocol-v1.json",
		runtimeReceipts: "packages/runtime-contracts-rs/src/receipts.rs",
		runtimeReceiptFixture: "packages/runtime-contracts-rs/fixtures/runtime-receipt-v1.json",
		transcript: "packages/local-host-rs/src/transcript.rs",
		thread: "packages/local-host-rs/src/hosted_runner/thread_protocol.rs",
		resident: "packages/local-host-rs/src/hosted_runner_cli.rs",
		hostedRunner: "packages/local-host-rs/src/hosted_runner.rs",
		rendezvous: "packages/local-host-rs/src/hosted_runner/rendezvous_protocol.rs",
	};
	const legacyCodeMode = `/// Runtime execution mode negotiated by a governing controller.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CodeMode {
    GovernedCode,
}

`;
	const legacyThreadConstants = `pub(super) const GOVERNED_THREAD_PROTOCOL_VERSION: &str = "evalops.maestro.thread.v2";
pub(super) const GOVERNED_THREAD_REQUIRED_FIELDS: &[&str] = &["codeMode", "toolGrant"];
`;
	const legacySources = {
		...sources,
		headlessRuntime: sources.headlessRuntime.replace(legacyCodeMode, ""),
		thread: sources.thread.replace(legacyThreadConstants, ""),
	};
	assert.notEqual(legacySources.headlessRuntime, sources.headlessRuntime);
	assert.notEqual(legacySources.thread, sources.thread);

	try {
		for (const [name, path] of Object.entries(sourcePaths)) {
			const target = resolve(sourceRoot, path);
			mkdirSync(dirname(target), { recursive: true });
			writeFileSync(target, legacySources[name]);
		}
		const sourceSha = "f3541cd1045f0091e7d7976d8284b2eecfbb362f";
		const buildDigest = `sha256:${"a".repeat(64)}`;
		const result = spawnSync(
			process.execPath,
			[
				SCRIPT,
				"--source-root",
				sourceRoot,
				"--out",
				output,
				"--source-sha",
				sourceSha,
				"--build-digest",
				buildDigest,
			],
			{ cwd: ROOT, encoding: "utf8" },
		);
		assert.equal(result.status, 0, result.stderr);
		const receipt = JSON.parse(readFileSync(output, "utf8"));
		assert.deepEqual(receipt.compatibility.thread.supportedVersions, [
			{ version: "evalops.maestro.thread.v1", governedCode: false },
		]);
		assert.deepEqual(receipt.compatibility.headless.capabilities.codeModes, []);
		assert.equal(receipt.compatibility.governedCode, null);
		assert.equal(receipt.buildIdentity.sourceSha, sourceSha);
		assert.equal(receipt.buildIdentity.buildDigest, buildDigest);
		assert(
			!receipt.generatedFrom.includes(
				"proto/maestro/v1/hosted-thread-compatibility-matrix.json",
			),
		);
		const withoutMatrix = { ...legacySources };
		delete withoutMatrix.threadCompatibilityMatrix;
		delete withoutMatrix.runtimeReceiptContractFixture;
		const expectedLegacy = buildCompatibilityManifest({ sources: withoutMatrix });
		assert.equal(receipt.compatibilityDigest, expectedLegacy.compatibilityDigest);
		assert.equal(
			receipt.compatibility.thread.contractDigest,
			expectedLegacy.compatibility.thread.contractDigest,
		);
	} finally {
		rmSync(directory, { recursive: true, force: true });
	}
});

test("release receipt rejects a partial artifact identity", () => {
	assert.throws(
		() => buildCompatibilityManifest({ sourceSha: "e".repeat(40) }),
		/source SHA and build digest must be set together/,
	);
});

test(
	"internal GHCR publish attaches the receipt to the immutable image digest",
	{ skip: IS_PUBLIC_PROJECTION && "internal publisher workflow is excluded from the public projection" },
	() => {
	const workflow = readFileSync(
		resolve(ROOT, ".github/workflows/ghcr-publish.yml"),
		"utf8",
	);
	assert.match(
		workflow,
		/MAESTRO_SOURCE_SHA: \$\{\{ steps\.source\.outputs\.sha \}\}/,
	);
	assert.match(
		workflow,
		/MAESTRO_BUILD_DIGEST: \$\{\{ steps\.conformance-image\.outputs\.digest \|\| steps\.image\.outputs\.digest \}\}/,
	);
	assert.match(workflow, /cosign attest --yes --timeout 120s/);
	assert.match(workflow, /cosign verify-attestation --timeout 120s/);
	assert.match(
		workflow,
		/https:\/\/evalops\.dev\/attestations\/maestro-protocol-compatibility\/v1/,
	);
	assert.match(workflow, /client_payload\[protocol_receipt_digest\]/);
	assert.match(workflow, /\.payload \| @base64d \| fromjson/);
	assert.match(workflow, /index\(\$expected\[0\]\) != null/);
	},
);

test(
	"GHCR manual and recovery dispatches are source-locked",
	{ skip: IS_PUBLIC_PROJECTION && "internal publisher workflow is excluded from the public projection" },
	() => {
	const workflow = readFileSync(
		resolve(ROOT, ".github/workflows/ghcr-publish.yml"),
		"utf8",
	);
	assert.match(workflow, /^  workflow_dispatch:\n    inputs:/m);
	assert.match(workflow, /publish_source_sha:[\s\S]*?required: true[\s\S]*?type: string/);
	assert.match(
		workflow,
		/refs\/heads\/main' && inputs\.publish_source_sha == github\.sha/,
	);
	assert.match(workflow, /ref: \$\{\{ inputs\.publish_source_sha \}\}/);
	assert.match(workflow, /path: manual-source/);
	assert.match(workflow, /node_version_file="manual-source\/\.node-version"/);
	assert.match(workflow, /node_version_file="recovery-source\/\.node-version"/);
	assert.match(workflow, /node_version_file="\.node-version"/);
	assert.match(
		workflow,
		/node-version-file: \$\{\{ steps\.source\.outputs\.node_version_file \}\}/,
	);
	assert.doesNotMatch(
		workflow,
		/node-version-file: \$\{\{ steps\.source\.outputs\.root \}\}\/\.node-version/,
	);
	assert.match(
		workflow,
		/paths-ignore:\n      - \.github\/workflows\/ghcr-publish\.yml\n      - scripts\/build-profile-contract\.test\.mjs\n      - scripts\/generate-protocol-compatibility-manifest\.mjs\n      - scripts\/generate-protocol-compatibility-manifest\.test\.mjs\n      - scripts\/measure-ci-build-latency\.mjs\n      - scripts\/recovery-publisher-once\.mjs/,
	);
	assert.match(
		workflow,
		/RECOVERY_BASE_SHA: ebdcda9905f27e0329d1554c7b1295e2a0386d69/,
	);
	assert.match(
		workflow,
		/RECOVERY_SOURCE_SHA: e0c1a2daf9ce23ce7f02639e4b5f834837f34b27/,
	);
	assert.match(workflow, /recovery-publisher-e0c1a2d-v2/);
	assert.match(workflow, /RECOVERY_PUBLISHER_TAG: recovery-publisher-e0c1a2d-v2/);
	assert.match(
		workflow,
		/RECOVERY_RESERVATION_TAG: recovery-publication-reserved-e0c1a2d-v2/,
	);
	assert.match(
		workflow,
		/cancel-in-progress: \$\{\{ github\.event_name != 'workflow_dispatch' \}\}/,
	);
	assert.match(
		workflow,
		/if: github\.event_name == 'push' \|\| \(github\.event_name == 'workflow_dispatch' && github\.ref == 'refs\/heads\/main' && inputs\.publish_source_sha == github\.sha\) \|\| \(github\.event_name == 'workflow_dispatch' && github\.ref == 'refs\/tags\/recovery-publisher-e0c1a2d-v2'\)/,
	);
	assert.match(workflow, /ref: \$\{\{ env\.RECOVERY_SOURCE_SHA \}\}/);
	assert.match(workflow, /path: recovery-source/);
	assert.match(workflow, /fetch-depth: 8/);
	const loginIndex = workflow.indexOf("- name: Log in to GHCR");
	const replayGateIndex = workflow.indexOf(
		"- name: Reject replayed recovery publication",
	);
	const buildIndex = workflow.indexOf("- name: Build image");
	assert(loginIndex >= 0);
	assert(replayGateIndex > loginIndex);
	assert(buildIndex > replayGateIndex);
	assert.match(workflow, /node scripts\/recovery-publisher-once\.mjs preflight/);
	assert.match(
		workflow,
		/if: env\.PUBLISH_MAIN == 'true' \|\| steps\.recovery-state\.outputs\.image_exists != 'true'/,
	);
	assert.match(workflow, /RESUMED_DIGEST: \$\{\{ steps\.recovery-state\.outputs\.image_digest \}\}/);
	assert.match(
		workflow,
		/image-digest: \$\{\{ steps\.conformance-image\.outputs\.digest \|\| steps\.image\.outputs\.digest \}\}/,
	);
	assert.doesNotMatch(workflow, /isolated conformance push digest differs from the BuildKit digest/);
	assert.match(workflow, /verified image tag \$\{tag\} resolved to \$\{pushed_digest\}, expected \$\{EXPECTED_DIGEST\}/);
	assert.match(workflow, /Verify resumable recovery image provenance/);
	assert.match(
		workflow,
		/ghcr-publish\\\.yml@refs\/tags\/recovery-publisher-e0c1a2d-v2\$/,
	);
	assert.match(workflow, /merge-base --is-ancestor/);
	assert.match(workflow, /HEADLESS_PROTOCOL_VERSION: &str = \"2026-08-07\"/);
	assert.match(workflow, /THREAD_PROTOCOL_VERSION: &str = \"evalops\.maestro\.thread\.v1\"/);
	assert.match(workflow, /recovery source unexpectedly advertises thread protocol v2/);
	assert.match(workflow, /maestro-resident-model-ready-v3/);
	assert.match(workflow, /context: \$\{\{ steps\.source\.outputs\.root \}\}/);
	assert.match(workflow, /--source-root \"\$\{SOURCE_ROOT\}\"/);
	assert.match(
		workflow,
		/type=raw,value=sha-\$\{\{ env\.RECOVERY_SOURCE_SHA \}\},enable=\$\{\{ github\.event_name == 'workflow_dispatch' && github\.ref == 'refs\/tags\/recovery-publisher-e0c1a2d-v2' \}\}/,
	);
	assert.match(workflow, /PUBLISH_MAIN: \$\{\{ github\.event_name == 'push'/);
	assert.match(workflow, /push: \$\{\{ github\.event_name == 'workflow_dispatch' && github\.ref == 'refs\/tags\/recovery-publisher-e0c1a2d-v2' \}\}/);
	const reservationIndex = workflow.indexOf(
		"node scripts/recovery-publisher-once.mjs reserve-and-dispatch",
	);
	assert(reservationIndex > buildIndex);
	const helper = readFileSync(
		resolve(ROOT, "scripts/recovery-publisher-once.mjs"),
		"utf8",
	);
	assert(helper.indexOf("await reserve();") < helper.indexOf("await dispatch();"));
	assert.match(helper, /client_payload\[image_tag\]=sha-\$\{sourceSha\}/);
	assert.match(helper, /timeout: 120_000/);
	},
);

test(
	"public projection retains the public-owned GHCR release workflow contract",
	{ skip: !IS_PUBLIC_PROJECTION && "public workflow contract is only evaluated in the public projection" },
	() => {
		const workflow = readFileSync(
			resolve(ROOT, ".github/workflows/ghcr-publish.yml"),
			"utf8",
		);
		assert.match(workflow, /^  push:\n    branches:\n      - main/m);
		assert.doesNotMatch(workflow, /^  pull_request:\s*$/m);
		assert.doesNotMatch(workflow, /^  workflow_dispatch:/m);
		assert.doesNotMatch(workflow, /^  build-and-publish:/m);
		assert.match(workflow, /type=sha,prefix=sha-/);
		assert.match(workflow, /push: true/);
	},
);

test("recovery publisher fails closed on ambiguity and resumes an exact partial digest", async () => {
	const missing = () => Promise.reject(new CommandFailure("probe", "HTTP 404 Not Found"));
	await assert.rejects(
		assertPublicationIsFresh({
			lookupReservation: () => Promise.reject(new CommandFailure("probe", "HTTP 503 upstream unavailable")),
			inspectImage: missing,
		}),
		/unable to prove reservation absence/,
	);
	const resumed = await assertPublicationIsFresh({
		lookupReservation: missing,
		inspectImage: async () => `sha256:${"a".repeat(64)}`,
	});
	assert.equal(resumed.imageDigest, `sha256:${"a".repeat(64)}`);
});

test("reservation survives a post-dispatch failure and blocks sequential replay", async () => {
	const state = { reserved: false, dispatches: 0 };
	const missing = () => Promise.reject(new CommandFailure("probe", "HTTP 404 Not Found"));
	const attempt = async (afterDispatch = async () => {}) => {
		await assertPublicationIsFresh({
			lookupReservation: state.reserved ? async () => "present" : missing,
			inspectImage: missing,
		});
		await reserveAndDispatch({
			reserve: async () => {
				if (state.reserved) throw new Error("reservation already exists");
				state.reserved = true;
			},
			dispatch: async () => {
				state.dispatches += 1;
			},
			afterDispatch,
		});
	};

	await assert.rejects(
		attempt(async () => {
			throw new Error("runner failed after dispatch acceptance");
		}),
		/runner failed after dispatch acceptance/,
	);
	await assert.rejects(attempt(), /already reserved/);
	assert.equal(state.dispatches, 1);
});

test("cosign DSSE verification selects the exact decoded predicate", () => {
	const directory = mkdtempSync(resolve(tmpdir(), "maestro-protocol-dsse-"));
	const expectedPath = resolve(directory, "expected.json");
	const verifiedPath = resolve(directory, "verified.jsonl");
	try {
		const expected = buildCompatibilityManifest({
			sourceSha: "f".repeat(40),
			buildDigest: `sha256:${"1".repeat(64)}`,
		});
		writeFileSync(expectedPath, JSON.stringify(expected));
		writeFileSync(
			verifiedPath,
			`${JSON.stringify({
				payload: Buffer.from(JSON.stringify({ predicate: expected })).toString("base64"),
			})}\n`,
		);
		const expression =
			"[.[] | (.payload | @base64d | fromjson).predicate] | index($expected[0]) != null";
		const result = spawnSync(
			"jq",
			["-s", "-e", "--slurpfile", "expected", expectedPath, expression, verifiedPath],
			{ encoding: "utf8" },
		);
		assert.equal(result.status, 0, result.stderr);
		writeFileSync(expectedPath, renderCompatibilityManifest());
		const mismatch = spawnSync(
			"jq",
			["-s", "-e", "--slurpfile", "expected", expectedPath, expression, verifiedPath],
			{ encoding: "utf8" },
		);
		assert.notEqual(mismatch.status, 0);
	} finally {
		rmSync(directory, { recursive: true, force: true });
	}
});

test("required Rust validation rejects compatibility manifest drift", () => {
	const workflow = readFileSync(
		resolve(ROOT, ".github/workflows/maestro-ci.yml"),
		"utf8",
	);
	assert.match(workflow, /npm run check/);
	assert.match(workflow, /^  protocol-contracts:/m);
	assert.match(workflow, /npm run check:protocol-manifest/);
	assert.match(workflow, /needs: protocol-contracts/);
	const packageJson = JSON.parse(readFileSync(resolve(ROOT, "package.json"), "utf8"));
	assert.match(packageJson.scripts.check, /check:protocol-manifest/);
});

test("npm package includes the checked compatibility template", () => {
	const packageJson = JSON.parse(readFileSync(resolve(ROOT, "package.json"), "utf8"));
	assert(
		packageJson.files.includes(
			"proto/maestro/v1/protocol-compatibility-manifest.json",
		),
	);
});

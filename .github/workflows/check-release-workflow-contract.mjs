#!/usr/bin/env node

import { readFile } from "node:fs/promises";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), "../..");
const DEFAULT_WORKFLOW = resolve(ROOT, ".github/workflows/release.yml");
const RELEASE_SHA_EXPRESSION = "${{ needs.prepare.outputs.release_sha }}";

function indentation(line) {
	return line.match(/^ */u)?.[0].length ?? 0;
}

function scalar(value) {
	return value.trim().replace(/^(["'])([\s\S]*)\1$/u, "$2").replace(/\s+#.*$/u, "");
}

export function parseWorkflow(source) {
	const lines = source.replaceAll("\r\n", "\n").split("\n");
	const jobs = {};
	const permissions = {};
	let concurrencyGroup = "";
	let inJobs = false;
	let rootSection = "";
	let job = null;
	let jobSection = "";
	let step = null;
	let stepSection = "";

	for (let index = 0; index < lines.length; index += 1) {
		const line = lines[index];
		const trimmed = line.trim();
		if (!trimmed || trimmed.startsWith("#")) continue;
		const indent = indentation(line);

		if (indent === 0) {
			inJobs = trimmed === "jobs:";
			rootSection = trimmed.endsWith(":") ? trimmed.slice(0, -1) : "";
			job = null;
			continue;
		}
		if (!inJobs) {
			if (indent === 2 && rootSection === "permissions") {
				const permission = /^  ([a-zA-Z0-9_-]+):\s*(\S.*?)\s*$/u.exec(line);
				if (permission) permissions[permission[1]] = scalar(permission[2]);
			}
			if (indent === 2 && rootSection === "concurrency") {
				const group = /^  group:\s*(\S.*?)\s*$/u.exec(line);
				if (group) concurrencyGroup = scalar(group[1]);
			}
			continue;
		}

		const jobMatch = /^  ([a-zA-Z0-9][a-zA-Z0-9_-]*):\s*(?:#.*)?$/u.exec(line);
		if (jobMatch) {
			job = {
				condition: "",
				continueOnError: "",
				environment: "",
				needs: [],
				outputs: {},
				permissions: {},
				runsOn: "",
				steps: [],
			};
			jobs[jobMatch[1]] = job;
			jobSection = "";
			step = null;
			continue;
		}
		if (!job) continue;

		const jobProperty = /^    ([a-zA-Z0-9_-]+):(?:\s*(.*))?$/u.exec(line);
		if (jobProperty) {
			const [, key, rawValue = ""] = jobProperty;
			const value = scalar(rawValue);
			jobSection = key;
			step = null;
			if (key === "environment" && value) job.environment = value;
			if (key === "if" && value) job.condition = value;
			if (key === "continue-on-error" && value) job.continueOnError = value;
			if (key === "runs-on" && value) job.runsOn = value;
			if (key === "needs" && value) {
				job.needs = value.startsWith("[")
					? value
							.slice(1, -1)
							.split(",")
							.map((item) => scalar(item))
							.filter(Boolean)
					: [value];
			}
			continue;
		}

		if (indent === 6 && jobSection === "permissions") {
			const permission = /^      ([a-zA-Z0-9_-]+):\s*(\S.*?)\s*$/u.exec(line);
			if (permission) job.permissions[permission[1]] = scalar(permission[2]);
			continue;
		}
		if (indent === 6 && jobSection === "outputs") {
			const output = /^      ([a-zA-Z0-9_-]+):\s*(\S.*?)\s*$/u.exec(line);
			if (output) job.outputs[output[1]] = scalar(output[2]);
			continue;
		}
		if (indent === 6 && jobSection === "environment") {
			const environmentName = /^      name:\s*(\S.*?)\s*$/u.exec(line);
			if (environmentName) job.environment = scalar(environmentName[1]);
			continue;
		}
		if (indent === 6 && jobSection === "needs") {
			const dependency = /^      -\s*(\S.*?)\s*$/u.exec(line);
			if (dependency) job.needs.push(scalar(dependency[1]));
			continue;
		}
		if (indent === 6 && jobSection === "steps") {
			const stepStart = /^      -\s*([a-zA-Z0-9_-]+):(?:\s*(.*))?$/u.exec(line);
			if (!stepStart) continue;
			step = {
				condition: "",
				continueOnError: "",
				env: {},
				id: "",
				name: "",
				run: "",
				uses: "",
				with: {},
			};
			job.steps.push(step);
			const [, key, rawValue = ""] = stepStart;
			step[key] = scalar(rawValue);
			stepSection = "";
			continue;
		}
		if (!step || indent < 8) continue;

		const stepProperty = /^        ([a-zA-Z0-9_-]+):(?:\s*(.*))?$/u.exec(line);
		if (stepProperty) {
			const [, key, rawValue = ""] = stepProperty;
			const value = scalar(rawValue);
			stepSection = key;
			if (key === "run" && (value === "|" || value === ">")) {
				const script = [];
				while (index + 1 < lines.length) {
					const next = lines[index + 1];
					if (next.trim() && indentation(next) <= 8) break;
					index += 1;
					script.push(next.length >= 10 ? next.slice(10) : "");
				}
				step.run = script.join("\n");
			} else if (key === "if") {
				step.condition = value;
			} else if (key === "continue-on-error") {
				step.continueOnError = value;
			} else if (key in step && value) {
				step[key] = value;
			}
			continue;
		}
		if (indent === 10 && (stepSection === "env" || stepSection === "with")) {
			const entry = /^          ([a-zA-Z0-9_-]+):\s*(.*?)\s*$/u.exec(line);
			if (entry) {
                let value = scalar(entry[2]);
                if (value === "|") {
                    const values = [];
                    while (index + 1 < lines.length) {
                        const next = lines[index + 1];
                        if (next.trim() && indentation(next) <= 10) break;
                        index += 1;
                        if (next.trim()) values.push(next.trim());
                    }
                    value = values.join("\n");
                }
                step[stepSection][entry[1]] = value;
            }
		}
	}

	return { concurrencyGroup, jobs, permissions };
}

function hasNeed(job, dependency) {
	return job?.needs.includes(dependency) ?? false;
}

function checkoutIsBound(
	job,
	{ workflowHelper = false, ref = RELEASE_SHA_EXPRESSION } = {},
) {
	const checkouts =
		job?.steps.filter((step) => step.uses.startsWith("actions/checkout@")) ?? [];
	const helperCheckouts = checkouts.filter(
		(step) => step.with.path === ".release-workflow",
	);
	if (workflowHelper) {
		if (
			helperCheckouts.length !== 1 ||
			helperCheckouts[0].condition ||
			(helperCheckouts[0].continueOnError &&
				helperCheckouts[0].continueOnError !== "false") ||
			helperCheckouts[0].with.ref !== "${{ github.sha }}" ||
			helperCheckouts[0].with["persist-credentials"] !== "false" ||
			helperCheckouts[0].with["sparse-checkout"] !== ".github/workflows"
		) {
			return false;
		}
	}
	const releaseCheckouts = checkouts.filter(
		(step) => step.with.path !== ".release-workflow",
	);
	return (
		releaseCheckouts.length === 1 &&
		!releaseCheckouts[0].condition &&
		(!releaseCheckouts[0].continueOnError ||
			releaseCheckouts[0].continueOnError === "false") &&
		releaseCheckouts[0].with.ref === ref
	);
}

function requiredStepCanBeSkippedOrIgnored(step) {
	return Boolean(
		step?.condition ||
			(step?.continueOnError && step.continueOnError !== "false"),
	);
}

function hasExactPermissions(actual, expected) {
	const actualEntries = Object.entries(actual).sort(([left], [right]) =>
		left.localeCompare(right),
	);
	const expectedEntries = Object.entries(expected).sort(([left], [right]) =>
		left.localeCompare(right),
	);
	return JSON.stringify(actualEntries) === JSON.stringify(expectedEntries);
}

function hasExactRecord(actual, expected) {
	const actualEntries = Object.entries(actual).sort(([left], [right]) =>
		left.localeCompare(right),
	);
	const expectedEntries = Object.entries(expected).sort(([left], [right]) =>
		left.localeCompare(right),
	);
	return JSON.stringify(actualEntries) === JSON.stringify(expectedEntries);
}

function executableLines(script) {
	return script
		.split("\n")
		.map((line) => line.trim())
		.filter((line) => line && !line.startsWith("#"));
}

function topLevelExecutableLines(script) {
	return script
		.split("\n")
		.filter((line) => line && !/^\s/u.test(line) && !line.startsWith("#"));
}

function findStep(job, name) {
	return job?.steps.find((step) => step.name === name);
}

export function validateReleaseWorkflow(source) {
	const { concurrencyGroup, jobs, permissions } = parseWorkflow(source);
	const failures = [];
	const prepare = jobs.prepare;
	const binaries = jobs.binaries;
	const publish = jobs.publish;
	const release = jobs["github-release"];
	const canary = jobs["post-publish-canary"];
	const identity = jobs["identity-readiness"];
	const identityAuthStep = findStep(
		identity,
		"Authenticate to the release-test Secret Manager lane",
	);
	const identityRefreshStep = findStep(
		identity,
		"Rotate and verify release-test Identity session",
	);
	const identityStep = findStep(identity, "Verify release-test Identity session");
	const identityEnv = {
		MAESTRO_EVALOPS_ORG_ID: "${{ vars.MAESTRO_RELEASE_TEST_ORG_ID }}",
	};
	const releaseTestAuthUses =
		"google-github-actions/auth@7c6bc770dae815cd3e89ee6cdf493a5fab2cc093";
	const releaseTestAuthEnv = {
		workload_identity_provider:
			"${{ vars.MAESTRO_RELEASE_TEST_GCP_WORKLOAD_IDENTITY_PROVIDER }}",
		service_account: "${{ vars.MAESTRO_RELEASE_TEST_GCP_SERVICE_ACCOUNT }}",
		create_credentials_file: "true",
		export_environment_variables: "true",
	};
	const releaseTestRefreshEnv = {
		MAESTRO_RELEASE_TEST_SESSION_SECRET:
			"${{ vars.MAESTRO_RELEASE_TEST_SESSION_SECRET }}",
		MAESTRO_RELEASE_TEST_ORG_ID: "${{ vars.MAESTRO_RELEASE_TEST_ORG_ID }}",
		MAESTRO_RELEASE_TEST_WORKSPACE_ID:
			"${{ vars.MAESTRO_RELEASE_TEST_WORKSPACE_ID }}",
		MAESTRO_RELEASE_TEST_SUBJECT: "${{ vars.MAESTRO_RELEASE_TEST_SUBJECT }}",
	};
	if (
		!identity ||
		identity.condition ||
		identity.continueOnError ||
		identity.environment !== "npm-release" ||
		!hasNeed(identity, "prepare") ||
		!hasExactPermissions(identity.permissions, {
			contents: "read",
			"id-token": "write",
		}) ||
		!checkoutIsBound(identity, { ref: "${{ github.sha }}" }) ||
		!identityAuthStep ||
		identityAuthStep.uses !== releaseTestAuthUses ||
		!hasExactRecord(identityAuthStep.with, releaseTestAuthEnv) ||
		requiredStepCanBeSkippedOrIgnored(identityAuthStep) ||
		!identityRefreshStep ||
		requiredStepCanBeSkippedOrIgnored(identityRefreshStep) ||
		identityRefreshStep.run !==
			"node .github/workflows/refresh-release-identity.mjs" ||
		!hasExactRecord(identityRefreshStep.env, releaseTestRefreshEnv) ||
		!identityStep ||
		requiredStepCanBeSkippedOrIgnored(identityStep) ||
		identityStep.run !== "node .github/workflows/check-release-identity.mjs" ||
		!hasExactRecord(identityStep.env, identityEnv) ||
		!hasNeed(publish, "identity-readiness")
	) {
		failures.push(
			"publication must require the protected release-test Identity preflight",
		);
	}
	const canaryAuthStep = findStep(
		canary,
		"Authenticate to the release-test Secret Manager lane",
	);
	const canaryRefreshStep = findStep(
		canary,
		"Rotate release-test Identity session",
	);
	const authenticatedReplayStep = findStep(
		canary,
		"Verify published package from npm",
	);
	if (
		canary?.environment !== "npm-release" ||
		!hasExactPermissions(canary.permissions, {
			contents: "read",
			"id-token": "write",
		}) ||
		!checkoutIsBound(canary, { workflowHelper: true }) ||
		!canaryAuthStep ||
		canaryAuthStep.uses !== releaseTestAuthUses ||
		!hasExactRecord(canaryAuthStep.with, releaseTestAuthEnv) ||
		requiredStepCanBeSkippedOrIgnored(canaryAuthStep) ||
		!canaryRefreshStep ||
		requiredStepCanBeSkippedOrIgnored(canaryRefreshStep) ||
		canaryRefreshStep.run !==
			"node .release-workflow/.github/workflows/refresh-release-identity.mjs" ||
		!hasExactRecord(canaryRefreshStep.env, releaseTestRefreshEnv) ||
		Object.entries(identityEnv).some(
			([key, value]) => authenticatedReplayStep?.env[key] !== value,
		) ||
		Object.hasOwn(authenticatedReplayStep?.env ?? {}, "MAESTRO_EVALOPS_ACCESS_TOKEN") ||
		!hasNeed(release, "post-publish-canary")
	) {
		failures.push(
			"GitHub publication must wait for the authenticated registry replay canary",
		);
	}


	for (const [name, job] of [
		["prepare", prepare],
		["binaries", binaries],
		["publish", publish],
		["github-release", release],
		["post-publish-canary", canary],
	]) {
		if (!job) failures.push(`missing ${name} job`);
	}
	if (!prepare || !binaries || !publish || !release || !canary) return failures;

	if (!hasExactPermissions(permissions, { contents: "read" })) {
		failures.push("workflow default permissions must be exactly contents: read");
	}
	const normalizedReleaseConcurrency =
		"${{ github.workflow }}-${{ startsWith(github.event.client_payload.version || inputs.version, 'v') && (github.event.client_payload.version || inputs.version) || format('v{0}', github.event.client_payload.version || inputs.version) }}";
	if (concurrencyGroup !== normalizedReleaseConcurrency) {
		failures.push(
			"release workflows must serialize only duplicate paths for the same normalized release tag",
		);
	}
	for (const [name, job] of [["prepare", prepare]]) {
		if (!hasExactPermissions(job.permissions, { contents: "read" })) {
			failures.push(`${name} permissions must be exactly contents: read`);
		}
	}
	if (!hasExactPermissions(binaries.permissions, { contents: "write" })) {
		failures.push("binaries permissions must be exactly contents: write to read the signed draft release");
	}
	if (
		!hasExactPermissions(publish.permissions, {
			contents: "read",
			"id-token": "write",
		})
	) {
		failures.push(
			"publish permissions must be exactly contents: read and id-token: write",
		);
	}
	if (
		!hasExactPermissions(release.permissions, {
			contents: "write",
			"id-token": "write",
		})
	) {
		failures.push(
			"github-release permissions must be exactly contents: write and id-token: write",
		);
	}
	for (const [name, job] of [
		["prepare", prepare],
		["binaries", binaries],
		["publish", publish],
		["github-release", release],
		["post-publish-canary", canary],
	]) {
		if (
			(name === "prepare"
				? job.condition !== "github.repository == 'dx-corp/maestro' && github.ref == 'refs/heads/main'"
                : job.condition) ||
			(job.continueOnError && job.continueOnError !== "false")
		) {
			failures.push(`${name} job must not be conditional or ignored`);
		}
	}
	for (const [name, job] of [
		["prepare", prepare],
		["publish", publish],
		["github-release", release],
		["post-publish-canary", canary],
	]) {
		if (job.runsOn !== "${{ vars.PUBLIC_RELEASE_RUNNER || 'ubuntu-latest' }}") {
			failures.push(
				`${name} must run on PUBLIC_RELEASE_RUNNER with an ubuntu-latest fallback`,
			);
		}
	}

	const resolveStep = findStep(prepare, "Resolve immutable release tag");
	const prepareCheckout = prepare.steps.find((step) =>
		step.uses.startsWith("actions/checkout@"),
	);
	if (prepareCheckout?.with["fetch-depth"] !== "0") {
		failures.push(
			"prepare checkout must include history for protected main ancestry verification",
		);
	}
	const resolveLines = executableLines(resolveStep?.run ?? "");
	const topLevelResolveLines = topLevelExecutableLines(resolveStep?.run ?? "");
	if (resolveStep?.id !== "release") {
		failures.push("immutable resolver step id must be release");
	}
	if (
		!hasExactRecord(prepare.outputs, {
			package_name: "${{ steps.release.outputs.package_name }}",
			release_sha: "${{ steps.release.outputs.release_sha }}",
			release_tag: "${{ steps.release.outputs.release_tag }}",
			release_version: "${{ steps.release.outputs.release_version }}",
			release_channel: "${{ steps.release.outputs.release_channel }}",
			npm_tag: "${{ steps.release.outputs.npm_tag }}",
		})
	) {
		failures.push("prepare outputs must bind exactly to the immutable resolver");
	}
	if (requiredStepCanBeSkippedOrIgnored(resolveStep)) {
		failures.push("immutable release resolution must not be conditional or ignored");
	}
	if (
		!hasExactRecord(resolveStep?.env ?? {}, {
			EVENT_NAME: "${{ github.event_name }}",
			REQUESTED: "${{ github.event.client_payload.version || github.event.inputs.version || github.ref_name }}",
			TRIGGER_SHA: "${{ github.sha }}",
			GH_TOKEN: "${{ github.token }}",
		})
	) {
		failures.push("immutable resolver must bind the triggering event and SHA");
	}
	const triggerShaIndex = resolveLines.indexOf('release_sha="$TRIGGER_SHA"');
	const dispatchIndex = resolveLines.indexOf(
		'if [[ "$EVENT_NAME" == "workflow_dispatch" || "$EVENT_NAME" == "repository_dispatch" ]]; then',
	);
	const fetchedShaIndex = resolveLines.indexOf(
		'release_sha="$(git rev-list -n 1 "$release_tag")"',
	);
	const pushGuardIndex = resolveLines.indexOf(
		'elif [[ "$EVENT_NAME" != "push" ]]; then',
	);
	if (
		triggerShaIndex < 0 ||
		dispatchIndex <= triggerShaIndex ||
		fetchedShaIndex <= dispatchIndex ||
		pushGuardIndex <= fetchedShaIndex ||
		resolveLines.filter((line) => line === 'release_sha="$TRIGGER_SHA"').length !==
			1 ||
		resolveLines.filter(
			(line) => line === 'release_sha="$(git rev-list -n 1 "$release_tag")"',
		).length !== 1
	) {
		failures.push(
			"tag pushes must preserve the triggering SHA and only dispatches may resolve a tag name",
		);
	}
	if (!topLevelResolveLines.includes('git checkout --detach "$release_sha"')) {
		failures.push("immutable release resolution must check out the resolved SHA");
	}
	const ancestryIndex = topLevelResolveLines.indexOf('git merge-base --is-ancestor "$release_sha" FETCH_HEAD');
    if (ancestryIndex < 0 || topLevelResolveLines.indexOf('timeout 60s git fetch --no-tags origin main') >= ancestryIndex ||
        !topLevelResolveLines.includes('timeout 60s git fetch --no-tags origin main') ||
        ancestryIndex >= topLevelResolveLines.indexOf('git checkout --detach "$release_sha"')) {
        failures.push("release source must pass protected main ancestry verification before checkout");
    }
	const boundedFetch =
		'fetch --force --no-tags origin "refs/tags/${release_tag}:refs/tags/${release_tag}"; then';
	if (
		!resolveLines.includes("for attempt in 1 2 3; do") ||
		!resolveLines.includes("if timeout 60s git \\") ||
		!resolveLines.includes("-c http.lowSpeedLimit=1000 \\") ||
		!resolveLines.includes("-c http.lowSpeedTime=30 \\") ||
		!resolveLines.includes(boundedFetch) ||
		!resolveLines.includes('if [[ "$attempt" -eq 3 ]]; then') ||
		!resolveLines.includes("sleep 2")
	) {
		failures.push("immutable tag fetch must use bounded retries and transport timeouts");
	}
	for (const output of [
		"package_name",
		"release_sha",
		"release_tag",
		"release_version",
		"release_channel",
		"npm_tag",
	]) {
		if (
			resolveLines.filter((line) => line.startsWith(`echo "${output}=`))
				.length !== 1 ||
			!resolveLines.includes(`echo "${output}=$${output}"`)
		) {
			failures.push(`immutable resolver must emit ${output} exactly once`);
		}
	}
	if (
		resolveLines.filter((line) => line.includes("GITHUB_OUTPUT")).length !== 1 ||
		!resolveLines.includes('} >> "$GITHUB_OUTPUT"')
	) {
		failures.push("immutable resolver outputs must use one grouped GITHUB_OUTPUT write");
	}

    const authenticate = findStep(binaries, "Authenticate artifacts and release receipts");
    const authLines = executableLines(authenticate?.run ?? "");
    // A strict shell preamble is permitted; no other command may precede authentication.
    if (authLines[0] === "set -euo pipefail") authLines.shift();
    const verifyCommand = 'node scripts/verify-staged-release.mjs release-binaries "$RELEASE_VERSION"';
    const authIndex = binaries.steps.indexOf(authenticate);
    if (requiredStepCanBeSkippedOrIgnored(authenticate) || authLines[0] !== verifyCommand ||
        authenticate?.env.RELEASE_VERSION !== "${{ needs.prepare.outputs.release_version }}" ||
        binaries.steps.some((step, index) => index < authIndex && (step.run.includes("tar -x") || step.uses.startsWith("actions/upload-artifact@")))) {
        failures.push("native artifacts must be authenticated for the immutable version before extraction or upload");
    }
	if (!hasNeed(binaries, "prepare")) failures.push("binaries must need prepare");
	if (!hasNeed(publish, "prepare") || !hasNeed(publish, "binaries")) {
		failures.push("publish must need prepare and binaries");
	}
	if (
		!hasNeed(release, "prepare") ||
		!hasNeed(release, "binaries") ||
		!hasNeed(release, "publish")
	) {
		failures.push("github-release must need prepare, binaries, and publish");
	}
	if (!hasNeed(canary, "prepare") || !hasNeed(canary, "publish")) {
		failures.push("post-publish canary must need prepare and publish");
	}
	for (const [name, job] of [
		["binaries", binaries],
		["publish", publish],
		["github-release", release],
		["post-publish-canary", canary],
	]) {
		if (!checkoutIsBound(job)) {
			failures.push(`${name} checkout must bind to the immutable release SHA`);
		}
	}

	if (publish.environment !== "npm-release") {
		failures.push("publish must use the npm-release environment");
	}
	const assetStep = findStep(publish, "Verify native package inputs");
	if (requiredStepCanBeSkippedOrIgnored(assetStep)) {
		failures.push("native package input verification must not be conditional or ignored");
	}
	if (
		JSON.stringify(executableLines(assetStep?.run ?? "")) !==
		JSON.stringify([
			"set -euo pipefail",
			`test "$(node -p "require('./package.json').version")" = "$RELEASE_VERSION"`,
			"test -f packages/web/dist/index.html",
			"npm run check:rust-only-runtime",
		])
	) {
		failures.push("publish must execute the exact native package input checks");
	}

	const publishStep = findStep(publish, "Publish to npm");
	const publishLines = executableLines(publishStep?.run ?? "");
	const topLevelPublishLines = topLevelExecutableLines(publishStep?.run ?? "");
	if (requiredStepCanBeSkippedOrIgnored(publishStep)) {
		failures.push("npm publication must not be conditional or ignored");
	}
	const expectedPackageCalls = [
		'publish_package "$PACKAGE_NAME" "$TARBALL" "$PACKED_INTEGRITY"',
		'publish_package "$ALIAS_PACKAGE_NAME" "$ALIAS_TARBALL" "$ALIAS_PACKED_INTEGRITY"',
	];
	const oidcCall =
		'if publish_or_verify publish_with_oidc "$package_name" "$tarball" "$packed_integrity"; then';
	const tokenCall =
		'publish_or_verify publish_with_token "$package_name" "$tarball" "$packed_integrity"';
	const packageCallIndexes = expectedPackageCalls.map((line) =>
		topLevelPublishLines.indexOf(line),
	);
	const firstPackageCallIndex = packageCallIndexes[0];
	const prePublishTerminations = topLevelPublishLines
		.slice(0, firstPackageCallIndex)
		.filter((line) => /^(?:exit|return)(?:\s|$)/u.test(line));
	if (
		packageCallIndexes.some((index) => index < 0) ||
		packageCallIndexes[1] <= packageCallIndexes[0] ||
		topLevelPublishLines.filter((line) => expectedPackageCalls.includes(line))
			.length !== 2 ||
		!publishLines.includes(tokenCall) ||
		prePublishTerminations.length !== 0 ||
		publishLines.includes("if false; then") ||
		publishLines.some((line) => /^npm\s*\(\)\s*\{/u.test(line)) ||
		!publishLines.includes(
			'command npm view "${package_name}@${RELEASE_VERSION}" --registry "$NPM_CONFIG_REGISTRY" dist.integrity 2>/dev/null',
		) ||
		!publishLines.includes(
			'if [[ "$registry_integrity" != "$packed_integrity" ]]; then',
		)
	) {
		failures.push("token-backed npm publication must not be bypassed");
	}
	const oidcCalls = publishLines.filter(
		(line) => line === oidcCall,
	).length;
	const tokenCalls = publishLines.filter(
		(line) => line === tokenCall,
	).length;
	const oidcCallIndex = publishLines.indexOf(oidcCall);
	const tokenPublishIndex = publishLines.indexOf(tokenCall);
	const npmPublishLines = publishLines.filter((line) =>
		line.startsWith("npx --yes npm@11.10.0 publish "),
	);
	const expectedPublishLines = new Set([
		'npx --yes npm@11.10.0 publish "$tarball" --access public --tag "$NPM_TAG" --registry "$NPM_CONFIG_REGISTRY"',
	]);
	if (
		oidcCalls !== 1 ||
		tokenCalls !== 1 ||
		oidcCallIndex < 0 ||
		tokenPublishIndex <= oidcCallIndex ||
		!publishLines.includes("publish_with_oidc() {") ||
		!publishLines.includes("publish_with_token() {") ||
		!publishLines.includes('if [[ -z "${NODE_AUTH_TOKEN:-}" ]]; then') ||
		publishLines.some(
			(line) =>
				line.includes("NPM_PUBLISH_AUTH_MODE") ||
				line.includes("--provenance"),
		)
	) {
		failures.push(
			"the public release lane must publish OIDC-first with a token-backed npm publication fallback",
		);
	}
	if (
		npmPublishLines.length !== 2 ||
		npmPublishLines.some((line) => !expectedPublishLines.has(line))
	) {
		failures.push("trusted and token modes must execute exact unswallowed npm publish commands");
	}
    for (const line of [
        'wait_for_registry "$PACKAGE_NAME" "$PACKED_INTEGRITY"',
        'wait_for_registry "$ALIAS_PACKAGE_NAME" "$ALIAS_PACKED_INTEGRITY"',
    ]) {
        if (!topLevelPublishLines.includes(line)) failures.push("publication must wait for exact package-index integrity after publishing");
    }
	const swallowedPublish = npmPublishLines.some((line) => {
		const index = publishLines.indexOf(line);
		return publishLines[index + 1] === "return 0";
	});
	if (
		swallowedPublish ||
		publishLines.filter(
			(line) =>
				line ===
				'verify_published_tarball "$package_name" "$packed_integrity" || registry_status=$?',
		).length !== 2 ||
		!publishLines.includes('"$publisher" "$tarball" || publish_status=$?') ||
		!publishLines.includes('return "$publish_status"') ||
		!expectedPackageCalls.every((line) => topLevelPublishLines.includes(line))
	) {
		failures.push(
			"npm publication errors and reruns must reconcile the exact registry tarball",
		);
	}
	if (
		!hasExactRecord(publishStep?.env ?? {}, {
			ALIAS_PACKAGE_NAME: "@evalops/maestro",
			ALIAS_PACKED_INTEGRITY: "${{ steps.pack.outputs.alias_integrity }}",
			ALIAS_TARBALL: "${{ steps.pack.outputs.alias_tarball }}",
			NODE_AUTH_TOKEN: "${{ secrets.NPM_TOKEN }}",
			NPM_TAG: "${{ needs.prepare.outputs.npm_tag }}",
			PACKAGE_NAME: "${{ needs.prepare.outputs.package_name }}",
			PACKED_INTEGRITY: "${{ steps.pack.outputs.integrity }}",
			RELEASE_VERSION: "${{ needs.prepare.outputs.release_version }}",
			TARBALL: "${{ steps.pack.outputs.tarball }}",
			NPM_CONFIG_FETCH_RETRIES: "1",
			NPM_CONFIG_FETCH_RETRY_MAXTIMEOUT: "2000",
			NPM_CONFIG_FETCH_RETRY_MINTIMEOUT: "1000",
			NPM_CONFIG_FETCH_TIMEOUT: "30000",
			NPM_CONFIG_REGISTRY: "https://registry.npmjs.org",
		})
	) {
		failures.push("publish must bind exact package identity and tarball integrity");
	}
	const boundedNpmEnv = {
		NPM_CONFIG_FETCH_RETRIES: "1",
		NPM_CONFIG_FETCH_RETRY_MAXTIMEOUT: "2000",
		NPM_CONFIG_FETCH_RETRY_MINTIMEOUT: "1000",
		NPM_CONFIG_FETCH_TIMEOUT: "30000",
		NPM_CONFIG_REGISTRY: "https://registry.npmjs.org",
	};
	for (const [name, step] of [
		["publish", publishStep],
		["packed-package smoke", findStep(publish, "Smoke packed package without JS runtime")],
		["post-publish canary", findStep(canary, "Verify published package from npm")],
	]) {
		for (const [key, value] of Object.entries(boundedNpmEnv)) {
			if (step?.env[key] !== value) {
				failures.push(`${name} must set bounded npm network configuration`);
				break;
			}
		}
	}

	for (const [name, job] of [
		["publish", publish],
		["post-publish canary", canary],
	]) {
		const setupNode = job.steps.find((step) =>
			step.uses.startsWith("actions/setup-node@"),
		);
		if (
			setupNode?.with["registry-url"] !== "https://registry.npmjs.org" ||
			setupNode.with["node-version"] !== "24"
		) {
			failures.push(`${name} must pin setup-node to the public npm registry`);
		}
	}

	const releaseSteps = Object.values(jobs).flatMap((job) =>
		job.steps.filter((step) =>
			step.uses.startsWith("softprops/action-gh-release@"),
		),
	);
	const releaseStep = releaseSteps[0];
	if (releaseSteps.length !== 1) {
		failures.push("workflow must contain exactly one GitHub release action");
	}
	if (!release.steps.includes(releaseStep)) {
		failures.push("GitHub release action must run in the retryable github-release job");
	}
	const releaseTagStep = findStep(release, "Verify release tag has not moved");
	const releaseTagLines = executableLines(releaseTagStep?.run ?? "");
	if (
		requiredStepCanBeSkippedOrIgnored(releaseTagStep) ||
		!hasExactRecord(releaseTagStep?.env ?? {}, {
			EXPECTED_RELEASE_SHA: "${{ needs.prepare.outputs.release_sha }}",
			RELEASE_TAG: "${{ needs.prepare.outputs.release_tag }}",
		}) ||
		release.steps.indexOf(releaseTagStep) < 0 ||
		release.steps.indexOf(releaseTagStep) >= release.steps.indexOf(releaseStep)
	) {
		failures.push(
			"GitHub release creation must verify the current tag immediately before release",
		);
	}
	for (const line of [
		"for attempt in 1 2 3; do",
		"if timeout 60s git \\",
		"-c http.lowSpeedLimit=1000 \\",
		"-c http.lowSpeedTime=30 \\",
		'fetch --force --no-tags origin "refs/tags/${RELEASE_TAG}:refs/tags/${RELEASE_TAG}"; then',
		'if [[ "$attempt" -eq 3 ]]; then',
		"sleep 2",
		'current_release_sha="$(git rev-list -n 1 "$RELEASE_TAG")"',
		'if [[ "$current_release_sha" != "$EXPECTED_RELEASE_SHA" ]]; then',
		"exit 1",
	]) {
		if (!releaseTagLines.includes(line)) {
			failures.push(
				"GitHub release creation must fail closed if the release tag moved",
			);
			break;
		}
	}
	if (requiredStepCanBeSkippedOrIgnored(releaseStep)) {
		failures.push("GitHub release creation must not be conditional or ignored");
	}
	if (
		releaseStep?.with.tag_name !==
			"${{ needs.prepare.outputs.release_tag }}" ||
		releaseStep?.with.name !==
			"Maestro ${{ needs.prepare.outputs.release_version }}" ||
		releaseStep?.with.files !== "release-assets/*.json\nrelease-assets/*.tgz\nrelease-assets/*.tar.gz\nrelease-assets/*.txt\nrelease-assets/*SUMS\nrelease-assets/*.bundle\nrelease-assets/maestro-linux-arm64\nrelease-assets/maestro-linux-x64\nrelease-assets/maestro-darwin-arm64\nrelease-assets/maestro-darwin-x64"
	) {
		failures.push(
			"GitHub release metadata and files must bind to immutable prepare outputs",
		);
	}
	if (releaseStep?.with.target_commitish) {
		failures.push(
			"GitHub release creation must use the verified tag and must not retarget a detached commit",
		);
	}
	const publishUploads = publish.steps.filter((step) =>
		step.uses.startsWith("actions/upload-artifact@"),
	);
	for (const artifactName of [
		"npm-tarball-${{ needs.prepare.outputs.release_tag }}",
		"release-web-dist-${{ needs.prepare.outputs.release_tag }}",
	]) {
		if (
			!publishUploads.some(
				(step) =>
					step.with.name === artifactName && step.with.overwrite === "true",
			)
		) {
			failures.push(`publish must persist retryable artifact ${artifactName}`);
		}
	}
	const releaseDownloads = release.steps.filter((step) =>
		step.uses.startsWith("actions/download-artifact@"),
	);
	if (
		releaseDownloads.length !== 3 ||
		!releaseDownloads.some(
			(step) =>
				step.with.name ===
					"npm-tarball-${{ needs.prepare.outputs.release_tag }}" &&
				step.with.path === "release-assets",
		) ||
		!releaseDownloads.some(
			(step) =>
				step.with.pattern === "maestro-*" &&
				step.with.path === "release-assets" &&
				step.with["merge-multiple"] === "true",
		) ||
		!releaseDownloads.some(
			(step) =>
				step.with.name ===
					"release-web-dist-${{ needs.prepare.outputs.release_tag }}" &&
				step.with.path === "release-assets",
		)
	) {
		failures.push("github-release must restore the exact immutable release artifacts");
	}
	const canaryStep = findStep(canary, "Verify published package from npm");
	const canaryLines = executableLines(canaryStep?.run ?? "");
	if (requiredStepCanBeSkippedOrIgnored(canaryStep)) {
		failures.push("registry canary must not be conditional or ignored");
	}
	const expectedCanaryLines = [
		"set -euo pipefail",
		"node scripts/smoke-registry-install.js \\",
		'--package "$PACKAGE_NAME" \\',
		'--version "$RELEASE_VERSION" \\',
		'--cli-command "$CLI_COMMAND"',
	];
	if (JSON.stringify(canaryLines) !== JSON.stringify(expectedCanaryLines)) {
		failures.push("post-publish canary must execute only the exact registry smoke");
	}
	if (
		canaryStep?.env.PACKAGE_NAME !==
			"${{ needs.prepare.outputs.package_name }}" ||
		canaryStep?.env.RELEASE_VERSION !==
			"${{ needs.prepare.outputs.release_version }}"
	) {
		failures.push("registry canary must bind package and version to prepare outputs");
	}
	const replayStep = findStep(canary, "Validate published replay evidence");
	if (!replayStep) {
		failures.push("post-publish canary must validate replay evidence");
	} else if (requiredStepCanBeSkippedOrIgnored(replayStep)) {
		failures.push("published replay validation must not be conditional or ignored");
	} else if (
		JSON.stringify(executableLines(replayStep.run)) !==
		JSON.stringify([
			"node scripts/verify-published-replay-evidence.js --evidence-dir published-replay-evidence",
		])
	) {
		failures.push("published replay validation must execute the exact evidence verifier");
	}
	const replayUpload = findStep(canary, "Upload published replay evidence");
	if (
		!replayUpload?.uses.startsWith("actions/upload-artifact@") ||
		replayUpload.with.name !==
			"published-replay-evidence-${{ needs.prepare.outputs.release_tag }}" ||
		replayUpload.with.path !== "published-replay-evidence/*.json" ||
		replayUpload.with.overwrite !== "true"
	) {
		failures.push(
			"post-publish canary must replace exact-tag replay evidence on reruns",
		);
	}

	return failures;
}

export async function checkReleaseWorkflow(path = DEFAULT_WORKFLOW) {
	const source = await readFile(path, "utf8");
	return validateReleaseWorkflow(source);
}

if (process.argv[1] === fileURLToPath(import.meta.url)) {
	const failures = await checkReleaseWorkflow();
	if (failures.length > 0) {
		console.error("Release workflow contract check failed:");
		for (const failure of failures) console.error(`- ${failure}`);
		process.exitCode = 1;
	} else {
		console.log("Release workflow contract check passed.");
	}
}

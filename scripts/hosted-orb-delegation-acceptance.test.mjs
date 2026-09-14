import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";
import {
	loadContract,
	REQUIRED_CASE_IDS,
	runRecordedAcceptance,
	validateContract,
} from "./hosted-orb-delegation-acceptance.mjs";

test("the public fixture is a repository-neutral hosted protocol contract", async () => {
	const contract = await loadContract();
	assert.doesNotThrow(() => validateContract(contract));
	assert.deepEqual(contract.cases.map((entry) => entry.id), REQUIRED_CASE_IDS);
	assert.equal(contract.contract_sources, undefined);
	assert.equal(contract.live_gate, undefined);
	assert.equal(contract.atomic_launch.current_origin_status, undefined);
	assert.equal(contract.atomic_launch.primitive, "computer_launch");
	assert.equal(contract.atomic_launch.compatibility_alias, "orb_launch_hosted_task");
});

test("the public validator reads the checked-in Orb fixture paths", async () => {
	const openapi = JSON.parse(await readFile(
		new URL("../test/fixtures/hosted-orb-public/products/orb/api/openapi.json", import.meta.url),
		"utf8",
	));
	const mcpDoc = await readFile(
		new URL("../test/fixtures/hosted-orb-public/products/orb/docs/mcp.md", import.meta.url),
		"utf8",
	);
	assert.ok(openapi.paths["/hosted/launches"]?.post);
	assert.match(mcpDoc, /`computer_launch`/);
	assert.match(mcpDoc, /`orb_launch_hosted_task`/);
	assert.doesNotMatch(mcpDoc, /\bprivate\b/i);
});

test("the public validator rejects route and scope drift", async () => {
	const routeDrifted = structuredClone(await loadContract());
	routeDrifted.tools.computer_launch.path = "/internal/launches";
	assert.throws(() => validateContract(routeDrifted), /absent from public Orb OpenAPI/);

	const scopeDrifted = structuredClone(await loadContract());
	scopeDrifted.tools.orb_start_task.scope = "unknown:write";
	assert.throws(() => validateContract(scopeDrifted), /scope.*drifted|drifted.*scope/i);
});

test("recorded acceptance covers every public case without network access", async () => {
	const result = await runRecordedAcceptance(await loadContract());
	assert.equal(result.status, "passed");
	assert.deepEqual(result.cases.map((entry) => entry.id), REQUIRED_CASE_IDS);
	assert.ok(result.operations >= REQUIRED_CASE_IDS.length);
	assert.equal(result.network_requests, 0);
});

test("the public validator rejects live-gate metadata", async () => {
	const contract = structuredClone(await loadContract());
	contract.live_gate = { enabled: true };
	assert.throws(() => validateContract(contract), /live-gate inventory/);
});

test("the managed hosted acceptance is a hard gate when explicitly configured", async () => {
	const workflow = await readFile(
		new URL("../.github/workflows/maestro-ci.yml", import.meta.url),
		"utf8",
	);
	const block =
		workflow.split("  hosted-orb-delegation-live:")[1]?.split(/\n  [a-z]/)[0] ??
		"";
	assert.match(block, /Optional hosted Orb acceptance was not requested; skipping\./);
	assert.match(block, /npm run smoke:hosted-orb-delegation/);
	assert.match(
		block,
		/if: \(github\.event_name != 'pull_request' \|\| github\.event\.pull_request\.head\.repo\.full_name == github\.repository\) && \(github\.event_name == 'schedule' \|\| github\.event_name == 'workflow_dispatch'\)/,
	);
	assert.match(workflow, /vars\.MAESTRO_HOSTED_ORB_LIVE_SMOKE \|\| ''/);
	assert.match(
		block,
		/continue-on-error:\s*\$\{\{\s*!\(github\.event\.inputs\.hosted_orb_live_smoke == '1' \|\| vars\.MAESTRO_HOSTED_ORB_LIVE_SMOKE == '1'\)\s*\}\}/,
	);
	assert.doesNotMatch(block, /^    continue-on-error:\s*true\b/m);
});

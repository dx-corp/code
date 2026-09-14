import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";
import { parseDocument } from "yaml";

import { evaluateIntegrationRequiredGate } from "./check-integration-required-gate.mjs";

const approvedSetupNodeRefs = new Set([
  "395ad3262231945c25e8478fd5baf05154b1d79f",
  "820762786026740c76f36085b0efc47a31fe5020",
]);

function hasApprovedSetupNodeGate(workflow) {
  const document = parseDocument(workflow, { uniqueKeys: true });
  if (document.errors.length > 0) {
    return false;
  }

  let parsed;
  try {
    parsed = document.toJS({ maxAliasCount: 100 });
  } catch {
    return false;
  }

  const steps = parsed?.jobs?.["integration-tests"]?.steps;
  if (!Array.isArray(steps)) {
    return false;
  }

  const setupNodeSteps = steps.filter(
    (step) =>
      step !== null &&
      typeof step === "object" &&
      typeof step.uses === "string" &&
      step.uses.toLowerCase().startsWith("actions/setup-node@"),
  );
  if (setupNodeSteps.length !== 1) {
    return false;
  }

  const [setupNodeStep] = setupNodeSteps;
  const ref = setupNodeStep.uses.slice("actions/setup-node@".length);
  const nodeVersion = setupNodeStep.with?.["node-version"];
  return approvedSetupNodeRefs.has(ref) && (nodeVersion === 22 || nodeVersion === "22");
}

function requiredGateFixture({ beforeSteps = "", steps }) {
  return `jobs:\n  integration-tests:\n${beforeSteps}    steps:\n${steps}`;
}

test("passes when irrelevant paths intentionally skip the integration suite", () => {
  assert.deepEqual(
    evaluateIntegrationRequiredGate({
      pathCheckResult: "success",
      integrationPathsChanged: "false",
      integrationSuiteResult: "skipped",
    }),
    {
      ok: true,
      message: "integration suite correctly skipped for irrelevant paths",
    },
  );
});

test("fails when integration path detection fails", () => {
  assert.equal(
    evaluateIntegrationRequiredGate({
      pathCheckResult: "failure",
      integrationPathsChanged: "",
      integrationSuiteResult: "skipped",
    }).ok,
    false,
  );
});

test("requires the integration suite to succeed for relevant paths", () => {
  assert.equal(
    evaluateIntegrationRequiredGate({
      pathCheckResult: "success",
      integrationPathsChanged: "true",
      integrationSuiteResult: "success",
    }).ok,
    true,
  );
  assert.equal(
    evaluateIntegrationRequiredGate({
      pathCheckResult: "success",
      integrationPathsChanged: "true",
      integrationSuiteResult: "failure",
    }).ok,
    false,
  );
});

test("maestro-ci retains the complete integration suite", () => {
  const workflow = readFileSync(
    new URL("../.github/workflows/maestro-ci.yml", import.meta.url),
    "utf8",
  );
  assert.match(workflow, /^  integration:/m);
  assert.match(workflow, /cargo test --locked -p maestro-runtime-gateway/);
  assert.match(workflow, /cargo test --locked -p maestro-tui --test tools_integration/);
  assert.match(workflow, /trap 'docker rm -f "\$redis" "\$postgres"/);
});

test("accepts only one structurally valid approved setup-node step", () => {
  for (const sha of approvedSetupNodeRefs) {
    const comment = sha.startsWith("820762") ? " # v7.0.0" : "";
    assert.equal(
      hasApprovedSetupNodeGate(
        requiredGateFixture({
          steps: `      - uses: actions/setup-node@${sha}${comment}\n        with:\n          node-version: 22`,
        }),
      ),
      true,
    );
  }

  assert.equal(
    hasApprovedSetupNodeGate(
      requiredGateFixture({
        steps: `      - name: Set up Node
        uses: actions/setup-node@820762786026740c76f36085b0efc47a31fe5020
        with:
          node-version: "22"`,
      }),
    ),
    true,
  );

  for (const ref of [
    "unpinned",
    "820762786026740c76f36085b0efc47a31fe5020-attacker",
    "820762786026740c76f36085b0efc47a31fe5020#attacker",
  ]) {
    assert.equal(
      hasApprovedSetupNodeGate(
        requiredGateFixture({
          steps: `      - uses: actions/setup-node@${ref}\n        with:\n          node-version: 22`,
        }),
      ),
      false,
    );
  }

  for (const usesKey of ['"uses"', "uses "]) {
    assert.equal(
      hasApprovedSetupNodeGate(
        requiredGateFixture({
          steps: `      - ${usesKey}: actions/setup-node@unpinned`,
        }),
      ),
      false,
    );
  }

  assert.equal(
    hasApprovedSetupNodeGate(
      requiredGateFixture({
        steps: `      - uses: actions/setup-node@820762786026740c76f36085b0efc47a31fe5020
        with:
          cache: npm
      - uses: actions/setup-node@unpinned
        with:
          node-version: 22`,
      }),
    ),
    false,
  );

  assert.equal(
    hasApprovedSetupNodeGate(
      requiredGateFixture({
        steps: `      - uses: actions/setup-node@820762786026740c76f36085b0efc47a31fe5020
        with:
          node-version: 22
      - uses: actions/setup-node@unpinned
        with:
          node-version: 22`,
      }),
    ),
    false,
  );

  assert.equal(
    hasApprovedSetupNodeGate(
      requiredGateFixture({
        steps: `      - uses: actions/setup-node@820762786026740c76f36085b0efc47a31fe5020
        with:
          node-version: 22
      - uses: Actions/setup-node@unpinned`,
      }),
    ),
    false,
  );

  assert.equal(
    hasApprovedSetupNodeGate(
      requiredGateFixture({
        steps: `      - name: Decoy setup-node text
        run: |
          - uses: actions/setup-node@820762786026740c76f36085b0efc47a31fe5020
            with:
              node-version: 22`,
      }),
    ),
    false,
  );

  assert.equal(
    hasApprovedSetupNodeGate(
      requiredGateFixture({
        beforeSteps: `    name: |
      - uses: actions/setup-node@820762786026740c76f36085b0efc47a31fe5020
        with:
          node-version: 22
`,
        steps: "      - uses: actions/checkout@de0fac2e4500dabe0009e67214ff5f5447ce83dd",
      }),
    ),
    false,
  );
});

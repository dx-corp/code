import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";

const deploymentTemplate = readFileSync(
	new URL("../deploy/helm/maestro/templates/deployment.yaml", import.meta.url),
	"utf8",
);
const values = readFileSync(
	new URL("../deploy/helm/maestro/values.yaml", import.meta.url),
	"utf8",
);
const exactSpaHealthPath = /^\s+path:\s+\/health\s*$/mu;

test("Helm HTTP probes use the control-plane health endpoint", () => {
	assert.equal(
		[...deploymentTemplate.matchAll(/^\s+path:\s+\/healthz\s*$/gmu)].length,
		1,
		"deployment liveness must use /healthz",
	);
	assert.doesNotMatch(
		deploymentTemplate,
		exactSpaHealthPath,
		"deployment probes must not use the SPA shell route /health",
	);
	assert.match(
		values,
		/^\s+path:\s+\/healthz\s*$/mu,
		"the configurable startup probe must use /healthz",
	);
	assert.doesNotMatch(
		values,
		exactSpaHealthPath,
		"the startup probe must not use the SPA shell route /health",
	);
});

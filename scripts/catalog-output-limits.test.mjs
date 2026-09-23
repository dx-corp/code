import assert from "node:assert/strict";
import test from "node:test";

import { omitUnverifiedSonarOutput } from "./catalog-output-limits.mjs";

test("only the unverified Sonar Reasoning Pro output cap is omitted", () => {
	assert.equal(omitUnverifiedSonarOutput("perplexity/sonar-reasoning-pro", 4096), undefined);
	assert.equal(omitUnverifiedSonarOutput("perplexity/sonar-reasoning-pro", 8192), 8192);
	assert.equal(omitUnverifiedSonarOutput("perplexity/another-model", 4096), 4096);
});

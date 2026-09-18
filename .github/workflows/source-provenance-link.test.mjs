import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { spawnSync } from "node:child_process";
import { test } from "node:test";

const workflow = readFileSync(new URL("./public-source-provenance.yml", import.meta.url), "utf8");
const pattern = workflow.match(/if grep -Eiq "([^"\n]*https:\/\/github[^"\n]*)" <<</)?.[1];
assert.ok(pattern, "actual workflow provenance expression");

for (const link of [
  "https://github.com/dx-corp/mono/pull/8492",
  "dx-corp/mono#8492",
  "https://github.com/evalops/mono/pull/8492",
  "evalops/mono#8492",
  "https://github.com/evalops/maestro-internal/pull/42",
  "evalops/maestro-internal#42",
  "maestro-internal#42",
]) {
  test(`accepts source-owner link: ${link}`, () => {
    assert.equal(spawnSync("grep", ["-Eiq", pattern], {input: link}).status, 0);
  });
}
for (const link of [
  "No source PR",
  "https://github.com/another/mono/pull/8492",
  "https://github.com/dx-corp/maestro/pull/1063",
  "https://github.com/evalops/maestro/pull/1063",
  "https://github.com/evalops/mono/issues/8492",
  "https://github.com/evalops/mono/pull/",
]) {
  test(`rejects non-source reference: ${link}`, () => {
    assert.equal(spawnSync("grep", ["-Eiq", pattern], {input: link}).status, 1);
  });
}

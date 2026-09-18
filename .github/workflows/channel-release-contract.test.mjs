import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { test } from "node:test";

const workflow = await readFile(new URL("./channel-release.yml", import.meta.url), "utf8");
const releaseWorkflow = await readFile(new URL("./release.yml", import.meta.url), "utf8");

test("preview schedules finalize only staged signed candidates", () => {
 assert.match(workflow, /if: github\.repository == 'dx-corp\/code' && github\.ref == 'refs\/heads\/main'/);
 assert.doesNotMatch(workflow, /github\.repository == '(?:evalops|dx-corp)\/maestro'/);
 assert.match(workflow, /"0 5 \* \* \*" # alpha/);
 assert.match(workflow, /"30 5 \* \* \*" # beta/);
 assert.match(workflow, /MONO_SHA256SUMS\.cosign\.bundle/);
 assert.match(workflow, /select\(\.draft/);
 assert.match(workflow, /contents: read/);
 assert.doesNotMatch(workflow, /contents: write/);
 assert.match(workflow, /gh workflow run release\.yml --ref main/);
 assert.match(workflow, /No staged signed/);
 assert.doesNotMatch(workflow, /git (?:push|tag|commit)/);
 assert.doesNotMatch(workflow, /scripts\/version\.js/);
 assert.doesNotMatch(workflow, /cancel-in-progress: true/);
});

test("channel pointers carry the signed native release contract", () => {
	assert.match(releaseWorkflow, /id-token: write/);
	assert.match(releaseWorkflow, /verify-staged-release\.mjs/);
 assert.match(releaseWorkflow, /\.receipt\.sourceSha/);
 assert.doesNotMatch(releaseWorkflow, /create-release-metadata\.mjs/);
	assert.match(releaseWorkflow, /create-release-channel-manifest\.mjs/);
	assert.match(releaseWorkflow, /softprops\/action-gh-release@[0-9a-f]{40}/);
	assert.match(releaseWorkflow, /release-assets\/channel-manifest\.json/);
	assert.match(releaseWorkflow, /release-assets\/manifest\.json/);
	assert.match(releaseWorkflow, /release-assets\/version\.json/);
	assert.match(releaseWorkflow, /cosign sign-blob --yes --bundle SHA256SUMS\.cosign\.bundle/);
	assert.match(releaseWorkflow, /cosign sign-blob --yes --bundle "\$\{binary\}\.cosign\.bundle"/);
	assert.match(releaseWorkflow, /files: \|\n\s+release-assets\/\*\.json/);
	assert.doesNotMatch(releaseWorkflow, /\.\/\.github\/actions\/gcs-artifacts/);
	assert.doesNotMatch(releaseWorkflow, /MAESTRO_RELEASES_PREFIX/);
	assert.doesNotMatch(releaseWorkflow, /gcloud storage/);
	assert.doesNotMatch(releaseWorkflow, /target_commitish/);
	assert.doesNotMatch(releaseWorkflow, /maestro-\$\{RELEASE_CHANNEL\}-channel/);
	assert.doesNotMatch(releaseWorkflow, /gh release upload "\$channel_tag"/);
});

const tagWorkflow = await readFile(new URL("./tag-release.yml", import.meta.url), "utf8");
test("tag retries dispatch main and correlate the normalized release version", () => {
  assert.match(releaseWorkflow, /run-name: Release \$\{\{ startsWith/);
  assert.match(tagWorkflow, /--ref main/);
  assert.doesNotMatch(tagWorkflow, /--ref "\$\{RELEASE_TAG\}"/);
  assert.equal((tagWorkflow.match(/\.displayTitle ==/g) || []).length, 4);
  assert.equal((tagWorkflow.match(/--json [^\n]*displayTitle/g) || []).length, 4);
  assert.match(tagWorkflow, /outputs\.staged_ready == 'true'/);
  assert.match(tagWorkflow, /MONO_SHA256SUMS\.cosign\.bundle/);
  assert.match(tagWorkflow, /elif grep -q 'HTTP 404'/);
  assert.match(tagWorkflow, /cat "\$release_error" >&2\n\s+exit 1/);
});

test("only authenticated enabled device helpers are extracted and published", () => {
  assert.match(releaseWorkflow, /--pattern 'code-device-\*\.json'/);
  assert.match(releaseWorkflow, /verify-staged-release\.mjs[\s\S]*jq -r '\.enabled'[\s\S]*tar -xzf/);
  assert.match(releaseWorkflow, /files\+=\(runtime-passport-maestro-\*\.json code-device-\*\.json\)/);
  assert.doesNotMatch(releaseWorkflow, /files\+=\([^\n]*deixic-code-device-\*/);
});

test("older source verifiers cannot authorize unhashed capability markers", () => {
  const authentication = releaseWorkflow.indexOf('node scripts/verify-staged-release.mjs release-binaries');
  const markerCheck = releaseWorkflow.indexOf('code-device-${platform}\\.json$');
  const decision = releaseWorkflow.indexOf("jq -r '.enabled'");
  assert.ok(authentication >= 0 && authentication < markerCheck && markerCheck < decision);
  assert.match(releaseWorkflow, /MONO_SHA256SUMS \| sha256sum --check --strict/);
});

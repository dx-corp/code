import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import { parseWorkflow } from "./check-release-workflow-contract.mjs";

const workflow = parseWorkflow(readFileSync(new URL("./tag-release.yml", import.meta.url), "utf8"));
const staged = workflow.jobs["tag-current-version"].steps.find(step => step.id === "staged-release");
const bundle = { name: "MONO_SHA256SUMS.cosign.bundle" };
const release = (tag, draft, assets) => ({ tag_name: tag, draft, assets });

function run(pages, ghStatus = 0) {
  const root = mkdtempSync(join(tmpdir(), "tag-release-staged-"));
  try {
    const tools = join(root, "tools");
    mkdirSync(tools);
    // Paginated `gh api` prints one JSON array per page, as the real CLI does.
    writeFileSync(join(root, "pages.json"), pages.map(page => JSON.stringify(page)).join("\n"));
    writeFileSync(
      join(tools, "gh"),
      `#!/bin/sh\n[ "$*" = "api --paginate repos/dx-corp/code/releases?per_page=100" ] || exit 91\ncat "${join(root, "pages.json")}"\nexit ${ghStatus}\n`,
      { mode: 0o755 },
    );
    const output = join(root, "output");
    const summary = join(root, "summary");
    writeFileSync(output, "");
    writeFileSync(summary, "");
    const result = spawnSync("bash", ["--noprofile", "--norc", "-e", "-c", staged.run], {
      encoding: "utf8",
      timeout: 10000,
      env: {
        ...process.env,
        PATH: `${tools}:${process.env.PATH}`,
        GITHUB_OUTPUT: output,
        GITHUB_REPOSITORY: "dx-corp/code",
        GITHUB_STEP_SUMMARY: summary,
        RELEASE_TAG: "v1.2.3",
        RUNNER_TEMP: root,
      },
    });
    return { status: result.status, output: readFileSync(output, "utf8"), summary: readFileSync(summary, "utf8") };
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
}

test("signed Mono artifacts staged on a draft release are ready to finish", () => {
  const older = release("v1.2.2", false, [bundle]);
  assert.deepEqual(run([[older], [release("v1.2.3", true, [{ name: "maestro-linux-x64" }, bundle])]]), {
    status: 0,
    output: "ready=true\n",
    summary: "",
  });
  assert.equal(run([[release("v1.2.3", false, [bundle])]]).output, "ready=true\n");
});

test("a tag without its own signed bundle keeps waiting", () => {
  for (const pages of [
    [[]],
    [[release("v1.2.2", false, [bundle])]],
    [[release("v1.2.3", true, [])]],
    [[release("v1.2.3", true, [{ name: "SHA256SUMS.cosign.bundle" }])]],
  ]) {
    const waiting = run(pages);
    assert.equal(waiting.status, 0);
    assert.equal(waiting.output, "ready=false\n");
    assert.match(waiting.summary, /v1\.2\.3 is awaiting signed Mono artifacts/);
  }
});

test("an unreadable release list stops the job; it is never read as not staged", () => {
  const failed = run([[release("v1.2.3", true, [bundle])]], 1);
  assert.notEqual(failed.status, 0);
  assert.equal(failed.output, "");
});

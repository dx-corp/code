import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import { parseWorkflow } from "./check-release-workflow-contract.mjs";

const source = readFileSync(new URL("./tag-release.yml", import.meta.url), "utf8");
const workflow = parseWorkflow(source);
const job = workflow.jobs["tag-current-version"];
const guard = job.steps.find(step => step.id === "frozen-release");

test("the existing-tag guard runs only for an unpublished tag behind main with no release in flight", () => {
  assert.ok(guard, "tag-release keeps its existing-tag guard");
  const condition = source.match(/- id: frozen-release\n.*\n\s+if: (.*)\n/)?.[1] ?? "";
  for (const required of [
    "steps.release.outputs.tag_exists == 'true'",
    "steps.release.outputs.tag_matches_head != 'true'",
    "steps.release.outputs.package_changed_since_tag == 'true'",
    "steps.registry-release.outputs.published != 'true'",
    "steps.active-release.outputs.active_count == '0'",
  ]) assert.ok(condition.includes(required), required);
  assert.match(guard.run, /exit 1/);
});

test("a frozen release tag keeps main green; a tag outside frozen history is refused", () => {
  const root = mkdtempSync(join(tmpdir(), "tag-release-frozen-"));
  try {
    const git = (...args) => {
      const result = spawnSync("git", args, { cwd: root, encoding: "utf8" });
      assert.equal(result.status, 0, result.stdout + result.stderr);
      return result.stdout.trim();
    };
    const commit = (version, source, message) => {
      writeFileSync(join(root, "package.json"), JSON.stringify({ name: "fixture", version }));
      writeFileSync(join(root, "source.txt"), source);
      git("add", ".");
      git("commit", "-m", message);
      return git("rev-parse", "HEAD");
    };
    git("init", "--initial-branch", "main");
    git("config", "user.name", "test");
    git("config", "user.email", "test@example.invalid");
    const frozenOnMain = commit("1.2.3", "reviewed", "release v1.2.3");
    git("checkout", "-b", "release-branch");
    const frozenOnBranch = commit("1.2.3", "reviewed release branch", "frozen release source");
    const otherVersionOnBranch = commit("1.2.4", "another version", "wrong version");
    git("checkout", "main");
    const head = commit("1.2.3", "later main", "main moved past the tag");
    const orphan = git("commit-tree", `${frozenOnMain}^{tree}`, "-m", "identical tree outside reviewed history");

    const run = (tagTarget, releaseBranchTip) => {
      spawnSync("git", ["update-ref", "-d", "refs/remotes/origin/releases/v1.2.3"], { cwd: root });
      if (releaseBranchTip) git("update-ref", "refs/remotes/origin/releases/v1.2.3", releaseBranchTip);
      const output = join(root, "github-output");
      writeFileSync(output, "");
      const result = spawnSync("bash", ["--noprofile", "--norc", "-e", "-c", guard.run], {
        cwd: root,
        encoding: "utf8",
        timeout: 10000,
        env: {
          ...process.env,
          GITHUB_OUTPUT: output,
          GITHUB_SHA: head,
          RELEASE_TAG: "v1.2.3",
          RELEASE_VERSION: "1.2.3",
          TAG_TARGET: tagTarget,
        },
      });
      return { ...result, output: readFileSync(output, "utf8") };
    };

    const onMain = run(frozenOnMain);
    assert.equal(onMain.status, 0, onMain.stdout + onMain.stderr);
    assert.equal(onMain.output, "frozen=true\n");
    assert.match(onMain.stdout, /frozen at [0-9a-f]{40} in main/);

    const onBranch = run(frozenOnBranch, otherVersionOnBranch);
    assert.equal(onBranch.status, 0, onBranch.stdout + onBranch.stderr);
    assert.match(onBranch.stdout, /in releases\/v1\.2\.3/);

    for (const [name, refused] of Object.entries({
      "tag outside main and release-branch history": run(orphan, otherVersionOnBranch),
      "release branch missing": run(frozenOnBranch),
      "tag carries another package version": run(otherVersionOnBranch, otherVersionOnBranch),
      "tag target is not a commit id": run("v1.2.3"),
    })) {
      assert.equal(refused.status, 1, `${name}: ${refused.stdout}${refused.stderr}`);
      assert.equal(refused.output, "", name);
      assert.match(refused.stdout, /::error title=Release tag is not a frozen release source::/, name);
    }
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

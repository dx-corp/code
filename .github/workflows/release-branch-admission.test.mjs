import assert from "node:assert/strict";
import { mkdtempSync, readFileSync, writeFileSync, mkdirSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { spawnSync } from "node:child_process";
import { test } from "node:test";
import { parseWorkflow } from "./check-release-workflow-contract.mjs";

const workflow = parseWorkflow(readFileSync(new URL("./release.yml", import.meta.url), "utf8"));
const resolver = workflow.jobs.prepare.steps.find(step => step.name === "Resolve immutable release tag");
const start = resolver.run.indexOf("timeout 60s git fetch --no-tags origin main");
const end = resolver.run.indexOf('git checkout --detach "$release_sha"');
assert.ok(start >= 0 && end > start);
const admission = resolver.run.slice(start, end);
const rules = ["pull_request", "non_fast_forward", "deletion", "required_linear_history"].map(type => ({type}));
rules.push({type:"required_status_checks", parameters:{strict_required_status_checks_policy:true, required_status_checks:[
  {context:"require-internal-pr", integration_id:15368},
  {context:"unresolved-review-threads / unresolved-review-threads", integration_id:15368},
  {context:"maestro-ci", integration_id:15368},
]}});

test("release admission requires protected history and all pinned checks", () => {
  const root = mkdtempSync(join(tmpdir(), "maestro-release-branch-"));
  try {
    const remote = join(root,"remote.git"), checkout = join(root,"checkout");
    let cwd = root;
    const git = (...args) => {
      const result = spawnSync("git", args, {cwd, encoding:"utf8"});
      assert.equal(result.status, 0, result.stdout + result.stderr);
      return result.stdout.trim();
    };
    git("init", "--bare", remote);
    git("clone", remote, checkout);
    cwd = checkout;
    git("checkout", "-b", "main");
    git("config", "user.name", "test");
    git("config", "user.email", "test@example.invalid");
    writeFileSync(join(checkout,"source.txt"), "base");
    git("add", "."); git("commit", "-m", "base");
    git("checkout", "-b", "releases/v0.10.76");
    writeFileSync(join(checkout,"source.txt"), "frozen release");
    git("commit", "-am", "reviewed release");
    const frozen = git("rev-parse", "HEAD");
    git("checkout", "main");
    writeFileSync(join(checkout,"source.txt"), "later main");
    git("commit", "-am", "advance main");
    git("push", "origin", "main", "releases/v0.10.76");
    const main = git("rev-parse", "HEAD");
    const orphan = git("commit-tree", "HEAD^{tree}", "-m", "unreviewed identical source");
    const tools = join(root,"tools"); mkdirSync(tools);
    writeFileSync(join(tools,"gh"), '#!/bin/sh\n[ "$*" = "api repos/evalops/maestro/rules/branches/releases%2Fv0.10.76" ] || exit 91\nprintf "%s\\n" "$TEST_RELEASE_RULES"\n', {mode:0o755});
    const run = (sha, policy) => spawnSync("bash", ["--norc", "-e", "-c", admission], {
      cwd, encoding:"utf8", timeout:10000,
      env:{...process.env, PATH:`${tools}:${process.env.PATH}`, release_sha:sha, release_tag:"v0.10.76", TEST_RELEASE_RULES:JSON.stringify(policy)},
    });
    const accepted = run(frozen, rules);
    assert.equal(accepted.status, 0, accepted.stdout + accepted.stderr);
    assert.equal(run(main, []).status, 0, "existing protected-main admission remains valid");
    const relaxed = structuredClone(rules); relaxed.at(-1).parameters.strict_required_status_checks_policy = false;
    const wrongApp = structuredClone(rules); wrongApp.at(-1).parameters.required_status_checks.at(-1).integration_id = 1;
    for (const policy of [[], rules.slice(1), relaxed, wrongApp]) assert.notEqual(run(frozen, policy).status, 0);
    assert.notEqual(run(orphan, rules).status, 0, "unreviewed identical trees remain refused");
    assert.equal(git("rev-parse", "origin/main"), main);
    assert.equal(git("rev-parse", "releases/v0.10.76"), frozen);
  } finally { rmSync(root, {recursive:true, force:true}); }
});

test("public main cannot create a release tag from moving source", () => {
  const tag = parseWorkflow(readFileSync(new URL("./tag-release.yml", import.meta.url), "utf8"));
  const release = tag.jobs["tag-current-version"].steps.find(step => step.id === "release");
  assert.equal(release.with["create-tag-if-missing"], "${{ github.repository != 'evalops/maestro' }}");
});

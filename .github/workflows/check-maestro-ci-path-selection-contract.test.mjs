import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";

import { parse as parseYaml } from "yaml";
import { classifyChangedPaths, selectForEvent } from "../../scripts/select-maestro-ci-changes.mjs";

const workflowText = readFileSync(new URL("./maestro-ci.yml", import.meta.url), "utf8");
const workflowDocument = parseYaml(workflowText);
const tooling = readFileSync(new URL("../../scripts/run-ci-tooling.sh", import.meta.url), "utf8");
const productCargoToml = readFileSync(new URL("../../Cargo.toml", import.meta.url), "utf8");
const BASE_REPOSITORY = "dx-corp/code";
const targetOutputs = {
  "rust-tests": "rust_tests",
  "native-release": "native_release",
  "jetbrains-plugin": "jetbrains_plugin",
};
const targetJobNames = new Set(Object.keys(targetOutputs));
const advisoryJobNames = new Set(["coverage", "perf-baseline", "hosted-orb-delegation-live"]);
const terminalNeeds = workflowDocument.jobs["maestro-ci"].needs;

function assertWorkflowContract(document) {
  assert.ok(document.jobs["select-changes"], "public workflow must include select-changes");
  assert.ok(document.jobs["maestro-ci"].needs.includes("select-changes"), "required summary must depend on select-changes");
  summaryPython(document);
}

function defaultSelectorOutputs(eventName = "pull_request") {
  const reason = eventName === "pull_request" ? "matched_path" : "non_pr";
  return {
    selection_valid: "true",
    rust_tests: "true",
    rust_tests_reason: reason,
    native_release: "true",
    native_release_reason: reason,
    jetbrains_plugin: "true",
    jetbrains_plugin_reason: reason,
  };
}

function workflowConditionContext({
  eventName = "pull_request",
  headRepo = BASE_REPOSITORY,
  protocolResult = "success",
  selectorResult = "success",
  outputs = defaultSelectorOutputs(eventName),
  cancelled = false,
} = {}) {
  return {
    cancelled,
    github: {
      event_name: eventName,
      repository: BASE_REPOSITORY,
      event:
        eventName === "pull_request"
          ? { pull_request: { head: { repo: { full_name: headRepo } } } }
          : {},
    },
    needs: {
      "protocol-contracts": { result: protocolResult },
      "select-changes": { result: selectorResult, outputs },
    },
  };
}

function evaluateWorkflowCondition(source, context) {
  const tokenPattern = /\s*(&&|\|\||==|!=|!|\(|\)|'[^']*'|[A-Za-z_][A-Za-z0-9_.-]*)/y;
  const tokens = [];
  let offset = 0;
  while (offset < source.length) {
    tokenPattern.lastIndex = offset;
    const match = tokenPattern.exec(source);
    if (!match) {
      if (source.slice(offset).trim() === "") break;
      throw new Error(`unsupported workflow expression near ${source.slice(offset)}`);
    }
    tokens.push(match[1]);
    offset = tokenPattern.lastIndex;
  }
  let index = 0;
  const take = (token) => {
    if (tokens[index] !== token) return false;
    index += 1;
    return true;
  };
  const valueAt = (path) => path.split(".").reduce((value, part) => (value == null ? undefined : value[part]), context);
  let parseOr;
  const parsePrimary = () => {
    if (take("(")) {
      const value = parseOr();
      assert.equal(take(")"), true);
      return value;
    }
    const token = tokens[index++];
    if (token?.startsWith("'")) return token.slice(1, -1);
    if (tokens[index] === "(") {
      index += 1;
      assert.equal(take(")"), true);
      if (token === "always") return true;
      if (token === "cancelled") return Boolean(context.cancelled);
      throw new Error(`unsupported workflow function ${token}`);
    }
    return valueAt(token);
  };
  const parseUnary = () => (take("!") ? !Boolean(parseUnary()) : parsePrimary());
  const parseEquality = () => {
    const left = parseUnary();
    if (take("==")) return left === parseUnary();
    if (take("!=")) return left !== parseUnary();
    return left;
  };
  const parseAnd = () => {
    let value = parseEquality();
    while (take("&&")) value = Boolean(parseEquality()) && Boolean(value);
    return value;
  };
  parseOr = () => {
    let value = parseAnd();
    while (take("||")) value = Boolean(parseAnd()) || Boolean(value);
    return value;
  };
  const value = parseOr();
  assert.equal(index, tokens.length, "workflow condition has unparsed tokens");
  return Boolean(value);
}

function summaryPython(document = workflowDocument) {
  const step = document.jobs["maestro-ci"].steps.find(
    (candidate) => candidate.name === "Require every needed maestro-ci job",
  );
  assert.ok(step, "terminal summary step must exist");
  const marker = "python3 - <<'PY'\n";
  const start = step.run.indexOf(marker);
  assert.notEqual(start, -1, "terminal summary must keep its inline Python implementation");
  const codeStart = start + marker.length;
  const end = step.run.indexOf("\nPY", codeStart);
  assert.notEqual(end, -1, "terminal summary must close its inline Python block");
  return step.run.slice(codeStart, end);
}

function completeNeeds(eventName = "pull_request") {
  const needs = Object.fromEntries(terminalNeeds.map((name) => [name, { result: "success" }]));
  needs["select-changes"] = { result: "success", outputs: defaultSelectorOutputs(eventName) };
  if (eventName !== "schedule" && eventName !== "workflow_dispatch") {
    for (const name of advisoryJobNames) needs[name] = { result: "skipped" };
  }
  return needs;
}

function executeSummary(eventName, needs) {
  return spawnSync("python3", ["-c", summaryPython()], {
    encoding: "utf8",
    timeout: 5_000,
    maxBuffer: 1024 * 1024,
    env: {
      ...process.env,
      HEAD_SHA: "a".repeat(40),
      EVENT_NAME: eventName,
      NEEDS_JSON: JSON.stringify(needs),
    },
  });
}

function runGit(repo, args) {
  const result = spawnSync("git", args, {
    cwd: repo,
    encoding: "utf8",
    timeout: 10_000,
    maxBuffer: 1024 * 1024,
    env: {
      ...process.env,
      GIT_AUTHOR_NAME: "Maestro selector test",
      GIT_AUTHOR_EMAIL: "selector-test@example.invalid",
      GIT_COMMITTER_NAME: "Maestro selector test",
      GIT_COMMITTER_EMAIL: "selector-test@example.invalid",
    },
  });
  assert.equal(result.status, 0, `${args.join(" ")} failed: ${result.stderr}`);
  return result.stdout.trim();
}

function commitGitTree(repo, message) {
  runGit(repo, ["add", "-A"]);
  runGit(repo, ["commit", "--quiet", "-m", message]);
  return runGit(repo, ["rev-parse", "HEAD"]);
}

test("selector job uses trusted light infrastructure, existing pins, and no credential persistence", () => {
  assertWorkflowContract(workflowDocument);
  const selector = workflowDocument.jobs["select-changes"];
  assert.equal(selector.if, workflowDocument.jobs["protocol-contracts"].if);
  assert.equal(selector["runs-on"], "${{ vars.MAESTRO_CI_LIGHT_RUNNER || 'evalops-internal-arc-light' }}");
  assert.deepEqual(selector.outputs, {
    selection_valid: "${{ steps.select.outputs.selection_valid }}",
    rust_tests: "${{ steps.select.outputs.rust_tests }}",
    rust_tests_reason: "${{ steps.select.outputs.rust_tests_reason }}",
    native_release: "${{ steps.select.outputs.native_release }}",
    native_release_reason: "${{ steps.select.outputs.native_release_reason }}",
    jetbrains_plugin: "${{ steps.select.outputs.jetbrains_plugin }}",
    jetbrains_plugin_reason: "${{ steps.select.outputs.jetbrains_plugin_reason }}",
  });
  const checkout = selector.steps.find((step) => step.uses?.startsWith("actions/checkout@"));
  const existingCheckout = workflowDocument.jobs["rust-tests"].steps.find((step) =>
    step.uses?.startsWith("actions/checkout@"),
  );
  assert.equal(checkout.uses, existingCheckout.uses);
  assert.deepEqual(checkout.with, { "fetch-depth": 0, "persist-credentials": false });
  assert.equal(
    selector.steps.find((step) => step.uses?.startsWith("actions/setup-node@")).uses,
    workflowDocument.jobs["rust-tests"].steps.find((step) => step.uses?.startsWith("actions/setup-node@")).uses,
  );
  assert.match(selector.steps.find((step) => step.id === "select").run, /node scripts\/select-maestro-ci-changes\.mjs/);
  assert.equal(selector.permissions, undefined);
});

test("public contract rejects a missing selector job or inline summary", () => {
  const withoutSelector = { ...workflowDocument, jobs: { ...workflowDocument.jobs } };
  delete withoutSelector.jobs["select-changes"];
  assert.throws(() => assertWorkflowContract(withoutSelector), /public workflow must include select-changes/);

  const terminal = workflowDocument.jobs["maestro-ci"];
  const withoutSummary = {
    ...workflowDocument,
    jobs: {
      ...workflowDocument.jobs,
      "maestro-ci": {
        ...terminal,
        steps: terminal.steps.filter((step) => step.name !== "Require every needed maestro-ci job"),
      },
    },
  };
  assert.throws(() => summaryPython(withoutSummary), /terminal summary step must exist/);
});

test("target jobs skip only on a valid irrelevant-path decision for same-repository PRs", () => {
  for (const [jobName, outputName] of Object.entries(targetOutputs)) {
    const job = workflowDocument.jobs[jobName];
    assert.deepEqual(job.needs, ["protocol-contracts", "select-changes"]);
    assert.match(job.if, /!cancelled\(\)/);

    const skipOutputs = defaultSelectorOutputs("pull_request");
    skipOutputs[outputName] = "false";
    skipOutputs[`${outputName}_reason`] = "no_relevant_paths";
    assert.equal(evaluateWorkflowCondition(job.if, workflowConditionContext({ outputs: skipOutputs })), false);

    for (const context of [
      workflowConditionContext({ selectorResult: "failure", outputs: skipOutputs }),
      workflowConditionContext({ outputs: { ...skipOutputs, selection_valid: "false" } }),
      workflowConditionContext({ outputs: { ...skipOutputs, [outputName]: undefined } }),
      workflowConditionContext({ outputs: { ...skipOutputs, [`${outputName}_reason`]: "invalid" } }),
      workflowConditionContext({ eventName: "push", outputs: skipOutputs }),
    ]) {
      assert.equal(evaluateWorkflowCondition(job.if, context), true, `${jobName} must fail closed to running`);
    }
    assert.equal(
      evaluateWorkflowCondition(job.if, workflowConditionContext({ protocolResult: "failure", outputs: skipOutputs })),
      false,
    );
    assert.equal(
      evaluateWorkflowCondition(job.if, workflowConditionContext({ headRepo: "contributor/fork", outputs: skipOutputs })),
      false,
    );
    assert.equal(
      evaluateWorkflowCondition(job.if, workflowConditionContext({ cancelled: true, outputs: skipOutputs })),
      false,
    );
  }
});

test("docs-only paths skip targets while consumed docs and unknown paths select full checks", () => {
  const docsOnly = classifyChangedPaths(["docs/CONTRIBUTOR_RUNBOOK.md"], productCargoToml);
  for (const outputName of Object.values(targetOutputs)) {
    assert.equal(docsOnly[outputName], "false");
    assert.equal(docsOnly[`${outputName}_reason`], "no_relevant_paths");
  }

  for (const path of ["docs/protocols/hosted-runner-contract.md", "README.md", ".github/workflows/maestro-ci.yml"]) {
    const outputs = classifyChangedPaths([path], productCargoToml);
    assert.equal(outputs.rust_tests, "true", path);
    assert.equal(outputs.native_release, "true", path);
    assert.equal(outputs.jetbrains_plugin, "true", path);
    for (const [jobName, outputName] of Object.entries(targetOutputs)) {
      assert.equal(
        evaluateWorkflowCondition(
          workflowDocument.jobs[jobName].if,
          workflowConditionContext({ outputs }),
        ),
        true,
        `${jobName} must run for ${path}`,
      );
    }
  }
});

test("a real contributor-runbook-only change skips targets and passes the actual summary", () => {
  const repo = mkdtempSync(join(tmpdir(), "maestro-runbook-selector-git-"));
  try {
    runGit(repo, ["init", "--quiet"]);
    runGit(repo, ["checkout", "--quiet", "-b", "main"]);
    mkdirSync(join(repo, "docs"), { recursive: true });
    writeFileSync(join(repo, "Cargo.toml"), productCargoToml);
    const runbookPath = "docs/CONTRIBUTOR_RUNBOOK.md";
    const runbook = readFileSync(new URL("../../docs/CONTRIBUTOR_RUNBOOK.md", import.meta.url), "utf8");
    writeFileSync(join(repo, runbookPath), runbook);
    const baseSha = commitGitTree(repo, "base with contributor runbook");

    runGit(repo, ["checkout", "--quiet", "-b", "docs-only"]);
    writeFileSync(join(repo, runbookPath), `${runbook}\nSelector-only fixture change.\n`);
    const headSha = commitGitTree(repo, "change contributor runbook only");
    const outputs = selectForEvent(
      "pull_request",
      { pull_request: { base: { sha: baseSha }, head: { sha: headSha } } },
      { cwd: repo },
    );
    for (const target of Object.values(targetOutputs)) {
      assert.equal(outputs[target], "false");
      assert.equal(outputs[`${target}_reason`], "no_relevant_paths");
    }

    const needs = completeNeeds("pull_request");
    needs["select-changes"].outputs = outputs;
    for (const target of targetJobNames) needs[target] = { result: "skipped" };
    const summary = executeSummary("pull_request", needs);
    assert.equal(summary.status, 0, summary.stderr);
    assert.match(
      summary.stdout,
      /maestro-ci passed for [a]{40} skipped=coverage,hosted-orb-delegation-live,jetbrains-plugin,native-release,perf-baseline,rust-tests/,
    );
  } finally {
    rmSync(repo, { recursive: true, force: true });
  }
});

test("terminal summary passes normal, docs-only, unknown-path, and non-PR results", () => {
  const normal = executeSummary("pull_request", completeNeeds("pull_request"));
  assert.equal(normal.status, 0, normal.stderr);
  assert.match(normal.stdout, /maestro-ci passed for/);

  const docsOnly = completeNeeds("pull_request");
  for (const [jobName, outputName] of Object.entries(targetOutputs)) {
    docsOnly["select-changes"].outputs[outputName] = "false";
    docsOnly["select-changes"].outputs[`${outputName}_reason`] = "no_relevant_paths";
    docsOnly[jobName] = { result: "skipped" };
  }
  const docsOnlyResult = executeSummary("pull_request", docsOnly);
  assert.equal(docsOnlyResult.status, 0, docsOnlyResult.stderr);
  assert.match(docsOnlyResult.stdout, /maestro-ci passed for/);
  for (const jobName of targetJobNames) assert.match(docsOnlyResult.stdout, new RegExp(jobName));

  const unknownPath = completeNeeds("pull_request");
  for (const outputName of Object.values(targetOutputs)) {
    unknownPath["select-changes"].outputs[outputName] = "true";
    unknownPath["select-changes"].outputs[`${outputName}_reason`] = "fallback_unknown_path";
  }
  const unknownResult = executeSummary("pull_request", unknownPath);
  assert.equal(unknownResult.status, 0, unknownResult.stderr);
  assert.match(unknownResult.stdout, /maestro-ci passed for/);

  const nonPrResult = executeSummary("push", completeNeeds("push"));
  assert.equal(nonPrResult.status, 0, nonPrResult.stderr);
  assert.match(nonPrResult.stdout, /maestro-ci passed for/);
});

test("terminal summary fails closed for missing selector evidence and unexplained skips", () => {
  const missingSelector = completeNeeds("pull_request");
  delete missingSelector["select-changes"];
  const missingSelectorResult = executeSummary("pull_request", missingSelector);
  assert.equal(missingSelectorResult.status, 1);
  assert.match(missingSelectorResult.stdout, /select-changes=<missing>/);

  const failedSelector = completeNeeds("pull_request");
  failedSelector["select-changes"].result = "failure";
  const failedSelectorResult = executeSummary("pull_request", failedSelector);
  assert.equal(failedSelectorResult.status, 1);
  assert.match(failedSelectorResult.stdout, /select-changes=failure/);

  const unexplainedSkip = completeNeeds("pull_request");
  unexplainedSkip["rust-tests"] = { result: "skipped" };
  const unexplainedResult = executeSummary("pull_request", unexplainedSkip);
  assert.equal(unexplainedResult.status, 1);
  assert.match(unexplainedResult.stdout, /rust-tests=skipped/);

  const unexpectedOutput = completeNeeds("pull_request");
  unexpectedOutput["select-changes"].outputs.rust_tests = "false";
  unexpectedOutput["select-changes"].outputs.rust_tests_reason = "unexpected";
  const invalidResult = executeSummary("pull_request", unexpectedOutput);
  assert.equal(invalidResult.status, 1);
  assert.match(invalidResult.stdout, /select-changes=success\/invalid-outputs/);
});

test("required summary retains the fork trust gate and public CI wires contracts mandatorily", () => {
  const terminal = workflowDocument.jobs["maestro-ci"];
  assert.match(terminal.name, /maestro-ci-not-validated/);
  assert.equal(terminal.steps.some((step) => step.uses?.startsWith("actions/checkout@")), false);
  assert.equal(
    evaluateWorkflowCondition(terminal.if, workflowConditionContext({ cancelled: true })),
    true,
    "the required summary must remain under always() after cancellation",
  );
  assert.equal(
    evaluateWorkflowCondition(terminal.if, workflowConditionContext({ headRepo: "contributor/fork" })),
    false,
    "fork pull requests must retain the existing summary trust restriction",
  );

  const ciContractStep = workflowDocument.jobs["ci-contracts"].steps.find(
    (step) => step.name === "CI configuration contracts",
  );
  assert.equal(workflowDocument.jobs["ci-contracts"].if, workflowDocument.jobs["protocol-contracts"].if);
  assert.match(ciContractStep.run, /node --test scripts\/check-ci-concurrency\.test\.mjs/);
  const ciContractLines = ciContractStep.run.split(/\r?\n/u);
  assert.ok(
    ciContractLines.includes("node --test .github/workflows/check-maestro-ci-path-selection-contract.test.mjs"),
    "public source-workflow contract must run unconditionally in ci-contracts",
  );
  assert.ok(
    ciContractLines.includes(
      "node --test .github/workflows/fork-status-reporter.test.mjs .github/workflows/source-provenance-link.test.mjs",
    ),
    "reporter and provenance contracts must run unconditionally in ci-contracts",
  );
  assert.match(ciContractStep.run, /node scripts\/check-rust-only-runtime\.mjs/);
  assert.match(ciContractStep.run, /node scripts\/check-hook-dispatch-coverage\.mjs/);
  assert.match(
    tooling,
    /if \[\[ -f \.github\/workflows\/check-maestro-ci-path-selection-contract\.test\.mjs \]\]; then\s+node --test \.github\/workflows\/check-maestro-ci-path-selection-contract\.test\.mjs/,
  );
});

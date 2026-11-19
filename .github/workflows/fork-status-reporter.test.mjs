import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { chmodSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";

import { parse as parseYaml } from "yaml";

const workflow = parseYaml(readFileSync(new URL("./public-source-provenance.yml", import.meta.url), "utf8"));
const maestroWorkflow = parseYaml(readFileSync(new URL("./maestro-ci.yml", import.meta.url), "utf8"));
const reporter = workflow.jobs["report-fork-status"];
const reporterStep = reporter.steps[0];
const BASE_REPOSITORY = "dx-corp/code";
const PR_NUMBER = 17;
const HEAD_SHA = "a".repeat(40);
const MERGE_SHA = "b".repeat(40);
const DESCRIPTION = "not validated: fork code is not run on trusted runners";

function reporterPython() {
  const marker = "python3 - <<'PY'\n";
  const start = reporterStep.run.indexOf(marker);
  assert.notEqual(start, -1, "reporter must use the checked-in inline Python implementation");
  const codeStart = start + marker.length;
  const end = reporterStep.run.indexOf("\nPY", codeStart);
  assert.notEqual(end, -1, "reporter must close its inline Python block");
  return reporterStep.run.slice(codeStart, end);
}

function makeEvent({
  repository = BASE_REPOSITORY,
  baseRepository = BASE_REPOSITORY,
  number = PR_NUMBER,
  pullRequestNumber = number,
  headSha = HEAD_SHA,
  headRepository = "contributor/code-fork",
} = {}) {
  return {
    number,
    repository: { full_name: repository },
    pull_request: {
      number: pullRequestNumber,
      base: { repo: { full_name: baseRepository } },
      head: {
        sha: headSha,
        repo: headRepository === null ? null : { full_name: headRepository },
      },
    },
  };
}

function makePullRequest({
  number = PR_NUMBER,
  state = "open",
  baseRepository = BASE_REPOSITORY,
  headSha = HEAD_SHA,
  headRepository = "contributor/code-fork",
  mergeable = false,
  mergeSha = MERGE_SHA,
} = {}) {
  return {
    number,
    state,
    base: { repo: { full_name: baseRepository } },
    head: {
      sha: headSha,
      repo: headRepository === null ? null : { full_name: headRepository },
    },
    mergeable,
    merge_commit_sha: mergeSha,
  };
}

function jsonResponse(json) {
  return { json };
}

function statusResponse() {
  return jsonResponse({ state: "failure", context: "maestro-ci", description: DESCRIPTION });
}

function runReporter({ event = makeEvent(), responses = [] } = {}) {
  const directory = mkdtempSync(join(tmpdir(), "maestro-fork-status-"));
  const eventPath = join(directory, "event.json");
  const ghPath = join(directory, "gh");
  const callsPath = join(directory, "calls.json");
  const indexPath = join(directory, "index.txt");
  writeFileSync(eventPath, JSON.stringify(event));
  writeFileSync(callsPath, "[]");
  writeFileSync(indexPath, "0");
  writeFileSync(
    ghPath,
    `#!/usr/bin/env node
const fs = require("node:fs");
const args = process.argv.slice(2);
const callsPath = process.env.GH_MOCK_CALLS;
const calls = JSON.parse(fs.readFileSync(callsPath, "utf8"));
calls.push(args);
fs.writeFileSync(callsPath, JSON.stringify(calls));
const indexPath = process.env.GH_MOCK_INDEX;
const index = Number(fs.readFileSync(indexPath, "utf8"));
fs.writeFileSync(indexPath, String(index + 1));
const responses = JSON.parse(process.env.GH_MOCK_RESPONSES);
const response = responses[index];
if (!response) {
  process.stderr.write("unexpected mocked API request");
  process.exit(23);
}
if (response.exitCode) {
  process.stderr.write("mocked API failure");
  process.exit(response.exitCode);
}
if (Object.hasOwn(response, "raw")) process.stdout.write(response.raw);
else process.stdout.write(JSON.stringify(response.json));
`,
    { mode: 0o755 },
  );
  chmodSync(ghPath, 0o755);

  try {
    const result = spawnSync("python3", ["-c", reporterPython()], {
      encoding: "utf8",
      timeout: 20_000,
      maxBuffer: 1024 * 1024,
      env: {
        ...process.env,
        GITHUB_EVENT_PATH: eventPath,
        GITHUB_REPOSITORY: BASE_REPOSITORY,
        GH_TOKEN: "test-token",
        GH_MOCK_CALLS: callsPath,
        GH_MOCK_INDEX: indexPath,
        GH_MOCK_RESPONSES: JSON.stringify(responses),
        PATH: `${directory}:${process.env.PATH}`,
      },
    });
    return { ...result, calls: JSON.parse(readFileSync(callsPath, "utf8")) };
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
}

function callDetails(args) {
  const methodIndex = args.indexOf("--method");
  const fields = [];
  for (let index = 0; index < args.length; index += 1) {
    if (args[index] === "-f") fields.push(args[index + 1]);
  }
  return {
    method: methodIndex === -1 ? "GET" : args[methodIndex + 1],
    endpoint: args.find((argument) => argument.startsWith("repos/")),
    fields,
  };
}

test("fork reporter is a trusted-event, no-checkout, minimally permissioned job", () => {
  const trigger = workflow.on["pull_request_target"];
  assert.deepEqual(trigger.types, ["opened", "edited", "synchronize", "reopened", "ready_for_review"]);
  assert.equal(workflow.on.pull_request, undefined);
  assert.equal(reporter.name, "Report unvalidated fork PR");
  assert.equal(reporter["runs-on"], "ubuntu-latest");
  assert.equal(reporter["timeout-minutes"], 5);
  assert.deepEqual(reporter.permissions, { "pull-requests": "read", statuses: "write" });
  assert.equal(reporter.steps.length, 1);
  assert.equal(reporterStep.uses, undefined);
  assert.doesNotMatch(reporterStep.run, /actions\/checkout|actions\/upload-artifact|\.\/\.github\/actions/);
  assert.doesNotMatch(reporterStep.run, /\$\{\{\s*github\.event\.pull_request/);
  assert.doesNotMatch(reporterStep.run, /state=success/);
  assert.match(reporterStep.run, /statuses\/\{sha\}/);
});

test("fork summary has a distinct display name while trusted summary keeps maestro-ci", () => {
  const summaryName = maestroWorkflow.jobs["maestro-ci"].name;
  assert.equal(
    summaryName,
    "${{ github.event_name != 'pull_request' && 'maestro-ci' || (github.event.pull_request.head.repo.full_name == github.repository && 'maestro-ci' || 'maestro-ci-not-validated') }}",
  );
  const expectedName = (eventName, headRepository) =>
    eventName !== "pull_request" || headRepository === BASE_REPOSITORY
      ? "maestro-ci"
      : "maestro-ci-not-validated";
  assert.equal(expectedName("push", undefined), "maestro-ci");
  assert.equal(expectedName("pull_request", BASE_REPOSITORY), "maestro-ci");
  assert.equal(expectedName("pull_request", "contributor/code-fork"), "maestro-ci-not-validated");
  assert.equal(expectedName("pull_request", null), "maestro-ci-not-validated");
  assert.equal(expectedName("pull_request", undefined), "maestro-ci-not-validated");
});

test("same-repository pull requests receive no status", () => {
  const result = runReporter({
    event: makeEvent({ headRepository: BASE_REPOSITORY }),
    responses: [jsonResponse(makePullRequest({ headRepository: BASE_REPOSITORY }))],
  });
  assert.equal(result.status, 0, result.stderr);
  assert.equal(result.calls.length, 1);
  assert.equal(callDetails(result.calls[0]).method, "GET");
});

test("malformed and cross-repository event/API metadata never writes a status", () => {
  const cases = [
    { event: makeEvent({ repository: "other/repository" }), responses: [], calls: 0 },
    { event: makeEvent({ number: 0 }), responses: [], calls: 0 },
    { event: makeEvent({ headSha: "not-a-sha" }), responses: [], calls: 0 },
    {
      event: makeEvent(),
      responses: [jsonResponse(makePullRequest({ baseRepository: "other/repository" }))],
      calls: 1,
    },
    {
      event: makeEvent(),
      responses: [jsonResponse(makePullRequest({ number: PR_NUMBER + 1 }))],
      calls: 1,
    },
    {
      event: makeEvent(),
      responses: [jsonResponse(makePullRequest({ headSha: "not-a-sha" }))],
      calls: 1,
    },
  ];
  for (const scenario of cases) {
    const result = runReporter(scenario);
    assert.equal(result.status, 1, result.stderr);
    assert.equal(result.calls.length, scenario.calls);
    assert.equal(result.calls.some((args) => callDetails(args).method === "POST"), false);
  }
});

test("old event heads are ignored without writing to an obsolete SHA", () => {
  const currentSha = "c".repeat(40);
  const result = runReporter({
    event: makeEvent(),
    responses: [jsonResponse(makePullRequest({ headSha: currentSha }))],
  });
  assert.equal(result.status, 0, result.stderr);
  assert.equal(result.calls.length, 1);
  assert.equal(result.calls.some((args) => callDetails(args).method === "POST"), false);

  const changedHeadRepository = runReporter({
    event: makeEvent(),
    responses: [jsonResponse(makePullRequest({ headRepository: "new-owner/different-fork" }))],
  });
  assert.equal(changedHeadRepository.status, 0, changedHeadRepository.stderr);
  assert.equal(changedHeadRepository.calls.some((args) => callDetails(args).method === "POST"), false);
});

test("current mergeable fork PR receives failure statuses on head first, then merge SHA", () => {
  const result = runReporter({
    responses: [
      jsonResponse(makePullRequest({ mergeable: true })),
      statusResponse(),
      jsonResponse(makePullRequest({ mergeable: true })),
      statusResponse(),
    ],
  });
  assert.equal(result.status, 0, result.stderr);
  const statuses = result.calls.map(callDetails).filter((call) => call.method === "POST");
  assert.deepEqual(statuses, [
    {
      method: "POST",
      endpoint: `repos/${BASE_REPOSITORY}/statuses/${HEAD_SHA}`,
      fields: ["state=failure", "context=maestro-ci", `description=${DESCRIPTION}`],
    },
    {
      method: "POST",
      endpoint: `repos/${BASE_REPOSITORY}/statuses/${MERGE_SHA}`,
      fields: ["state=failure", "context=maestro-ci", `description=${DESCRIPTION}`],
    },
  ]);
});

test("unknown mergeability retries against current metadata before reporting the merge SHA", () => {
  const result = runReporter({
    responses: [
      jsonResponse(makePullRequest({ mergeable: null, mergeSha: null })),
      statusResponse(),
      jsonResponse(makePullRequest({ mergeable: true })),
      statusResponse(),
    ],
  });
  assert.equal(result.status, 0, result.stderr);
  const statuses = result.calls.map(callDetails).filter((call) => call.method === "POST");
  assert.deepEqual(statuses.map((status) => status.endpoint), [
    `repos/${BASE_REPOSITORY}/statuses/${HEAD_SHA}`,
    `repos/${BASE_REPOSITORY}/statuses/${MERGE_SHA}`,
  ]);
});

test("unknown mergeability fails after bounded reads while false keeps the head failure", () => {
  const unknown = makePullRequest({ mergeable: null, mergeSha: null });
  const unresolved = runReporter({
    responses: [
      jsonResponse(unknown),
      statusResponse(),
      jsonResponse(unknown),
      jsonResponse(unknown),
      jsonResponse(unknown),
    ],
  });
  assert.equal(unresolved.status, 1);
  assert.match(unresolved.stderr, /Mergeability remained unknown after bounded reads/);
  assert.equal(unresolved.calls.filter((args) => callDetails(args).method === "GET").length, 4);
  assert.deepEqual(
    unresolved.calls.map(callDetails).filter((call) => call.method === "POST").map((call) => call.endpoint),
    [`repos/${BASE_REPOSITORY}/statuses/${HEAD_SHA}`],
  );

  const nonmergeable = runReporter({
    responses: [jsonResponse(makePullRequest({ mergeable: false })), statusResponse()],
  });
  assert.equal(nonmergeable.status, 0, nonmergeable.stderr);
  assert.deepEqual(
    nonmergeable.calls.map(callDetails).filter((call) => call.method === "POST").map((call) => call.endpoint),
    [`repos/${BASE_REPOSITORY}/statuses/${HEAD_SHA}`],
  );
});

test("a deleted fork head repository remains untrusted", () => {
  const result = runReporter({
    event: makeEvent({ headRepository: null }),
    responses: [
      jsonResponse(makePullRequest({ headRepository: null, mergeable: false })),
      statusResponse(),
    ],
  });
  assert.equal(result.status, 0, result.stderr);
  assert.deepEqual(
    result.calls.map(callDetails).filter((call) => call.method === "POST").map((call) => call.endpoint),
    [`repos/${BASE_REPOSITORY}/statuses/${HEAD_SHA}`],
  );
});

test("API errors and malformed API responses fail the reporter", () => {
  const failedRead = runReporter({ responses: [{ exitCode: 1 }] });
  assert.equal(failedRead.status, 1);
  assert.equal(failedRead.calls.length, 1);

  const failedWrite = runReporter({
    responses: [jsonResponse(makePullRequest()), { exitCode: 1 }],
  });
  assert.equal(failedWrite.status, 1);
  assert.equal(failedWrite.calls.filter((args) => callDetails(args).method === "POST").length, 1);

  const malformedWrite = runReporter({
    responses: [jsonResponse(makePullRequest()), jsonResponse({ state: "success", context: "maestro-ci" })],
  });
  assert.equal(malformedWrite.status, 1);

  const malformedJson = runReporter({ responses: [{ raw: "{" }] });
  assert.equal(malformedJson.status, 1);

  const malformedMergeSha = runReporter({
    responses: [
      jsonResponse(makePullRequest({ mergeable: true, mergeSha: "invalid" })),
      statusResponse(),
      jsonResponse(makePullRequest({ mergeable: true, mergeSha: "invalid" })),
    ],
  });
  assert.equal(malformedMergeSha.status, 1);
  assert.deepEqual(
    malformedMergeSha.calls.map(callDetails).filter((call) => call.method === "POST").map((call) => call.endpoint),
    [`repos/${BASE_REPOSITORY}/statuses/${HEAD_SHA}`],
  );

  const failedRefresh = runReporter({
    responses: [jsonResponse(makePullRequest({ mergeable: true })), statusResponse(), { exitCode: 1 }],
  });
  assert.equal(failedRefresh.status, 1);
  assert.deepEqual(
    failedRefresh.calls.map(callDetails).filter((call) => call.method === "POST").map((call) => call.endpoint),
    [`repos/${BASE_REPOSITORY}/statuses/${HEAD_SHA}`],
  );
});

test("a head change after the first write prevents a stale merge status", () => {
  const newerSha = "d".repeat(40);
  const result = runReporter({
    responses: [
      jsonResponse(makePullRequest({ mergeable: true })),
      statusResponse(),
      jsonResponse(makePullRequest({ headSha: newerSha, mergeable: true })),
    ],
  });
  assert.equal(result.status, 0, result.stderr);
  assert.deepEqual(
    result.calls.map(callDetails).filter((call) => call.method === "POST").map((call) => call.endpoint),
    [`repos/${BASE_REPOSITORY}/statuses/${HEAD_SHA}`],
  );
});

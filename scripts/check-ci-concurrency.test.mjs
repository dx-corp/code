import assert from "node:assert/strict";
import { mkdtempSync, mkdirSync, readFileSync, writeFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import { spawnSync } from "node:child_process";
import test from "node:test";
import { parse as parseYaml } from "yaml";
import {
	classifyChangedPaths,
	GIT_MAX_BUFFER,
	GIT_TIMEOUT_MS,
	getChangedPaths,
	parseGitDiffPaths,
	parseWorkspacePackages,
	selectForEvent,
} from "./select-maestro-ci-changes.mjs";

const MONO_SETUP_JAVA_SHA = "77dd5b2ce225ea11469928ac317850245a74ae78";
const PUBLIC_SETUP_JAVA_SHA = "de7274f081f381c8f8158605e0321c36c376e2e6";

function expectedSetupJavaSha(testUrl) {
	const pathSegments = fileURLToPath(testUrl).split(/[\\/]/);
	return pathSegments.slice(-4, -1).join("/") === "products/maestro/scripts"
		? MONO_SETUP_JAVA_SHA
		: undefined;
}

function assertSetupJavaConfiguration(workflowText, expectedSha) {
	const action = workflowText.match(
		/^([ \t]*)-[ \t]+uses:[ \t]+actions\/setup-java@([^\s#]+)[^\n]*\n/m,
	);
	assert.ok(action, "setup-java must be configured in a workflow step");
	const actionIndent = action[1].length;
	const stepLines = [];
	for (const line of workflowText
		.slice(action.index + action[0].length)
		.split(/\r?\n/)) {
		const lineIndent = line.match(/^[ \t]*/)[0].length;
		if (
			line.trim() &&
			lineIndent <= actionIndent &&
			line.trimStart().startsWith("- ")
		) {
			break;
		}
		stepLines.push(line);
	}
	const stepInputs = stepLines.join("\n");
	assert.match(action[2], /^[0-9a-f]{40}$/, "setup-java must use a full commit SHA");
	if (expectedSha) {
		assert.equal(action[2], expectedSha, "setup-java must use the reviewed Mono source pin");
	}
	assert.match(stepInputs, /^[ \t]+distribution:[ \t]+temurin[ \t]*$/m);
	assert.match(stepInputs, /^[ \t]+java-version:[ \t]+[\"']?21[\"']?[ \t]*$/m);
}

function setupJavaWorkflow(pin, javaVersion = "21", distribution = "temurin") {
	return `jobs:
  ci-contracts:
    steps:
      - uses: actions/setup-java@${pin} # setup-java
        with:
          distribution: ${distribution}
          java-version: "${javaVersion}"
`;
}

const workflow = readFileSync(
	process.env.MAESTRO_CI_WORKFLOW_PATH ??
		new URL("../.github/workflows/maestro-ci.yml", import.meta.url),
	"utf8",
);
const nextest = readFileSync(
	new URL("../.config/nextest.toml", import.meta.url),
	"utf8",
);
const tooling = readFileSync(
	new URL("./run-ci-tooling.sh", import.meta.url),
	"utf8",
);
const jetbrains = readFileSync(
	new URL("./run-ci-jetbrains.sh", import.meta.url),
	"utf8",
);
const coverage = readFileSync(
	new URL("./run-ci-coverage.sh", import.meta.url),
	"utf8",
);
const a2aTmuxSmoke = readFileSync(
	new URL("./smoke-maestro-a2a-tmux.sh", import.meta.url),
	"utf8",
);
const interactiveTuiSmoke = readFileSync(
	new URL("./smoke-tui-interactive.sh", import.meta.url),
	"utf8",
);
const setupRust = readFileSync(
	new URL("../.github/actions/setup-rust/action.yml", import.meta.url),
	"utf8",
);
const workflowDocument = parseYaml(workflow);
const selectorScript = fileURLToPath(new URL("./select-maestro-ci-changes.mjs", import.meta.url));
const productCargoToml = readFileSync(new URL("../Cargo.toml", import.meta.url), "utf8");
function workflowConditionContext({
	eventName = "pull_request",
	headRepo = "dx-corp/code",
	cancelled = false,
} = {}) {
	return {
		cancelled,
		github: {
			event_name: eventName,
			repository: "dx-corp/code",
			event:
				eventName === "pull_request"
					? { pull_request: { head: { repo: { full_name: headRepo } } } }
					: {},
		},
		needs: { "protocol-contracts": { result: "success" } },
	};
}

function evaluateWorkflowCondition(source, context) {
	const tokenPattern = /\s*(\&\&|\|\||==|!=|!|\(|\)|'[^']*'|[A-Za-z_][A-Za-z0-9_.-]*)/y;
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
	const valueAt = (path) =>
		path.split(".").reduce((value, part) => (value == null ? undefined : value[part]), context);
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

function runGit(root, args) {
	const result = spawnSync("git", args, {
		cwd: root,
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

function commitGitTree(root, message) {
	runGit(root, ["add", "-A"]);
	runGit(root, ["commit", "--quiet", "-m", message]);
	return runGit(root, ["rev-parse", "HEAD"]);
}

function runSelector(repo, eventName, eventPath, outputPath) {
	const env = { ...process.env, GITHUB_EVENT_NAME: eventName, GITHUB_OUTPUT: outputPath };
	if (eventPath) env.GITHUB_EVENT_PATH = eventPath;
	else delete env.GITHUB_EVENT_PATH;
	return spawnSync(process.execPath, [selectorScript], {
		cwd: repo,
		encoding: "utf8",
		timeout: 10_000,
		maxBuffer: 1024 * 1024,
		env,
	});
}

function readGitHubOutputs(path) {
	return Object.fromEntries(
		readFileSync(path, "utf8")
			.trim()
			.split("\n")
			.map((line) => {
				const separator = line.indexOf("=");
				return [line.slice(0, separator), line.slice(separator + 1)];
			}),
	);
}

test("coverage sends the CI profile to Nextest rather than test binaries", () => {
	const root = mkdtempSync(join(tmpdir(), "maestro-coverage-routing-"));
	try {
		const bin = join(root, "bin");
		mkdirSync(bin);
		const calls = join(root, "calls.jsonl");
		writeFileSync(join(bin, "cargo"), `#!/usr/bin/env node
const fs = require('node:fs');
const args = process.argv.slice(2);
if (args.join(' ') === 'llvm-cov --version') { console.log('cargo-llvm-cov 0.9.0'); process.exit(0); }
if (args.join(' ') === 'nextest --version') { console.log('cargo-nextest 0.9.143'); process.exit(0); }
fs.appendFileSync(process.env.COVERAGE_CALLS, JSON.stringify({args, target:process.env.CARGO_TARGET_DIR}) + '\\n');
if (args[1] === 'nextest' && args.includes('--')) { console.error('Nextest received unsupported test-binary arguments'); process.exit(96); }
if (args[1] === 'nextest' && args.includes('--ignore-run-fail') && args.includes('--no-fail-fast')) { console.error('--ignore-run-fail may not be used together with --no-fail-fast'); process.exit(97); }
`, { mode: 0o755 });
		for (const tool of ["cargo-llvm-cov", "cargo-nextest"]) {
			writeFileSync(join(bin, tool), "#!/bin/sh\nexit 0\n", { mode: 0o755 });
		}
		writeFileSync(join(bin, "timeout"), "#!/bin/sh\nshift\nshift\nshift\nexec \"$@\"\n", { mode: 0o755 });
		const result = spawnSync("bash", [fileURLToPath(new URL("./run-ci-coverage.sh", import.meta.url))], {
			cwd: root,
			encoding: "utf8",
			env: { ...process.env, PATH: `${bin}:${process.env.PATH}`, MAESTRO_CI_CACHE_ROOT: join(root, "cache"), COVERAGE_CALLS: calls },
		});
		assert.equal(result.status, 0, result.stderr);
		const recorded = readFileSync(calls, "utf8").trim().split("\n").map(JSON.parse);
		const run = recorded[0];
		assert.deepEqual(run.args.slice(0, 2), ["llvm-cov", "nextest"]);
		assert.equal(run.args[run.args.indexOf("--profile") + 1], "ci");
		for (const flag of ["--workspace", "--lib", "--locked", "--no-clean", "--ignore-run-fail"]) {
			assert.ok(run.args.includes(flag), `missing coverage option ${flag}`);
		}
		assert.ok(!run.args.includes("--no-fail-fast"), "--ignore-run-fail already enables --no-fail-fast");
		assert.equal(run.target, join(root, "cache", "cargo-target-cov"));
		assert.deepEqual(recorded.slice(1).map(call => call.args), [
			["llvm-cov", "report", "--summary-only"],
			["llvm-cov", "report", "--lcov", "--output-path", "coverage-report/lcov.info"],
		]);
	} finally {
		rmSync(root, { recursive: true, force: true });
	}
});

test("setup-java pin and Java configuration match the workflow tree", () => {
	assertSetupJavaConfiguration(workflow, expectedSetupJavaSha(import.meta.url));
});

test("setup-java validation rejects an unexpected pin or Java configuration", () => {
	assert.throws(() =>
		assertSetupJavaConfiguration(
			setupJavaWorkflow("a".repeat(40)),
			MONO_SETUP_JAVA_SHA,
		),
	);
	assert.throws(() =>
		assertSetupJavaConfiguration(setupJavaWorkflow("v6.2.0"), MONO_SETUP_JAVA_SHA),
	);
	assert.throws(() =>
		assertSetupJavaConfiguration(
			setupJavaWorkflow(MONO_SETUP_JAVA_SHA, "22"),
			MONO_SETUP_JAVA_SHA,
		),
	);
});

test("projected setup-java check accepts the retained public workflow pin", () => {
	const root = mkdtempSync(join(tmpdir(), "maestro-public-workflow-"));
	try {
		const projectedTestUrl = pathToFileURL(
			join(root, "scripts", "check-ci-concurrency.test.mjs"),
		);
		const workflowUrl = new URL("../.github/workflows/maestro-ci.yml", projectedTestUrl);
		mkdirSync(join(root, "scripts"), { recursive: true });
		mkdirSync(join(root, ".github", "workflows"), { recursive: true });
		writeFileSync(
			fileURLToPath(workflowUrl),
			setupJavaWorkflow(PUBLIC_SETUP_JAVA_SHA),
		);
		assert.equal(expectedSetupJavaSha(projectedTestUrl), undefined);
		assertSetupJavaConfiguration(
			readFileSync(workflowUrl, "utf8"),
			expectedSetupJavaSha(projectedTestUrl),
		);
	} finally {
		rmSync(root, { recursive: true, force: true });
	}
});

test("maestro-ci concurrency is per ref and cancels in-progress PR runs", () => {
	assert.match(
		workflow,
		/group: maestro-ci-\$\{\{ github\.event\.pull_request\.head\.repo\.full_name \|\| github\.repository \}\}-\$\{\{ github\.head_ref \|\| github\.ref \}\}/,
	);
	assert.match(
		workflow,
		/cancel-in-progress: \$\{\{ github\.event_name == 'pull_request' \}\}/,
	);
});

test("maestro-ci uses owned runner labels through vars indirection", () => {
	assert.match(
		workflow,
		/vars\.MAESTRO_CI_HEAVY_RUNNER \|\| 'evalops-internal-arc'/,
	);
	assert.match(
		workflow,
		/vars\.MAESTRO_CI_LIGHT_RUNNER \|\| 'evalops-internal-arc-light'/,
	);
	assert.doesNotMatch(workflow, /runs-on:\s*ubuntu-/);
	assert.doesNotMatch(workflow, /runs-on:\s*macos-/);
	assert.doesNotMatch(workflow, /runs-on:\s*windows-/);
	assert.doesNotMatch(workflow, /hetzner-linux-heavy/);
});

test("heavy Rust and Docker jobs use the heavy owned runner", () => {
	for (const job of [
		"lint",
		"rust-tests",
		"native-release",
		"integration",
		"scenario-replay",
		"linux-check",
		"coverage",
		"perf-baseline",
	]) {
		const block = workflow.split(`  ${job}:`)[1]?.split(/\n  [a-z]/)[0] ?? "";
		assert.match(
			block,
			/vars\.MAESTRO_CI_HEAVY_RUNNER \|\| 'evalops-internal-arc'/,
			`${job} must run on the heavy owned runner`,
		);
	}
});

test("light checks use the light owned runner", () => {
	for (const job of [
		"protocol-contracts",
		"ci-contracts",
		"workflow-tooling",
		"supply-chain",
		"jetbrains-plugin",
		"hosted-orb-delegation-live",
		"maestro-ci",
	]) {
		const block = workflow.split(`  ${job}:`)[1]?.split(/\n  [a-z]/)[0] ?? "";
		assert.match(
			block,
			/vars\.MAESTRO_CI_LIGHT_RUNNER \|\| 'evalops-internal-arc-light'/,
			`${job} must run on the light owned runner`,
		);
	}
});

test("advisory coverage and perf are scheduled, not required on push or PR", () => {
	assert.match(
		workflow,
		/coverage:[\s\S]*?if: \(github\.event_name != 'pull_request' \|\| github\.event\.pull_request\.head\.repo\.full_name == github\.repository\) && \(github\.event_name == 'schedule' \|\| github\.event_name == 'workflow_dispatch'\)/,
	);
	assert.match(
		workflow,
		/perf-baseline:[\s\S]*?if: \(github\.event_name != 'pull_request' \|\| github\.event\.pull_request\.head\.repo\.full_name == github\.repository\) && \(github\.event_name == 'schedule' \|\| github\.event_name == 'workflow_dispatch'\)/,
	);
	assert.match(workflow, /coverage:[\s\S]*?continue-on-error: true/);
	assert.match(workflow, /perf-baseline:[\s\S]*?continue-on-error: true/);
});

test("fork pull requests never start jobs on internal runners", () => {
	for (const job of [
		"protocol-contracts",
		"lint",
		"rust-tests",
		"native-release",
		"integration",
		"scenario-replay",
		"ci-contracts",
		"workflow-tooling",
		"supply-chain",
		"jetbrains-plugin",
		"linux-check",
		"coverage",
		"perf-baseline",
		"hosted-orb-delegation-live",
		"maestro-ci",
	]) {
		const condition = workflowDocument.jobs[job]?.if;
		assert.equal(typeof condition, "string", `${job} must declare a trusted-head condition`);
		assert.equal(
			evaluateWorkflowCondition(
				condition,
				workflowConditionContext({ headRepo: "fork/maestro" }),
			),
			false,
			`${job} must skip fork pull requests`,
		);
	}
	assert.doesNotMatch(
		workflow,
		/^    if: github\.event_name == 'schedule' \|\| github\.event_name == 'workflow_dispatch'$/m,
		"advisory jobs must keep the trusted-head conjunct",
	);
});

test("path classifier selects only known target inputs and fails closed elsewhere", () => {
	const vectors = [
		{ path: ".vscode/settings.json", selected: [], reason: "no_relevant_paths" },
		{ path: "docs/CONTRIBUTOR_RUNBOOK.md", selected: [], reason: "no_relevant_paths" },
		{ path: "packages/jetbrains-plugin/src/main/kotlin/Main.kt", selected: ["jetbrains_plugin"] },
		{ path: "packages/runtime-rs/src/lib.rs", selected: ["rust_tests", "native_release"] },
		{ path: "proto/maestro/v1/headless.proto", selected: ["rust_tests", "native_release", "jetbrains_plugin"] },
		{ path: "Cargo.toml", selected: ["rust_tests", "native_release"] },
		{ path: ".config/nextest.toml", selected: ["rust_tests"] },
		{ path: "vendor/serde/Cargo.toml", selected: ["rust_tests", "native_release"] },
		{ path: "scripts/run-ci-jetbrains.sh", selected: ["jetbrains_plugin"] },
		{ path: "scripts/run-nextest-partition.sh", selected: ["rust_tests"] },
		{ path: "scripts/materialize-native-package.mjs", selected: ["native_release"] },
		{ path: ".github/workflows/maestro-ci.yml", selected: ["rust_tests", "native_release", "jetbrains_plugin"] },
		{ path: "scripts/select-maestro-ci-changes.mjs", selected: ["rust_tests", "native_release", "jetbrains_plugin"], reason: "fallback_unknown_path" },
		{ path: "scripts/check-ci-concurrency.test.mjs", selected: ["rust_tests", "native_release", "jetbrains_plugin"], reason: "fallback_unknown_path" },
		{ path: "docs/protocols/hosted-runner-contract.md", selected: ["rust_tests", "native_release", "jetbrains_plugin"], reason: "fallback_unknown_path" },
		{ path: "README.md", selected: ["rust_tests", "native_release", "jetbrains_plugin"], reason: "fallback_unknown_path" },
		{ path: "packages/not-a-workspace-member/src/lib.rs", selected: ["rust_tests", "native_release", "jetbrains_plugin"], reason: "fallback_unknown_path" },
	];
	for (const vector of vectors) {
		const outputs = classifyChangedPaths([vector.path], productCargoToml);
		for (const target of ["rust_tests", "native_release", "jetbrains_plugin"]) {
			const selected = vector.selected.includes(target);
			assert.equal(outputs[target], selected ? "true" : "false", vector.path);
			assert.equal(
				outputs[`${target}_reason`],
				vector.reason ?? (selected ? "matched_path" : "no_relevant_paths"),
				vector.path,
			);
		}
		assert.equal(outputs.selection_valid, "true", vector.path);
	}
	assert.ok(parseWorkspacePackages(productCargoToml).has("runtime-rs"));
	assert.deepEqual(classifyChangedPaths([], productCargoToml), {
		selection_valid: "true",
		rust_tests: "true",
		rust_tests_reason: "fallback_empty_paths",
		native_release: "true",
		native_release_reason: "fallback_empty_paths",
		jetbrains_plugin: "true",
		jetbrains_plugin_reason: "fallback_empty_paths",
	});
	assert.equal(
		classifyChangedPaths(["packages/runtime-rs/../README.md"], productCargoToml).rust_tests_reason,
		"fallback_malformed_input",
	);
	assert.throws(() => parseWorkspacePackages("[workspace]\nmembers = [\n  \"examples/not-a-package\",\n]\n"));
	assert.throws(() => parseGitDiffPaths(Buffer.alloc(0)), /fallback_empty_paths/);
	assert.throws(() => parseGitDiffPaths(Buffer.from("path-without-terminator")), /fallback_malformed_input/);
	assert.throws(() => parseGitDiffPaths(Buffer.from("path\nwith-newline\0")), /fallback_malformed_input/);
	assert.ok(Number.isFinite(GIT_TIMEOUT_MS) && GIT_TIMEOUT_MS > 0);
	assert.ok(Number.isFinite(GIT_MAX_BUFFER) && GIT_MAX_BUFFER > 0);
});

test("Git selector uses merge-base and exposes deleted and added rename paths", () => {
	const root = mkdtempSync(join(tmpdir(), "maestro-path-selector-git-"));
	try {
		runGit(root, ["init", "--quiet"]);
		runGit(root, ["checkout", "--quiet", "-b", "main"]);
		mkdirSync(join(root, "packages/runtime-rs/src"), { recursive: true });
		writeFileSync(
			join(root, "Cargo.toml"),
			'[workspace]\nresolver = "2"\nmembers = [\n  "packages/runtime-rs",\n]\n',
		);
		writeFileSync(join(root, "packages/runtime-rs/Cargo.toml"), "[package]\nname = \"runtime\"\n");
		writeFileSync(join(root, "packages/runtime-rs/src/lib.rs"), "pub fn value() -> u8 { 1 }\n");
		writeFileSync(join(root, "packages/runtime-rs/src/old.rs"), "pub fn old() {}\n");
		writeFileSync(join(root, "packages/runtime-rs/src/deleted.rs"), "pub fn deleted() {}\n");
		commitGitTree(root, "base");

		runGit(root, ["checkout", "--quiet", "-b", "feature"]);
		writeFileSync(join(root, "packages/runtime-rs/src/lib.rs"), "pub fn value() -> u8 { 2 }\n");
		runGit(root, ["mv", "packages/runtime-rs/src/old.rs", "packages/runtime-rs/src/new.rs"]);
		rmSync(join(root, "packages/runtime-rs/src/deleted.rs"));
		const headSha = commitGitTree(root, "feature changes");

		runGit(root, ["checkout", "--quiet", "main"]);
		writeFileSync(join(root, "base-only.txt"), "not part of the PR diff\n");
		const baseSha = commitGitTree(root, "advance base");
		const paths = getChangedPaths({ baseSha, headSha, cwd: root });
		assert.deepEqual(paths, [
			"packages/runtime-rs/src/deleted.rs",
			"packages/runtime-rs/src/lib.rs",
			"packages/runtime-rs/src/new.rs",
			"packages/runtime-rs/src/old.rs",
		]);
		const outputs = classifyChangedPaths(paths, readFileSync(join(root, "Cargo.toml"), "utf8"));
		assert.equal(outputs.rust_tests, "true");
		assert.equal(outputs.native_release, "true");
		assert.equal(outputs.jetbrains_plugin, "false");

		const eventPath = join(root, "event.json");
		const outputPath = join(root, "github-output.txt");
		writeFileSync(
			eventPath,
			JSON.stringify({ pull_request: { base: { sha: baseSha }, head: { sha: headSha } } }),
		);
		const result = runSelector(root, "pull_request", eventPath, outputPath);
		assert.equal(result.status, 0, result.stderr);
		const cliOutputs = readGitHubOutputs(outputPath);
		assert.equal(cliOutputs.selection_valid, "true");
		assert.equal(cliOutputs.rust_tests, "true");
		assert.equal(cliOutputs.native_release, "true");
		assert.equal(cliOutputs.jetbrains_plugin, "false");

		for (const eventName of ["push", "schedule", "workflow_dispatch", "release"]) {
			const nonPrOutput = join(root, `${eventName}-output.txt`);
			const nonPr = runSelector(root, eventName, undefined, nonPrOutput);
			assert.equal(nonPr.status, 0, nonPr.stderr);
			const nonPrOutputs = readGitHubOutputs(nonPrOutput);
			for (const target of ["rust_tests", "native_release", "jetbrains_plugin"]) {
				assert.equal(nonPrOutputs[target], "true", eventName);
				assert.equal(nonPrOutputs[`${target}_reason`], "non_pr", eventName);
			}
		}
	} finally {
		rmSync(root, { recursive: true, force: true });
	}
});

test("a real contributor-runbook-only change selects no targets", () => {
	const root = mkdtempSync(join(tmpdir(), "maestro-runbook-selector-git-"));
	try {
		runGit(root, ["init", "--quiet"]);
		runGit(root, ["checkout", "--quiet", "-b", "main"]);
		mkdirSync(join(root, "docs"), { recursive: true });
		writeFileSync(join(root, "Cargo.toml"), productCargoToml);
		const runbookPath = "docs/CONTRIBUTOR_RUNBOOK.md";
		const runbook = readFileSync(new URL("../docs/CONTRIBUTOR_RUNBOOK.md", import.meta.url), "utf8");
		writeFileSync(join(root, runbookPath), runbook);
		const baseSha = commitGitTree(root, "base with contributor runbook");

		runGit(root, ["checkout", "--quiet", "-b", "docs-only"]);
		writeFileSync(join(root, runbookPath), `${runbook}\nSelector-only fixture change.\n`);
		const headSha = commitGitTree(root, "change contributor runbook only");
		const outputs = selectForEvent(
			"pull_request",
			{ pull_request: { base: { sha: baseSha }, head: { sha: headSha } } },
			{ cwd: root },
		);
		for (const target of ["rust_tests", "native_release", "jetbrains_plugin"]) {
			assert.equal(outputs[target], "false");
			assert.equal(outputs[`${target}_reason`], "no_relevant_paths");
		}
	} finally {
		rmSync(root, { recursive: true, force: true });
	}
});

test("uncertain Git or event inputs select every target", () => {
	const root = mkdtempSync(join(tmpdir(), "maestro-path-selector-fallback-"));
	try {
		runGit(root, ["init", "--quiet"]);
		runGit(root, ["checkout", "--quiet", "-b", "main"]);
		writeFileSync(join(root, "Cargo.toml"), '[workspace]\nmembers = [\n  "packages/runtime-rs",\n]\n');
		const baseSha = commitGitTree(root, "base");
		const validPullRequest = { pull_request: { base: { sha: baseSha }, head: { sha: baseSha } } };
		for (const [eventName, event, options, reason] of [
			["pull_request", validPullRequest, { cwd: root }, "fallback_empty_paths"],
			["pull_request", { pull_request: { base: { sha: "bad" }, head: { sha: baseSha } } }, { cwd: root }, "fallback_invalid_sha"],
			["pull_request", { pull_request: { base: { sha: baseSha }, head: { sha: "f".repeat(40) } } }, { cwd: root }, "fallback_missing_object"],
			["pull_request", validPullRequest, { cwd: root, gitBinary: join(root, "missing-git") }, "fallback_git_error"],
			["pull_request", {}, { cwd: root }, "fallback_event_input"],
		]) {
			const outputs = selectForEvent(eventName, event, options);
			for (const target of ["rust_tests", "native_release", "jetbrains_plugin"]) {
				assert.equal(outputs[target], "true", reason);
				assert.equal(outputs[`${target}_reason`], reason);
			}
		}

		const ambiguousGit = join(root, "ambiguous-git");
		writeFileSync(
			ambiguousGit,
			`#!/bin/sh
if [ "$1" = "rev-parse" ]; then printf '%s\\n' "${baseSha}"; exit 0; fi
if [ "$1" = "merge-base" ]; then [ "$2" = "--all" ] || exit 3; printf '%s\\n%s\\n' "${baseSha}" "${"f".repeat(40)}"; exit 0; fi
exit 2
`,
			{ mode: 0o755 },
		);
		const ambiguous = selectForEvent("pull_request", validPullRequest, {
			cwd: root,
			gitBinary: ambiguousGit,
		});
		assert.equal(ambiguous.rust_tests_reason, "fallback_ambiguous_merge_base");
		assert.equal(ambiguous.native_release, "true");
		assert.equal(ambiguous.jetbrains_plugin, "true");

		for (const [name, json] of [
			["undefined", "undefined"],
			["invalid", "{invalid json"],
			["null", "null"],
		]) {
			const eventPath = join(root, `${name}-event.json`);
			const outputPath = join(root, `${name}-event-output.txt`);
			writeFileSync(eventPath, json);
			const result = runSelector(root, "pull_request", eventPath, outputPath);
			assert.equal(result.status, 0, result.stderr);
			const outputs = readGitHubOutputs(outputPath);
			for (const target of ["rust_tests", "native_release", "jetbrains_plugin"]) {
				assert.equal(outputs[target], "true", name);
				assert.equal(outputs[`${target}_reason`], "fallback_event_input", name);
			}
		}
		const nonPr = selectForEvent("workflow_dispatch", null);
		assert.equal(nonPr.rust_tests_reason, "non_pr");
		const missingEvent = selectForEvent(undefined, null);
		assert.equal(missingEvent.rust_tests_reason, "fallback_event_input");
	} finally {
		rmSync(root, { recursive: true, force: true });
	}
});

test("maestro-ci covers every migrated validation family", () => {
	for (const job of [
		"protocol-contracts",
		"lint",
		"rust-tests",
		"native-release",
		"integration",
		"scenario-replay",
		"ci-contracts",
		"workflow-tooling",
		"supply-chain",
		"jetbrains-plugin",
		"linux-check",
		"coverage",
		"perf-baseline",
		"hosted-orb-delegation-live",
		"maestro-ci",
	]) {
		assert.match(workflow, new RegExp(`^  ${job}:`, "m"), `missing job ${job}`);
	}
	assert.match(workflow, /bash scripts\/run-ci-tooling\.sh/);
	assert.match(workflow, /bash scripts\/run-ci-supply-chain\.sh/);
	assert.match(workflow, /bash scripts\/run-ci-jetbrains\.sh/);
	assert.match(workflow, /bash scripts\/run-ci-coverage\.sh/);
	assert.match(workflow, /bash scripts\/run-ci-perf\.sh/);
	assert.match(workflow, /bash scripts\/ci-linux-check\.sh/);
	assert.match(workflow, /scripts\/run-nextest-partition\.sh/);
	assert.match(workflow, /use-sccache: "true"/);
	assert.match(
		workflow,
		/vars\.MAESTRO_HOSTED_ORB_LIVE_SMOKE \|\| ''/,
	);
	const hostedOrb = workflow.split("  hosted-orb-delegation-live:")[1]?.split(/\n  [a-z]/)[0] ?? "";
	assert.match(
		hostedOrb,
		/continue-on-error:\s*\$\{\{\s*!\(github\.event\.inputs\.hosted_orb_live_smoke == '1' \|\| vars\.MAESTRO_HOSTED_ORB_LIVE_SMOKE == '1'\)\s*\}\}/,
	);
	assert.doesNotMatch(hostedOrb, /^    continue-on-error:\s*true\b/m);
});

test("supply-chain can read the pull request and issue timeline for deny.toml checks", () => {
	const workflowPermissions = workflow.split("\njobs:")[0] ?? "";
	assert.match(workflowPermissions, /^permissions:\n  contents: read$/m);
	assert.doesNotMatch(workflowPermissions, /pull-requests:/);
	assert.doesNotMatch(workflowPermissions, /issues:/);
	const block = workflow.split("  supply-chain:")[1]?.split(/\n  [a-z]/)[0] ?? "";
	assert.match(block, /^    permissions:\n      contents: read\n      pull-requests: read\n      issues: read$/m);
	assert.match(block, /GH_TOKEN: \$\{\{ github\.token \}\}/);
	assert.match(block, /bash scripts\/run-ci-supply-chain\.sh/);
});

test("portable selector path tests are included in the tooling lane", () => {
	assert.match(tooling, /node --test scripts\/check-ci-concurrency\.test\.mjs/);
});

test("nextest runs as a four-way hash partition matrix", () => {
	assert.match(workflow, /matrix:\n        partition: \[1, 2, 3, 4\]/);
	assert.match(
		workflow,
		/scripts\/run-nextest-partition\.sh "\$\{NEXTEST_PARTITION\}\/4"/,
	);
	assert.match(nextest, /\[profile\.ci\]/);
	assert.match(nextest, /filter = 'binary\(pty_e2e\)'/);
	assert.match(nextest, /test-group = 'pty-e2e'/);
	assert.match(nextest, /\[test-groups\.pty-e2e\]\nmax-threads = 1/);
});

test("CI caps rustc codegen units and compile jobs", () => {
	assert.match(workflow, /CARGO_PROFILE_DEV_CODEGEN_UNITS: "16"/);
	assert.match(workflow, /CARGO_PROFILE_TEST_CODEGEN_UNITS: "16"/);
	assert.match(workflow, /CARGO_BUILD_JOBS: "4"/);
	assert.match(workflow, /RUST_TOOLCHAIN: "1\.95\.0"/);
});

test("Rust setup uses Google-backed sccache and forbids the GitHub backend", () => {
	const configure =
		setupRust
			.split("    - name: Configure Google-backed sccache")[1]
			?.split("\n    - name: Cache Cargo")[0] ?? "";
	const cargoCache =
		setupRust
			.split("    - name: Cache Cargo")[1]
			?.split("\n    - name: Configure Cargo network resilience")[0] ?? "";
	assert.match(configure, /uses: \.\/\.github\/actions\/setup-sccache/);
	assert.match(configure, /version: v0\.17\.0/);
	assert.match(configure, /backend: auto/);
	assert.match(configure, /rust-toolchain: \$\{\{ inputs\.toolchain \}\}/);
	assert.match(configure, /allow-gha-fallback: "false"/);
	assert.doesNotMatch(configure, /SCCACHE_GHA_ENABLED/);
	assert.match(cargoCache, /cargo-inputs-v4-runner-temp/);
	assert.match(cargoCache, /cache-targets: "false"/);
});

test("interactive TUI smoke isolates state and skips first-run onboarding", () => {
	assert.match(interactiveTuiSmoke, /SMOKE_HOME="\$\(mktemp -d/);
	assert.match(interactiveTuiSmoke, /trap cleanup EXIT/);
	assert.match(interactiveTuiSmoke, /\{"onboardingSeen":true\}/);
	assert.match(interactiveTuiSmoke, /export MAESTRO_HOME=\$QUOTED_SMOKE_HOME/);
	assert.match(interactiveTuiSmoke, /export MAESTRO_TELEMETRY=0/);
	assert.match(interactiveTuiSmoke, /export MAESTRO_AUTO_UPDATE=0/);
	assert.ok(
		interactiveTuiSmoke.indexOf("export MAESTRO_HOME=$QUOTED_SMOKE_HOME") <
			interactiveTuiSmoke.indexOf("'$BIN' --provider openai"),
		"isolated preferences must be active before the TUI starts",
	);
});

test("network and long-running operations are bounded", () => {
	assert.equal((workflow.match(/5m npm ci --ignore-scripts/g) ?? []).length, 7);
	assert.match(workflow, /npm_config_fetch_retries: "1"/);
	assert.match(workflow, /npm_config_fetch_timeout: "30000"/);
	assert.equal((workflow.match(/2m docker pull/g) ?? []).length, 2);
	for (const command of [
		"45m npm run check",
		"30m npm run lint",
		"20m cargo test --workspace --locked --doc",
		"30m cargo build --locked -p maestro-tui",
		"45m npm run build",
		"30m cargo test --locked -p maestro-runtime-gateway",
		"30m cargo test --locked -p maestro-tui --test tools_integration",
		"30m cargo build --locked -p maestro-scenario",
	]) {
		assert.match(workflow, new RegExp(command.replaceAll("-", "\\-")));
	}
});

test("protocol lock fails the build before heavy jobs start", () => {
	assert.match(workflow, /^  protocol-contracts:/m);
	assert.match(workflow, /npm run check:protocol-manifest/);
	const protocolDependents = Object.entries(workflowDocument.jobs).filter(
		([name, job]) =>
			name !== "maestro-ci" &&
			(Array.isArray(job.needs)
				? job.needs.includes("protocol-contracts")
				: job.needs === "protocol-contracts"),
	);
	assert.equal(
		protocolDependents.length,
		10,
		"all validation jobs must wait for the protocol lock",
	);
});

test("rust-tests explicitly proves CI machine auth fails closed without privileged tokens", () => {
	assert.match(
		workflow,
		/cargo test --locked -p maestro-local-host ci_auth_conformance/,
	);
	assert.doesNotMatch(workflow, /ACTIONS_ID_TOKEN_REQUEST_TOKEN/);
});

test("workflow tooling installs pinned binaries without requiring Go", () => {
	assert.doesNotMatch(tooling, /go install/);
	assert.match(tooling, /actionlint_1\.7\.9_\$\{actionlint_platform\}\.tar\.gz/);
	assert.match(tooling, /233b280d05e100837f4af1433c7b40a5dcb306e3aa68fb4f17f8a7f45a7df7b4/);
	assert.match(tooling, /sha256sum --check --status/);
	assert.match(tooling, /shasum -a 256 --check --status/);
	assert.match(tooling, /! -f \.github\/workflows\/sync-public-release-mirror\.yml/);
});

test("JetBrains validation bounds Gradle workers and heap", () => {
	assert.match(jetbrains, /10m \\\n\s+\.\/gradlew check buildPlugin --no-daemon \\/);
	assert.match(jetbrains, /org\.gradle\.workers\.max=1/);
	assert.match(
		jetbrains,
		/org\.gradle\.jvmargs="-Xmx1g -XX:MaxMetaspaceSize=512m -XX:\+ExitOnOutOfMemoryError"/,
	);
});

test("advisory coverage uses nextest and an isolated instrumented target dir", () => {
	assert.match(coverage, /CARGO_TARGET_DIR="\$\{cache_root\}\/cargo-target-cov"/);
	assert.match(coverage, /cargo llvm-cov nextest/);
	assert.match(coverage, /--lib/);
	assert.match(coverage, /--no-clean/);
	assert.doesNotMatch(coverage, /llvm-cov nextest[\s\S]*--no-report/);
	assert.match(coverage, /--ignore-run-fail/);
	assert.match(coverage, /--profile ci/);
	assert.match(coverage, /cargo-llvm-cov-x86_64-unknown-linux-gnu\.tar\.gz/);
	assert.match(
		coverage,
		/b068f7c98841aacb9c4f382b4a0c184ae82f49b56a32d442b429b2961c73be15/,
	);
	assert.doesNotMatch(coverage, /cargo install cargo-llvm-cov/);
	assert.doesNotMatch(coverage, /--html/);
});

test("internal-only contracts are conditional in the shared public pipeline", () => {
	assert.match(
		workflow,
		/if \[\[ -d test\/internal \]\]; then\n\s+npm run test:internal\n\s+fi/,
	);
	assert.match(
		workflow,
		/if \[\[ -f scripts\/measure-ci-build-latency\.test\.mjs \]\]; then/,
	);
});

test("public projections skip the internal mirror workflow contract", () => {
	assert.match(
		tooling,
		/check-sync-public-release-mirror-workflow\.test\.mjs[\s\S]*?\.github\/workflows\/sync-public-release-mirror\.yml/,
	);
});

test("A2A tmux smoke atomically reserves its session before resetting durable task databases", () => {
	const cleanup = a2aTmuxSmoke.split("cleanup() {")[1]?.split("a2a_cli() {")[0] ?? "";
	const startup = a2aTmuxSmoke.split('cd "$ROOT_DIR"')[1] ?? "";
	const reservation = startup.indexOf(
		'tmux new-session -d -s "$SESSION_NAME" -n peer-a "sleep 300"',
	);
	const resetStart = startup.indexOf("rm -f");
	const respawn = startup.indexOf('tmux respawn-window -k -t "$SESSION_NAME:peer-a"');
	assert.match(cleanup, /if \[\[ "\$OWNS_SESSION" == "1" \]\]; then/);
	assert.match(startup, /if tmux new-session[^\n]+; then\n\tOWNS_SESSION=1/);
	assert.ok(reservation >= 0, "smoke must atomically reserve the tmux session");
	assert.ok(resetStart > reservation, "state reset must follow session reservation");
	assert.ok(respawn > resetStart, "peer A must start after state reset");
	assert.doesNotMatch(startup, /tmux has-session/);
	const reset = startup.slice(resetStart, respawn);
	for (const tasks of ["TASKS_A", "TASKS_B"]) {
		for (const suffix of ["", ".sqlite3", ".sqlite3-wal", ".sqlite3-shm"]) {
			assert.ok(reset.includes(`"$${tasks}${suffix}"`), `reset must remove $${tasks}${suffix}`);
		}
	}
});

test("legacy GitHub validation workflows are absent", () => {
	const names = [
		"actionlint.yml",
		"ci.yml",
		"coverage.yml",
		"evals.yml",
		"hooks.yml",
		"integration.yml",
		"jetbrains-plugin.yml",
		"perf-baselines.yml",
		"required-checks-invariant.yml",
		"scenario-replay.yml",
		"shellcheck.yml",
		"supply-chain.yml",
	];
	for (const name of names) {
		try {
			readFileSync(new URL(`../.github/workflows/${name}`, import.meta.url));
			assert.fail(`legacy workflow ${name} must not exist`);
		} catch (error) {
			assert.equal(error.code, "ENOENT");
		}
	}
});

const runner = fileURLToPath(new URL("./run-ci-jetbrains.sh", import.meta.url));
const hasScript = spawnSync("bash", ["-lc", "command -v script"], {
	encoding: "utf8",
}).status === 0;

test(
	"JetBrains CI detaches Gradle stdin from the controlling terminal",
	{ skip: process.platform !== "linux" || !hasScript },
	() => {
		const root = mkdtempSync(join(tmpdir(), "maestro-gradle-stdin-"));
		try {
			const bin = join(root, "bin");
			const plugin = join(root, "packages/jetbrains-plugin");
			mkdirSync(bin);
			mkdirSync(plugin, { recursive: true });
			writeFileSync(
				join(bin, "java"),
				'#!/bin/sh\necho \'openjdk version "21.0.12"\' >&2\n',
				{ mode: 0o755 },
			);
			writeFileSync(
				join(bin, "timeout"),
				`#!/bin/bash
args=("$@")
for i in "\${!args[@]}"; do
  [[ "\${args[$i]}" != 10m ]] || args[$i]=1s
  [[ "\${args[$i]}" != --kill-after=30s ]] || args[$i]=--kill-after=1s
done
exec /usr/bin/timeout "\${args[@]}"
`,
				{ mode: 0o755 },
			);
			writeFileSync(
				join(plugin, "gradlew"),
				'#!/bin/sh\nexec "$TEST_NODE" "$TEST_READER"\n',
				{ mode: 0o755 },
			);
			const reader = join(root, "read-stdin.cjs");
			writeFileSync(
				reader,
				`const fs = require('node:fs');
fs.readSync(0, Buffer.alloc(1), 0, 1, null);
console.log('gradle stdin reached EOF');
`,
			);
			const result = spawnSync(
				"script",
				["-q", "-e", "-c", `bash '${runner.replaceAll("'", "'\\''")}'`, "/dev/null"],
				{
					cwd: root,
					env: {
						...process.env,
						PATH: `${bin}:${process.env.PATH}`,
						TEST_NODE: process.execPath,
						TEST_READER: reader,
						MAESTRO_CI_CACHE_ROOT: root,
					},
					input: "",
					encoding: "utf8",
					timeout: 5000,
				},
			);
			assert.ifError(result.error);
			assert.equal(result.status, 0, result.stdout + result.stderr);
			assert.match(result.stdout, /gradle stdin reached EOF/);
		} finally {
			rmSync(root, { recursive: true, force: true });
		}
	},
);

test("tool bootstrap rejects an installed but unusable actionlint shim", () => {
	const directory = mkdtempSync(join(tmpdir(), "maestro-tool-shim-"));
	try {
		const bin = join(directory, "bin");
		mkdirSync(bin);
		writeFileSync(
			join(bin, "curl"),
			"#!/bin/sh\necho bootstrap-download-requested\nexit 77\n",
			{ mode: 0o755 },
		);
		const bootstrap = tooling.slice(0, tooling.indexOf("if ! shellcheck --version"));
		assert.ok(bootstrap.includes("if ! actionlint --version"));
		for (const usable of [false, true]) {
			writeFileSync(
				join(bin, "actionlint"),
				`#!/bin/sh\n[ "$1" = "--version" ] || exit 90\nexit ${usable ? 0 : 1}\n`,
				{ mode: 0o755 },
			);
			const result = spawnSync("bash", ["-c", bootstrap], {
				cwd: directory,
				env: {
					...process.env,
					MAESTRO_CI_CACHE_ROOT: directory,
					PATH: `${bin}:${process.env.PATH}`,
				},
				encoding: "utf8",
			});
			assert.equal(result.status, usable ? 0 : 77, result.stderr);
			assert.equal(result.stdout.includes("bootstrap-download-requested"), !usable);
		}
	} finally {
		rmSync(directory, { recursive: true, force: true });
	}
});

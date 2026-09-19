import assert from "node:assert/strict";
import { mkdtempSync, mkdirSync, readFileSync, writeFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { spawnSync } from "node:child_process";
import test from "node:test";

const workflow = readFileSync(
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

test("maestro-ci concurrency is per ref and cancels in-progress PR runs", () => {
	assert.match(
		workflow,
		/group: maestro-ci-\$\{\{ github\.event\.pull_request\.number \|\| github\.ref \}\}/,
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

const trustedHead =
	/github\.event_name != 'pull_request' \|\| github\.event\.pull_request\.head\.repo\.full_name == github\.repository/;

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
		const block = workflow.split(`  ${job}:`)[1]?.split(/\n  [a-z]/)[0] ?? "";
		assert.match(block, trustedHead, `${job} must skip fork pull requests`);
	}
	assert.doesNotMatch(
		workflow,
		/^    if: github\.event_name == 'schedule' \|\| github\.event_name == 'workflow_dispatch'$/m,
		"advisory jobs must keep the trusted-head conjunct",
	);
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
	assert.match(workflow, /actions\/setup-java@de7274f081f381c8f8158605e0321c36c376e2e6/);
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

test("terminal maestro-ci job fails unless needed jobs succeeded or skipped", () => {
	assert.match(
		workflow,
		/^  maestro-ci:\n    name: maestro-ci\n    if: always\(\) && \(github\.event_name != 'pull_request' \|\| github\.event\.pull_request\.head\.repo\.full_name == github\.repository\)/m,
	);
	assert.match(workflow, /result == "skipped"/);
	assert.match(workflow, /maestro-ci failed for/);
	const terminal = workflow.split("  maestro-ci:")[1] ?? "";
	assert.match(terminal, /for name, body in sorted\(needs\.items\(\)\):/);
	assert.doesNotMatch(
		terminal,
		/if name in \(|hosted-orb-delegation-live.*continue-on-error|result == "failure"/,
	);
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
	assert.match(cargoCache, /cargo-inputs-v3/);
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
	assert.equal(
		(workflow.match(/needs: protocol-contracts/g) ?? []).length,
		10,
		"heavy jobs must wait for the protocol lock",
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

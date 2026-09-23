import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { mkdtempSync, readFileSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import test from "node:test";

const repositoryRoot = fileURLToPath(new URL("..", import.meta.url));
const checker = join(repositoryRoot, "scripts/check-docker-runtime-workspaces.mjs");
const sourceDockerfile = readFileSync(join(repositoryRoot, "Dockerfile"), "utf8");
const runtimeCopy = "COPY packages/runtime-rs ./packages/runtime-rs";
const runtimeContractsCopy =
	"COPY packages/runtime-contracts-rs ./packages/runtime-contracts-rs";
const embeddedConfigCopy =
	"COPY config/vendor-corrections.json ./config/vendor-corrections.json";

const sourceCargoManifest = readFileSync(join(repositoryRoot, "Cargo.toml"), "utf8");

function runChecker(dockerfile, cargoManifest = sourceCargoManifest) {
	const fixtureRoot = mkdtempSync(join(tmpdir(), "maestro-docker-runtime-"));
	writeFileSync(join(fixtureRoot, "Dockerfile"), dockerfile);
	writeFileSync(join(fixtureRoot, "Cargo.toml"), cargoManifest);
	return execFileSync(process.execPath, [checker], {
		cwd: fixtureRoot,
		encoding: "utf8",
		stdio: ["ignore", "pipe", "pipe"],
	});
}

test("Docker runtime guard rejects a runtime copy missing from the native stage", () => {
	const firstCopy = sourceDockerfile.indexOf(runtimeCopy);
	const secondCopy = sourceDockerfile.indexOf(runtimeCopy, firstCopy + runtimeCopy.length);
	assert.notEqual(firstCopy, -1, "planner runtime copy fixture");
	assert.notEqual(secondCopy, -1, "native runtime copy fixture");

	const withoutNativeCopy =
		sourceDockerfile.slice(0, secondCopy) + sourceDockerfile.slice(secondCopy + runtimeCopy.length);
	const duplicatedPlannerCopy = withoutNativeCopy.replace(
		runtimeCopy,
		`${runtimeCopy}\n${runtimeCopy}`,
	);

	assert.throws(
		() => runChecker(duplicatedPlannerCopy),
		(error) => {
			assert.equal(error.status, 1);
			assert.match(
				`${error.stdout}\n${error.stderr}`,
				/native runtime facade crate in native Docker stage/,
			);
			return true;
		},
	);
});

test("Docker runtime guard rejects a contracts copy missing from the native stage", () => {
	const firstCopy = sourceDockerfile.indexOf(runtimeContractsCopy);
	const secondCopy = sourceDockerfile.indexOf(
		runtimeContractsCopy,
		firstCopy + runtimeContractsCopy.length,
	);
	assert.notEqual(firstCopy, -1, "planner runtime contracts copy fixture");
	assert.notEqual(secondCopy, -1, "native runtime contracts copy fixture");

	const withoutNativeCopy =
		sourceDockerfile.slice(0, secondCopy) +
		sourceDockerfile.slice(secondCopy + runtimeContractsCopy.length);
	const duplicatedPlannerCopy = withoutNativeCopy.replace(
		runtimeContractsCopy,
		`${runtimeContractsCopy}\n${runtimeContractsCopy}`,
	);

	assert.throws(
		() => runChecker(duplicatedPlannerCopy),
		(error) => {
			assert.equal(error.status, 1);
			assert.match(
				`${error.stdout}\n${error.stderr}`,
				/dependency-light runtime contracts crate in native Docker stage/,
			);
			return true;
		},
	);
});

test("Docker runtime guard accepts the checked-in Dockerfile and workspace", () => {
	const output = runChecker(sourceDockerfile);
	assert.match(output, /Verified native-only Docker runtime contract\./);
});

test("Docker runtime guard requires embedded vendor corrections before compilation", () => {
	assert.ok(sourceDockerfile.includes(embeddedConfigCopy));
	assert.throws(
		() => runChecker(sourceDockerfile.replace(`${embeddedConfigCopy}\n`, "")),
		/embedded vendor corrections before native Maestro build/,
	);
	const lateCopy = sourceDockerfile
		.replace(`${embeddedConfigCopy}\n`, "")
		.replace(
			"RUN cargo build --release --locked -p maestro",
			`RUN cargo build --release --locked -p maestro\n${embeddedConfigCopy}`,
		);
	assert.throws(
		() => runChecker(lateCopy),
		/embedded vendor corrections before native Maestro build/,
	);
});

test("Docker runtime guard rejects missing or late vendored Cargo inputs", () => {
	const paths = [...sourceCargoManifest.matchAll(/\bpath\s*=\s*"(vendor\/[^"\n]+)"/g)]
		.map((match) => match[1]);
	assert.equal(new Set(paths).size, 3);
	for (const path of new Set(paths)) {
		const copy = `COPY ${path} ./${path}`;
		const without = sourceDockerfile.replaceAll(`${copy}\n`, "");
		assert.throws(() => runChecker(without), /before cargo chef/);
		const tooLate = without
			.replace("RUN cargo chef prepare --recipe-path recipe.json", `RUN cargo chef prepare --recipe-path recipe.json\n${copy}`)
			.replace("RUN cargo chef cook --release --locked -p maestro --recipe-path recipe.json", `RUN cargo chef cook --release --locked -p maestro --recipe-path recipe.json\n${copy}`);
		assert.throws(() => runChecker(tooLate), /before cargo chef/);
	}
});

test("Docker runtime guard rejects a workspace member no stage copies", () => {
	const ledgerCopy = "COPY packages/a2a-ledger-rs ./packages/a2a-ledger-rs";
	assert.ok(
		sourceDockerfile.includes(ledgerCopy),
		"the checked-in Dockerfile copies the ledger crate",
	);

	assert.throws(
		() => runChecker(sourceDockerfile.replaceAll(`${ledgerCopy}\n`, "")),
		(error) => {
			assert.equal(error.status, 1);
			assert.match(
				`${error.stdout}\n${error.stderr}`,
				/does not copy every Cargo workspace member: packages\/a2a-ledger-rs in the planner stage, packages\/a2a-ledger-rs in the native stage/,
			);
			return true;
		},
	);
});

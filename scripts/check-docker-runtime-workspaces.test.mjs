import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
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

function runChecker(dockerfile, cargoManifest = sourceCargoManifest, { transitive = false, standalone = false, viaVendor = false, crossWorkspace = false, codemodeDependencies = "" } = {}) {
	const directory = mkdtempSync(join(tmpdir(), "maestro-docker-runtime-"));
	const fixtureRoot = join(directory, "products/maestro");
	mkdirSync(fixtureRoot, { recursive: true });
	writeFileSync(join(fixtureRoot, "Dockerfile"), dockerfile);
	if (viaVendor || standalone) {
		cargoManifest = cargoManifest.replace('"vendor/*"', '"vendor/*", "vendor/bridge", "vendor/dex-loop"');
	}
	writeFileSync(join(fixtureRoot, "Cargo.toml"), cargoManifest);
	const crate = (path, name, dependencies = "") => {
		mkdirSync(join(directory, path, "src"), { recursive: true });
		writeFileSync(join(directory, path, "Cargo.toml"),
			`[package]\nname = "${name}"\nversion = "0.1.0"\nedition = "2024"\n${dependencies}`);
		writeFileSync(join(directory, path, "src/lib.rs"), "");
	};
	for (const [, member] of cargoManifest.matchAll(/"(packages\/[^"\n]+)"/g)) {
		crate(`products/maestro/${member}`, member === "packages/codemode-rs" ? "agent-codemode" : member.split("/").at(-1),
			member === "packages/dex-host-rs"
				? `[dependencies]\ndex-loop = { path = "${viaVendor ? "../../vendor/bridge" : standalone ? "../../vendor/dex-loop" : "../../../../rust/crates/dex-loop"}" }\n`
				: member === "packages/codemode-rs" ? codemodeDependencies : "");
	}
	if (viaVendor) {
		crate("products/maestro/vendor/bridge", "dex-loop",
			'[dependencies]\nmanaged-inference-contract = { path = "../../../../rust/crates/managed-inference-contract" }\n[workspace]\n');
	}
	if (standalone) {
		crate("products/maestro/vendor/dex-loop", "dex-loop");
	} else {
		mkdirSync(join(directory, "rust"), { recursive: true });
		writeFileSync(join(directory, "rust/Cargo.toml"),
			'[workspace]\nresolver = "3"\nmembers = ["crates/dex-loop", "crates/managed-inference-contract"]\n'
			+ '[workspace.dependencies]\nmanaged-inference-contract = { path = "crates/managed-inference-contract" }\n'
			+ (crossWorkspace ? 'agent-codemode = { path = "../products/maestro/packages/codemode-rs" }\n' : ""));
		crate("rust/crates/dex-loop", "dex-loop",
			'[dependencies]\nmanaged-inference-contract.workspace = true\n'
			+ (crossWorkspace ? 'agent-codemode.workspace = true\n' : ""));
		crate("rust/crates/managed-inference-contract", "managed-inference-contract",
			transitive ? '[target.\'cfg(unix)\'.build-dependencies]\nreceipt-contract = { path = "../receipt-contract" }\n' : "");
		if (transitive) crate("rust/crates/receipt-contract", "receipt-contract");
	}
	try {
		return execFileSync(process.execPath, [checker], {
			cwd: fixtureRoot,
			encoding: "utf8",
			stdio: ["ignore", "pipe", "pipe"],
		});
	} finally {
		rmSync(directory, { recursive: true, force: true });
	}
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
	assert.equal(new Set(paths).size, 4);
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

test("Docker runtime guard rejects a missing named-context dependency manifest", () => {
	const copy = "COPY --from=dex-loop-workspace /crates/managed-inference-contract /rust/crates/managed-inference-contract\n";
	assert.throws(() => runChecker(sourceDockerfile.replace(copy, "")),
		/managed-inference-contract.*before cargo chef prepare/);
});

test("Docker runtime guard rejects a named-context dependency outside the narrowed workspace", () => {
	assert.throws(() => runChecker(sourceDockerfile.replace(', "crates/managed-inference-contract"', "")),
		/managed-inference-contract.*narrowed Rust workspace/);
});

test("Docker runtime guard resolves transitive target dependencies from Cargo manifests", () => {
	assert.throws(() => runChecker(sourceDockerfile, sourceCargoManifest, { transitive: true }),
		/receipt-contract.*before cargo chef prepare/);
	const complete = sourceDockerfile
		.replace('"crates/managed-inference-contract"]', '"crates/managed-inference-contract", "crates/receipt-contract"]')
		.replace("RUN cargo chef prepare --recipe-path recipe.json",
			"COPY --from=dex-loop-workspace /crates/receipt-contract /rust/crates/receipt-contract\nRUN cargo chef prepare --recipe-path recipe.json");
	assert.match(runChecker(complete, sourceCargoManifest, { transitive: true }), /Verified native-only Docker runtime contract\./);
});

test("Docker runtime guard rejects a named-context copy after recipe preparation", () => {
	const copy = "COPY --from=dex-loop-workspace /crates/managed-inference-contract /rust/crates/managed-inference-contract\n";
	const late = sourceDockerfile.replace(copy, "").replace(
		"RUN cargo chef prepare --recipe-path recipe.json", `RUN cargo chef prepare --recipe-path recipe.json\n${copy}`);
	assert.throws(() => runChecker(late), /managed-inference-contract.*before cargo chef prepare/);
});

test("Docker runtime guard requires planner dependencies before the native cook", () => {
	const copy = "COPY --from=planner /rust /rust\n";
	const without = sourceDockerfile.replace(copy, "");
	assert.throws(() => runChecker(without), /planner Rust dependency tree before cargo chef cook/);
	const late = without.replace(
		"RUN cargo chef cook --release --locked -p maestro --recipe-path recipe.json",
		`RUN cargo chef cook --release --locked -p maestro --recipe-path recipe.json\n${copy}`);
	assert.throws(() => runChecker(late), /planner Rust dependency tree before cargo chef cook/);
});

test("Docker runtime guard keeps standalone local dependencies inside their projection", () => {
	assert.match(runChecker(sourceDockerfile, sourceCargoManifest, { standalone: true }),
		/Verified native-only Docker runtime contract\./);
});

test("Docker runtime guard follows external Rust paths through a nonmember vendor crate", () => {
	const copy = "COPY --from=dex-loop-workspace /crates/managed-inference-contract /rust/crates/managed-inference-contract\n";
	assert.throws(() => runChecker(sourceDockerfile.replace(copy, ""), sourceCargoManifest, { viaVendor: true }),
		/managed-inference-contract.*before cargo chef prepare/);
	assert.throws(() => runChecker(sourceDockerfile.replace(', "crates/managed-inference-contract"', ""), sourceCargoManifest, { viaVendor: true }),
		/managed-inference-contract.*narrowed Rust workspace/);
	assert.match(runChecker(sourceDockerfile, sourceCargoManifest, { viaVendor: true }),
		/Verified native-only Docker runtime contract\./);
});

test("Docker runtime guard remaps a Rust dependency back into the Maestro image workspace", () => {
	const remap = `    && sed -i 's#path = "../products/maestro/#path = "/app/#g' /rust/Cargo.toml`;
	assert.ok(sourceDockerfile.includes(remap));
	assert.match(runChecker(sourceDockerfile, sourceCargoManifest, { crossWorkspace: true }),
		/Verified native-only Docker runtime contract\./);
	const without = sourceDockerfile.replace(remap, "    && true");
	assert.throws(() => runChecker(without, sourceCargoManifest, { crossWorkspace: true }),
		/Rust-to-Maestro dependency path.*before cargo chef prepare/);
	const late = without.replace("RUN cargo chef prepare --recipe-path recipe.json",
		`RUN cargo chef prepare --recipe-path recipe.json\nRUN ${remap.trim().slice(3)}`);
	assert.throws(() => runChecker(late, sourceCargoManifest, { crossWorkspace: true }),
		/Rust-to-Maestro dependency path.*before cargo chef prepare/);
});

test("Docker runtime guard preserves an actual library consumed by external Rust during cook", () => {
	const filter = `sed -i '/^    "packages\\/codemode-rs",$/d' Cargo.toml`;
	const exclude = `sed -i 's|"vendor/\\*"|"vendor/*", "packages/codemode-rs"|' Cargo.toml`;
	assert.ok(sourceDockerfile.includes(filter));
	assert.ok(sourceDockerfile.includes(exclude));
	assert.match(runChecker(sourceDockerfile, sourceCargoManifest, { crossWorkspace: true }),
		/Verified native-only Docker runtime contract\./);
	for (const command of [filter, exclude]) {
		assert.throws(() => runChecker(sourceDockerfile.replace(command, "true"), sourceCargoManifest, { crossWorkspace: true }),
			/actual shared library.*before cargo chef prepare/);
	}
	const late = sourceDockerfile.replace(filter, "true").replace(exclude, "true").replace(
		"RUN cargo chef prepare --recipe-path recipe.json",
		`RUN cargo chef prepare --recipe-path recipe.json\nRUN ${filter} && ${exclude}`);
	assert.throws(() => runChecker(late, sourceCargoManifest, { crossWorkspace: true }),
		/actual shared library.*before cargo chef prepare/);
	const restore = "COPY Cargo.toml Cargo.lock ./";
	const nativeCopy = sourceDockerfile.indexOf(restore, sourceDockerfile.indexOf(restore) + restore.length);
	assert.notEqual(nativeCopy, -1);
	const withoutRestore = sourceDockerfile.slice(0, nativeCopy) + sourceDockerfile.slice(nativeCopy + restore.length);
	assert.throws(() => runChecker(withoutRestore, sourceCargoManifest, { crossWorkspace: true }),
		/original Maestro workspace after cook and before native build/);
});

test("Docker runtime guard copies shared external dependencies into the fresh native stage before cook", () => {
	const nativeStart = sourceDockerfile.indexOf("FROM chef AS native");
	const native = sourceDockerfile.slice(nativeStart);
	const cook = "RUN cargo chef cook --release --locked -p maestro --recipe-path recipe.json";
	const cookStart = native.indexOf(cook);
	const copy = "COPY packages/codemode-rs ./packages/codemode-rs\n";
	assert.notEqual(nativeStart, -1);
	assert.notEqual(cookStart, -1);
	// Retain the existing post-cook COPY: final workspace completeness alone
	// cannot prove that an external path dependency exists during the cook.
	const without = sourceDockerfile.slice(0, nativeStart)
		+ native.slice(0, cookStart).replaceAll(copy, "") + native.slice(cookStart);
	assert.throws(() => runChecker(without, sourceCargoManifest, { crossWorkspace: true }),
		/shared dependency packages\/codemode-rs before cargo chef cook/);
	assert.match(runChecker(without.replace(cook, `${copy}${cook}`), sourceCargoManifest, { crossWorkspace: true }),
		/Verified native-only Docker runtime contract\./);
});

test("Docker runtime guard rejects workspace inheritance in an image-excluded shared library", () => {
	// Run 37264900952: `sha2.workspace = true` in codemode-rs, which the image
	// moves out of the Maestro workspace, failed `cargo chef cook`.
	assert.match(runChecker(sourceDockerfile, sourceCargoManifest,
		{ crossWorkspace: true, codemodeDependencies: '[dependencies]\nsha2 = "0.10"\n' }),
	/Verified native-only Docker runtime contract\./);
	for (const inherited of ["sha2.workspace = true", "sha2 = { workspace = true }"]) {
		assert.throws(() => runChecker(sourceDockerfile, sourceCargoManifest,
			{ crossWorkspace: true, codemodeDependencies: `[dependencies]\n${inherited}\n` }),
		/Image-excluded shared libraries must not inherit workspace keys: packages\/codemode-rs\/Cargo.toml/);
	}
});

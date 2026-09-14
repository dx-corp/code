import {
	existsSync,
	mkdirSync,
	mkdtempSync,
	readFileSync,
	rmSync,
	writeFileSync,
} from "node:fs";
import { test } from "node:test";
import assert from "node:assert/strict";
import { tmpdir } from "node:os";
import { join, posix } from "node:path";
import { spawnSync } from "node:child_process";

const read = (path) => readFileSync(new URL(`../${path}`, import.meta.url), "utf8");
const packageJson = JSON.parse(read("package.json"));
const isPublicProjection = read("AGENTS.md").includes("generated public mirror");
const cargo = read("Cargo.toml");
const materialize = read("scripts/materialize-native-package.mjs");
const releaseBuilder = read("scripts/build-release-binary.mjs");
const dockerfile = read("Dockerfile");
const updateCli = read("packages/tui-rs/src/update_cli.rs");
const ci = read(".github/workflows/maestro-ci.yml");
const release = read(".github/workflows/release.yml");
const ghcr = read(".github/workflows/ghcr-publish.yml");
const mirrorPath = new URL("../.github/workflows/public-release-mirror.yml", import.meta.url);
const mirror = existsSync(mirrorPath) ? read(".github/workflows/public-release-mirror.yml") : "";
const runtimeConformance = read("scripts/run-runtime-conformance.mjs");
const passport = read("scripts/generate-runtime-passport.mjs");

test("fast validation and distribution profiles have distinct optimization contracts", () => {
	assert.match(cargo, /\[profile\.fast-validation\][\s\S]*?lto = false[\s\S]*?codegen-units = 16/);
	assert.match(cargo, /\[profile\.release\][\s\S]*?lto = "thin"[\s\S]*?codegen-units = 1/);
	assert.match(cargo, /\[profile\.release-dist\][\s\S]*?lto = "thin"[\s\S]*?codegen-units = 1/);
});

test("PR build selects fast-validation and materializes its profile output", () => {
	assert.match(packageJson.scripts.build, /--profile fast-validation/);
	assert.match(packageJson.scripts.build, /materialize-native-package\.mjs --profile fast-validation/);
	assert.match(ci, /npm run build\n\s+npm run smoke:release-native-only/);
	assert.match(materialize, /const profileIndex = process\.argv\.indexOf\("--profile"\)/);
	assert.match(materialize, /process\.env\.CARGO_TARGET_DIR \|\| "target"/);
});

test("native materialization honors CARGO_TARGET_DIR", () => {
	const directory = mkdtempSync(join(tmpdir(), "maestro-materialize-"));
	const targetDirectory = join(directory, "cargo-target");
	const profileDirectory = join(targetDirectory, "fast-validation");
	mkdirSync(profileDirectory, { recursive: true });
	writeFileSync(join(profileDirectory, "maestro"), "native-binary");

	try {
		const result = spawnSync(
			process.execPath,
			[
				new URL("materialize-native-package.mjs", import.meta.url).pathname,
				"--profile",
				"fast-validation",
			],
			{
				cwd: directory,
				env: { ...process.env, CARGO_TARGET_DIR: targetDirectory },
				encoding: "utf8",
			},
		);

		assert.equal(result.status, 0, result.stderr);
		assert.equal(
			readFileSync(
				join(directory, "vendor", "maestro", `${process.platform}-${process.arch}`, "maestro"),
				"utf8",
			),
			"native-binary",
		);
		assert(existsSync(join(directory, "bin", "maestro")));
		assert(existsSync(join(directory, "bin", "deixic-code")));
		const launcher = readFileSync(join(directory, "bin", "maestro"), "utf8");
		assert.match(
			launcher,
			/root=\$\(CDPATH='' cd -- "\$\(dirname -- "\$script"\)\/\.\." && pwd -P\)/,
		);
		assert.match(launcher, /MAESTRO_INSTALL_METHOD=package/);
		assert(launcher.includes(`MAESTRO_PACKAGE_NAME='${packageJson.name}'`));
		assert.match(launcher, /MAESTRO_PACKAGE_ROOT="\$root"/);
		assert(launcher.includes(`MAESTRO_VERSION='${packageJson.version}'`));
		const canonicalLauncher = readFileSync(
			join(directory, "bin", "deixic-code"),
			"utf8",
		);
		assert.match(canonicalLauncher, /exec "\$bin_dir\/maestro" "\$@"/);
	} finally {
		rmSync(directory, { recursive: true, force: true });
	}
});

test("release and container entrypoints select optimized release", () => {
	assert.match(packageJson.scripts["build:release"], /--profile release/);
	assert.match(packageJson.scripts["release:check"], /build:release/);
	assert.match(releaseBuilder, /profile: "release"/);
	assert.match(releaseBuilder, /"--profile"[\s\S]*?options\.profile/);
	assert.match(releaseBuilder, /target\/\$\{target\}\/\$\{options\.profile\}/);
	assert.match(dockerfile, /cargo chef prepare --recipe-path recipe\.json/);
	assert.match(dockerfile, /cargo chef cook --release --locked -p maestro/);
	const embeddedInstallerReference = updateCli.match(
		/const EMBEDDED_INSTALLER: &str = include_str!\("([^"]+)"\);/,
	)?.[1];
	assert.ok(embeddedInstallerReference, "updater must declare an embedded installer source");
	const embeddedInstallerPath = posix.normalize(
		posix.join("packages/tui-rs/src", embeddedInstallerReference),
	);
	const nativeStage = dockerfile.match(/^FROM chef AS native\n([\s\S]*?)(?=^FROM )/m)?.[1];
	assert.ok(nativeStage, "native Docker stage must exist");
	const installerCopy = "COPY " + embeddedInstallerPath + " ./" + embeddedInstallerPath;
	const buildCommand = "RUN cargo build --release --locked -p maestro";
	const installerCopyIndex = nativeStage.indexOf(installerCopy);
	const buildCommandIndex = nativeStage.indexOf(buildCommand);
	assert.ok(
		existsSync(new URL("../" + embeddedInstallerPath, import.meta.url)),
		"updater's embedded installer source must exist in the Docker build context",
	);
	assert.ok(
		installerCopyIndex >= 0 && installerCopyIndex < buildCommandIndex,
		"native image must copy the updater's embedded installer input before compilation",
	);
	assert.match(dockerfile, /cargo build --release --locked -p maestro/);
	assert.match(dockerfile, /target\/release\/maestro/);
	if (isPublicProjection) {
		assert.match(ghcr, /cancel-in-progress: false/);
	} else {
		assert.match(
			ghcr,
			/cancel-in-progress: \$\{\{ github\.event_name != 'workflow_dispatch' \}\}/,
		);
	}
	assert.match(ghcr, /docker\/build-push-action/);
	if (mirror) assert.match(mirror, /npm run build:release/);
	if (isPublicProjection && release.includes("verify-staged-release.mjs")) {
		assert.match(release, /node scripts\/verify-staged-release\.mjs release-binaries/);
		assert.ok(release.indexOf("Authenticate artifacts and release receipts") < release.indexOf("Materialize native npm package"));
		const verifier = readFileSync(new URL("./verify-staged-release.mjs", import.meta.url), "utf8");
		assert.match(verifier, /cosign/);
		assert.match(verifier, /maestro-release\.yml@refs\/heads\/main/);
	} else {
		assert.match(release, /build-release-binary\.mjs --platform/);
	}
});

test("internal release artifacts are proven by exact-runtime conformance and passports", {
	skip: isPublicProjection && "internal release workflow is excluded from the public projection",
}, () => {
	assert.match(runtimeConformance, /--docker-image/);
	assert.match(runtimeConformance, /--binary-launcher/);
	assert.match(runtimeConformance, /approval_request_and_resolution/);
	assert.match(runtimeConformance, /drain_terminal_receipt/);
	assert.match(runtimeConformance, /CONFORMANCE_FIXTURE/);
	assert.match(runtimeConformance, /runtime-conformance-v1\.json/);
	assert.match(runtimeConformance, /FETCH_TIMEOUT_MS/);
	assert.match(runtimeConformance, /--artifact-digest/);
	assert.match(passport, /runtime-passport\.v1/);
	assert.match(release, /Conformance against the exact native release artifact/);
	assert.match(release, /Conformance against the exact release binary/);
	assert.match(release, /host_arch.*uname -m/);
	assert.match(release, /runtime-passport-maestro-\$\{platform\}\.cosign\.bundle/);
	assert.match(ghcr, /Conformance against the exact OCI release artifact/);
	assert.match(ghcr, /push: \$\{\{ github\.event_name == 'workflow_dispatch' && github\.ref == 'refs\/tags\/recovery-publisher-e0c1a2d-v2' \}\}/);
	assert.match(ghcr, /load: \$\{\{ env\.PUBLISH_MAIN == 'true' \}\}/);
	assert.match(ghcr, /conformance-\$\{\{ github\.run_id \}\}-\$\{\{ github\.run_attempt \}\}/);
	assert.match(ghcr, /IMAGE: \$\{\{ env\.IMAGE_NAME \}\}@\$\{\{ steps\.conformance-image\.outputs\.digest \}\}/);
	assert.match(ghcr, /--docker-image "\$IMAGE"/);
	assert.match(ghcr, /Publish isolated conformance image/);
	assert.match(ghcr, /timeout --signal=TERM --kill-after=30s 10m docker push "\$IMAGE_TAG"/);
	assert.match(ghcr, /pushed_digest.*EXPECTED_DIGEST/);
	assert.doesNotMatch(ghcr, /for \(index = 1; index <= NF; index \+= 1\)/);
	assert.match(ghcr, /for \(field_index = 1; field_index <= NF; field_index \+= 1\)/);
	assert.match(ghcr, /Remove isolated conformance image/);
	assert.match(ghcr, /always\(\).*env\.PUBLISH_MAIN == 'true'/);
	assert.match(ghcr, /PUSH_MARKER/);
	assert.match(ghcr, /touch "\$PUSH_MARKER"/);
	assert.match(ghcr, /gh api "orgs\/\$\{package_owner\}\/packages\/container\/\$\{package_name\}\/versions\?per_page=100"/);
	assert.match(ghcr, /packages\/container\/\$\{package_name\}\/versions/);
	assert.match(ghcr, /\[\.\[\] \| select\(\(\.metadata\.container\.tags/);
	assert.doesNotMatch(ghcr, /\[\.\[\]\[\] \| select\(\(\.metadata\.container\.tags/);
	assert.match(ghcr, /gh api --method DELETE/);
	assert.match(ghcr, /refusing to delete .*also carries/);
	const conformanceIndex = ghcr.indexOf("- name: Conformance against the exact OCI release artifact");
	const isolatedConformanceIndex = ghcr.indexOf("- name: Publish isolated conformance image");
	const cleanupIndex = ghcr.indexOf("- name: Remove isolated conformance image");
	const publishIndex = ghcr.indexOf("- name: Publish verified image tags");
	const signIndex = ghcr.indexOf("- name: Sign image with cosign");
	assert(conformanceIndex >= 0 && conformanceIndex < publishIndex);
	assert(isolatedConformanceIndex >= 0 && isolatedConformanceIndex < conformanceIndex);
	assert(conformanceIndex < cleanupIndex && cleanupIndex < publishIndex);
	assert(publishIndex < signIndex);
	assert.match(ghcr, /timeout --signal=TERM --kill-after=30s 10m docker push/);
	assert.match(ghcr, /maestro-runtime-passport\/v1/);
});

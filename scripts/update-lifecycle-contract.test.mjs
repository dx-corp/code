import assert from "node:assert/strict";
import { existsSync, readFileSync } from "node:fs";
import { test } from "node:test";
import { buildReleaseMetadata } from "./create-release-metadata.mjs";

const read = (path) => readFileSync(new URL(`../${path}`, import.meta.url), "utf8");
const readReleaseWorkflow = () =>
	readFileSync(new URL("../.github/workflows/release.yml", import.meta.url), "utf8");

test("update lifecycle contract names every machine-readable surface", () => {
	const contract = JSON.parse(read("docs/protocols/update-lifecycle.json"));
	assert.equal(contract.schemaVersion, "evalops.maestro.update-lifecycle.v1");
	assert.deepEqual(Object.keys(contract.commands).sort(), ["apply", "history", "rollback", "status"]);
	assert.equal(contract.persistence.maximumAttempts, 32);
	assert.equal(contract.commands.apply.jsonSchema.channel, "stable|beta|alpha");
	assert.equal(contract.persistence.receiptSchema, "evalops.maestro.install-receipt.v1");
	assert.equal(contract.persistence.channelManifestSchema, "evalops.maestro.release-channel.v1");
	assert.equal(contract.commands.status.jsonSchema.channel, "stable|beta|alpha");
	assert.equal(
		contract.persistence.channelManifestSource.releaseListUrl,
		"https://api.github.com/repos/evalops/maestro/releases",
	);
	assert.equal(contract.persistence.channelManifestSource.releaseAsset, "channel-manifest.json");
	assert.equal(
		contract.commands.rollback.jsonSchema.launcherWarning,
		"string|null; launcher was replaced but parent-directory durability sync reported an error",
	);
});

test("release metadata carries changelog notes and exact runtime passports", async () => {
	const passport = {
		artifact: { name: "maestro-linux-x64", digest: `sha256:${"a".repeat(64)}` },
		schemaVersion: "evalops.maestro.runtime-passport.v1",
	};
	const metadata = await buildReleaseMetadata({
		version: "1.2.3",
		releaseTag: "v1.2.3",
		sourceSha: "b".repeat(40),
		changelog: "## [1.2.3] - 2026-08-16\n\n### Fixed\n\n- Preserve the receipt.\n\n## [1.2.2] - 2026-08-15\n",
		passports: [passport],
	});
	assert.equal(metadata.schemaVersion, "evalops.maestro.release-metadata.v1");
	assert.equal(metadata.releaseNotes, "### Fixed\n\n- Preserve the receipt.");
	assert.equal(metadata.receipt.sourceSha, "b".repeat(40));
	assert.deepEqual(metadata.receipt.artifacts, [
		{ name: "maestro-linux-x64", digest: `sha256:${"a".repeat(64)}`, runtimePassport: passport },
	]);
});

test("installer and release workflow preserve receipt metadata", () => {
	const installer = read("scripts/install.sh");
	const release = readReleaseWorkflow();
	const updater = read("packages/tui-rs/src/update_cli.rs");
	const channelManifest = read("scripts/create-release-channel-manifest.mjs");
	const channelResolver = read("scripts/resolve-release-channel.mjs");
	assert.match(installer, /release-metadata\.json/);
	assert.match(installer, /install-receipt\.json/);
	assert.doesNotMatch(installer, /\$web_asset/);
	assert.doesNotMatch(installer, /webSha256/);
	assert.match(installer, /MAESTRO_STARTUP_UPDATE_STATE/);
	assert.match(installer, /MAESTRO_INSTALL_CHANNEL/);
	assert.match(installer, /MAESTRO_UPDATE_CHANNEL/);
	assert.match(installer, /MAESTRO_CHANNEL_MANIFEST_URL/);
	assert.match(installer, /releases\/latest\/download\/channel-manifest\.json/);
	assert.match(installer, /resolve_channel_release_url/);
	assert.match(installer, /fetch_github_releases/);
	assert.match(installer, /validate_channel_manifest/);
	assert.match(installer, /channel_version_matches/);
	assert.match(installer, /bootstrap_cosign/);
	assert.match(installer, /signature-digest-algorithm sha512/);
	assert.doesNotMatch(installer, /pkeyutl -verify/);
	assert.doesNotMatch(installer, /python3/);
	assert.doesNotMatch(installer, /maestro-\$\{install_channel\}-channel/);
	assert.match(installer, /receipt_hash_file/);
	assert.doesNotMatch(installer, /refusing installation without release receipt metadata/);
	assert.doesNotMatch(updater, /restore_verified_web_tree/);
	assert.doesNotMatch(updater, /web_sha256/);
	assert.doesNotMatch(updater, /MAESTRO_WEB_STATIC_ROOT/);
	assert.match(updater, /load_verified_release_metadata/);
	assert.doesNotMatch(updater, /Command::new\("tar"\)/);
	assert.match(updater, /durability_warning/);
	assert.match(updater, /legacy_channel_manifest_url/);
	assert.match(updater, /GITHUB_RELEASES_API_URL/);
	assert.match(updater, /releases\/latest\/download\/channel-manifest\.json/);
	assert.match(updater, /resolve_github_channel_manifest_url/);
	assert.match(updater, /legacyExplicit/);
	assert.doesNotMatch(updater, /MAESTRO_UPDATE_PREVIEW_FALLBACK/);
	assert.doesNotMatch(read("docs/protocols/release-channels.json"), /MAESTRO_UPDATE_PREVIEW_FALLBACK/);
	// The nested Mono template creates the signed receipt. In the generated
	// public mirror, the public-owned workflow authenticates that same receipt
	// before packaging and publishing it. CI must enforce the actual owner in
	// each checkout, rather than accepting a private build template as the
	// public publishing entrypoint.
	const publicMirror = existsSync(new URL("../.github/workflows/check-release-workflow-contract.mjs", import.meta.url));
	if (publicMirror) {
		assert.match(release, /repository_dispatch:/);
		assert.match(release, /maestro-signed-release/);
		assert.match(release, /MONO_SHA256SUMS\.cosign\.bundle/);
		assert.match(release, /node scripts\/verify-staged-release\.mjs release-binaries "\$RELEASE_VERSION"/);
		assert.match(release, /files\+=\(release-metadata\.json/);
		assert.doesNotMatch(release, /evalops-internal-arc/);
	} else {
		assert.match(release, /create-release-metadata\.mjs/);
		assert.match(release, /files\+=\([^\n]*release-metadata\.json/);
	}
	assert.match(release, /release-metadata\.json/);
	assert.match(release, /create-release-channel-manifest\.mjs/);
	assert.match(release, /channel-manifest\.json/);
	assert.doesNotMatch(release, /maestro-web-dist/);
	assert.doesNotMatch(release, /packages\/web\/dist/);
	assert.match(channelManifest, /createPrivateKey/);
	assert.match(channelResolver, /alpha or beta/);
});

test("installer certificate identity accepts historical and renamed organization aliases only", () => {
	const installer = read("scripts/install.sh");
	const source = installer.match(/^COSIGN_IDENTITY_REGEXP='([^']+)'$/m)?.[1];
	assert.ok(source, "installer must define its Cosign certificate identity regular expression");
	const identity = new RegExp(source);
	for (const owner of ["evalops", "dx-corp"]) {
		assert.match(
			`https://github.com/${owner}/maestro-internal/.github/workflows/release.yml@refs/tags/v0.10.90`,
			identity,
		);
		assert.match(
			`https://github.com/${owner}/maestro/.github/workflows/release.yml@refs/tags/v0.10.90`,
			identity,
		);
		assert.match(
			`https://github.com/${owner}/mono/.github/workflows/maestro-release.yml@refs/heads/main`,
			identity,
		);
	}
	assert.doesNotMatch(
		"https://github.com/attacker/mono/.github/workflows/maestro-release.yml@refs/heads/main",
		identity,
	);
	assert.doesNotMatch(
		"https://github.com/dx-corp/mono/.github/workflows/release.yml@refs/heads/main",
		identity,
	);
});

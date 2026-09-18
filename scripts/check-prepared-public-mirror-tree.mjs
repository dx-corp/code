#!/usr/bin/env node

import { spawnSync } from "node:child_process";
import { existsSync, readdirSync } from "node:fs";
import { join, relative, resolve } from "node:path";

function parseArgs(argv) {
	const args = {
		target: "",
	};
	for (let index = 0; index < argv.length; index += 1) {
		const arg = argv[index];
		switch (arg) {
			case "--target":
				args.target = argv[++index] ?? "";
				break;
			default:
				throw new Error(`Unknown argument: ${arg}`);
		}
	}
	if (!args.target) {
		throw new Error("Missing required --target <path>");
	}
	return args;
}

function fail(message) {
	console.error(message);
	process.exit(1);
}

const NON_MIRRORED_SCAN_DIRECTORIES = new Set([
	".git",
	"coverage",
	"dist",
	"node_modules",
	"target",
	"tmp",
]);

function findCredentialArtifacts(root, current = root) {
	const matches = [];
	for (const entry of readdirSync(current, { withFileTypes: true })) {
		if (entry.isDirectory() && NON_MIRRORED_SCAN_DIRECTORIES.has(entry.name)) {
			continue;
		}
		const path = join(current, entry.name);
		if (entry.isDirectory()) {
			matches.push(...findCredentialArtifacts(root, path));
		} else if (entry.isFile() && /^gha-creds-.*\.json$/u.test(entry.name)) {
			matches.push(relative(root, path));
		}
	}
	return matches;
}

const options = parseArgs(process.argv.slice(2));
const targetRoot = resolve(options.target);

if (!existsSync(targetRoot)) {
	fail(`Prepared public mirror target does not exist: ${targetRoot}`);
}

if (!existsSync(resolve(targetRoot, "rustfmt.toml"))) {
	fail("Prepared public mirror is missing the canonical rustfmt.toml configuration.");
}

if (existsSync(resolve(targetRoot, ".github/public-release-mirror.exclude"))) {
	fail(".github/public-release-mirror.exclude must not exist in the prepared public mirror tree.");
}

const credentialArtifacts = findCredentialArtifacts(targetRoot);
if (credentialArtifacts.length > 0) {
	fail(
		`Prepared public mirror contains GitHub Actions credential artifact(s): ${credentialArtifacts.join(", ")}`,
	);
}

const boundaryScript = resolve(
	targetRoot,
	"scripts/check-public-surface-boundary.mjs",
);
if (!existsSync(boundaryScript)) {
	fail(`Missing public surface boundary checker in prepared public mirror tree: ${boundaryScript}`);
}

const result = spawnSync(process.execPath, [boundaryScript], {
	cwd: targetRoot,
	encoding: "utf8",
});

if (result.stdout) {
	process.stdout.write(result.stdout);
}
if (result.stderr) {
	process.stderr.write(result.stderr);
}

if (result.status !== 0) {
	fail(`Public surface boundary smoke failed with exit code ${result.status ?? "unknown"}.`);
}

console.log("Prepared public mirror tree smoke passed.");

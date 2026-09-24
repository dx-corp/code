#!/usr/bin/env node
import { assertPublicProtocolArtifact } from "./check-public-protocol-artifact.mjs";
import { execFileSync } from "node:child_process";
import { chmodSync, copyFileSync, mkdirSync, statSync, readFileSync } from "node:fs";
import { dirname, resolve } from "node:path";

const TARGETS = new Map([
	["linux-x64", "x86_64-unknown-linux-gnu"],
	["linux-arm64", "aarch64-unknown-linux-gnu"],
	["darwin-x64", "x86_64-apple-darwin"],
	["darwin-arm64", "aarch64-apple-darwin"],
]);

const options = {
	platform: "linux-x64",
	outfile: "",
	target: "",
	profile: "release",
	skipBuild: false,
};
for (let i = 2; i < process.argv.length; i++) {
	const arg = process.argv[i];
	if (arg === "--platform") options.platform = process.argv[++i] ?? "";
	else if (arg === "--outfile") options.outfile = process.argv[++i] ?? "";
	else if (arg === "--target") options.target = process.argv[++i] ?? "";
	else if (arg === "--profile") options.profile = process.argv[++i] ?? "";
	else if (arg === "--skip-build") options.skipBuild = true;
	else throw new Error(`Unknown argument: ${arg}`);
}
const target = options.target || TARGETS.get(options.platform);
if (!target) throw new Error(`Unsupported platform: ${options.platform}`);
if (!options.profile || !/^[A-Za-z0-9_-]+$/.test(options.profile)) {
	throw new Error(`Invalid Cargo profile: ${options.profile || "(missing)"}`);
}
const outfile = resolve(options.outfile || `dist/release/maestro-${options.platform}`);
if (!options.skipBuild) {
	execFileSync(
		"cargo",
		[
			"build",
			"--profile",
			options.profile,
			"--locked",
			"-p",
			"maestro",
			"--target",
			target,
		],
		{ stdio: "inherit" },
	);
}
const source = resolve(`target/${target}/${options.profile}/maestro`);
mkdirSync(dirname(outfile), { recursive: true });
copyFileSync(source, outfile);
chmodSync(outfile, 0o755);
assertPublicProtocolArtifact(readFileSync(outfile), outfile);
console.log(`Built native release ${outfile} (${statSync(outfile).size} bytes).`);

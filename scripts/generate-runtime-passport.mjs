#!/usr/bin/env node

import { createHash } from "node:crypto";
import { readFileSync, writeFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const PASSPORT_VERSION = "evalops.maestro.runtime-passport.v1";
const PREDICATE_TYPE = "https://evalops.dev/attestations/maestro-runtime-passport/v1";
const CONFORMANCE_VERSION = "evalops.maestro.runtime-conformance.v1";
const FIXTURE = resolve(ROOT, "packages/runtime-contracts-rs/fixtures/runtime-conformance-v1.json");
const PASSPORT_CONTRACT = resolve(ROOT, "packages/runtime-contracts-rs/fixtures/runtime-passport-contract-v1.json");
const DRIVER = resolve(ROOT, "scripts/run-runtime-conformance.mjs");
const NATIVE_FIXTURE = resolve(ROOT, "packages/hosted-runner-rs/src/hosted_runner_conformance.rs");

function canonicalize(value) {
	if (value === null || typeof value === "boolean" || typeof value === "string") {
		return JSON.stringify(value);
	}
	if (typeof value === "number") return JSON.stringify(value);
	if (Array.isArray(value)) return `[${value.map(canonicalize).join(",")}]`;
	return `{${Object.keys(value)
		.sort()
		.map((key) => `${JSON.stringify(key)}:${canonicalize(value[key])}`)
		.join(",")}}`;
}

function digest(value) {
	return `sha256:${createHash("sha256").update(value).digest("hex")}`;
}

function fileDigest(path) {
	return digest(readFileSync(path));
}

export function buildRuntimeConformanceIdentity() {
	const fixtureDigest = digest(canonicalize(JSON.parse(readFileSync(FIXTURE, "utf8"))));
	const driverDigest = fileDigest(DRIVER);
	const nativeFixtureDigest = fileDigest(NATIVE_FIXTURE);
	const suiteDigest = digest(canonicalize({ fixtureDigest, driverDigest, nativeFixtureDigest }));
	return { fixtureDigest, driverDigest, nativeFixtureDigest, suiteDigest };
}

function parseArgs(argv) {
	const values = { profiles: [] };
	for (let index = 0; index < argv.length; index += 1) {
		const argument = argv[index];
		if (argument === "--behavior-profile") values.profiles.push(argv[++index] ?? "");
		else if (argument.startsWith("--")) values[argument.slice(2).replaceAll("-", "_")] = argv[++index] ?? "";
		else throw new Error(`Unknown argument: ${argument}`);
	}
	for (const field of [
		"artifact_kind",
		"artifact_name",
		"artifact_digest",
		"source_sha",
		"compatibility_digest",
		"launch_spec_version",
		"receipt_version",
		"rustc",
		"target",
		"out",
	]) {
		if (!values[field]) throw new Error(`missing required --${field.replaceAll("_", "-")}`);
	}
	if (!values.profiles.length) throw new Error("at least one --behavior-profile is required");
	return values;
}

function validateSourceSha(value) {
	if (!/^[0-9a-f]{40}$/i.test(value)) throw new Error("source SHA must be forty hexadecimal characters");
}

function validateArtifactKind(value) {
	if (!new Set(["native_binary", "oci_image"]).has(value)) {
		throw new Error("artifact kind must be native_binary or oci_image");
	}
}

function validateDigest(value, label) {
	if (!/^sha256:[0-9a-f]{64}$/i.test(value)) throw new Error(`${label} must be a sha256 digest`);
}

export function buildRuntimePassport(options) {
	validateArtifactKind(options.artifact_kind);
	if (!options.artifact_name?.trim()) throw new Error("artifact name must not be empty");
	validateSourceSha(options.source_sha);
	for (const [label, value] of [
		["artifact digest", options.artifact_digest],
		["compatibility digest", options.compatibility_digest],
	]) {
		validateDigest(value, label);
	}
	for (const [label, value] of [
		["launch spec version", options.launch_spec_version],
		["receipt version", options.receipt_version],
		["rustc identity", options.rustc],
		["target", options.target],
	]) {
		if (!value?.trim()) throw new Error(`${label} must not be empty`);
	}
	const passportContract = JSON.parse(readFileSync(PASSPORT_CONTRACT, "utf8"));
	for (const profile of options.profiles) {
		if (!passportContract.behaviorProfiles.includes(profile)) {
			throw new Error(`behavior profile is not exercised by the conformance suite: ${profile}`);
		}
	}
	const { fixtureDigest, suiteDigest } = buildRuntimeConformanceIdentity();
	const passport = {
		schemaVersion: PASSPORT_VERSION,
		predicateType: PREDICATE_TYPE,
		artifact: {
			kind: options.artifact_kind,
			name: options.artifact_name,
			digest: options.artifact_digest.toLowerCase(),
		},
		sourceSha: options.source_sha.toLowerCase(),
		compatibilityDigest: options.compatibility_digest.toLowerCase(),
		launchSpecVersion: options.launch_spec_version,
		receiptVersion: options.receipt_version,
		conformance: {
			suiteVersion: CONFORMANCE_VERSION,
			fixtureDigest,
			suiteDigest,
		},
		behaviorProfiles: [...new Set(options.profiles)].sort(),
		toolchain: { rustc: options.rustc, target: options.target },
	};
	if (passport.behaviorProfiles.length !== options.profiles.length) {
		throw new Error("behavior profiles must be unique");
	}
	return passport;
}

if (process.argv[1] && import.meta.url === pathToFileURL(resolve(process.argv[1])).href) {
	const options = parseArgs(process.argv.slice(2));
	const passport = buildRuntimePassport(options);
	writeFileSync(resolve(ROOT, options.out), `${JSON.stringify(passport, null, 2)}\n`);
	console.log(`Wrote runtime passport ${resolve(ROOT, options.out)}`);
}

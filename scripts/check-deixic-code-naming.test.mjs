import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";

import { findDeixicCodeNamingProblems } from "./check-deixic-code-naming.mjs";

const root = new URL("..", import.meta.url).pathname;

test("the source tree keeps Deixic Code canonical and Maestro compatible", () => {
	assert.deepEqual(findDeixicCodeNamingProblems(root), []);
});

test("the generated canonical package keeps the compatibility contract", () => {
	const packagePath = new URL("../package.json", import.meta.url);
	const packageJson = JSON.parse(readFileSync(packagePath, "utf8"));
	packageJson.name = packageJson.maestro.canonicalPackageName;
	packageJson.maestro.packageAliases = [
		packageJson.name,
		...packageJson.maestro.packageAliases,
	];
	assert.deepEqual(
		findDeixicCodeNamingProblems(
			root,
			new Map([["package.json", JSON.stringify(packageJson)]]),
		),
		[],
	);
});

test("the guard rejects a missing command alias", () => {
	const packagePath = new URL("../package.json", import.meta.url);
	const packageJson = JSON.parse(readFileSync(packagePath, "utf8"));
	delete packageJson.bin.maestro;
	const problems = findDeixicCodeNamingProblems(
		root,
		new Map([["package.json", JSON.stringify(packageJson)]]),
	);
	assert(problems.some((problem) => problem.includes("maestro binary alias")));
});

test("the guard rejects stale customer-facing Maestro display copy", () => {
	const path = "packages/maestro-rs/src/main.rs";
	const content = readFileSync(new URL(`../${path}`, import.meta.url), "utf8")
		.replace('const HELP: &str = "Deixic Code', 'const HELP: &str = "Maestro');
	const problems = findDeixicCodeNamingProblems(root, new Map([[path, content]]));
	assert(problems.some((problem) => problem.includes("stale display text")));
});

test("the guard rejects stale Maestro copy in contributor documentation", () => {
	const path = "CONTRIBUTING.md";
	const content = readFileSync(new URL(`../${path}`, import.meta.url), "utf8")
		.replace("# Contributing to Deixic Code", "# Contributing to Maestro");
	const problems = findDeixicCodeNamingProblems(root, new Map([[path, content]]));
	assert(problems.some((problem) => problem.includes("# Contributing to Maestro")));
});

test("the guard rejects stale Maestro copy in runtime messages", () => {
	const path = "packages/runtime-gateway-rs/src/a2a/tasks.rs";
	const content = readFileSync(new URL(`../${path}`, import.meta.url), "utf8")
		.replace("Deixic Code is working on the A2A task", "Maestro is working on the A2A task");
	const problems = findDeixicCodeNamingProblems(root, new Map([[path, content]]));
	assert(problems.some((problem) => problem.includes("Maestro is working on the A2A task")));
});

test("the guard requires the Dex Code terminal title and shared title renderer", () => {
	const path = "packages/presentation-rs/src/components/deixic_logo.rs";
	const content = readFileSync(new URL(`../${path}`, import.meta.url), "utf8");
	for (const stale of [
		content.replace('PRODUCT_TITLE: &str = "Dex Code"', 'PRODUCT_TITLE: &str = "Dex"'),
		content.replace("shimmer_spans(PRODUCT_TITLE)", 'shimmer_spans("Deixic Code")'),
	]) {
		const problems = findDeixicCodeNamingProblems(root, new Map([[path, stale]]));
		assert(problems.some((problem) => problem.startsWith(`${path} is missing`)));
	}
});

test("the guard requires the extracted native runtime description", () => {
	const path = "packages/runtime-rs/Cargo.toml";
	const content = readFileSync(new URL(`../${path}`, import.meta.url), "utf8");
	for (const stale of [
		"runtime compatibility facade for Deixic Code",
		"Native agent runtime for Maestro",
	]) {
		const problems = findDeixicCodeNamingProblems(root, new Map([
			[path, content.replace("Native agent runtime for Deixic Code", stale)],
		]));
		assert(problems.some((problem) => problem.startsWith(`${path} is missing`)));
	}
});

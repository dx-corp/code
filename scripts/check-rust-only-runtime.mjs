#!/usr/bin/env node

import { readdir, readFile } from "node:fs/promises";
import { dirname, extname, relative, resolve, sep } from "node:path";
import { fileURLToPath } from "node:url";

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const SCAN_ROOTS = ["."];
const TYPESCRIPT_EXTENSIONS = new Set([".ts", ".tsx", ".mts", ".cts"]);
const SOURCE_EXTENSIONS = new Set([...TYPESCRIPT_EXTENSIONS, ".js", ".jsx", ".mjs", ".cjs"]);
const SKIP_SEGMENTS = new Set(["dist", "node_modules", "target", "coverage", ".git"]);
// #9990 vendored the ZCode desktop client under desktop/. It is a TypeScript
// Electron app with its own upstream, not Maestro agent-runtime source, so the
// runtime guard does not read it. Its Agent constructions are undici connection
// dispatchers and its agent factory calls write agent config files; neither is
// a TypeScript agent runtime. Anything promoted out of desktop/ into Maestro
// source is scanned again by this guard.
const VENDORED_ROOTS = new Set(["desktop"]);

const CONTENT_RULES = [
	["Agent construction", /\bnew\s+Agent\s*\(/g],
	["provider transport construction", /\bnew\s+ProviderTransport\s*\(/g],
	["agent factory invocation", /\bcreateAgent\s*\(/g],
	["TypeScript agent fallback switch", /MAESTRO_(?:ALLOW_)?TS_AGENT/g],
	["internal TypeScript agent SDK import", /(?:from\s+|import\s*\()["']@evalops\/ai(?:\/[^"']*)?["']/g],
];

function normalized(path) {
	return path.split(sep).join("/");
}

async function collectFiles(directory) {
	const absolute = resolve(ROOT, directory);
	let entries;
	try {
		entries = await readdir(absolute, { withFileTypes: true });
	} catch (error) {
		if (error?.code === "ENOENT") return [];
		throw error;
	}

	const files = [];
	for (const entry of entries) {
		if (SKIP_SEGMENTS.has(entry.name)) continue;
		if (directory === "." && VENDORED_ROOTS.has(entry.name)) continue;
		const path = resolve(absolute, entry.name);
		if (entry.isDirectory()) {
			files.push(...(await collectFiles(relative(ROOT, path))));
		} else if (entry.isFile() && SOURCE_EXTENSIONS.has(extname(entry.name))) {
			files.push(path);
		}
	}
	return files;
}

function lineNumber(source, offset) {
	return source.slice(0, offset).split("\n").length;
}

const violations = [];
const files = (await Promise.all(SCAN_ROOTS.map(collectFiles))).flat().sort();

for (const absolute of files) {
	const path = normalized(relative(ROOT, absolute));
	if (TYPESCRIPT_EXTENSIONS.has(extname(path))) {
		violations.push({ path, line: 1, rule: "TypeScript source is not allowed" });
		continue;
	}

	const source = await readFile(absolute, "utf8");
	for (const [rule, pattern] of CONTENT_RULES) {
		pattern.lastIndex = 0;
		for (const match of source.matchAll(pattern)) {
			violations.push({ path, line: lineNumber(source, match.index), rule });
		}
	}
}

if (violations.length > 0) {
	console.error("Rust-only source guard found forbidden surfaces:");
	for (const violation of violations) {
		console.error(`- ${violation.path}:${violation.line} [${violation.rule}]`);
	}
	console.error(`\n${violations.length} violation(s). Maestro source must remain Rust-only.`);
	process.exitCode = 1;
} else {
	console.log("Rust-only source guard passed.");
}

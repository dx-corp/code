#!/usr/bin/env node

import { spawnSync } from "node:child_process";
import { existsSync, readdirSync, readFileSync, statSync } from "node:fs";
import { dirname, join, relative, resolve } from "node:path";
import process from "node:process";
import { pathToFileURL } from "node:url";

function parseTarget(argv) {
	const index = argv.indexOf("--target");
	const target = index >= 0 ? argv[index + 1] : "";
	if (!target) throw new Error("Missing required --target <path>");
	return resolve(target);
}

/**
 * Projection-specific checks on the prepared public tree before opening or
 * updating the sync PR. Public-only replacement scripts and sanitized
 * contracts exist only in this tree and must be exercised here.
 */
export function preparedPublicMirrorGuardrailCommands() {
	return [
		{
			command: "node",
			args: ["--test", "deploy/helm/maestro/tests/render.test.mjs"],
			label: "Rendered native Helm lifecycle contract",
		},
		{
			command: "npm",
			args: ["run", "check:hosted-orb-delegation"],
			label: "Hosted Computer delegation contract",
		},
		{
			command: "cargo",
			args: ["metadata", "--locked", "--no-deps", "--format-version", "1"],
			label: "Public Cargo dependency containment",
		},
		{
			command: "npm",
			args: ["run", "check:docker-runtime-workspaces"],
			label: "Standalone Docker workspace contract",
		},
		{
			command: "npm",
			args: ["run", "check:rust-only-runtime"],
			label: "Rust-only source guard",
		},
	];
}

const SCRIPT_SCAN_SKIP_DIRECTORIES = new Set([
	".git",
	"coverage",
	"dist",
	"node_modules",
	"target",
	"tmp",
]);

function collectModuleScripts(root, current = root) {
	const files = [];
	for (const entry of readdirSync(current, { withFileTypes: true })) {
		if (entry.isDirectory()) {
			if (!SCRIPT_SCAN_SKIP_DIRECTORIES.has(entry.name)) {
				files.push(...collectModuleScripts(root, join(current, entry.name)));
			}
		} else if (
			entry.isFile() &&
			// The root package declares "type": "module", so shipped .js files
			// are ES modules too and get the same syntax/reference validation.
			(entry.name.endsWith(".mjs") || entry.name.endsWith(".js"))
		) {
			files.push(join(current, entry.name));
		}
	}
	return files;
}

// Keywords after which a `/` begins a regex literal rather than division.
const REGEX_PREFIX_KEYWORDS = new Set([
	"await",
	"case",
	"delete",
	"do",
	"else",
	"in",
	"instanceof",
	"new",
	"of",
	"return",
	"typeof",
	"void",
	"yield",
]);

// Punctuators after which a `/` begins a regex literal. `}` is deliberately
// absent: it is genuinely ambiguous (function-body expression followed by
// division vs. block followed by a regex), and treating it as a regex prefix
// let `function() {} / 2` swallow all subsequent source as regex content —
// a silent false negative. Reading a rare block-adjacent regex as division
// instead only leaks inert regex text into the code stream, which fails
// closed for this guard.
const REGEX_PREFIX_CHARS = new Set([
	"",
	"(",
	",",
	"=",
	":",
	"[",
	"!",
	"&",
	"|",
	"?",
	"{",
	";",
	"+",
	"-",
	"*",
	"%",
	"<",
	">",
]);

/**
 * Tokenize module source into code and string tokens. Comments are dropped,
 * regex literals are recognized (including after expression keywords such as
 * `return`) so their contents stay inert, and string/template contents are
 * isolated as string tokens instead of raw text. Downstream, dependency
 * specifiers are read only from string tokens in import positions, so prose
 * in comments and import-shaped text inside ordinary strings can neither
 * create nor hide a dependency.
 */
function tokenizeModuleSource(source) {
	const tokens = [];
	let code = "";
	let index = 0;
	// Rolling context for the regex-vs-division decision: the previous
	// significant character and, when that character ends an identifier, the
	// full identifier (to recognize expression keywords).
	let lastChar = "";
	let lastWord = "";
	// Mode stack: code frames track brace depth so a `}` can be told apart
	// from the end of a template interpolation; template frames accumulate
	// literal content and remember whether any interpolation occurred.
	const stack = [{ type: "code", braceDepth: 0 }];

	const flushCode = () => {
		if (code) {
			tokens.push({ type: "code", text: code });
			code = "";
		}
	};
	const regexCanStart = () =>
		REGEX_PREFIX_CHARS.has(lastChar) ||
		(/[A-Za-z_$]/u.test(lastChar) && REGEX_PREFIX_KEYWORDS.has(lastWord));
	const noteCodeChar = (char) => {
		code += char;
		if (!/\s/u.test(char)) {
			lastChar = char;
			lastWord = /[A-Za-z0-9_$]/u.test(char) ? lastWord + char : "";
		}
	};

	while (index < source.length) {
		const frame = stack[stack.length - 1];
		const char = source[index];
		const next = source[index + 1];

		if (frame.type === "template") {
			if (char === "\\") {
				frame.value += char + (next ?? "");
				index += 2;
				continue;
			}
			if (char === "`") {
				// Literal-only templates are usable specifiers; interpolated
				// templates are dynamic and marked partial so they are never
				// treated as (or mistaken for) a literal dependency.
				tokens.push({
					type: "string",
					value: frame.value,
					partial: frame.hasInterpolation,
				});
				stack.pop();
				lastChar = "`";
				lastWord = "";
				index += 1;
				continue;
			}
			if (char === "$" && next === "{") {
				// The interpolation body is executable code: imports inside it
				// are real dependencies and must reach the code stream.
				frame.hasInterpolation = true;
				stack.push({ type: "code", braceDepth: 0 });
				index += 2;
				continue;
			}
			frame.value += char;
			index += 1;
			continue;
		}

		if (char === "/" && next === "/") {
			while (index < source.length && source[index] !== "\n") index += 1;
			continue;
		}
		if (char === "/" && next === "*") {
			index += 2;
			while (
				index < source.length &&
				!(source[index] === "*" && source[index + 1] === "/")
			) {
				index += 1;
			}
			index += 2;
			code += " ";
			continue;
		}
		if (char === '"' || char === "'") {
			const quote = char;
			flushCode();
			let value = "";
			index += 1;
			while (index < source.length) {
				const inner = source[index];
				index += 1;
				if (inner === "\\") {
					value += inner + (source[index] ?? "");
					index += 1;
					continue;
				}
				if (inner === quote) break;
				value += inner;
			}
			tokens.push({ type: "string", value });
			lastChar = quote;
			lastWord = "";
			continue;
		}
		if (char === "`") {
			flushCode();
			stack.push({ type: "template", value: "", hasInterpolation: false });
			index += 1;
			continue;
		}
		if (char === "/" && regexCanStart()) {
			// Regex literal: consume to the unescaped closing slash, honoring
			// character classes, so patterns containing // or /* stay inert.
			// The contents are dropped from the code stream entirely.
			index += 1;
			let inClass = false;
			while (index < source.length) {
				const inner = source[index];
				index += 1;
				if (inner === "\\") {
					index += 1;
					continue;
				}
				if (inner === "[") inClass = true;
				else if (inner === "]") inClass = false;
				else if (inner === "/" && !inClass) break;
			}
			code += " /re/ ";
			lastChar = "/";
			lastWord = "";
			continue;
		}
		if (char === "{") {
			frame.braceDepth += 1;
			noteCodeChar(char);
			index += 1;
			continue;
		}
		if (char === "}") {
			if (frame.braceDepth === 0 && stack.length > 1) {
				// End of a template interpolation: return to the template.
				flushCode();
				stack.pop();
				index += 1;
				continue;
			}
			frame.braceDepth = Math.max(0, frame.braceDepth - 1);
			noteCodeChar(char);
			index += 1;
			continue;
		}
		noteCodeChar(char);
		index += 1;
	}
	flushCode();
	return tokens;
}

// A string token is a dependency specifier only when the code immediately
// before it is an import position: `import` (side-effect), `from`, dynamic
// `import(`, or a sibling spawn via `resolve(scriptDir|__dirname,`. The
// lookbehinds exclude property access and longer identifiers, so method
// calls such as `loader.import("...")` or `Array.from("...")` are never
// treated as module dependencies.
const IMPORT_POSITION = /(?:(?<![.\w$])import\s*\(?|(?<![.\w$])from)\s*$/u;
const SIBLING_RESOLVE_POSITION =
	/\bresolve\(\s*(?:scriptDir|__dirname)\s*,\s*$/u;

function moduleDependencySpecifiers(source) {
	const specifiers = [];
	let precedingCode = "";
	for (const token of tokenizeModuleSource(source)) {
		if (token.type === "code") {
			precedingCode = `${precedingCode}${token.text}`.slice(-120);
			continue;
		}
		if (!token.partial) {
			if (IMPORT_POSITION.test(precedingCode)) {
				if (token.value.startsWith("./") || token.value.startsWith("../")) {
					specifiers.push(token.value);
				}
			} else if (
				SIBLING_RESOLVE_POSITION.test(precedingCode) &&
				/\.(?:mjs|js)$/u.test(token.value)
			) {
				specifiers.push(token.value);
			}
		}
		// The string itself is context for what follows (e.g. the comma after
		// a resolve() argument), but its contents never are.
		precedingCode = `${precedingCode}""`.slice(-120);
	}
	return specifiers;
}

/**
 * Every shipped module script must parse, and every literal reference it
 * makes to a sibling script must resolve inside the prepared tree. The
 * reference check exists because the drift checker shipped to evalops/maestro
 * while spawning scripts/prepare-public-release-mirror.mjs, which the mirror
 * deletes — syntactically valid, broken on first run (maestro#957, finding
 * 11). Only literal specifiers are checked; dynamic paths are out of scope.
 */
export function shippedScriptErrors(targetRoot) {
	const errors = [];
	for (const file of collectModuleScripts(targetRoot)) {
		const label = relative(targetRoot, file);
		const parsed = spawnSync(process.execPath, ["--check", file], {
			encoding: "utf8",
		});
		if (parsed.status !== 0) {
			errors.push(`${label}: syntax check failed: ${parsed.stderr.trim()}`);
			continue;
		}
		const referenced = new Set();
		for (const specifier of moduleDependencySpecifiers(
			readFileSync(file, "utf8"),
		)) {
			referenced.add(resolve(dirname(file), specifier));
		}
		for (const path of referenced) {
			// A reference that resolves outside the prepared tree can exist in
			// this checkout (the workflow prepares the tree inside the internal
			// workspace) yet be absent from a standalone public clone, so
			// containment is required before existence is even meaningful.
			const containedPath = relative(targetRoot, path);
			if (containedPath.startsWith("..")) {
				errors.push(
					`${label}: references ${path}, which resolves outside the prepared tree`,
				);
				continue;
			}
			if (!existsSync(path)) {
				errors.push(
					`${label}: references ${containedPath}, which does not exist in the prepared tree`,
				);
				continue;
			}
			// Node's ESM resolver rejects directory imports
			// (ERR_UNSUPPORTED_DIR_IMPORT); an existing directory is still a
			// broken reference.
			if (!statSync(path).isFile()) {
				errors.push(
					`${label}: references ${containedPath}, which is not an importable file`,
				);
			}
		}
	}
	return errors;
}

function main() {
	const targetRoot = parseTarget(process.argv.slice(2));
	if (!existsSync(resolve(targetRoot, "package.json"))) {
		throw new Error(`Prepared public mirror target is invalid: ${targetRoot}`);
	}
	console.log("\n## Shipped script syntax and sibling references");
	const scriptErrors = shippedScriptErrors(targetRoot);
	if (scriptErrors.length > 0) {
		throw new Error(
			`Shipped script check failed:\n- ${scriptErrors.join("\n- ")}`,
		);
	}
	console.log("Shipped module scripts parse and resolve their references.");
	for (const { command, args, label } of preparedPublicMirrorGuardrailCommands()) {
		console.log(`\n## ${label}`);
		const result = spawnSync(command, args, { cwd: targetRoot, stdio: "inherit" });
		if (result.error) throw result.error;
		if (result.status !== 0) throw new Error(`${label} failed with exit code ${result.status}.`);
	}
	console.log("Prepared public mirror native guardrails passed.");
}

// Run only when executed directly, so tests can import shippedScriptErrors
// without triggering the CLI entrypoint.
if (
	process.argv[1] &&
	import.meta.url === pathToFileURL(resolve(process.argv[1])).href
) {
	try {
		main();
	} catch (error) {
		console.error(error instanceof Error ? error.message : String(error));
		process.exit(1);
	}
}

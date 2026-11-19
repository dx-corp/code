import { appendFileSync, readFileSync } from "node:fs";
import { spawnSync } from "node:child_process";
import { posix, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { TextDecoder } from "node:util";

export const GIT_TIMEOUT_MS = 15_000;
export const GIT_MAX_BUFFER = 8 * 1024 * 1024;

const TARGETS = ["rust_tests", "native_release", "jetbrains_plugin"];
const SHA_RE = /^[0-9a-f]{40}$/;
const decoder = new TextDecoder("utf-8", { fatal: true });

// hand-list: these explicit packaging and smoke scripts are native-release inputs.
const NATIVE_ONLY_SCRIPTS = new Set([
	"scripts/materialize-native-package.mjs",
	"scripts/smoke-release-native-only.mjs",
	"scripts/run-runtime-conformance.mjs",
]);
const RUST_ONLY_SCRIPTS = new Set([
	"scripts/run-nextest-partition.sh",
	"scripts/smoke-tui-interactive.sh",
]);
const TARGET_SCRIPT = "scripts/run-ci-jetbrains.sh";
const KNOWN_IRRELEVANT_DOCS = new Set(["docs/CONTRIBUTOR_RUNBOOK.md"]);
// hand-list: these explicit manifests and configs are shared Rust/native inputs.
const RUST_NATIVE_FILES = new Set([
	"Cargo.toml",
	"Cargo.lock",
	"rustfmt.toml",
	"clippy.toml",
	"rust-toolchain",
	"rust-toolchain.toml",
	".node-version",
	"package.json",
	"package-lock.json",
]);
const FALLBACK_REASONS = new Set([
	"fallback_event_input",
	"fallback_invalid_sha",
	"fallback_missing_object",
	"fallback_git_error",
	"fallback_malformed_input",
	"fallback_empty_paths",
	"fallback_unknown_path",
	"fallback_workspace_manifest",
	"fallback_ambiguous_merge_base",
]);

class SelectionError extends Error {
	constructor(reason) {
		super(reason);
		this.reason = reason;
	}
}

class GitCommandError extends Error {
	constructor(exitCode = null) {
		super("git command failed");
		this.exitCode = exitCode;
	}
}

function allSelected(reason) {
	const outputs = { selection_valid: "true" };
	for (const target of TARGETS) {
		outputs[target] = "true";
		outputs[`${target}_reason`] = reason;
	}
	return outputs;
}

function selectedTargets(selected) {
	const outputs = { selection_valid: "true" };
	for (const target of TARGETS) {
		const run = selected.has(target);
		outputs[target] = run ? "true" : "false";
		outputs[`${target}_reason`] = run ? "matched_path" : "no_relevant_paths";
	}
	return outputs;
}

function validRelativePath(path) {
	return (
		typeof path === "string" &&
		path.length > 0 &&
		!path.startsWith("/") &&
		!path.includes("\\") &&
		!/[\u0000-\u001f\u007f]/u.test(path) &&
		posix.normalize(path) === path &&
		!path.split("/").some((segment) => segment === "" || segment === "." || segment === "..")
	);
}

export function parseGitDiffPaths(output) {
	if (!Buffer.isBuffer(output) || output.length === 0) {
		throw new SelectionError("fallback_empty_paths");
	}
	if (output[output.length - 1] !== 0) {
		throw new SelectionError("fallback_malformed_input");
	}

	const paths = [];
	let start = 0;
	for (let index = 0; index < output.length; index += 1) {
		if (output[index] !== 0) continue;
		if (index === start) throw new SelectionError("fallback_malformed_input");
		let path;
		try {
			path = decoder.decode(output.subarray(start, index));
		} catch {
			throw new SelectionError("fallback_malformed_input");
		}
		if (!validRelativePath(path)) throw new SelectionError("fallback_malformed_input");
		paths.push(path);
		start = index + 1;
	}
	if (start !== output.length || paths.length === 0) {
		throw new SelectionError("fallback_malformed_input");
	}
	return [...new Set(paths)].sort();
}

export function parseWorkspacePackages(manifest) {
	if (typeof manifest !== "string") throw new SelectionError("fallback_workspace_manifest");
	const header = /^\[workspace\]\s*$/m.exec(manifest);
	if (!header) throw new SelectionError("fallback_workspace_manifest");

	const remaining = manifest.slice(header.index + header[0].length);
	const nextHeader = /^\[[^\]]+\]\s*$/m.exec(remaining);
	const section = nextHeader ? remaining.slice(0, nextHeader.index) : remaining;
	const members = /^\s*members\s*=\s*\[([^\]]*)\]/m.exec(section);
	if (!members) throw new SelectionError("fallback_workspace_manifest");

	const packages = [];
	for (const line of members[1].split(/\r?\n/u)) {
		if (line.trim() === "") continue;
		const entry = /^\s*"packages\/([^/"\\]+)"\s*,?\s*(?:#.*)?$/u.exec(line);
		if (!entry) throw new SelectionError("fallback_workspace_manifest");
		packages.push(entry[1]);
	}
	if (packages.length === 0 || new Set(packages).size !== packages.length) {
		throw new SelectionError("fallback_workspace_manifest");
	}
	return new Set(packages);
}

export function classifyChangedPaths(paths, manifest) {
	if (!Array.isArray(paths)) return allSelected("fallback_malformed_input");
	if (paths.length === 0) return allSelected("fallback_empty_paths");
	if (paths.some((path) => !validRelativePath(path))) {
		return allSelected("fallback_malformed_input");
	}

	let workspacePackages;
	try {
		workspacePackages = parseWorkspacePackages(manifest);
	} catch {
		return allSelected("fallback_workspace_manifest");
	}

	const selected = new Set();
	for (const path of paths) {
		if (path === ".vscode" || path.startsWith(".vscode/")) continue;
		if (KNOWN_IRRELEVANT_DOCS.has(path)) continue;
		if (path === ".github" || path.startsWith(".github/")) {
			return allSelected("matched_path");
		}
		if (path === "proto" || path.startsWith("proto/")) {
			return allSelected("matched_path");
		}
		if (path === "vendor" || path.startsWith("vendor/")) {
			selected.add("rust_tests");
			selected.add("native_release");
			continue;
		}
		if (path === "packages/jetbrains-plugin" || path.startsWith("packages/jetbrains-plugin/")) {
			if (workspacePackages.has("jetbrains-plugin")) {
				return allSelected("fallback_workspace_manifest");
			}
			selected.add("jetbrains_plugin");
			continue;
		}
		if (path.startsWith("packages/")) {
			const packageName = path.split("/")[1];
			if (!workspacePackages.has(packageName)) {
				return allSelected("fallback_unknown_path");
			}
			selected.add("rust_tests");
			selected.add("native_release");
			continue;
		}
		if (
			RUST_NATIVE_FILES.has(path) ||
			path === ".cargo" ||
			path.startsWith(".cargo/")
		) {
			selected.add("rust_tests");
			selected.add("native_release");
			continue;
		}
		if (path === ".config/nextest.toml") {
			selected.add("rust_tests");
			continue;
		}
		if (NATIVE_ONLY_SCRIPTS.has(path)) {
			selected.add("native_release");
			continue;
		}
		if (RUST_ONLY_SCRIPTS.has(path)) {
			selected.add("rust_tests");
			continue;
		}
		if (path === TARGET_SCRIPT) {
			selected.add("jetbrains_plugin");
			continue;
		}
		return allSelected("fallback_unknown_path");
	}

	return selectedTargets(selected);
}

function runGit(args, { cwd, gitBinary, timeoutMs, maxBuffer }) {
	const result = spawnSync(gitBinary, args, {
		cwd,
		encoding: "buffer",
		timeout: timeoutMs,
		maxBuffer,
		windowsHide: true,
	});
	if (result.error || result.status !== 0) {
		throw new GitCommandError(result.error ? null : result.status);
	}
	return result.stdout;
}

function decodeGitObject(output) {
	try {
		return decoder.decode(output).trim();
	} catch {
		throw new SelectionError("fallback_git_error");
	}
}

export function getChangedPaths({
	baseSha,
	headSha,
	cwd = process.cwd(),
	gitBinary = "git",
	timeoutMs = GIT_TIMEOUT_MS,
	maxBuffer = GIT_MAX_BUFFER,
}) {
	if (!SHA_RE.test(baseSha ?? "") || !SHA_RE.test(headSha ?? "")) {
		throw new SelectionError("fallback_invalid_sha");
	}
	const options = { cwd, gitBinary, timeoutMs, maxBuffer };
	for (const sha of [baseSha, headSha]) {
		try {
			const resolved = decodeGitObject(
				runGit(["rev-parse", "--verify", "--end-of-options", `${sha}^{commit}`], options),
			);
			if (resolved !== sha) throw new SelectionError("fallback_missing_object");
		} catch (error) {
			if (error instanceof SelectionError) throw error;
			if (error instanceof GitCommandError && error.exitCode !== null) {
				throw new SelectionError("fallback_missing_object");
			}
			throw new SelectionError("fallback_git_error");
		}
	}

	let mergeBase;
	try {
		const mergeBaseOutput = decodeGitObject(
			runGit(["merge-base", "--all", baseSha, headSha], options),
		);
		const mergeBases = mergeBaseOutput.split(/\r?\n/u);
		if (mergeBases.length !== 1) {
			throw new SelectionError("fallback_ambiguous_merge_base");
		}
		[mergeBase] = mergeBases;
	} catch (error) {
		if (error instanceof SelectionError) throw error;
		throw new SelectionError("fallback_git_error");
	}
	if (!SHA_RE.test(mergeBase)) throw new SelectionError("fallback_git_error");

	let diff;
	try {
		diff = runGit(
			["diff", "--no-renames", "--name-only", "-z", mergeBase, headSha],
			options,
		);
	} catch {
		throw new SelectionError("fallback_git_error");
	}
	return parseGitDiffPaths(diff);
}

export function selectForEvent(eventName, event, options = {}) {
	if (typeof eventName !== "string" || eventName.length === 0) {
		return allSelected("fallback_event_input");
	}
	if (eventName !== "pull_request") return allSelected("non_pr");

	const baseSha = event?.pull_request?.base?.sha;
	const headSha = event?.pull_request?.head?.sha;
	if (typeof baseSha !== "string" || typeof headSha !== "string") {
		return allSelected("fallback_event_input");
	}

	let paths;
	try {
		paths = getChangedPaths({ baseSha, headSha, ...options });
	} catch (error) {
		return allSelected(
			FALLBACK_REASONS.has(error?.reason) ? error.reason : "fallback_git_error",
		);
	}

	let manifest;
	try {
		manifest = readFileSync(resolve(options.cwd ?? process.cwd(), "Cargo.toml"), "utf8");
	} catch {
		return allSelected("fallback_workspace_manifest");
	}
	return classifyChangedPaths(paths, manifest);
}

function outputLines(outputs) {
	return [
		`selection_valid=${outputs.selection_valid}`,
		...TARGETS.flatMap((target) => [
			`${target}=${outputs[target]}`,
			`${target}_reason=${outputs[`${target}_reason`]}`,
		]),
	].join("\n");
}

export function main(env = process.env) {
	if (typeof env.GITHUB_OUTPUT !== "string" || env.GITHUB_OUTPUT.length === 0) {
		throw new Error("GITHUB_OUTPUT is required");
	}

	let outputs;
	if (typeof env.GITHUB_EVENT_NAME !== "string" || env.GITHUB_EVENT_NAME.length === 0) {
		outputs = allSelected("fallback_event_input");
	} else if (env.GITHUB_EVENT_NAME !== "pull_request") {
		outputs = allSelected("non_pr");
	} else {
		let event;
		try {
			event = JSON.parse(readFileSync(env.GITHUB_EVENT_PATH, "utf8"));
		} catch {
			outputs = allSelected("fallback_event_input");
		}
		if (event !== undefined) {
			outputs = selectForEvent(env.GITHUB_EVENT_NAME, event, { cwd: process.cwd() });
		}
	}

	if (Object.values(outputs).some((value) => value.startsWith("fallback_"))) {
		process.stderr.write("Maestro CI path selection is uncertain; selecting all target jobs.\n");
	}
	appendFileSync(env.GITHUB_OUTPUT, `${outputLines(outputs)}\n`, "utf8");
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
	main();
}

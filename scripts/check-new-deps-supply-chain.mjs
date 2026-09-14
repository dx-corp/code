#!/usr/bin/env node

/**
 * PR-time cargo-deny gate, scoped to newly introduced dependencies.
 *
 * `cargo deny check` (advisories/licenses/bans/sources) runs on every PR and
 * its full human-readable output always goes into the job summary, but a
 * fresh RUSTSEC advisory landing against a dependency this PR never touched
 * should not spuriously red an unrelated PR — that's the same "gate that
 * fails for the wrong reason" problem this repo is trying to get away from.
 * The CI supply-chain lane (`scripts/run-ci-supply-chain.sh`)
 * is where advisories against the existing tree get enforced unconditionally.
 *
 * This script narrows PR-time enforcement to findings whose *own* flagged
 * crate@version is new in this PR's Cargo.lock relative to the PR's base —
 * i.e. a dependency this PR added, bumped into a bad version, or re-pointed
 * to a different (git / alternate-registry) source at the same version.
 * Pre-existing findings against untouched dependencies
 * are reported (present in the job summary) but do not fail this job.
 *
 * Input: a JSON-lines diagnostics stream from `cargo deny -f json check`
 * (cargo-deny writes structured diagnostics to stderr; the caller redirects
 * that to a file and passes it via --report). Only `severity: "error"`
 * diagnostics can fail anything — cargo-deny already demotes ignored
 * advisories (see deny.toml) to a lower severity, so an intentionally
 * accepted exception never trips this gate either.
 */

import { readFileSync } from "node:fs";

const LOCKFILE_PACKAGE_RE =
	/^\[\[package\]\]\nname = "([^"]+)"\nversion = "([^"]+)"(?:\nsource = "([^"]+)")?/gm;

function packageIdentity(name, version, source) {
	return source ? `${name}@${version} (${source})` : `${name}@${version}`;
}

function parseArgs(argv) {
	const args = {
		report: null,
		baseLockfile: null,
		headLockfile: "Cargo.lock",
		dependencyInputChanged: false,
		failOnPreexisting: false,
	};
	for (let index = 0; index < argv.length; index += 1) {
		const arg = argv[index];
		switch (arg) {
			case "--report":
				args.report = argv[++index] ?? args.report;
				break;
			case "--base-lockfile":
				args.baseLockfile = argv[++index] ?? args.baseLockfile;
				break;
			case "--head-lockfile":
				args.headLockfile = argv[++index] ?? args.headLockfile;
				break;
			case "--dependency-input-changed":
				args.dependencyInputChanged = true;
				break;
			case "--fail-on-preexisting":
				args.failOnPreexisting = true;
				break;
			default:
				throw new Error(`Unknown argument: ${arg}`);
		}
	}
	if (!args.report) throw new Error("--report <path> is required");
	if (!args.baseLockfile) throw new Error("--base-lockfile <path> is required");
	return args;
}

/**
 * Parses a Cargo.lock's `[[package]]` stanzas into a Set of package
 * identities. The lockfile `source` is part of the identity — crates.io
 * packages carry ` (registry+...)`, git/alternate-registry packages their
 * own source, and source-less workspace/path packages keep bare
 * `name@version` — so re-pointing an existing crate to a git or alternate
 * registry at the SAME version still reads as a changed dependency instead
 * of bypassing the source-provenance gate.
 */
export function lockfilePackageSet(lockfileText) {
	const packages = new Set();
	for (const match of lockfileText.matchAll(LOCKFILE_PACKAGE_RE)) {
		const source = match[3];
		packages.add(
			source ? `${match[1]}@${match[2]} (${source})` : `${match[1]}@${match[2]}`,
		);
	}
	return packages;
}

function lockfileGraph(lockfileText) {
	const packages = [];
	for (const stanza of lockfileText.split(/^\[\[package\]\]\n/m).slice(1)) {
		const name = stanza.match(/^name = "([^"]+)"/m)?.[1];
		const version = stanza.match(/^version = "([^"]+)"/m)?.[1];
		if (!name || !version) continue;
		const source = stanza.match(/^source = "([^"]+)"/m)?.[1];
		const dependencies = stanza
			.match(/^dependencies = \[\n([\s\S]*?)^\]/m)?.[1]
			?.split("\n")
			.map((line) => line.trim().match(/^"(.+)",?$/)?.[1])
			.filter(Boolean) ?? [];
		packages.push({
			name,
			version,
			source,
			identity: packageIdentity(name, version, source),
			dependencies,
		});
	}

	const byName = new Map();
	for (const pkg of packages) {
		const entries = byName.get(pkg.name) ?? [];
		entries.push(pkg);
		byName.set(pkg.name, entries);
	}

	const edges = new Map();
	for (const pkg of packages) {
		for (const dependency of pkg.dependencies) {
			const parsed = dependency.match(/^(\S+)(?: (\S+)(?: \((.+)\))?)?$/);
			if (!parsed) continue;
			const [, name, version, sourceWithParen] = parsed;
			const candidates = byName.get(name) ?? [];
			const target =
				candidates.find(
					(candidate) =>
						(!version || candidate.version === version) &&
						(!sourceWithParen || candidate.source === sourceWithParen),
				) ?? (candidates.length === 1 ? candidates[0] : null);
			if (target) edges.set(`${pkg.identity} -> ${target.identity}`, target.identity);
		}
	}
	return edges;
}

/** Existing package identities reached by dependency edges added in `head`. */
export function newDependencyEdgeTargets(baseLockfileText, headLockfileText) {
	const baseEdges = lockfileGraph(baseLockfileText);
	const headEdges = lockfileGraph(headLockfileText);
	const targets = new Set();
	for (const [edge, target] of headEdges) {
		if (!baseEdges.has(edge)) targets.add(target);
	}
	return targets;
}

/** Packages present in `head` but not in `base` (added, bumped, or re-sourced). */
export function newPackages(baseSet, headSet) {
	const added = new Set();
	for (const pkg of headSet) {
		if (!baseSet.has(pkg)) added.add(pkg);
	}
	return added;
}

/**
 * Parses cargo-deny's `-f json check` diagnostics (JSON-lines on stderr).
 * Returns only error-severity diagnostics, each reduced to its message and
 * the top-level crate(s) the finding is actually about.
 */
export function parseDenyDiagnostics(reportText) {
	const findings = [];
	for (const line of reportText.split("\n")) {
		const trimmed = line.trim();
		if (!trimmed) continue;
		let obj;
		try {
			obj = JSON.parse(trimmed);
		} catch {
			continue; // tolerate stray non-JSON lines (e.g. advisory-db fetch logs)
		}
		if (obj.type !== "diagnostic") continue;
		const fields = obj.fields ?? {};
		if (fields.severity !== "error") continue;
		const graphs = Array.isArray(fields.graphs) ? fields.graphs : [];
		const crates = graphs
			.map((graph) => graph?.Krate)
			.filter(Boolean)
			.map((krate) => `${krate.name}@${krate.version}`);
		findings.push({ code: fields.code, message: fields.message, crates });
	}
	return findings;
}

/**
 * Whether every nonblank line is valid JSON and the report contains the
 * crate-scoped error expected from a findings exit.
 */
export function isCompleteErrorDenyReport(reportText) {
	let hasCrateError = false;
	for (const line of reportText.split("\n")) {
		const trimmed = line.trim();
		if (!trimmed) continue;
		let obj;
		try {
			obj = JSON.parse(trimmed);
		} catch {
			return false;
		}
		if (obj.type !== "diagnostic") continue;
		const fields = obj.fields ?? {};
		if (fields.severity !== "error") continue;
		const graphs = Array.isArray(fields.graphs) ? fields.graphs : [];
		if (graphs.some((graph) => graph?.Krate?.name && graph?.Krate?.version)) {
			hasCrateError = true;
		}
	}
	return hasCrateError;
}

/**
 * Findings whose flagged crate is in `newSet`. New-set identities may carry
 * a lockfile source suffix (`name@version (source)`), while cargo-deny
 * reports crates as bare `name@version`, so a finding matches when it equals
 * an identity or is its name@version stem.
 */
export function newDependencyFindings(findings, newSet) {
	const isNew = (crate) =>
		[...newSet].some((pkg) => pkg === crate || pkg.startsWith(`${crate} (`));
	return findings.filter((finding) => finding.crates.some(isNew));
}

/** Findings that do not touch a package or edge newly introduced by the PR. */
export function preexistingDependencyFindings(findings, newSet) {
	return findings.filter(
		(finding) => newDependencyFindings([finding], newSet).length === 0,
	);
}

/**
 * Cargo.lock includes optional edges even when their feature is inactive.
 * Cargo.toml and release-build command changes can therefore expand shipped
 * code without changing the lockfile graph. Fail closed when either input
 * changes by treating every current finding as in-scope; lockfile-only changes
 * retain package/edge scoping.
 */
export function scopedDependencyFindings(
	findings,
	changedSet,
	dependencyInputChanged,
) {
	return dependencyInputChanged
		? findings
		: newDependencyFindings(findings, changedSet);
}

export function basePolicyBlocksPreexisting(preexistingCount) {
	return preexistingCount > 0;
}

function main() {
	const args = parseArgs(process.argv.slice(2));

	const reportText = readFileSync(args.report, "utf8");
	const baseLockText = readFileSync(args.baseLockfile, "utf8");
	const headLockText = readFileSync(args.headLockfile, "utf8");

	const baseSet = lockfilePackageSet(baseLockText);
	const headSet = lockfilePackageSet(headLockText);
	const added = newPackages(baseSet, headSet);
	const addedEdgeTargets = newDependencyEdgeTargets(baseLockText, headLockText);
	const changed = new Set([...added, ...addedEdgeTargets]);

	const findings = parseDenyDiagnostics(reportText);
	const blocking = scopedDependencyFindings(
		findings,
		changed,
		args.dependencyInputChanged,
	);
	const preexistingFindings = preexistingDependencyFindings(findings, changed);
	const preexisting = preexistingFindings.length;

	console.log(
		`cargo-deny: ${findings.length} error-severity finding(s), ${added.size} new/changed package identity(s), and ${addedEdgeTargets.size} target(s) of new dependency edges in this PR.`,
	);

	if (args.dependencyInputChanged && findings.length > 0) {
		console.log(
			"A dependency activation input changed; all findings are in scope because manifest or release-build feature changes can expand the effective dependency graph without changing Cargo.lock.",
		);
	}

	if (preexisting > 0) {
		console.log(
			`${preexisting} finding(s) are against dependencies this PR did not add or change — not failing this PR lane. ` +
				"They are enforced by the full CI supply-chain check and visible in this job's output.",
		);
	}

	if (args.failOnPreexisting) {
		if (basePolicyBlocksPreexisting(preexisting)) {
			console.error(
				`${preexisting} cargo-deny error(s) affect dependencies this PR did not add or change; the policy change would suppress an existing finding.`,
			);
			process.exitCode = 1;
		} else {
			console.log(
				"Base-policy findings are limited to dependencies introduced or newly reached by this PR; approved policy change is scoped to those dependencies.",
			);
		}
		return;
	}

	if (blocking.length === 0) {
		console.log("No new/changed dependency in this PR triggers a cargo-deny error. OK.");
		return;
	}

	console.error(
		`${blocking.length} cargo-deny error(s) implicate a dependency added or changed by this PR:`,
	);
	for (const finding of blocking) {
		console.error(`  [${finding.code}] ${finding.message} (${finding.crates.join(", ")})`);
	}
	process.exitCode = 1;
}

if (import.meta.url === `file://${process.argv[1]}`) {
	if (process.argv[2] === "--validate-report" && process.argv.length === 4) {
		const reportText = readFileSync(process.argv[3], "utf8");
		if (!isCompleteErrorDenyReport(reportText)) {
			console.error(
				"cargo-deny report is malformed or contains no crate-scoped error diagnostic",
			);
			process.exitCode = 1;
		}
	} else {
		main();
	}
}

#!/usr/bin/env node
// @ts-check

import { execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
import { existsSync, readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";

const DEFAULT_MAX_ITEMS_PER_SECTION = 12;
const SECTION_ORDER = ["Added", "Changed", "Fixed"];
const RELEASE_NOTE_MARKER_PATTERN =
	/<!--\s*maestro-release-note:([a-f0-9]{12})\s*-->/i;

const MIRROR_SYNC_SUBJECTS = [
	/^chore:\s+sync public mirror from internal\b/i,
	/^chore:\s+sync release mirror from internal\b/i,
	/^release v\d+\.\d+\.\d+/i,
];

/**
 * @param {string} subject
 */
export function shouldSkipReleaseNoteSubject(subject) {
	const normalized = subject.trim();
	if (!normalized) {
		return true;
	}
	return MIRROR_SYNC_SUBJECTS.some((pattern) => pattern.test(normalized));
}

/**
 * @param {string} subject
 */
export function classifyReleaseNoteSubject(subject) {
	const normalized = subject.trim();
	const conventionalMatch = normalized.match(
		/^([a-z]+)(?:\([^)]+\))?!?:\s+(.+)$/i,
	);
	const type = conventionalMatch?.[1]?.toLowerCase();

	if (type === "feat") {
		return "Added";
	}
	if (type === "fix" || type === "revert") {
		return "Fixed";
	}
	return "Changed";
}

/**
 * @param {string} subject
 */
export function formatReleaseNoteSubject(subject) {
	const withoutConventionalPrefix = subject
		.trim()
		.replace(/^[a-z]+(?:\([^)]+\))?!?:\s+/i, "");
	const withoutTrailingPeriod = withoutConventionalPrefix.replace(/[.]\s*$/, "");
	if (!withoutTrailingPeriod) {
		return "";
	}
	return `${withoutTrailingPeriod[0]?.toUpperCase() ?? ""}${withoutTrailingPeriod.slice(1)}.`;
}

/**
 * @param {string} subject
 */
function getReleaseNoteMarker(subject) {
	return createHash("sha256").update(subject.trim()).digest("hex").slice(0, 12);
}

/**
 * @param {string} note
 * @param {string} subject
 */
function appendReleaseNoteMarker(note, subject) {
	return `${note} <!-- maestro-release-note:${getReleaseNoteMarker(subject)} -->`;
}

/**
 * @param {string} line
 */
function extractReleaseNoteMarker(line) {
	return line.match(RELEASE_NOTE_MARKER_PATTERN)?.[1] ?? "";
}

/**
 * @param {string} line
 */
function stripReleaseNoteMarker(line) {
	return line.replace(RELEASE_NOTE_MARKER_PATTERN, "").trim();
}

/**
 * @param {string[]} subjects
 * @param {{ includeMarkers?: boolean; maxItemsPerSection?: number }} [options]
 */
export function groupReleaseNotes(subjects, options = {}) {
	const includeMarkers = options.includeMarkers ?? false;
	const maxItemsPerSection =
		options.maxItemsPerSection ?? DEFAULT_MAX_ITEMS_PER_SECTION;
	/** @type {Record<string, string[]>} */
	const grouped = {
		Added: [],
		Changed: [],
		Fixed: [],
	};
	const seen = new Set();

	for (const subject of subjects) {
		if (shouldSkipReleaseNoteSubject(subject)) {
			continue;
		}
		const note = formatReleaseNoteSubject(subject);
		if (!note || seen.has(note)) {
			continue;
		}
		seen.add(note);
		const section = classifyReleaseNoteSubject(subject);
		if (grouped[section].length < maxItemsPerSection) {
			grouped[section].push(
				includeMarkers ? appendReleaseNoteMarker(note, subject) : note,
			);
		}
	}

	if (Object.values(grouped).every((notes) => notes.length === 0)) {
		grouped.Changed.push(
			"Maintenance release with repository, CI, or documentation updates since the previous tag.",
		);
	}

	return grouped;
}

/**
 * @param {{ version: string; date?: string; subjects: string[]; maxItemsPerSection?: number }} options
 */
export function buildChangelogEntry(options) {
	const date = options.date ?? new Date().toISOString().slice(0, 10);
	const grouped = groupReleaseNotes(options.subjects, {
		includeMarkers: true,
		maxItemsPerSection: options.maxItemsPerSection,
	});
	const lines = [`## [${options.version}] - ${date}`];

	for (const section of SECTION_ORDER) {
		const notes = grouped[section] ?? [];
		if (notes.length === 0) {
			continue;
		}
		lines.push("", `### ${section}`, "", ...notes.map((note) => `- ${note}`));
	}

	return `${lines.join("\n")}\n`;
}

/**
 * @param {string} content
 * @param {string} entry
 */
export function insertChangelogEntry(content, entry) {
	const versionHeading = content.match(/^##\s+\[?\d+\.\d+\.\d+\]?/m);
	if (!versionHeading || versionHeading.index === undefined) {
		return `${content.replace(/\s*$/u, "")}\n\n${entry.trim()}\n`;
	}

	const before = content.slice(0, versionHeading.index).replace(/\s*$/u, "");
	const after = content.slice(versionHeading.index).replace(/^\s*/u, "");
	return `${before}\n\n${entry.trim()}\n\n${after}`;
}

/**
 * @param {string} content
 * @param {string} version
 * @param {string} entry
 */
export function replaceOrInsertChangelogEntry(content, version, entry) {
	const existing = extractChangelogEntry(content, version);
	if (!existing) {
		return insertChangelogEntry(content, entry);
	}
	return content.replace(existing, entry.trim());
}

/**
 * @param {string} entry
 */
function parseChangelogEntrySections(entry) {
	const lines = entry.trim().split("\n");
	const heading = lines[0] ?? "";
	/** @type {Record<string, string[]>} */
	const sections = {};
	/** @type {string[]} */
	const intro = [];
	let currentSection = "";

	for (const line of lines.slice(1)) {
		const sectionMatch = line.match(/^###\s+(.+?)\s*$/);
		if (sectionMatch) {
			currentSection = sectionMatch[1] ?? "";
			sections[currentSection] ??= [];
			continue;
		}
		if (!currentSection) {
			if (line.trim()) {
				intro.push(line);
			}
			continue;
		}
		if (currentSection && line.trim()) {
			sections[currentSection].push(line);
		}
	}

	return { heading, intro, sections };
}

/**
 * @param {string} line
 */
function getSectionLineIdentityKeys(line) {
	const marker = extractReleaseNoteMarker(line);
	const visibleLine = stripReleaseNoteMarker(line);
	const keys = [];
	if (marker) {
		keys.push(`marker:${marker}`);
	}
	if (visibleLine) {
		keys.push(`visible:${visibleLine}`);
	}
	return keys;
}

/**
 * @param {Record<string, string[]>} sections
 */
function getExistingSectionLineKeys(sections) {
	const keys = new Set();
	for (const lines of Object.values(sections)) {
		for (const line of lines) {
			for (const key of getSectionLineIdentityKeys(line)) {
				keys.add(key);
			}
		}
	}
	return keys;
}

/**
 * @param {Record<string, string[]>} sections
 * @param {string} section
 * @param {string} line
 * @param {Set<string>} existingKeys
 */
function pushUniqueSectionLine(sections, section, line, existingKeys) {
	sections[section] ??= [];
	const lineKeys = getSectionLineIdentityKeys(line);
	if (lineKeys.some((key) => existingKeys.has(key))) {
		return;
	}
	sections[section].push(line);
	for (const key of lineKeys) {
		existingKeys.add(key);
	}
}

/**
 * @param {string} heading
 * @param {{ intro?: string[]; sections: Record<string, string[]> }} parsed
 */
function formatChangelogEntryFromSections(heading, parsed) {
	const lines = [heading];
	if (parsed.intro && parsed.intro.length > 0) {
		lines.push("", ...parsed.intro);
	}
	const orderedSections = [
		...SECTION_ORDER,
		...Object.keys(parsed.sections).filter(
			(section) => !SECTION_ORDER.includes(section),
		),
	];

	for (const section of orderedSections) {
		const entries = parsed.sections[section] ?? [];
		if (entries.length === 0) {
			continue;
		}
		lines.push("", `### ${section}`, "", ...entries);
	}

	return lines.join("\n");
}

/**
 * @param {string} content
 * @param {string} version
 * @param {string} entry
 */
export function mergeOrInsertChangelogEntry(content, version, entry) {
	const existing = extractChangelogEntry(content, version);
	if (!existing) {
		return insertChangelogEntry(content, entry);
	}

	const existingEntry = parseChangelogEntrySections(existing);
	const generatedEntry = parseChangelogEntrySections(entry);
	const mergedSections = { ...existingEntry.sections };
	const existingLineKeys = getExistingSectionLineKeys(mergedSections);

	for (const section of Object.keys(generatedEntry.sections)) {
		for (const line of generatedEntry.sections[section] ?? []) {
			pushUniqueSectionLine(mergedSections, section, line, existingLineKeys);
		}
	}

	const mergedEntry = formatChangelogEntryFromSections(
		existingEntry.heading || generatedEntry.heading,
		{ intro: existingEntry.intro, sections: mergedSections },
	);

	return content.replace(existing, mergedEntry);
}

/**
 * @param {string} content
 * @param {string} version
 */
export function extractChangelogEntry(content, version) {
	const escapedVersion = version.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
	const startPattern = new RegExp(
		`^##\\s+\\[?${escapedVersion}\\]?(?:\\s+-\\s+[^\\n]+)?\\s*$`,
		"m",
	);
	const start = content.match(startPattern);
	if (!start || start.index === undefined) {
		return "";
	}
	const rest = content.slice(start.index);
	const nextHeading = rest.slice(start[0].length).match(/^##\s+/m);
	if (!nextHeading || nextHeading.index === undefined) {
		return rest.trim();
	}
	return rest.slice(0, start[0].length + nextHeading.index).trim();
}

export function getLatestReachableReleaseTag(ref = "HEAD") {
	try {
		return execFileSync(
			"git",
			["describe", "--tags", "--abbrev=0", "--match", "v[0-9]*", ref],
			{ encoding: "utf8", stdio: ["ignore", "pipe", "ignore"] },
		).trim();
	} catch {
		return "";
	}
}

/**
 * Collect commits that touch the current workspace directory.
 * In a standalone repository this includes the whole repository.
 * @param {string} [fromRef]
 * @param {string} [toRef]
 */
export function collectReleaseNoteSubjects(
	fromRef = getLatestReachableReleaseTag(),
	toRef = "HEAD",
) {
	const range = fromRef ? `${fromRef}..${toRef}` : toRef;
	const output = execFileSync("git", ["log", "--no-merges", "--format=%s", range, "--", "."], {
		encoding: "utf8",
	});
	return output
		.split("\n")
		.map((line) => line.trim())
		.filter(Boolean);
}

/**
 * @param {string} version
 * @param {{ toRef?: string }} [options]
 */
export function buildChangelogEntryFromGit(version, options = {}) {
	const toRef = options.toRef ?? "HEAD";
	let subjects = [];
	try {
		subjects = collectReleaseNoteSubjects(
			getLatestReachableReleaseTag(toRef),
			toRef,
		);
	} catch (error) {
		console.warn(
			`Unable to collect release notes from git: ${
				error instanceof Error ? error.message : String(error)
			}`,
		);
	}
	return buildChangelogEntry({ version, subjects });
}

function parseCliArgs(argv) {
	const args = {
		command: argv[0] ?? "",
		version: "",
		path: "CHANGELOG.md",
	};
	for (let index = 1; index < argv.length; index += 1) {
		const arg = argv[index];
		switch (arg) {
			case "--version":
				args.version = argv[++index] ?? "";
				break;
			case "--path":
				args.path = argv[++index] ?? args.path;
				break;
			default:
				throw new Error(`Unknown argument: ${arg}`);
		}
	}
	return args;
}

if (process.argv[1] && fileURLToPath(import.meta.url) === process.argv[1]) {
	const args = parseCliArgs(process.argv.slice(2));
	if (args.command !== "latest-entry") {
		console.error("Usage: node scripts/release-notes.js latest-entry --version <semver>");
		process.exit(1);
	}
	if (!args.version) {
		console.error("Missing required --version <semver>.");
		process.exit(1);
	}
	if (!existsSync(args.path)) {
		console.error(`Missing changelog: ${args.path}`);
		process.exit(1);
	}
	const content = readFileSync(args.path, "utf8");
	const entry = extractChangelogEntry(content, args.version);
	if (!entry) {
		console.error(`No changelog entry found for ${args.version}.`);
		process.exit(1);
	}
	process.stdout.write(`${entry}\n`);
}

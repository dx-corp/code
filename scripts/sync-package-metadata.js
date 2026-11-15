#!/usr/bin/env node
// @ts-check

import { existsSync, readFileSync, writeFileSync } from "node:fs";
import { getGlobalInstallCommand, getPackageMetadata } from "./package-metadata.js";

const checkOnly = process.argv.includes("--check");
const { name, cliCommand, canonicalPackageName } = getPackageMetadata();
const npmInstall = getGlobalInstallCommand("npm");
const serverCommand = `${cliCommand} serve --port 8080`;
const publishedPackageSummary =
	name === canonicalPackageName
		? `- The release workflow currently publishes \`${name}\`.`
		: `- The release source projects \`${canonicalPackageName}\` as canonical and retains \`${name}\` as the compatibility alias.`;
const internalPublishedPackageSummary =
	name === canonicalPackageName
		? `- The public npm package currently resolves to \`${name}\`.`
		: `- The canonical public npm package is \`${canonicalPackageName}\`; \`${name}\` remains the compatibility package.`;

/**
 * @param {string} content
 * @param {RegExp} pattern
 * @param {string} replacement
 * @param {string} description
 */
function replaceRequired(content, pattern, replacement, description) {
	if (!pattern.test(content)) {
		throw new Error(`Could not find ${description}`);
	}
	return content.replace(pattern, replacement);
}

const targets = [
	{
		path: "packages/jetbrains-plugin/README.md",
		transform(content) {
			let next = replaceRequired(
				content,
				/^npm install -g\s+.+$/m,
				npmInstall,
				"JetBrains README npm install command",
			);
			next = replaceRequired(
				next,
				/^(?:composer|maestro|deixic-code) (?:web|serve)(?: --port \d+)?$/m,
				serverCommand,
				"JetBrains README server command",
			);
			return next;
		},
	},
	{
		path: "packages/jetbrains-plugin/src/main/resources/META-INF/plugin.xml",
		transform(content) {
			let next = replaceRequired(
				content,
				/<code>npm install -g [^<]+<\/code>/,
				`<code>${npmInstall}</code>`,
				"JetBrains plugin XML install command",
			);
			next = replaceRequired(
				next,
				/<code>(?:composer|maestro|deixic-code) (?:web|serve)(?: --port \d+)?<\/code>/,
				`<code>${serverCommand}</code>`,
				"JetBrains plugin XML server command",
			);
			return next;
		},
	},
	{
		path: "SECURITY.md",
		transform(content) {
			return replaceRequired(
				content,
				/^- `[^`]+` and all `@evalops\/\*` packages$/m,
				`- \`${name}\` and all \`@evalops/*\` packages`,
				"Security policy package scope",
			);
		},
	},
	{
		path: "docs/release-ops.md",
		transform(content) {
			if (
				content.includes("The internal repo does not publish npm packages.") ||
				content.includes("The private tree does not publish npm packages.")
			) {
				return replaceRequired(
					content,
					/- The public repo owns npm publishing and trusted publishing setup\.\n(?:- (?:The public|The canonical)[^\n]+npm package[^\n]*\n)?/,
					`- The public repo owns npm publishing and trusted publishing setup.\n${internalPublishedPackageSummary}\n`,
					"Internal release ops package summary",
				);
			}

			return replaceRequired(
				content,
				/- The release workflow (?:publishes|currently publishes) `[^`]+`(?: through npm trusted publishing)?(?:; the cutover target is `[^`]+`)?\.$/m,
				publishedPackageSummary,
				"Release ops package summary",
			);
		},
	},
];

const changedFiles = [];

for (const target of targets) {
	if (!existsSync(target.path)) {
		continue;
	}
	const current = readFileSync(target.path, "utf-8");
	const next = target.transform(current);

	if (next === current) {
		continue;
	}

	changedFiles.push(target.path);
	if (!checkOnly) {
		writeFileSync(target.path, next);
	}
}

if (checkOnly) {
	if (changedFiles.length > 0) {
		console.error("Package metadata is out of sync:");
		for (const file of changedFiles) {
			console.error(`- ${file}`);
		}
		process.exit(1);
	}
	console.log("Package metadata is in sync.");
} else if (changedFiles.length > 0) {
	console.log(`Synced package metadata in ${changedFiles.length} files.`);
} else {
	console.log("Package metadata already in sync.");
}

#!/usr/bin/env node

/**
 * Derive the desktop built-in provider's Claude model rules from the bundled
 * Rust catalog.
 *
 * `desktop/config/provider/zcode-builtin.json` stated Claude context windows
 * and output ceilings with three hand-written `modelMatch` regexes covering
 * only the `-5` generation plus `claude-haiku-4-5`. Everything else fell
 * through to the catch-all `.*` rule at 200,000 tokens, so 8 of the 15
 * Anthropic models in the catalog disagreed with it: Opus 4.6, 4.7 and 4.8 were
 * capped at 200,000 context and 32,000 output where the catalog says 1,000,000
 * and 128,000, and Opus 4.7 and 4.8 were offered no `xhigh` effort despite
 * advertising it.
 *
 * Rules are now generated from `model_catalog_data.json`, itself regenerated
 * daily from models.dev by `fetch-model-catalog.mjs`. A new Anthropic model
 * arrives with the right window, output ceiling, modalities and effort ladder,
 * with nothing to hand-edit.
 *
 * Every value comes from a catalog row. Nothing is synthesized: the dotted
 * OpenRouter spelling is not derived from the direct id, it is taken from the
 * `anthropic/...` rows the catalog already carries.
 *
 * Usage:
 *   node scripts/sync-desktop-claude-model-rules.mjs            # write
 *   node scripts/sync-desktop-claude-model-rules.mjs --check    # fail on drift
 */

import { readFileSync, writeFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const CATALOG = resolve(ROOT, "packages/local-host-rs/src/model_catalog_data.json");
const CONFIG = resolve(ROOT, "desktop/config/provider/zcode-builtin.json");

const isClaudeRule = (rule) => String(rule?.modelMatch ?? "").includes("claude");

/** Accept the suffixes the config already allows after an id: dated snapshots,
 *  `[1m]` variants, and provider or version separators. */
function modelMatch(id) {
	return `.*${id.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")}(?:[.\\-:/\\[].*)?`;
}

function ruleFor(model) {
	const caps = model.capabilities ?? {};
	const modalities = Array.isArray(model.input_modalities) ? model.input_modalities : [];
	const efforts = Array.isArray(model.reasoning_efforts) ? model.reasoning_efforts : [];

	const config = {
		properties: {
			contextWindow: caps.context_tokens,
			inputFormat: {
				supportsImage: modalities.includes("image"),
				supportsVideo: modalities.includes("video"),
				supportsAudio: modalities.includes("audio"),
				supportsPdf: modalities.includes("pdf"),
			},
			supportsJsonSchemaOutput: true,
		},
	};

	const optionSpecs = {};
	if (efforts.length > 0) {
		optionSpecs.reasoningLevel = { values: efforts };
	}
	if (typeof caps.output_tokens === "number") {
		optionSpecs.maxOutputTokens = { max: caps.output_tokens };
	}
	if (Object.keys(optionSpecs).length > 0) {
		config.optionSpecs = optionSpecs;
	}
	return { modelMatch: modelMatch(model.id), config };
}

function claudeRows(catalog) {
	return catalog.models
		.filter(
			(model) =>
				model.id.includes("claude") &&
				(model.provider === "anthropic" ||
					(model.provider === "openrouter" && model.id.startsWith("anthropic/"))),
		)
		// Shortest id first: the resolver applies later matches over earlier
		// ones, so a more specific id must sit after the family it extends.
		.sort((left, right) => left.id.length - right.id.length || left.id.localeCompare(right.id));
}

function syncedRules(catalog, existing) {
	const generated = claudeRows(catalog).map(ruleFor);
	const at = existing.findIndex(isClaudeRule);
	const kept = existing.filter((rule) => !isClaudeRule(rule));
	// Splice in at the original position so precedence against neighbouring
	// hand-authored rules is unchanged.
	const index = at === -1 ? kept.length : at;
	return [...kept.slice(0, index), ...generated, ...kept.slice(index)];
}

function main() {
	const check = process.argv.includes("--check");
	const catalog = JSON.parse(readFileSync(CATALOG, "utf8"));
	const raw = readFileSync(CONFIG, "utf8");
	const config = JSON.parse(raw);

	const rules = config.config.modelConfigRules.modelRules;
	const next = syncedRules(catalog, rules);
	const changed = JSON.stringify(rules) !== JSON.stringify(next);
	config.config.modelConfigRules.modelRules = next;
	if (changed && !check) {
		config.revision += 1;
	}
	const serialized = `${JSON.stringify(config, null, 2)}\n`;

	if (check) {
		if (changed) {
			console.error(
				`${CONFIG} Claude model rules are out of date with the bundled catalog;\nrun: node scripts/sync-desktop-claude-model-rules.mjs`,
			);
			process.exit(1);
		}
		console.log(`Desktop Claude model rules are current (${next.filter(isClaudeRule).length}).`);
		return;
	}
	writeFileSync(CONFIG, serialized, "utf8");
	console.log(
		`wrote ${next.filter(isClaudeRule).length} Claude model rules of ${next.length} total (${changed ? "changed" : "no change"})`,
	);
}

main();

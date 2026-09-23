#!/usr/bin/env node

/**
 * Regenerate the bundled model catalog snapshot consumed by
 * `packages/local-host-rs/src/model_catalog.rs` via `include_str!`.
 *
 * Native provider rows come from the MIT-licensed community catalog at
 * models.dev (https://models.dev/api.json). OpenRouter rows come from
 * OpenRouter's public `/api/v1/models` catalog so Maestro ships every current
 * interactive OpenRouter route (`:batch` variants are omitted). The mapping
 * rules here must stay in sync with `map_models_dev_catalog` and
 * `map_openrouter_catalog` in `model_catalog.rs`, which apply the same rules
 * to runtime refreshes.
 *
 * Usage:
 *   node scripts/fetch-model-catalog.mjs [--out <path>] [--timeout-ms <ms>]
 */

import { readFileSync } from "node:fs";
import { writeFile } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";

const MODELS_DEV_API_URL = "https://models.dev/api.json";
const OPENROUTER_MODELS_API_URL = "https://openrouter.ai/api/v1/models";
const DEFAULT_TIMEOUT_MS = 20_000;
const DESCRIPTION_MAX_LEN = 120;

const REPO_ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const DEFAULT_OUT = path.join(REPO_ROOT, "packages/local-host-rs/src/model_catalog_data.json");

// Maestro provider id -> fixed catalog protocol. OpenAI is per-model below.
const PROVIDER_PROTOCOLS = {
	anthropic: "anthropic",
	google: "google",
	xai: "openai-chat",
	openai: null,
};

/**
 * Mirror of `uses_responses_api` in packages/ai-rs/src/openai.rs:
 * Codex, GPT-5, GPT-6 Astra, and o3 models use the Responses API.
 */
function openAiProtocol(modelId) {
	return modelId.includes("codex") || modelId.startsWith("gpt-5") || modelId === "gpt-6-astra" || modelId.startsWith("o3")
		? "openai-responses"
		: "openai-chat";
}

function truncate(text, maxLen) {
	if (text.length <= maxLen) {
		return text;
	}
	const cut = text.slice(0, maxLen - 1);
	const lastSpace = cut.lastIndexOf(" ");
	return `${lastSpace > 0 ? cut.slice(0, lastSpace) : cut}…`;
}

function supportedParameter(model, parameter) {
	return Array.isArray(model?.supported_parameters) && model.supported_parameters.includes(parameter);
}

/**
 * An output limit, or nothing when the number is really the context window.
 *
 * Aggregators conflate the two. LiteLLM's own schema says of `max_tokens`:
 * "LEGACY parameter. set to max_output_tokens if provider specifies it. IF not
 * set to max_input_tokens" - so a consumer reading it can get an input limit
 * and send it as an output cap. 1,067 of LiteLLM's 4,179 entries have no
 * max_output_tokens at all.
 *
 * Rejecting `output >= context` caught the exact-equality case. It did not
 * catch near-equality: OpenRouter reports qwen/qwen3.6-27b with a 262,144
 * context and a 262,140 output limit, which leaves four tokens for the prompt
 * and cannot be a real limit.
 *
 * Within 1% of the window is treated as the same mistake. The threshold is
 * deliberately tight rather than a tidy fraction, because real limits go
 * higher than intuition suggests: o3 publishes 100,000 output against a
 * 200,000 window, exactly half, and the xAI models sit at 0.9. A rule at
 * either of those would discard published numbers.
 */
function distinctOutputTokens(context, output) {
	if (!Number.isInteger(output) || output <= 0) {
		return undefined;
	}
	if (Number.isInteger(context) && context > 0 && output >= context * 0.99) {
		return undefined;
	}
	// Exactly 90% of the context window is OpenRouter's fallback, not a vendor's
	// limit. 32 rows match it to six significant figures and every one is an
	// OpenRouter row, spread across 17 vendor namespaces: x-ai, meta, qwen,
	// google, perplexity, deepseek, moonshotai, ibm-granite and more.
	// Seventeen vendors do not independently choose the same ratio.
	//
	// xAI's own agent settles it for their models. xai-org/grok-build declares
	// `max_tokens: Option<u32>` with skip_serializing_if = "Option::is_none",
	// defaults it to None, and the only reference to its `set_max_tokens`
	// builder is the definition, so the field is never sent. Its model config
	// carries `context_window` and no output limit at all, and its docs say a
	// model defined without one defaults to a 200,000 context. There is no
	// per-model output cap for xAI to publish, so 900,000 for grok-4.3 is a
	// number OpenRouter computed.
	//
	// Dropping it leaves the field absent, which is the honest state, and
	// conductor's resolveMaxOutputTokens falls back to each provider's floor.
	if (Number.isInteger(context) && context > 0 && output === Math.round(context * 0.9)) {
		return undefined;
	}
	return output;
}

function limitTokens(model, field) {
	const value = model?.limit?.[field];
	return Number.isInteger(value) && value > 0 ? value : undefined;
}

function indexModelsDevTokenLimits(modelsDevCatalog) {
	const entries = new Map();
	const push = (key, context, output) => {
		if (!entries.has(key)) {
			entries.set(key, []);
		}
		entries.get(key).push({ context, output });
	};
	const indexProvider = (providerId, providerModels) => {
		if (!providerModels || typeof providerModels !== "object") {
			return;
		}
		for (const [modelId, model] of Object.entries(providerModels)) {
			const context = limitTokens(model, "context");
			const output = limitTokens(model, "output");
			if (providerId === "openrouter") {
				push(modelId, context, output);
			} else {
				push(`${providerId}/${modelId}`, context, output);
			}
		}
	};
	for (const [providerId, provider] of Object.entries(modelsDevCatalog)) {
		if (providerId === "openrouter") {
			continue;
		}
		indexProvider(providerId, provider?.models);
	}
	indexProvider("openrouter", modelsDevCatalog.openrouter?.models);
	return entries;
}

function resolveOpenRouterOutput(entries, id, context, advertised) {
	const distinct = distinctOutputTokens(context, advertised);
	if (distinct !== undefined) {
		return distinct;
	}
	const candidates = entries.get(id) ?? [];
	for (const candidate of candidates) {
		const resolved = distinctOutputTokens(context, candidate.output);
		if (resolved !== undefined) {
			return resolved;
		}
	}
	return undefined;
}

function mapOpenRouterModel(model, tokenLimits) {
	const id = typeof model?.id === "string" ? model.id.trim() : "";
	if (id === "" || id.endsWith(":batch")) {
		return null;
	}
	const context =
		Number.isInteger(model.context_length) && model.context_length > 0
			? model.context_length
			: Number.isInteger(model.top_provider?.context_length) && model.top_provider.context_length > 0
				? model.top_provider.context_length
				: 0;
	if (context <= 0) {
		return null;
	}
	const advertised = model.top_provider?.max_completion_tokens ?? model.max_completion_tokens;
	const inputs = Array.isArray(model.architecture?.input_modalities)
		? model.architecture.input_modalities
		: [];
	return {
		id,
		name: typeof model.name === "string" && model.name !== "" ? model.name : id,
		provider: "openrouter",
		description: truncate(
			typeof model.description === "string" && model.description !== ""
				? model.description
				: (model.name ?? id),
			DESCRIPTION_MAX_LEN,
		),
		capabilities: {
			// OpenRouter's stable surface is Chat Completions. Do not inherit
			// OpenAI's Responses heuristic from the nested vendor id.
			protocol: "openai-chat",
			tools: supportedParameter(model, "tools") || supportedParameter(model, "tool_choice"),
			vision: inputs.includes("image"),
			reasoning:
				supportedParameter(model, "reasoning") ||
				supportedParameter(model, "include_reasoning") ||
				(model.reasoning != null && typeof model.reasoning === "object"),
			streaming: true,
			context_tokens: context,
			output_tokens: resolveOpenRouterOutput(tokenLimits, id, context, advertised),
			// OpenRouter lists `temperature` in `supported_parameters` for
			// routes that accept it. Omitted when the route lists no
			// parameters at all.
			...(Array.isArray(model.supported_parameters)
				? { temperature: supportedParameter(model, "temperature") }
				: {}),
		},
		cost: mapOpenRouterCost(model.pricing),
		verification: {
			state: "catalog",
			source: "openrouter",
		},
	};
}

/**
 * The effort levels a models.dev entry advertises, in ladder order.
 *
 * models.dev states these under `reasoning_options` as one or more blocks of
 * `{type, values}`; only the `effort` block is a level ladder.
 */
function reasoningEfforts(model) {
	const options = Array.isArray(model?.reasoning_options) ? model.reasoning_options : [];
	const values = options
		.filter((option) => option?.type === "effort")
		.flatMap((option) => (Array.isArray(option.values) ? option.values : []))
		.filter((value) => typeof value === "string");
	return [...new Set(values)];
}

/**
 * Per-million-token USD rates, carried so cost reporting reads the same
 * snapshot as everything else instead of a hand-maintained table.
 *
 * models.dev already states `cost` in USD per million tokens. Only finite,
 * non-negative numbers are kept; `input` and `output` are required, and the
 * two cache rates are optional because not every model publishes them.
 */
/**
 * OpenRouter states pricing as USD per token, in strings. Scale to USD per
 * million tokens so routed rows match the direct-provider rows.
 */
function mapOpenRouterCost(pricing) {
	const rate = (value) => {
		const parsed = typeof value === "string" ? Number.parseFloat(value) : value;
		if (typeof parsed !== "number" || !Number.isFinite(parsed) || parsed < 0) {
			return undefined;
		}
		// Scaling a per-token float leaves artifacts (2e-7 * 1e6 is
		// 0.19999999999999998). Round so the snapshot stays byte-stable.
		return Math.round(parsed * 1_000_000 * 1e6) / 1e6;
	};
	const input = rate(pricing?.prompt);
	const output = rate(pricing?.completion);
	if (input === undefined || output === undefined) {
		return undefined;
	}
	const mapped = { input, output };
	const cacheRead = rate(pricing?.input_cache_read);
	if (cacheRead !== undefined) {
		mapped.cache_read = cacheRead;
	}
	const cacheWrite = rate(pricing?.input_cache_write);
	if (cacheWrite !== undefined) {
		mapped.cache_write = cacheWrite;
	}
	return mapped;
}

/**
 * Remove a long-context tier from a model that cannot reach the threshold.
 *
 * models.dev is internally inconsistent for at least one row:
 * gemini-2.5-computer-use-preview-10-2025 has a 131,072-token context and a
 * tier that starts at 200,000, which can never apply. The block looks copied
 * from gemini-2.5-pro, which has the same base rates and a 1M window. An
 * unreachable tier is noise at best and a mis-copied pricing block at worst,
 * so it is dropped and named.
 */
function dropUnreachableContextTiers(models) {
	const dropped = [];
	for (const model of models) {
		if (!model.cost?.above_200k) continue;
		const context = model.capabilities?.context_tokens;
		if (typeof context === "number" && context > 200_000) continue;
		delete model.cost.above_200k;
		dropped.push(`${model.id} (context ${context})`);
	}
	if (dropped.length > 0) {
		console.log(
			`dropped ${dropped.length} unreachable long-context tier(s): ${dropped.join(", ")}`,
		);
	}
}

/**
 * Give a direct-provider row the price its OpenRouter twin publishes.
 *
 * models.dev carries no cost for some direct rows even when OpenRouter prices
 * the same weights: `gemma-4-26b-a4b-it` arrived with cost null while
 * `google/gemma-4-26b-a4b-it` publishes $0.09/$0.30 per million. A row with no
 * cost is not free, but every consumer treats it that way, because
 * conductor's computeCostDetails returns a zero breakdown when it finds no
 * pricing entry. Filling the gap from the twin is the difference between
 * reporting a real number and reporting nothing as if it were nothing owed.
 *
 * Only a row with no cost at all is touched, so a published direct price
 * always wins over the OpenRouter route's.
 */
function backfillMissingCosts(models) {
	const priced = new Map();
	for (const model of models) {
		if (!model.id.includes("/")) continue;
		if (typeof model.cost?.input !== "number") continue;
		const bare = model.id.split("/").pop();
		// An exact twin wins; never let two routes fight over one bare name.
		if (bare && !priced.has(bare)) priced.set(bare, model.cost);
	}
	let filled = 0;
	for (const model of models) {
		if (model.cost && typeof model.cost.input === "number") continue;
		if (model.id.includes("/")) continue;
		const twin = priced.get(model.id);
		if (!twin) continue;
		model.cost = { ...twin };
		filled += 1;
	}
	if (filled > 0) {
		console.log(`filled ${filled} missing cost(s) from an OpenRouter twin`);
	}
}

/**
 * Cost keys models.dev emits that the catalog deliberately does not carry.
 *
 * Anything models.dev prices and this list does not name is a dimension the
 * catalog would drop silently, so mapCost throws on it. That is not
 * hypothetical: models.dev publishes `reasoning` for 149 models,
 * `context_over_200k` for 458 and `input_audio` for 134, and every one of
 * them was being discarded here without a word.
 */
const UNMAPPED_COST_FIELDS = new Map([
	[
		"tiers",
		"the general form of context_over_200k; carried through that field until a consumer needs arbitrary tiers",
	],
	["output_audio", "no consumer bills audio output yet; add it with the consumer, not before"],
]);

function mapCost(cost) {
	const rate = (value) =>
		typeof value === "number" && Number.isFinite(value) && value >= 0 ? value : undefined;
	const input = rate(cost?.input);
	const output = rate(cost?.output);
	if (input === undefined || output === undefined) {
		return undefined;
	}

	const mapped = { input, output };
	for (const [key, target] of [
		["cache_read", "cache_read"],
		["cache_write", "cache_write"],
		// models.dev prices reasoning tokens apart from output tokens where a
		// vendor does. Perplexity publishes $3 per million for
		// sonar-deep-research reasoning against $8 for output, so folding them
		// into output over-bills them by 2.7x.
		["reasoning", "reasoning"],
		// Audio input is 2x to 8x text input on the live models.
		["input_audio", "input_audio"],
	]) {
		const value = rate(cost?.[key]);
		if (value !== undefined) {
			mapped[target] = value;
		}
	}

	// Long-context pricing. Anthropic, OpenAI and Google all charge more above
	// 200k tokens: gpt-5.5 is $5/$30 below and $10/$45 above. A flat rate
	// under-bills every request past the threshold by roughly half.
	const over = cost?.context_over_200k;
	if (over && typeof over === "object") {
		const tier = {};
		for (const key of ["input", "output", "cache_read", "cache_write"]) {
			const value = rate(over[key]);
			if (value !== undefined) tier[key] = value;
		}
		if (tier.input !== undefined || tier.output !== undefined) {
			mapped.above_200k = tier;
		}
	}

	const unknown = Object.keys(cost ?? {}).filter(
		(key) =>
			!["input", "output", "cache_read", "cache_write", "reasoning", "input_audio", "context_over_200k"].includes(
				key,
			) && !UNMAPPED_COST_FIELDS.has(key),
	);
	if (unknown.length > 0) {
		throw new Error(
			`models.dev prices cost dimension(s) the catalog would drop silently: ${unknown.join(", ")}. ` +
				"Map them in mapCost or name them in UNMAPPED_COST_FIELDS with a reason.",
		);
	}

	return mapped;
}

/**
 * Context windows where models.dev disagrees with the vendor's own docs.
 *
 * models.dev lists Claude Sonnet 4.5 with a 1,000,000-token context window.
 * Anthropic states 200k twice in
 * platform.claude.com/docs/en/build-with-claude/context-windows: it names the
 * twelve models that have 1M and says "Other Claude models, including Claude
 * Sonnet 4.5, have a 200k-token context window", then repeats it for context
 * awareness — "1M tokens for Claude Sonnet 5 and Claude Sonnet 4.6, and 200k
 * tokens for Claude Sonnet 4.5 and Claude Haiku 4.5".
 *
 * This matters more than a wrong number in a table. The overflow detector
 * compacts at a fraction of the declared window, so a 1,000,000 value lets a
 * Sonnet 4.5 session run to roughly 750k tokens before compacting, while the
 * API rejects it at 200k with "prompt is too long". The conservative value
 * fails safe; the optimistic one fails the request.
 *
 * Corrections are keyed by exact catalog id, including provider routes when
 * the route's advertised limit contradicts the vendor's published limit.
 * Every entry cites the vendor source and should be deleted once upstream
 * corrects it.
 */
const VENDOR_CORRECTIONS_PATH = path.join(REPO_ROOT, "config/vendor-corrections.json");

/**
 * Documented vendor facts that override what the aggregators report.
 *
 * This used to be a two-entry map of context windows written into this file.
 * It covers any field now, because the aggregators are not selectively wrong:
 * models.dev reported Sonnet 4.5 with a 1,000,000-token window against
 * Anthropic's published 200,000, and nothing about that failure mode is
 * specific to context windows.
 *
 * Applied after mapping, so a correction is expressed once and reaches both
 * the models.dev and OpenRouter paths. A corrected row records where its
 * number came from, so a reader can tell a vendor-checked value from a
 * scraped one without going to the generator.
 */
function loadVendorCorrections() {
	const parsed = JSON.parse(readFileSync(VENDOR_CORRECTIONS_PATH, "utf8"));
	if (parsed.schemaVersion !== "maestro.vendor-corrections.v1") {
		throw new Error(`unsupported vendor-corrections schema ${parsed.schemaVersion}`);
	}
	for (const correction of parsed.corrections ?? []) {
		for (const key of ["id", "field", "source", "reason"]) {
			if (typeof correction[key] !== "string" || correction[key] === "") {
				throw new Error(`vendor correction is missing ${key}: ${JSON.stringify(correction)}`);
			}
		}
		if (correction.value === undefined || correction.value === null) {
			throw new Error(`vendor correction has no value: ${correction.id} ${correction.field}`);
		}
	}
	return parsed.corrections ?? [];
}

/** Read or write a dotted path such as `capabilities.context_tokens`. */
function atPath(model, field) {
	const parts = field.split(".");
	let cursor = model;
	for (const part of parts.slice(0, -1)) {
		if (cursor === null || typeof cursor !== "object") return undefined;
		cursor = cursor[part];
	}
	if (cursor === null || typeof cursor !== "object") return undefined;
	return { container: cursor, key: parts[parts.length - 1] };
}

/**
 * Apply every correction and report what each one did.
 *
 * A correction upstream already agrees with is reported as a no-op rather than
 * silently succeeding: it is dead weight, and leaving it in place means the
 * next reader cannot tell which corrections are still holding a wrong value
 * down.
 */
function applyVendorCorrections(models, corrections) {
	const byId = new Map(models.map((model) => [model.id, model]));
	const applied = [];
	const noops = [];
	const missing = [];
	for (const correction of corrections) {
		const model = byId.get(correction.id);
		if (!model) {
			missing.push(correction);
			continue;
		}
		const slot = atPath(model, correction.field);
		if (!slot) {
			missing.push(correction);
			continue;
		}
		if (slot.container[slot.key] === correction.value) {
			noops.push(correction);
			continue;
		}
		slot.container[slot.key] = correction.value;
		// `verified` is the tier the snapshot schema already has for a value
		// checked against something better than an aggregator; the source
		// carries the vendor page it was checked against.
		model.verification = { state: "verified", source: correction.source };
		applied.push(correction);
	}
	return { applied, noops, missing };
}

/**
 * The lifecycle models.dev publishes for a model, when it publishes one.
 *
 * 125 upstream rows are marked `deprecated` and 28 `beta`, and the catalog was
 * dropping the field. That is the same information #10177 reconstructed by
 * hand from Gemini id patterns, and upstream had it all along: o1 and o1-pro
 * are `deprecated` upstream and conductor offers both unflagged.
 *
 * Only the two values upstream actually uses are accepted. A third value is a
 * change in upstream's vocabulary and should be looked at rather than passed
 * through as if understood.
 */
const UPSTREAM_STATUSES = new Set(["deprecated", "beta"]);

/**
 * The lifecycle the model's OWN vendor publishes, or nothing.
 *
 * 125 upstream rows are marked `deprecated` and 28 `beta`, and the catalog was
 * dropping the field. That is the same information #10177 reconstructed by
 * hand from Gemini id patterns, and upstream had it all along.
 *
 * Resolved strictly from the vendor's own row. Aggregators disagree: for
 * `o4-mini`, models.dev has `openai` and `azure` saying deprecated while
 * `llmgateway`, `helicone` and `abacus` say nothing, and for
 * `deepseek/deepseek-r1` the DeepSeek provider has no row at all while an
 * aggregator does. Accepting any provider's answer attributes one vendor's
 * retirement to another, which is the loose-match mistake this whole body of
 * work keeps finding. No vendor row means no status.
 */
function mapStatus(model) {
	const status = model?.status;
	if (status === undefined || status === null) {
		return undefined;
	}
	if (typeof status !== "string" || !UPSTREAM_STATUSES.has(status)) {
		throw new Error(
			`models.dev published an unrecognised status ${JSON.stringify(status)}; ` +
				"add it to UPSTREAM_STATUSES once its meaning is known",
		);
	}
	return status;
}

function mapModel(providerId, modelId, model) {
	const protocol =
		providerId === "openai" ? openAiProtocol(modelId) : PROVIDER_PROTOCOLS[providerId];
	return {
		id: modelId,
		name: typeof model.name === "string" && model.name !== "" ? model.name : modelId,
		provider: providerId,
		description: truncate(
			typeof model.description === "string" && model.description !== ""
				? model.description
				: (model.name ?? modelId),
			DESCRIPTION_MAX_LEN,
		),
		capabilities: {
			protocol,
			tools: true,
			vision: Array.isArray(model.modalities?.input) && model.modalities.input.includes("image"),
			reasoning: model.reasoning === true,
			streaming: true,
			context_tokens: model.limit.context,
			// Per-response output ceiling (reasoning included) from
			// models.dev `limit.output`. Omitted when the source lacks it or
			// copies the context window into output.
			output_tokens: distinctOutputTokens(model.limit?.context, model.limit?.output),
			// Whether the model accepts the `temperature` parameter, from
			// models.dev. Sending it to a model that rejects it returns 400,
			// so the Rust request-capability guards compare against this.
			// Omitted when the source does not state it.
			...(typeof model.temperature === "boolean" ? { temperature: model.temperature } : {}),
		},
		// Input modalities and the effort ladder, carried so consumers that
		// need them (the desktop built-in provider rules) derive from this
		// snapshot instead of restating them in a second hand-edited file.
		input_modalities: Array.isArray(model.modalities?.input)
			? model.modalities.input.filter((value) => typeof value === "string")
			: [],
		reasoning_efforts: reasoningEfforts(model),
		cost: mapCost(model.cost),
		// Carried only when true. An open-weights model is self-hosted, so it
		// has no vendor rate; without this flag a missing `cost` cannot be
		// told apart from a rate that should be there and is not.
		...(model.open_weights === true ? { open_weights: true } : {}),
		...(mapStatus(model) === undefined ? {} : { status: mapStatus(model) }),
		verification: {
			state: "catalog",
			source: "models.dev",
		},
	};
}

function parseArgs(argv) {
	const args = { out: DEFAULT_OUT, timeoutMs: DEFAULT_TIMEOUT_MS };
	for (let i = 0; i < argv.length; i += 1) {
		if (argv[i] === "--out") {
			args.out = path.resolve(argv[i + 1]);
			i += 1;
		} else if (argv[i] === "--timeout-ms") {
			args.timeoutMs = Number.parseInt(argv[i + 1], 10);
			i += 1;
		} else {
			throw new Error(`unknown argument: ${argv[i]}`);
		}
	}
	if (!Number.isFinite(args.timeoutMs) || args.timeoutMs <= 0) {
		throw new Error("--timeout-ms must be a positive integer");
	}
	return args;
}

async function main() {
	const args = parseArgs(process.argv.slice(2));

	const headers = { accept: "application/json", "user-agent": "maestro-model-catalog-fetcher" };
	const [modelsDevCatalog, openrouterPayload] = await Promise.all([
		fetchJson(MODELS_DEV_API_URL, args.timeoutMs, headers),
		fetchJson(OPENROUTER_MODELS_API_URL, args.timeoutMs, headers),
	]);

	const models = [];
	for (const providerId of Object.keys(PROVIDER_PROTOCOLS)) {
		const providerModels = modelsDevCatalog[providerId]?.models;
		if (!providerModels || typeof providerModels !== "object") {
			throw new Error(`models.dev payload is missing provider "${providerId}"`);
		}
		for (const [modelId, model] of Object.entries(providerModels)) {
			if (model?.tool_call !== true || model?.status === "deprecated") {
				continue;
			}
			const context = model?.limit?.context;
			if (!Number.isInteger(context) || context <= 0) {
				continue;
			}
			models.push(mapModel(providerId, modelId, model));
		}
	}

	// Native launch metadata from OpenAI while models.dev catches up.
	if (!models.some((model) => model.provider === "openai" && model.id === "gpt-6-astra")) {
		const astra = mapModel("openai", "gpt-6-astra", {
			name: "GPT-6 Astra",
			description: "Reasoning, coding, research, and document creation",
			reasoning: true,
			modalities: { input: ["text", "image"] },
			limit: { context: 1050000, output: 128000 },
		});
		astra.verification.source = "https://developers.openai.com/api/docs/models/gpt-6-astra";
		models.push(astra);
	}

	const openrouterModels = Array.isArray(openrouterPayload?.data) ? openrouterPayload.data : null;
	if (!openrouterModels) {
		throw new Error("OpenRouter payload is missing a data array");
	}
	const tokenLimits = indexModelsDevTokenLimits(modelsDevCatalog);
	for (const model of openrouterModels) {
		const mapped = mapOpenRouterModel(model, tokenLimits);
		if (mapped) {
			models.push(mapped);
		}
	}

	if (models.length === 0) {
		throw new Error("catalog fetch produced an empty catalog; refusing to write");
	}
	if (!models.some((model) => model.provider === "openrouter")) {
		throw new Error("OpenRouter payload produced no catalog rows; refusing to write");
	}

	backfillMissingCosts(models);
	dropUnreachableContextTiers(models);

	const corrections = loadVendorCorrections();
	const { applied, noops, missing } = applyVendorCorrections(models, corrections);
	for (const correction of applied) {
		console.log(`corrected ${correction.id} ${correction.field} -> ${correction.value}`);
	}
	if (noops.length > 0) {
		console.log(
			`${noops.length} vendor correction(s) now agree with upstream and should be deleted from ` +
				`config/vendor-corrections.json:`,
		);
		for (const correction of noops) {
			console.log(`  ${correction.id} ${correction.field}`);
		}
	}
	if (missing.length > 0) {
		throw new Error(
			`vendor corrections target models or fields the catalog does not have: ${missing
				.map((correction) => `${correction.id} ${correction.field}`)
				.join(", ")}`,
		);
	}

	models.sort(
		(left, right) => left.provider.localeCompare(right.provider) || left.id.localeCompare(right.id),
	);

	const snapshot = {
		generated_at: Math.floor(Date.now() / 1000),
		source: `${MODELS_DEV_API_URL}+${OPENROUTER_MODELS_API_URL}`,
		// The corrections that actually changed something this run. Recording
		// them here is what lets a checker verify every correction is still
		// load-bearing without keeping a second copy of the list.
		vendor_corrections: applied.map((correction) => ({
			id: correction.id,
			field: correction.field,
			value: correction.value,
			source: correction.source,
		})),
		models,
	};

	await writeFile(args.out, `${JSON.stringify(snapshot, null, 2)}\n`, "utf8");
	const counts = Object.fromEntries(
		[...Object.keys(PROVIDER_PROTOCOLS), "openrouter"].map((providerId) => [
			providerId,
			models.filter((model) => model.provider === providerId).length,
		]),
	);
	console.log(`wrote ${models.length} models to ${path.relative(REPO_ROOT, args.out)}`, counts);
}

async function fetchJson(url, timeoutMs, headers) {
	const controller = new AbortController();
	const timeout = setTimeout(() => controller.abort(), timeoutMs);
	let response;
	try {
		response = await fetch(url, { signal: controller.signal, headers });
	} finally {
		clearTimeout(timeout);
	}
	if (!response.ok) {
		throw new Error(`${url} fetch failed: HTTP ${response.status}`);
	}
	return response.json();
}

main().catch((error) => {
	console.error(`fetch-model-catalog: ${error.message}`);
	process.exitCode = 1;
});

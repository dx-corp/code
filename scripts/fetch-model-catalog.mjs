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

function distinctOutputTokens(context, output) {
	if (!Number.isInteger(output) || output <= 0) {
		return undefined;
	}
	if (Number.isInteger(context) && context > 0 && output >= context) {
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
			context_tokens: correctedContextWindow(id, context),
			output_tokens: resolveOpenRouterOutput(tokenLimits, id, context, advertised),
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

function mapCost(cost) {
	const rate = (value) =>
		typeof value === "number" && Number.isFinite(value) && value >= 0 ? value : undefined;
	const input = rate(cost?.input);
	const output = rate(cost?.output);
	if (input === undefined || output === undefined) {
		return undefined;
	}
	const mapped = { input, output };
	const cacheRead = rate(cost?.cache_read);
	if (cacheRead !== undefined) {
		mapped.cache_read = cacheRead;
	}
	const cacheWrite = rate(cost?.cache_write);
	if (cacheWrite !== undefined) {
		mapped.cache_write = cacheWrite;
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
 * Keyed by the exact catalog id. Only direct-provider ids belong here. An
 * OpenRouter id names a route, not the vendor default: OpenRouter sends the
 * 1M-context beta header on its own account, so `anthropic/claude-sonnet-4.5`
 * genuinely accepts the 1,000,000 tokens its /api/v1/models entry advertises.
 * Overriding that id down to 200,000 would discard 80% of a window the route
 * accepts. Every entry needs a comment naming the vendor source, and entries
 * should be deleted once upstream corrects them.
 */
const CONTEXT_WINDOW_OVERRIDES = new Map([
	// https://docs.anthropic.com/en/docs/about-claude/models/overview - 200K context
	["claude-sonnet-4-5", 200_000],
	["claude-sonnet-4-5-20250929", 200_000],
]);

/** Apply a documented vendor correction to an upstream context window. */
function correctedContextWindow(id, context) {
	return CONTEXT_WINDOW_OVERRIDES.get(id) ?? context;
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
			context_tokens: correctedContextWindow(modelId, model.limit.context),
			// Per-response output ceiling (reasoning included) from
			// models.dev `limit.output`. Omitted when the source lacks it or
			// copies the context window into output.
			output_tokens: distinctOutputTokens(model.limit?.context, model.limit?.output),
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

	models.sort(
		(left, right) => left.provider.localeCompare(right.provider) || left.id.localeCompare(right.id),
	);

	const snapshot = {
		generated_at: Math.floor(Date.now() / 1000),
		source: `${MODELS_DEV_API_URL}+${OPENROUTER_MODELS_API_URL}`,
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

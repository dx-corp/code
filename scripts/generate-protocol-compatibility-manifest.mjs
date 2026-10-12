#!/usr/bin/env node

import { createHash } from "node:crypto";
import { existsSync, readFileSync, writeFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const DEFAULT_OUTPUT = resolve(
	ROOT,
	"proto/maestro/v1/protocol-compatibility-manifest.json",
);
const SCHEMA_VERSION = "evalops.maestro.protocol-compatibility-manifest.v1";

const SOURCE_PATHS = {
	headlessSchema: "proto/maestro/v1/headless.proto",
	headlessGenerated: "packages/local-host-rs/src/headless/generated_protocol.rs",
	headlessRuntime: "packages/local-host-rs/src/headless/messages.rs",
	runtimeProtocol: "packages/runtime-contracts-rs/src/protocol.rs",
	runtimeFixture: "packages/runtime-contracts-rs/fixtures/headless-protocol-v1.json",
	runtimeReceipts: "packages/runtime-contracts-rs/src/receipts.rs",
	runtimeReceiptFixture: "packages/runtime-contracts-rs/fixtures/runtime-receipt-v1.json",
	runtimeReceiptContractFixture:
		"packages/runtime-contracts-rs/fixtures/runtime-receipt-contract-v1.json",
	transcript: "packages/local-host-rs/src/transcript.rs",
	thread: "packages/hosted-runner-rs/src/hosted_runner/thread_protocol.rs",
	threadCompatibilityMatrix: "proto/maestro/v1/hosted-thread-compatibility-matrix.json",
	resident: "packages/hosted-runner-rs/src/hosted_runner_cli.rs",
	hostedRunner: "packages/hosted-runner-rs/src/hosted_runner.rs",
	rendezvous: "packages/hosted-runner-rs/src/hosted_runner/rendezvous_protocol.rs",
};

const ENUM_EXPORTS = {
	NotificationType: "notifications",
};

function parseArgs(argv) {
	const options = {
		check: false,
		output: DEFAULT_OUTPUT,
		sourceRoot: ROOT,
		sourceSha: undefined,
		buildDigest: undefined,
	};

	for (let index = 0; index < argv.length; index += 1) {
		const argument = argv[index];
		switch (argument) {
			case "--check":
				options.check = true;
				break;
			case "--out":
				options.output = resolve(ROOT, requireValue(argv, ++index, argument));
				break;
			case "--source-root":
				options.sourceRoot = resolve(requireValue(argv, ++index, argument));
				break;
			case "--source-sha":
				options.sourceSha = requireValue(argv, ++index, argument);
				break;
			case "--build-digest":
				options.buildDigest = requireValue(argv, ++index, argument);
				break;
			default:
				throw new Error(`Unknown argument: ${argument}`);
		}
	}

	return options;
}

function requireValue(argv, index, argument) {
	const value = argv[index];
	if (!value) throw new Error(`${argument} requires a value`);
	return value;
}

function toEnumPrefix(enumName) {
	return enumName.replace(/([a-z0-9])([A-Z])/g, "$1_$2").toUpperCase();
}

function parseProtoVersion(source) {
	const match = source.match(/protocol-version:\s*([0-9-]+)/);
	if (!match) throw new Error("headless.proto is missing protocol-version");
	return match[1];
}

function parseProtoEnums(source) {
	const enums = new Map();
	for (const match of source.matchAll(/enum\s+(\w+)\s*\{([\s\S]*?)\n\}/g)) {
		const [, enumName, body] = match;
		const prefix = `${toEnumPrefix(enumName)}_`;
		const values = [];
		for (const valueMatch of body.matchAll(/^\s*([A-Z0-9_]+)\s*=\s*\d+;/gm)) {
			const identifier = valueMatch[1];
			if (identifier.endsWith("_UNSPECIFIED")) continue;
			if (!identifier.startsWith(prefix)) {
				throw new Error(`${enumName} value ${identifier} lacks prefix ${prefix}`);
			}
			values.push(identifier.slice(prefix.length).toLowerCase());
		}
		enums.set(enumName, values);
	}
	return enums;
}

function parseOneofFields(source, messageName) {
	const message = source.match(
		new RegExp(`message\\s+${messageName}\\s*\\{[\\s\\S]*?oneof\\s+payload\\s*\\{([\\s\\S]*?)\\n\\s*\\}\\n\\}`),
	);
	if (!message) throw new Error(`${messageName} is missing oneof payload`);
	return [...message[1].matchAll(/^\s*\w+\s+(\w+)\s*=\s*\d+;/gm)].map(
		(match) => match[1],
	);
}

function parseOptionalBoolFields(source, messageName) {
	const message = source.match(
		new RegExp(`message\\s+${messageName}\\s*\\{([\\s\\S]*?)\\n\\}`),
	);
	if (!message) throw new Error(`Missing protobuf message ${messageName}`);
	return [...message[1].matchAll(/^\s*optional\s+bool\s+(\w+)\s*=\s*\d+;/gm)].map(
		(match) => match[1],
	);
}

function parseRustStringConstant(source, name) {
	const match = source.match(
		new RegExp(`(?:pub(?:\\([^)]*\\))?\\s+)?const\\s+${name}:\\s*&str\\s*=\\s*"([^"]+)"`),
	);
	if (!match) throw new Error(`Missing Rust string constant ${name}`);
	return match[1];
}

function parseOptionalRustStringConstant(source, name) {
	const match = source.match(
		new RegExp(`(?:pub(?:\\([^)]*\\))?\\s+)?const\\s+${name}:\\s*&str\\s*=\\s*"([^"]+)"`),
	);
	return match?.[1] ?? null;
}

function parseRustStringArrayConstant(source, name, bindings = {}) {
	const match = source.match(
		new RegExp(`const\\s+${name}:\\s*&\\[&str\\]\\s*=\\s*&\\[([\\s\\S]*?)\\];`),
	);
	if (!match) throw new Error(`Missing Rust string array constant ${name}`);
	return match[1]
		.split(",")
		.map((value) => value.trim())
		.filter(Boolean)
		.map((value) => {
			if (/^"[^"]+"$/.test(value)) return value.slice(1, -1);
			if (Object.hasOwn(bindings, value)) return bindings[value];
			throw new Error(`Unresolved ${name} value ${value}`);
		});
}

function extractRustBlock(source, declaration) {
	const start = source.indexOf(declaration);
	if (start === -1) throw new Error(`Missing Rust declaration ${declaration}`);
	const open = source.indexOf("{", start + declaration.length);
	if (open === -1) throw new Error(`Missing opening brace for ${declaration}`);
	let depth = 0;
	for (let index = open; index < source.length; index += 1) {
		if (source[index] === "{") depth += 1;
		if (source[index] === "}") depth -= 1;
		if (depth === 0) return source.slice(open + 1, index);
	}
	throw new Error(`Missing closing brace for ${declaration}`);
}

function toSnakeCase(value) {
	return value.replace(/([a-z0-9])([A-Z])/g, "$1_$2").toLowerCase();
}

function serdeRenameAll(source, declaration) {
	const start = source.indexOf(declaration);
	if (start === -1) throw new Error(`Missing Rust declaration ${declaration}`);
	const attributesStart = source.lastIndexOf("#[derive", start);
	if (attributesStart === -1) return null;
	const attributes = source.slice(attributesStart, start);
	return attributes.match(/#\[serde\([^\]]*rename_all\s*=\s*"([^"]+)"[^\]]*\)\]/)?.[1] ?? null;
}

function applySerdeRenameAll(value, renameAll) {
	switch (renameAll) {
		case "snake_case":
			return toSnakeCase(value);
		case "camelCase": {
			const snake = toSnakeCase(value);
			return snake.replace(/_([a-z])/g, (_, letter) => letter.toUpperCase());
		}
		case "lowercase":
			return value.toLowerCase();
		case null:
			return value;
		default:
			throw new Error(`Unsupported serde rename_all value ${renameAll}`);
	}
}

function parseRustEnumVariants(source, name) {
	const declaration = `pub enum ${name}`;
	const body = extractRustBlock(source, declaration);
	const renameAll = serdeRenameAll(source, declaration);
	const variants = [];
	let depth = 0;
	let pendingRename = null;
	for (const line of body.split("\n")) {
		if (depth === 0) {
			const rename = line.match(/#\[serde\(rename\s*=\s*"([^"]+)"\)\]/);
			if (rename) pendingRename = rename[1];
			const match = line.match(/^\s*([A-Z][A-Za-z0-9_]*)\b/);
			if (match) {
				variants.push(pendingRename ?? applySerdeRenameAll(match[1], renameAll));
				pendingRename = null;
			}
		}
		for (const character of line) {
			if (character === "{") depth += 1;
			if (character === "}") depth -= 1;
		}
	}
	return variants;
}

function parseOptionalRustEnumVariants(source, name) {
	return source.includes(`pub enum ${name}`)
		? parseRustEnumVariants(source, name)
		: [];
}

function parseRustStructFields(source, name) {
	const declaration = `pub struct ${name}`;
	const body = extractRustBlock(source, declaration);
	const renameAll = serdeRenameAll(source, declaration);
	const fields = [];
	let pendingRename = null;
	for (const line of body.split("\n")) {
		const rename = line.match(/#\[serde\(rename\s*=\s*"([^"]+)"[^\]]*\)\]/);
		if (rename) pendingRename = rename[1];
		const field = line.match(/^\s*pub\s+(\w+):/);
		if (field) {
			fields.push(pendingRename ?? applySerdeRenameAll(field[1], renameAll));
			pendingRename = null;
		}
	}
	return fields;
}

export function canonicalizeForDigest(value) {
	if (value === null || typeof value === "boolean" || typeof value === "string") {
		return JSON.stringify(value);
	}
	if (typeof value === "number") {
		if (!Number.isFinite(value)) throw new Error("non-finite JSON number");
		return JSON.stringify(value);
	}
	if (Array.isArray(value)) {
		return `[${value.map(canonicalizeForDigest).join(",")}]`;
	}
	if (typeof value === "object") {
		return `{${Object.keys(value)
			.sort()
			.map(
				(key) =>
					`${JSON.stringify(key)}:${canonicalizeForDigest(value[key])}`,
			)
			.join(",")}}`;
	}
	throw new Error(`unsupported JSON value type: ${typeof value}`);
}

function stripSourceComments(source) {
	let output = "";
	let index = 0;
	let quote = null;
	while (index < source.length) {
		const current = source[index];
		const next = source[index + 1];
		if (quote) {
			output += current;
			if (current === "\\") {
				output += next ?? "";
				index += 2;
				continue;
			}
			if (current === quote) quote = null;
			index += 1;
			continue;
		}
		if (current === '"' || current === "'") {
			quote = current;
			output += current;
			index += 1;
			continue;
		}
		if (current === "/" && next === "/") {
			index = source.indexOf("\n", index + 2);
			if (index === -1) break;
			output += "\n";
			index += 1;
			continue;
		}
		if (current === "/" && next === "*") {
			let depth = 1;
			index += 2;
			while (index < source.length && depth > 0) {
				if (source[index] === "/" && source[index + 1] === "*") {
					depth += 1;
					index += 2;
				} else if (source[index] === "*" && source[index + 1] === "/") {
					depth -= 1;
					index += 2;
				} else {
					index += 1;
				}
			}
			continue;
		}
		output += current;
		index += 1;
	}
	return output;
}

function sourceContractDigest(...sources) {
	const tokens = stripSourceComments(sources.join("\n"))
		.match(/"(?:\\.|[^"\\])*"|'(?:\\.|[^'\\])*'|[A-Za-z_][A-Za-z0-9_]*|[0-9][A-Za-z0-9_.]*|::|->|=>|==|!=|<=|>=|&&|\|\||\.\.=|\.\.|[^\s]/g)
		?.join("\n") ?? "";
	return `sha256:${createHash("sha256").update(tokens, "utf8").digest("hex")}`;
}

function jsonContractDigest(source) {
	const parsed = JSON.parse(source);
	return `sha256:${createHash("sha256")
		.update(canonicalizeForDigest(parsed), "utf8")
		.digest("hex")}`;
}

function buildRuntimeReceiptCompatibility(sources) {
	const hasReceiptSource = sources.runtimeReceipts !== undefined;
	const hasReceiptFixture = sources.runtimeReceiptFixture !== undefined;
	if (!hasReceiptSource && !hasReceiptFixture) return null;
	if (hasReceiptSource !== hasReceiptFixture) {
		throw new Error(
			"runtime receipt source and fixture must be present together",
		);
	}
	// The checked-in JSON fixture is the canonical serialized projection of the
	// typed Rust receipt. Hashing it keeps comments and cfg(test) coverage out of
	// the compatibility identity while the Rust fixture test binds the
	// projection back to the live serde model.
	const contractSource =
		sources.runtimeReceiptContractFixture ?? sources.runtimeReceiptFixture;
	const contractProjection = JSON.stringify({
		contract: JSON.parse(contractSource),
		representative: JSON.parse(sources.runtimeReceiptFixture),
	});
	const contractDigest = jsonContractDigest(contractProjection);
	return {
		schemaVersion: parseRustStringConstant(
			sources.runtimeReceipts,
			"RUNTIME_RECEIPT_VERSION",
		),
		sourceDigest: contractDigest,
		contractDigest,
	};
}

function validateSourceSha(value) {
	if (value !== null && !/^[0-9a-f]{40}$/i.test(value)) {
		throw new Error("source SHA must contain 40 hexadecimal characters");
	}
}

function validateBuildDigest(value) {
	if (value !== null && !/^sha256:[0-9a-f]{64}$/i.test(value)) {
		throw new Error("build digest must be a sha256 OCI digest");
	}
}

export function readCanonicalSources(root = ROOT) {
	return Object.fromEntries(
		Object.entries(SOURCE_PATHS).flatMap(([name, path]) => {
			const sourcePath = resolve(root, path);
			if (
				[
					"threadCompatibilityMatrix",
					"runtimeReceipts",
					"runtimeReceiptFixture",
					"runtimeReceiptContractFixture",
				].includes(
					name,
				) &&
				!existsSync(sourcePath)
			) {
				return [];
			}
			return [[name, readFileSync(sourcePath, "utf8")]];
		}),
	);
}

export function buildCompatibilityManifest({
	sources = readCanonicalSources(),
	sourceSha = null,
	buildDigest = null,
} = {}) {
	validateSourceSha(sourceSha);
	validateBuildDigest(buildDigest);
	if ((sourceSha === null) !== (buildDigest === null)) {
		throw new Error("source SHA and build digest must be set together");
	}
	const normalizedSourceSha = sourceSha?.toLowerCase() ?? null;
	const normalizedBuildDigest = buildDigest?.toLowerCase() ?? null;

	const protoEnums = parseProtoEnums(sources.headlessSchema);
	const capabilities = {};
	for (const [enumName, key] of Object.entries(ENUM_EXPORTS)) {
		const values = protoEnums.get(enumName);
		if (!values) throw new Error(`headless.proto is missing enum ${enumName}`);
		capabilities[key] = values;
	}
	capabilities.serverRequests = parseRustEnumVariants(
		sources.headlessRuntime,
		"ServerRequestType",
	);
	capabilities.connectionRoles = parseRustEnumVariants(
		sources.headlessRuntime,
		"ConnectionRole",
	);
	capabilities.utilityOperations = parseRustEnumVariants(
		sources.headlessRuntime,
		"UtilityOperation",
	);
	capabilities.codeModes = parseOptionalRustEnumVariants(
		sources.headlessRuntime,
		"CodeMode",
	);
	capabilities.clientFields = parseRustStructFields(
		sources.headlessRuntime,
		"ClientCapabilities",
	);
	capabilities.transcriptGrades = parseRustEnumVariants(
		sources.transcript,
		"TranscriptGrade",
	);
	capabilities.schemaOnlyServerRequests = (
		protoEnums.get("ServerRequestType") ?? []
	).filter((value) => !capabilities.serverRequests.includes(value));

	const schemaToRuntime = parseOneofFields(sources.headlessSchema, "ToAgentEnvelope");
	const schemaFromRuntime = parseOneofFields(sources.headlessSchema, "FromAgentEnvelope");
	const toRuntime = parseRustEnumVariants(sources.headlessRuntime, "ToAgentMessage");
	const fromRuntime = parseRustEnumVariants(sources.headlessRuntime, "FromAgentMessage");
	const schemaClientFeatureFlags = parseOptionalBoolFields(
		sources.headlessSchema,
		"ClientCapabilities",
	);
	capabilities.runtimeOnlyClientFields = capabilities.clientFields.filter(
		(field) =>
			!["server_requests", "utility_operations", ...schemaClientFeatureFlags].includes(field),
	);
	const threadV1 = parseRustStringConstant(sources.thread, "THREAD_PROTOCOL_VERSION");
	const threadV2 = parseOptionalRustStringConstant(
		sources.thread,
		"GOVERNED_THREAD_PROTOCOL_VERSION",
	);
	const threadVersions = [{ version: threadV1, governedCode: false }];
	if (threadV2 !== null) {
		threadVersions.push({
			version: threadV2,
			governedCode: true,
			requiredFields: parseRustStringArrayConstant(
				sources.thread,
				"GOVERNED_THREAD_REQUIRED_FIELDS",
			),
		});
	}
	const threadContractSources = [sources.thread];
	if (sources.threadCompatibilityMatrix === undefined) {
		if (threadV2 !== null) {
			throw new Error(
				"governed thread sources require hosted-thread-compatibility-matrix.json",
			);
		}
	} else {
		threadContractSources.push(sources.threadCompatibilityMatrix);
	}

	const compatibility = {
		headless: {
			supportedVersions: parseRustStringArrayConstant(
				sources.headlessRuntime,
				"SUPPORTED_CLIENT_PROTOCOL_VERSIONS",
				{
					HEADLESS_PROTOCOL_VERSION: parseRustStringConstant(
						sources.headlessGenerated,
						"HEADLESS_PROTOCOL_VERSION",
					),
				},
			),
			schemaDeclaredVersion: parseProtoVersion(sources.headlessSchema),
			schemaContractDigest: sourceContractDigest(sources.headlessSchema),
			runtimeContractDigest: sourceContractDigest(
				sources.headlessGenerated,
				sources.headlessRuntime,
				sources.transcript,
			),
			messages: {
				toRuntime,
				fromRuntime,
				schemaOnlyToRuntime: schemaToRuntime.filter(
					(value) => !toRuntime.includes(value),
				),
				schemaOnlyFromRuntime: schemaFromRuntime.filter(
					(value) => !fromRuntime.includes(value),
				),
				runtimeOnlyToRuntime: toRuntime.filter(
					(value) => !schemaToRuntime.includes(value),
				),
				runtimeOnlyFromRuntime: fromRuntime.filter(
					(value) => !schemaFromRuntime.includes(value),
				),
			},
			capabilities,
		},
		thread: {
			contractDigest: sourceContractDigest(...threadContractSources),
			supportedVersions: threadVersions,
		},
		resident: {
			contractDigest: sourceContractDigest(
				extractRustBlock(sources.resident, "fn validate_resident_contract"),
			),
			modelReadyContractRevision: parseRustStringConstant(
				sources.resident,
				"RESIDENT_MODEL_READY_CONTRACT_REVISION",
			),
			identityProtocolVersion: parseRustStringConstant(
				sources.hostedRunner,
				"HOSTED_RUNNER_IDENTITY_PROTOCOL_VERSION",
			),
			identityBindingProtocolVersion: parseRustStringConstant(
				sources.hostedRunner,
				"HOSTED_RUNNER_IDENTITY_BINDING_PROTOCOL_VERSION",
			),
			rendezvousProtocolVersion: parseRustStringConstant(
				sources.rendezvous,
				"RENDEZVOUS_PROTOCOL_VERSION",
			),
		},
		runtime: {
			schemaVersion: parseRustStringConstant(
				sources.runtimeProtocol,
				"HEADLESS_PROTOCOL_SCHEMA_VERSION",
			),
			contractDigest: jsonContractDigest(sources.runtimeFixture),
			receipt: buildRuntimeReceiptCompatibility(sources),
		},
		governedCode:
			threadV2 === null
				? null
				: {
						codeModes: capabilities.codeModes,
						clientCapability: "governed_code_mode",
						threadProtocolVersion: threadV2,
						toRuntimeMessages: toRuntime.filter((name) =>
							name.startsWith("governed_"),
						),
						fromRuntimeMessages: fromRuntime.filter((name) =>
							name.startsWith("governed_"),
						),
					},
	};

	const compatibilityDigest = `sha256:${createHash("sha256")
		.update(canonicalizeForDigest(compatibility), "utf8")
		.digest("hex")}`;
	const digestEncoding = {
		hash: "sha256",
		canonicalization: "rfc8785",
		encoding: "utf-8",
	};
	const buildIdentity = {
		sourceSha: normalizedSourceSha,
		buildDigest: normalizedBuildDigest,
		hooks: {
			sourceShaEnvironmentVariable: "MAESTRO_SOURCE_SHA",
			buildDigestEnvironmentVariable: "MAESTRO_BUILD_DIGEST",
		},
	};
	const receiptPayload = {
		schemaVersion: SCHEMA_VERSION,
		digestEncoding,
		compatibilityDigest,
		buildIdentity,
		generatedFrom: Object.entries(SOURCE_PATHS)
			.filter(([name]) => sources[name] !== undefined)
			.map(([, path]) => path),
		compatibility,
	};
	const receiptDigest = normalizedSourceSha
		? `sha256:${createHash("sha256")
				.update(canonicalizeForDigest(receiptPayload), "utf8")
				.digest("hex")}`
		: null;

	return {
		...receiptPayload,
		buildIdentity: {
			...buildIdentity,
			receiptDigest,
		},
	};
}

export function renderCompatibilityManifest(options = {}) {
	return `${JSON.stringify(buildCompatibilityManifest(options), null, 2)}\n`;
}

function main() {
	const options = parseArgs(process.argv.slice(2));
	const sourceSha = options.check
		? null
		: (options.sourceSha ?? process.env.MAESTRO_SOURCE_SHA ?? null);
	const buildDigest = options.check
		? null
		: (options.buildDigest ?? process.env.MAESTRO_BUILD_DIGEST ?? null);
	const rendered = renderCompatibilityManifest({
		sources: readCanonicalSources(options.sourceRoot),
		sourceSha,
		buildDigest,
	});

	if (options.check) {
		const current = readFileSync(options.output, "utf8");
		if (current !== rendered) {
			throw new Error(
				`${options.output} is stale; run node scripts/generate-protocol-compatibility-manifest.mjs`,
			);
		}
		console.log("Protocol compatibility manifest is current.");
		return;
	}

	writeFileSync(options.output, rendered);
	console.log(`Wrote ${options.output}`);
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
	try {
		main();
	} catch (error) {
		console.error(error instanceof Error ? error.message : String(error));
		process.exitCode = 1;
	}
}

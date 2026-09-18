#!/usr/bin/env node
import { spawn, spawnSync } from "node:child_process";
import {
	chmodSync,
	existsSync,
	mkdtempSync,
	readFileSync,
	symlinkSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";

// The agent only serves protocol versions it implements, so the smoke has to
// announce the version this build speaks rather than a placeholder.
const protocolVersion = readFileSync(
	resolve("packages/local-host-rs/src/headless/generated_protocol.rs"),
	"utf8",
).match(/pub const HEADLESS_PROTOCOL_VERSION: &str = "([^"]+)";/)?.[1];
if (!protocolVersion) throw new Error("could not read HEADLESS_PROTOCOL_VERSION");

const binary = resolve(process.argv[2] ?? "bin/maestro");
if (!existsSync(binary)) throw new Error(`Missing native package binary: ${binary}`);
const sandbox = mkdtempSync(join(tmpdir(), "maestro-native-smoke-"));
const pathDir = join(sandbox, "path");
await import("node:fs/promises").then(({ mkdir }) => mkdir(pathDir));
for (const utility of ["dirname", "tr", "uname"]) {
	symlinkSync(`/usr/bin/${utility}`, join(pathDir, utility));
}
chmodSync(binary, 0o755);
const env = {
	HOME: sandbox,
	MAESTRO_HOME: join(sandbox, ".maestro"),
	MAESTRO_DISABLE_KEYCHAIN: "1",
	MAESTRO_MODEL: "gpt-5.5",
	MAESTRO_WEB_REQUIRE_KEY: "0",
	MAESTRO_WEB_REQUIRE_REDIS: "0",
	OPENAI_API_KEY: "native-release-smoke-test-key",
	PATH: pathDir,
	TERM: "xterm-256color",
};

function run(args, input) {
	const result = spawnSync(binary, args, {
		encoding: "utf8",
		env,
		input,
		timeout: 30_000,
	});
	if (result.status !== 0) {
		throw new Error(`${args.join(" ")} failed:\n${result.stderr}\n${result.stdout}`);
	}
	return result.stdout;
}

if (!run(["--version"]).startsWith("deixic-code "))
	throw new Error("version smoke failed");
if (!run(["--help"]).includes("Usage:")) throw new Error("help smoke failed");
const headlessInput = `${JSON.stringify({ type: "hello", protocol_version: protocolVersion, client_info: { name: "native-smoke", version: "1" }, role: "controller" })}\n${JSON.stringify({ type: "shutdown" })}\n`;
const headlessResult = spawnSync(binary, ["--headless"], {
	encoding: "utf8",
	env,
	input: headlessInput,
	timeout: 30_000,
});
if (headlessResult.status === 0) {
	const headless = headlessResult.stdout;
	if (
		!headless.includes('"type":"ready"') ||
		!headless.includes('"model":"gpt-5.5"') ||
		!headless.includes('"type":"hello_ok"')
	)
		throw new Error("headless smoke failed");
} else {
	const output = `${headlessResult.stderr}\n${headlessResult.stdout}`;
	if (!output.includes("An EvalOps Identity account is required")) {
		throw new Error(`--headless failed:\n${output}`);
	}
}

const scenario = resolve("test/fixtures/scripted-replay/basic.json");
if (existsSync(scenario)) {
	const scenarioResult = spawnSync(binary, ["scenario", "run", scenario, "--json"], {
		encoding: "utf8",
		env,
		timeout: 30_000,
	});
	if (scenarioResult.status !== 0) {
		const output = `${scenarioResult.stderr}\n${scenarioResult.stdout}`;
		if (!output.includes("An EvalOps Identity account is required")) {
			throw new Error(`scenario run ${scenario} failed:\n${output}`);
		}
	}
}

const port = 31_000 + Math.floor(Math.random() * 1_000);
const server = spawn(binary, ["serve", "--port", String(port)], {
	env,
	stdio: ["ignore", "pipe", "pipe"],
});
try {
	let healthy = false;
	for (let attempt = 0; attempt < 50; attempt++) {
		try {
			const response = await fetch(`http://127.0.0.1:${port}/healthz`);
			if (response.ok && (await response.text()).trim() === "ok") {
				healthy = true;
				break;
			}
		} catch {}
		await new Promise((resolveDelay) => setTimeout(resolveDelay, 100));
	}
	if (!healthy) throw new Error("native serve health smoke failed");
} finally {
	server.kill("SIGTERM");
}

console.log("Rust-only release smoke passed with no Node, npm, Bun, or npx in child PATH.");

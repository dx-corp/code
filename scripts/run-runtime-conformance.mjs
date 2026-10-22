#!/usr/bin/env node

import assert from "node:assert/strict";
import { execFile, spawn } from "node:child_process";
import { createHash } from "node:crypto";
import { createServer } from "node:net";
import {
	mkdtempSync,
	readFileSync,
	rmSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { promisify } from "node:util";
import { fileURLToPath } from "node:url";

const execFileAsync = promisify(execFile);
const ROOT = resolve(fileURLToPath(new URL("..", import.meta.url)));
const CONFORMANCE_FIXTURE = JSON.parse(
	readFileSync(resolve(ROOT, "packages/runtime-contracts-rs/fixtures/runtime-conformance-v1.json"), "utf8"),
);
const AUTH_TOKEN = "runtime-conformance-local-only";
const SESSION_ID = "runtime-conformance-session-v1";
const RUNNER_SESSION_ID = "runtime-conformance-runner-v1";
const CONFORMANCE_FIXTURE_FILE = "runtime-conformance-fixture.txt";
const CONFORMANCE_PROFILE = CONFORMANCE_FIXTURE.profile;
const FETCH_TIMEOUT_MS = 15_000;
// Container creation on a loaded publisher runner can exceed an HTTP fetch
// budget right after a large image export; bound the Docker CLI separately.
const DOCKER_CLI_TIMEOUT_MS = 120_000;
const CONFORMANCE_EXECUTION_CASES = Object.freeze([
	"startup_identity_and_readiness",
	"wrong_session_rejected",
	"harmless_shell_command",
	"file_search_and_read",
	"approval_request_and_resolution",
	"idempotent_response_replay",
	"drain_terminal_receipt",
]);

export function conformanceFixtureCommand() {
	return `printf 'runtime conformance fixture\\nreversible test data\\n' > ${CONFORMANCE_FIXTURE_FILE}; printf runtime-conformance-shell`;
}

export function dockerImageRefForRun(dockerImage) {
	const pinned = dockerImage.match(/^(?<name>[^@]+)@(?<digest>sha256:[a-f0-9]{64})$/i);
	if (pinned?.groups && !pinned.groups.name.includes("/")) {
		// Local tags pinned by image id (`maestro-conformance:run@sha256:...`)
		// are not registry references. `docker run name@sha256` tries Docker Hub.
		return pinned.groups.digest.toLowerCase();
	}
	return dockerImage;
}

export function dockerConformanceRunArgs({ containerName, dockerImage }) {
	return [
		"run",
		"-d",
		// The fixture waits for stdin EOF as its shutdown signal. Keep stdin open
		// in detached mode so the container remains alive for HTTP conformance.
		"-i",
		"--rm",
		"--name",
		containerName,
		"-p",
		"127.0.0.1::8080",
		"-e",
		`MAESTRO_RUNNER_SESSION_ID=${RUNNER_SESSION_ID}`,
		"-e",
		`MAESTRO_SESSION_ID=${SESSION_ID}`,
		"-e",
		"MAESTRO_WORKSPACE_ROOT=/conformance-workspace",
		"-e",
		"MAESTRO_HOSTED_RUNNER_LISTEN=0.0.0.0:8080",
		"-e",
		`MAESTRO_HOSTED_RUNNER_AUTH_TOKEN=${AUTH_TOKEN}`,
		"-e",
		"MAESTRO_MODEL=conformance-fixture",
		// The release runner's Docker daemon is containerized. A host-path bind
		// mount from the Node runner can therefore resolve to an empty daemon-side
		// directory. Keep the workspace in the daemon; the conformance shell case
		// creates its reversible fixture through Maestro after startup.
		"--mount",
		"type=tmpfs,destination=/conformance-workspace",
		dockerImageRefForRun(dockerImage),
		"conformance",
	];
}

function parseArgs(argv) {
	const options = { binary: null, dockerImage: null, binaryLauncher: null, artifactDigest: null };
	for (let index = 0; index < argv.length; index += 1) {
		const argument = argv[index];
		if (argument === "--binary") options.binary = argv[++index] ?? "";
		else if (argument === "--docker-image") options.dockerImage = argv[++index] ?? "";
		else if (argument === "--binary-launcher") options.binaryLauncher = argv[++index] ?? "";
		else if (argument === "--artifact-digest") options.artifactDigest = argv[++index] ?? "";
		else throw new Error(`Unknown argument: ${argument}`);
	}
	if ((options.binary === null) === (options.dockerImage === null) ||
		(options.binary !== null && !options.binary) ||
		(options.dockerImage !== null && !options.dockerImage)) {
		throw new Error("provide exactly one of --binary or --docker-image");
	}
	if (options.binaryLauncher && options.binary === null) {
		throw new Error("--binary-launcher requires --binary");
	}
	if (options.artifactDigest !== null && !/^sha256:[a-f0-9]{64}$/.test(options.artifactDigest)) {
		throw new Error("--artifact-digest must be a sha256 digest");
	}
	if (options.dockerImage !== null) {
		const imageDigest =
			options.dockerImage.match(/@(?<digest>sha256:[a-f0-9]{64})$/i)?.groups?.digest?.toLowerCase();
		if (!imageDigest) {
			throw new Error("--docker-image must be pinned by @sha256:<digest> for immutable conformance");
		}
		if (options.artifactDigest !== null && options.artifactDigest !== imageDigest) {
			throw new Error("--artifact-digest must match the digest pinned in --docker-image");
		}
		options.artifactDigest ??= imageDigest;
	}
	return options;
}

function sha256(value) {
	return `sha256:${createHash("sha256").update(value).digest("hex")}`;
}

function parseJsonLines(value) {
	return value
		.split("\n")
		.map((line) => line.trim())
		.filter(Boolean)
		.flatMap((line) => {
			try {
				return [JSON.parse(line)];
			} catch {
				return [];
			}
		});
}

async function waitFor(check, label, timeoutMs = 15_000) {
	const deadline = Date.now() + timeoutMs;
	let lastError = null;
	while (Date.now() < deadline) {
		try {
			const value = await check();
			if (value) return value;
		} catch (error) {
			lastError = error;
		}
		await new Promise((resolvePromise) => setTimeout(resolvePromise, 100));
	}
	throw new Error(`${label} timed out${lastError ? `: ${lastError.message}` : ""}`);
}

async function waitWithTimeout(promise, label, timeoutMs = FETCH_TIMEOUT_MS) {
	let timer;
	try {
		return await Promise.race([
			promise,
			new Promise((_, reject) => {
				timer = setTimeout(() => reject(new Error(`${label} timed out`)), timeoutMs);
			}),
		]);
	} finally {
		clearTimeout(timer);
	}
}

export function validateConformanceReceipt(receipt) {
	assert.deepEqual(
		receipt.cases.map((entry) => entry.name),
		CONFORMANCE_FIXTURE.cases,
		"conformance receipt must cover every canonical fixture case exactly once",
	);
	assert.ok(
		receipt.cases.every((entry) => entry.outcome === "passed"),
		"expected negative cases must be recorded as passed after their rejection is verified",
	);
}

export function validateConformanceExecutionOrder(executedCases) {
	assert.deepEqual(
		executedCases,
		CONFORMANCE_EXECUTION_CASES,
		"conformance must create the daemon-local fixture before file search and cover every runtime case",
	);
}

async function fetchWithTimeout(url, init, label, consume = (response) => response) {
	const controller = new AbortController();
	const timer = setTimeout(() => controller.abort(), FETCH_TIMEOUT_MS);
	try {
		const response = await fetch(url, { ...init, signal: controller.signal });
		return await consume(response);
	} catch (error) {
		if (controller.signal.aborted) throw new Error(`${label} timed out`);
		throw error;
	} finally {
		clearTimeout(timer);
	}
}

async function freeTcpPort() {
	const server = createServer();
	await new Promise((resolvePromise, reject) => {
		server.once("error", reject);
		server.listen(0, "127.0.0.1", resolvePromise);
	});
	const address = server.address();
	const port = typeof address === "object" && address ? address.port : 0;
	await new Promise((resolvePromise) => server.close(resolvePromise));
	if (!port) throw new Error("failed to allocate a local conformance port");
	return port;
}

async function startRuntime(options, workspace) {
	if (options.binary) {
		const binaryPath = resolve(options.binary);
		const port = await freeTcpPort();
		const command = options.binaryLauncher || binaryPath;
		const commandArgs = options.binaryLauncher ? [binaryPath, "conformance"] : ["conformance"];
		const child = spawn(command, commandArgs, {
			cwd: ROOT,
			env: {
				...process.env,
				MAESTRO_RUNNER_SESSION_ID: RUNNER_SESSION_ID,
				MAESTRO_SESSION_ID: SESSION_ID,
				MAESTRO_WORKSPACE_ROOT: workspace,
				MAESTRO_HOSTED_RUNNER_LISTEN: `127.0.0.1:${port}`,
				MAESTRO_HOSTED_RUNNER_AUTH_TOKEN: AUTH_TOKEN,
				MAESTRO_MODEL: "conformance-fixture",
			},
			stdio: ["pipe", "pipe", "inherit"],
		});
		const exitPromise = new Promise((resolvePromise) => {
			child.once("close", (code, signal) => resolvePromise({ code, signal }));
		});
		let stdout = "";
		child.stdout.setEncoding("utf8");
		child.stdout.on("data", (chunk) => {
			stdout += chunk;
		});
		let baseUrl;
		try {
			baseUrl = await waitFor(
				async () => parseJsonLines(stdout).find((line) => line.baseUrl)?.baseUrl,
				"native conformance startup",
			);
		} catch (error) {
			child.kill("SIGTERM");
			try {
				await waitWithTimeout(exitPromise, "native conformance startup cleanup");
			} catch {
				child.kill("SIGKILL");
			}
			throw error;
		}
		return {
			baseUrl,
			headers: { authorization: `Bearer ${AUTH_TOKEN}` },
			stop: async () => {
				if (child.exitCode === null && child.signalCode === null) child.stdin.end();
				let outcome;
				try {
					outcome = await waitWithTimeout(exitPromise, "native conformance shutdown");
				} catch (error) {
					if (child.exitCode === null && child.signalCode === null) child.kill("SIGTERM");
					throw error;
				}
				if (outcome.code !== 0 && outcome.code !== null) {
					throw new Error(`native conformance exited ${outcome.code} (${outcome.signal ?? "no signal"})`);
				}
			},
		};
	}

	const containerName = `maestro-runtime-conformance-${process.pid}`;
	const dockerExec = (args) =>
		execFileAsync("docker", args, {
			timeout: DOCKER_CLI_TIMEOUT_MS,
			killSignal: "SIGTERM",
			maxBuffer: 1024 * 1024,
		});
	const removeContainer = async () => {
		try {
			await dockerExec(["rm", "-f", containerName]);
		} catch {
			// --rm may already have removed a container that exited on failure.
		}
	};
	try {
		const { stdout: containerOutput } = await dockerExec(
			dockerConformanceRunArgs({
				containerName,
				dockerImage: options.dockerImage,
			}),
		);
		const containerId = containerOutput.trim();
		const { stdout: portOutput } = await dockerExec(["port", containerId, "8080/tcp"]);
		const port = portOutput.trim().match(/:(\d+)\s*$/)?.[1];
		if (!port) throw new Error(`docker did not publish conformance port: ${portOutput}`);
		return {
			baseUrl: `http://127.0.0.1:${port}`,
			headers: { authorization: `Bearer ${AUTH_TOKEN}` },
			stop: removeContainer,
		};
	} catch (error) {
		await removeContainer();
		throw error;
	}
}

function sseCollector(response) {
	const reader = response.body.getReader();
	const decoder = new TextDecoder();
	let buffer = "";
	const queue = [];
	const waiters = [];
	let closed = false;
	const readWithTimeout = async () => {
		let timer;
		try {
			return await Promise.race([
				reader.read(),
				new Promise((_, reject) => {
					timer = setTimeout(() => reject(new Error("SSE event stream timed out")), FETCH_TIMEOUT_MS);
				}),
			]);
		} finally {
			clearTimeout(timer);
		}
	};

	const deliver = (event) => {
		const waiter = waiters.findIndex((candidate) => candidate.predicate(event));
		if (waiter >= 0) {
			const [{ resolve: resolveWaiter }] = waiters.splice(waiter, 1);
			resolveWaiter(event);
		} else queue.push(event);
	};

	const pump = (async () => {
		try {
			while (true) {
				const { done, value } = await readWithTimeout();
				if (done) break;
				buffer += decoder.decode(value, { stream: true });
				const blocks = buffer.split("\n\n");
				buffer = blocks.pop() ?? "";
				for (const block of blocks) {
					const data = block
						.split("\n")
						.find((line) => line.startsWith("data: "))
						?.slice("data: ".length);
					if (data) deliver(JSON.parse(data));
				}
			}
		} finally {
			closed = true;
			for (const waiter of waiters.splice(0)) {
				waiter.reject(new Error("conformance SSE stream closed"));
			}
		}
	})();

	return {
		async next(predicate, label) {
			const existing = queue.findIndex(predicate);
			if (existing >= 0) return queue.splice(existing, 1)[0];
			if (closed) throw new Error(`SSE closed before ${label}`);
			return new Promise((resolvePromise, reject) => {
				const waiter = {
					predicate,
					resolve: (event) => {
						clearTimeout(waiter.timer);
						resolvePromise(event);
					},
					reject: (error) => {
						clearTimeout(waiter.timer);
						reject(error);
					},
					timer: setTimeout(() => {
						const index = waiters.indexOf(waiter);
						if (index >= 0) waiters.splice(index, 1);
						reject(new Error(`${label} timed out`));
					}, 15_000),
				};
				waiters.push(waiter);
			});
		},
		async close() {
			await reader.cancel();
			await pump;
		},
	};
}

async function main() {
	const options = parseArgs(process.argv.slice(2));
	const binaryPath = options.binary ? resolve(options.binary) : null;
	const workspace = mkdtempSync(join(tmpdir(), "maestro-runtime-conformance-"));
	let runtime;
	let events;
	let primaryError;
	const receipt = {
		schemaVersion: "evalops.maestro.runtime-conformance.v1",
		profile: CONFORMANCE_PROFILE,
		artifact: binaryPath ?? options.dockerImage,
		artifactDigest:
			options.artifactDigest ??
			(binaryPath ? sha256(readFileSync(binaryPath)) : options.dockerImage.split("@")[1] ?? null),
		cases: [],
	};
	try {
		runtime = await startRuntime(options, workspace);
		await waitFor(
			async () =>
				fetchWithTimeout(
					`${runtime.baseUrl}/readyz`,
					{ headers: runtime.headers },
					"runtime readiness",
					async (response) => {
						const ok = response.ok;
						await response.arrayBuffer();
						return ok;
					},
				),
			"runtime readiness",
		);
		const request = async (path, init = {}) => {
			return fetchWithTimeout(
				`${runtime.baseUrl}${path}`,
				{
					...init,
					headers: { ...runtime.headers, ...(init.headers ?? {}) },
				},
				`request ${path}`,
				async (response) => {
					const text = await response.text();
					let body = null;
					try {
						body = text ? JSON.parse(text) : null;
					} catch {
						body = text;
					}
					return { status: response.status, body };
				},
			);
		};
		const record = (name, identity, outcome) => {
			receipt.cases.push({ name, tool: identity, outcome });
		};
		const executedCases = [];
		const markExecuted = (name) => executedCases.push(name);

		const identity = await request("/.well-known/evalops/remote-runner/identity");
		assert.equal(identity.status, 200);
		assert.equal(identity.body.runner_session_id, RUNNER_SESSION_ID);
		markExecuted("startup_identity_and_readiness");
		record("startup_identity_and_readiness", "GET identity + GET /readyz", "passed");

		const wrongSession = await request("/api/headless/connections", {
			method: "POST",
			headers: { "content-type": "application/json" },
			body: JSON.stringify({ sessionId: "wrong-session", connectionId: "wrong" }),
		});
		assert.ok(wrongSession.status >= 400 && wrongSession.status < 500);
		markExecuted("wrong_session_rejected");
		record("wrong_session_rejected", "POST /api/headless/connections", "passed");

		const connection = await request("/api/headless/connections", {
			method: "POST",
			headers: { "content-type": "application/json" },
			body: JSON.stringify({
				sessionId: SESSION_ID,
				connectionId: "conformance-controller",
				connectionCapabilityRequired: true,
				protocolVersion: "2026-08-08",
				role: "controller",
			}),
		});
		assert.equal(connection.status, 200);
		const capability = connection.body.connection_capability;
		assert.ok(capability);
		const subscription = await request(`/api/headless/sessions/${SESSION_ID}/subscribe`, {
			method: "POST",
			headers: { "content-type": "application/json" },
			body: JSON.stringify({
				connectionId: "conformance-controller",
				connectionCapability: capability,
				connectionCapabilityRequired: true,
				role: "controller",
			}),
		});
		assert.equal(subscription.status, 200);
		const subscriptionId = subscription.body.subscription_id;
		const commandHeaders = {
			"content-type": "application/json",
			"x-maestro-headless-connection-id": "conformance-controller",
			"x-maestro-headless-subscriber-id": subscriptionId,
			"x-maestro-headless-connection-capability": capability,
		};
		const streamResponse = await fetchWithTimeout(
			`${runtime.baseUrl}/api/headless/sessions/${SESSION_ID}/events?cursor=0&subscriptionId=${encodeURIComponent(subscriptionId)}`,
			{ headers: runtime.headers },
			"SSE stream startup",
		);
		assert.equal(streamResponse.status, 200);
		events = sseCollector(streamResponse);

		const shell = await request(`/api/headless/sessions/${SESSION_ID}/messages`, {
			method: "POST",
			headers: commandHeaders,
			body: JSON.stringify({
				type: "utility_command_start",
				command_id: "command-1",
				command: conformanceFixtureCommand(),
				cwd: ".",
				shell_mode: "shell",
				terminal_mode: "pipe",
				allow_stdin: false,
			}),
		});
		assert.equal(shell.status, 200);
		const outputEvent = await events.next(
			(event) => event.message?.type === "utility_command_output" && event.message.command_id === "command-1",
			"harmless shell output",
		);
		assert.match(outputEvent.message.content, /runtime-conformance-shell/);
		const shellExit = await events.next(
			(event) => event.message?.type === "utility_command_exited" && event.message.command_id === "command-1",
			"harmless shell exit",
		);
		assert.equal(shellExit.message.success, true);
		assert.equal(shellExit.message.exit_code, 0);
		markExecuted("harmless_shell_command");

		const search = await request(`/api/headless/sessions/${SESSION_ID}/messages`, {
			method: "POST",
			headers: commandHeaders,
			body: JSON.stringify({
				type: "utility_file_search",
				search_id: "search-1",
				query: "fixture",
				cwd: ".",
				limit: 10,
			}),
		});
		assert.equal(search.status, 200);
		const searchEvent = await events.next(
			(event) => event.message?.type === "utility_file_search_results" && event.message.search_id === "search-1",
			"file search result",
		);
		assert.ok(
			searchEvent.message.results.some((match) => match.path.endsWith(CONFORMANCE_FIXTURE_FILE)),
			`file search results: ${JSON.stringify(searchEvent.message.results)}`,
		);

		const read = await request(`/api/headless/sessions/${SESSION_ID}/messages`, {
			method: "POST",
			headers: commandHeaders,
			body: JSON.stringify({
				type: "utility_file_read",
				read_id: "read-1",
				path: CONFORMANCE_FIXTURE_FILE,
				cwd: ".",
				offset: 0,
				limit: 10,
			}),
		});
		assert.equal(read.status, 200);
		const readEvent = await events.next(
			(event) => event.message?.type === "utility_file_read_result" && event.message.read_id === "read-1",
			"file read result",
		);
		assert.match(readEvent.message.content, /reversible test data/);
		markExecuted("file_search_and_read");
		record("file_search_and_read", "utility_file_search + utility_file_read", "passed");
		record("harmless_shell_command", "utility_command_start", "passed");

		const requestId = "approval-1";
		const approval = await request(`/api/headless/sessions/${SESSION_ID}/messages`, {
			method: "POST",
			headers: commandHeaders,
			body: JSON.stringify({
				type: "prompt",
				content: `${"__maestro_conformance_approval__:"}${JSON.stringify({
					request_id: requestId,
					tool: "harmless_shell",
					args: { command: "printf approved" },
					reason: "conformance approval path",
				})}`,
			}),
		});
		assert.equal(approval.status, 200);
		const requestEvent = await events.next(
			(event) => event.message?.type === "server_request" && event.message.request_id === requestId,
			"approval request",
		);
		assert.equal(requestEvent.message.request_type, "approval");
		assert.equal(requestEvent.message.call_id, requestId);
		assert.equal(requestEvent.message.tool, "harmless_shell");
		assert.deepEqual(requestEvent.message.args, { command: "printf approved" });
		assert.equal(requestEvent.message.reason, "conformance approval path");
		const response = {
			type: "server_request_response",
			request_id: requestId,
			request_type: "approval",
			approved: true,
		};
		const responseHeaders = { ...commandHeaders, "x-maestro-idempotency-key": "approval-response-1" };
		const resolved = await request(`/api/headless/sessions/${SESSION_ID}/messages`, {
			method: "POST",
			headers: responseHeaders,
			body: JSON.stringify(response),
		});
		assert.equal(resolved.status, 200);
		const resolvedEvent = await events.next(
			(event) => event.message?.type === "server_request_resolved" && event.message.request_id === requestId,
			"approval resolution",
		);
		assert.equal(resolvedEvent.message.resolution, "approved");
		const replay = await request(`/api/headless/sessions/${SESSION_ID}/messages`, {
			method: "POST",
			headers: responseHeaders,
			body: JSON.stringify(response),
		});
		assert.equal(replay.status, 200);
		assert.equal(replay.body.replayed, true);
		markExecuted("approval_request_and_resolution");
		markExecuted("idempotent_response_replay");
		record("approval_request_and_resolution", "prompt + server_request_response", "passed");
		record("idempotent_response_replay", "x-maestro-idempotency-key", "passed");

		const drain = await request("/.well-known/evalops/remote-runner/drain", {
			method: "POST",
			headers: { "content-type": "application/json" },
			body: JSON.stringify({ requested_by: "runtime-conformance", reason: "conformance" }),
		});
		assert.equal(drain.status, 200);
		assert.equal(drain.body.status, "drained");
		assert.equal(drain.body.runtime_receipt.kind, "drained");
		markExecuted("drain_terminal_receipt");
		record("drain_terminal_receipt", "POST /.well-known/evalops/remote-runner/drain", "passed");

		validateConformanceExecutionOrder(executedCases);
		validateConformanceReceipt(receipt);

		console.log(JSON.stringify(receipt));
	} catch (error) {
		primaryError = error;
	} finally {
		const cleanupErrors = [];
		if (events) {
			try {
				await events.close();
			} catch (error) {
				cleanupErrors.push(error);
			}
		}
		if (runtime) {
			try {
				await runtime.stop();
			} catch (error) {
				cleanupErrors.push(error);
			}
		}
		rmSync(workspace, { recursive: true, force: true });
		if (primaryError) {
			if (cleanupErrors.length) {
				primaryError.message += `; cleanup failed: ${cleanupErrors.map((error) => error.message).join("; ")}`;
			}
			throw primaryError;
		}
		if (cleanupErrors.length) throw new AggregateError(cleanupErrors, "conformance cleanup failed");
	}
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
	await main();
}

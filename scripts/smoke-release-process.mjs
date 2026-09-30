import { spawnSync } from "node:child_process";

const DEFAULT_TIMEOUT_MS = 30_000;

export function firstLaunchTimeoutMs(platform, releasePlatform) {
	// Cold Rosetta translation of the signed Intel release can exceed the
	// ordinary smoke budget. Keep the launch bounded and test every exit.
	return platform === "darwin" && releasePlatform === "darwin-x64"
		? 120_000
		: DEFAULT_TIMEOUT_MS;
}

export function runNativeSmokeCommand(binary, args, { env, input, timeoutMs = DEFAULT_TIMEOUT_MS }) {
	const result = spawnSync(binary, args, {
		encoding: "utf8",
		env,
		input,
		timeout: timeoutMs,
	});
	if (result.status !== 0) {
		const details = [
			`status=${result.status ?? "none"}`,
			`signal=${result.signal ?? "none"}`,
			`error=${result.error?.code ?? "none"}`,
			`timeout=${timeoutMs}ms`,
		].join(", ");
		throw new Error(`${args.join(" ")} failed (${details}):\n${result.stderr ?? ""}\n${result.stdout ?? ""}`);
	}
	return result.stdout;
}

import assert from "node:assert/strict";
import { chmodSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import { firstLaunchTimeoutMs, runNativeSmokeCommand } from "./smoke-release-process.mjs";

test("only Darwin x64 gets a longer first-launch smoke budget", () => {
	assert.equal(firstLaunchTimeoutMs("darwin", "darwin-x64"), 120_000);
	assert.equal(firstLaunchTimeoutMs("darwin", "darwin-arm64"), 30_000);
	assert.equal(firstLaunchTimeoutMs("linux", "darwin-x64"), 30_000);
});

test("a timed-out native binary reports the timeout, signal, and spawn error", () => {
	const directory = mkdtempSync(join(tmpdir(), "maestro-smoke-process-"));
	try {
		const binary = join(directory, "slow-binary");
		writeFileSync(binary, "#!/bin/sh\n/bin/sleep 1\n");
		chmodSync(binary, 0o755);
		assert.throws(
			() => runNativeSmokeCommand(binary, ["--version"], { env: process.env, timeoutMs: 20 }),
			(error) => {
				assert.match(error.message, /--version failed/);
				assert.match(error.message, /error=ETIMEDOUT/);
				assert.match(error.message, /timeout=20ms/);
				return true;
			},
		);
	} finally {
		rmSync(directory, { recursive: true, force: true });
	}
});

test("a native binary with a successful exit returns its output", () => {
	const directory = mkdtempSync(join(tmpdir(), "maestro-smoke-process-"));
	try {
		const binary = join(directory, "version-binary");
		writeFileSync(binary, "#!/bin/sh\nprintf 'deixic-code 0.10.110\\n'\n");
		chmodSync(binary, 0o755);
		assert.equal(
			runNativeSmokeCommand(binary, ["--version"], { env: process.env }),
			"deixic-code 0.10.110\n",
		);
	} finally {
		rmSync(directory, { recursive: true, force: true });
	}
});

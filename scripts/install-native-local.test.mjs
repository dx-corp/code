import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { chmodSync, existsSync, mkdirSync, mkdtempSync, readFileSync, readlinkSync, realpathSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";

const installer = new URL("install-native-local.sh", import.meta.url).pathname;
function fixture(t, { signingFails = false, noIdentity = false, smokeFails = false, capacityFails = false } = {}) {
	const root = mkdtempSync(join(tmpdir(), "maestro-local-install-"));
	t.after(() => rmSync(root, { recursive: true, force: true }));
	const bin = join(root, "tools");
	const prefix = join(root, "install");
	const target = join(root, "shared target", "debug");
	mkdirSync(bin); mkdirSync(prefix); mkdirSync(target, { recursive: true });
	const executable = join(target, "maestro");
	const native = `#!${process.execPath}
const args = process.argv.slice(2);
if (args[0] === '--version') console.log('deixic-code 1.2.3');
else if (args[0] === '--help') { ${smokeFails ? "process.exit(1);" : "console.log('Usage: fixture');"} }
else if (args[0] === '--headless') {
  require('node:fs').readFileSync(0);
  console.log(JSON.stringify({type:'hello_ok'}));
  console.log(JSON.stringify({type:'ready',model:'gpt-5.5'}));
} else if (args[0] === 'serve') {
  require('node:http').createServer((req, res) => res.end('ok')).listen(Number(args[args.indexOf('--port')+1]), '127.0.0.1');
}
`;
	writeFileSync(executable, native);
	chmodSync(executable, 0o755);
	writeFileSync(join(prefix, "maestro"), "previous trusted binary");
	const tool = (name, body) => {
		writeFileSync(join(bin, name), `#!/bin/sh\n${body}\n`);
		chmodSync(join(bin, name), 0o755);
	};
	tool("uname", "echo Darwin");
	tool("make", capacityFails ? "exit 1" : ":");
	tool("cargo", `printf '%s' "$CARGO_BUILD_JOBS" > "$FIXTURE_ROOT/jobs"\nprintf '%s\\n' '${JSON.stringify({ reason: "compiler-artifact", target: { name: "maestro", kind: ["bin"] }, executable })}'`);
	tool("codesign", `printf '%s\\n' "$*" >> "$FIXTURE_ROOT/codesign.log"\ncase "$1" in\n -dv) printf '%s\\n' 'Identifier=maestro' 'TeamIdentifier=TEAMID1234' 'Authority=Developer ID Application: Fixture (TEAMID1234)' >&2;;\n --force) ${signingFails ? "exit 1" : ":"};;\nesac`);
	tool("security", noIdentity ? "echo '0 valid identities found'" : "echo '  1) ABC \"Developer ID Application: Fixture (TEAMID1234)\"'");
	return { root, prefix, executable, native, run: (extraEnv = {}) => spawnSync("bash", [installer], {
		encoding: "utf8", env: { ...process.env, PATH: `${bin}:${process.env.PATH}`, FIXTURE_ROOT: root,
			MAESTRO_INSTALL_PREFIX: prefix, MAESTRO_CARGO_PROFILE: "dev", CARGO_TARGET_DIR: join(root, "shared target"),
			MAESTRO_RELEASE_DEVELOPER_ID_AUTHORITY: "Developer ID Application: Fixture (TEAMID1234)",
			MAESTRO_RELEASE_DEVELOPER_ID_TEAM_IDENTIFIER: "TEAMID1234", ...extraEnv },
	}) };
}

test("local install signs the actual Cargo artifact and preserves the build cache", (t) => {
	const f = fixture(t);
	const result = f.run();
	assert.equal(result.status, 0, result.stderr);
	for (const name of ["maestro", "maestro-tui", "deixic-code"]) {
		assert.equal(readFileSync(join(f.prefix, name), "utf8"), f.native);
		assert.equal(realpathSync(join(f.prefix, name)), realpathSync(join(f.prefix, "maestro")));
	}
	assert.equal(readFileSync(f.executable, "utf8"), f.native);
	const signing = readFileSync(join(f.root, "codesign.log"), "utf8");
	assert.match(signing, /--force --identifier maestro --options runtime --timestamp --sign Developer ID Application/);
	assert.ok(!signing.includes(f.executable), "sign a staged copy, never Cargo's cached artifact");
});

test("failed signing leaves the previous installation intact", (t) => {
	const f = fixture(t, { signingFails: true });
	assert.notEqual(f.run().status, 0);
	assert.equal(readFileSync(join(f.prefix, "maestro"), "utf8"), "previous trusted binary");
	assert.ok(!existsSync(join(f.prefix, "deixic-code")));
});

test("failed preflight or staged smoke leaves existing commands untouched", (t) => {
	for (const options of [{ capacityFails: true }, { smokeFails: true }]) {
		const f = fixture(t, options);
		assert.notEqual(f.run().status, 0);
		assert.equal(readFileSync(join(f.prefix, "maestro"), "utf8"), "previous trusted binary");
		assert.ok(!existsSync(join(f.prefix, "deixic-code")));
		assert.ok(!existsSync(join(f.prefix, ".maestro-local", "current")));
	}
});

test("managed upgrades switch one pointer and retain the previous build", (t) => {
	const f = fixture(t);
	assert.equal(f.run({ CARGO_BUILD_JOBS: "16" }).status, 0);
	assert.equal(readFileSync(join(f.root, "jobs"), "utf8"), "4");
	const current = join(f.prefix, ".maestro-local", "current");
	const previous = realpathSync(current);
	const aliases = ["maestro", "deixic-code", "maestro-tui"].map(name => readlinkSync(join(f.prefix, name)));
	writeFileSync(f.executable, f.native.replace('1.2.3', '1.2.4'));
	const result = f.run({ CARGO_BUILD_JOBS: "2" });
	assert.equal(result.status, 0, result.stderr);
	assert.equal(readFileSync(join(f.root, "jobs"), "utf8"), "2");
	assert.notEqual(realpathSync(current), previous);
	assert.equal(readFileSync(join(previous, "maestro"), "utf8"), f.native);
	for (const [index, name] of ["maestro", "deixic-code", "maestro-tui"].entries()) {
		assert.equal(readlinkSync(join(f.prefix, name)), aliases[index]);
		assert.match(readFileSync(join(f.prefix, name), "utf8"), /1\.2\.4/);
	}
	const active = realpathSync(current);
	writeFileSync(f.executable, f.native.replace("console.log('Usage: fixture');", "process.exit(1);"));
	assert.notEqual(f.run().status, 0);
	assert.equal(realpathSync(current), active);
	for (const name of ["maestro", "deixic-code", "maestro-tui"]) {
		assert.match(readFileSync(join(f.prefix, name), "utf8"), /1\.2\.4/);
	}
});

test("a directory in an alias slot fails without replacing existing commands", (t) => {
	const f = fixture(t);
	mkdirSync(join(f.prefix, "deixic-code"));
	assert.notEqual(f.run().status, 0);
	assert.equal(readFileSync(join(f.prefix, "maestro"), "utf8"), "previous trusted binary");
});

test("missing Developer ID refuses to overwrite a trusted installation", (t) => {
    const f = fixture(t, { noIdentity: true });
    const result = f.run();
    assert.notEqual(result.status, 0);
    assert.match(result.stderr, /identity is unavailable/);
    assert.equal(readFileSync(join(f.prefix, "maestro"), "utf8"), "previous trusted binary");
});

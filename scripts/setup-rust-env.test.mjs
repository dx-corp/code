import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { mkdtempSync, readFileSync, rmSync, writeFileSync, mkdirSync, statSync, symlinkSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";

test("copied rustup recreates the env file sourced by an existing login profile", () => {
  const root = mkdtempSync(join(tmpdir(), "maestro-rust-env-"));
  try {
    const action = readFileSync(".github/actions/setup-rust/action.yml", "utf8");
    const step = action.split("      run: | # zizmor:")[1].split("\n    - name:")[0];
    const script = step.split("\n").slice(1).map((line) => line.startsWith("        ") ? line.slice(8) : line).join("\n")
      .replace("cache_root=/tmp/maestro-rust", `cache_root="${root}/cache"`);
    const bin = join(root, "bin"); mkdirSync(bin);
    writeFileSync(join(bin, "rustup"), "#!/bin/sh\nexit 0\n", {mode: 0o755});
    const env = {...process.env, GITHUB_ACTIONS: "false", PATH: `${bin}:${process.env.PATH}`, RUNNER_OS: "Linux", GITHUB_REPOSITORY: "conformance/env", TOOLCHAIN_INPUT: "1.95.0", WORKSPACES_INPUT: ".", GITHUB_ENV: join(root,"env"), GITHUB_PATH: join(root,"path"), GITHUB_OUTPUT: join(root,"output")};
    const cargoHome = join(root,"cache/conformance-env/1.95.0/home/cargo");
    const profile = join(root,"profile"); writeFileSync(profile, `. "${cargoHome}/env"\n`);
    for (let run = 0; run < 2; run++) {
      execFileSync("bash", ["-c", script], {env, cwd: process.cwd()});
      assert.ok(readFileSync(join(cargoHome, "env"), "utf8").includes("$PATH"), "env retains runtime PATH expansion");
      const output = execFileSync("sh", ["-c", `. "${profile}"; printf runtime-conformance-shell`], {env, encoding: "utf8"});
      assert.equal(output, "runtime-conformance-shell");
      const path = execFileSync("sh", ["-c", `. "${profile}"; . "${profile}"; printf '%s' "$PATH"`], {env, encoding:"utf8"});
      assert.equal(path.split(":").filter((entry) => entry === `${cargoHome}/bin`).length, 1);
    }
  } finally { rmSync(root, {recursive:true,force:true}); }
});

test("legacy Maestro Rust profile sources refresh to the active repository home", () => {
  const root = mkdtempSync(join(tmpdir(), "maestro-rust-legacy-profile-"));
  try {
    const cacheRoot = join(root, "cache");
    const cargoHome = join(cacheRoot, "dx-corp-code/1.95.0/home/cargo");
    mkdirSync(cargoHome, { recursive: true });
    writeFileSync(join(cargoHome, "env"), 'case ":$PATH:" in\n *:"'+cargoHome+'/bin":*) ;;\n *) export PATH="'+cargoHome+'/bin:$PATH" ;;\nesac\n');
    const profile = join(root, "profile");
    const legacyEnv = join(cacheRoot, "evalops-maestro/1.95.0/home/cargo/env");
    const aliasEnv = join(cacheRoot, "dx-corp-maestro/1.95.0/home/cargo/env");
    const untouched = `# . "${legacyEnv}"\nexport MAESTRO_PROFILE_MARKER=preserved\n`;
    writeFileSync(profile, `${untouched}. "${legacyEnv}"\nsource "${aliasEnv}" # generated Rust source\n`, { mode: 0o640 });
    assert.throws(() => execFileSync("bash", ["-ec", `. "${profile}"; printf runtime-conformance-shell`], { encoding: "utf8", stdio: "pipe" }), /No such file/);
    for (let run = 0; run < 2; run++) {
      execFileSync("bash", [".github/actions/setup-rust/repair-login-profile.sh", cacheRoot, "1.95.0", cargoHome, profile]);
      const content = readFileSync(profile, "utf8");
      assert.equal(content, `${untouched}. "${cargoHome}/env"\nsource "${cargoHome}/env" # generated Rust source\n`);
      assert.equal(statSync(profile).mode & 0o777, 0o640);
      const output = execFileSync("bash", ["-c", `. "${profile}"; printf '%s:%s' "$MAESTRO_PROFILE_MARKER" runtime-conformance-shell`], { encoding: "utf8" });
      assert.equal(output, "preserved:runtime-conformance-shell");
      const path = execFileSync("sh", ["-c", `. "${cargoHome}/env"; . "${cargoHome}/env"; printf '%s' "$PATH"`], { encoding: "utf8" });
      assert.equal(path.split(":").filter((entry) => entry === `${cargoHome}/bin`).length, 1);
    }
    const unrelated = join(root, "unrelated");
    const unrelatedContent = `. "${cacheRoot}/other-repo/1.95.0/home/cargo/env"\n. "${cacheRoot}/evalops-maestro/1.94.0/home/cargo/env"\n`;
    writeFileSync(unrelated, unrelatedContent);
    execFileSync("bash", [".github/actions/setup-rust/repair-login-profile.sh", cacheRoot, "1.95.0", cargoHome, unrelated]);
    assert.equal(readFileSync(unrelated, "utf8"), unrelatedContent);
    const linkedProfile = join(root, "linked-profile");
    symlinkSync(unrelated, linkedProfile);
    assert.throws(() => execFileSync("bash", [".github/actions/setup-rust/repair-login-profile.sh", cacheRoot, "1.95.0", cargoHome, linkedProfile], { stdio: "pipe" }), /non-symlink profile/);
    assert.equal(readFileSync(unrelated, "utf8"), unrelatedContent);
  } finally { rmSync(root, { recursive: true, force: true }); }
});

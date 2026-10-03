import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { mkdtempSync, readFileSync, rmSync, writeFileSync, mkdirSync } from "node:fs";
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
    const env = {...process.env, PATH: `${bin}:${process.env.PATH}`, RUNNER_OS: "Linux", GITHUB_REPOSITORY: "conformance/env", TOOLCHAIN_INPUT: "1.95.0", WORKSPACES_INPUT: ".", GITHUB_ENV: join(root,"env"), GITHUB_PATH: join(root,"path"), GITHUB_OUTPUT: join(root,"output")};
    const cargoHome = join(root,"cache/conformance-env/1.95.0/home/cargo");
    const profile = join(root,"profile"); writeFileSync(profile, `. "${cargoHome}/env"\n`);
    for (let run = 0; run < 2; run++) {
      execFileSync("bash", ["-c", script], {env, cwd: process.cwd()});
      const output = execFileSync("sh", ["-c", `. "${profile}"; printf runtime-conformance-shell`], {env, encoding: "utf8"});
      assert.equal(output, "runtime-conformance-shell");
      const path = execFileSync("sh", ["-c", `. "${profile}"; . "${profile}"; printf '%s' "$PATH"`], {env, encoding:"utf8"});
      assert.equal(path.split(":").filter((entry) => entry === `${cargoHome}/bin`).length, 1);
    }
  } finally { rmSync(root, {recursive:true,force:true}); }
});

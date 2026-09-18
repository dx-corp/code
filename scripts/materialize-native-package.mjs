#!/usr/bin/env node
import {
	chmodSync,
	copyFileSync,
	cpSync,
	existsSync,
	mkdirSync,
	readFileSync,
	readdirSync,
	rmSync,
	writeFileSync,
} from "node:fs";
import { basename, join, resolve } from "node:path";

const supported = new Set([
	"darwin-arm64",
	"darwin-x64",
	"linux-arm64",
	"linux-x64",
]);
const packageMetadata = JSON.parse(
	readFileSync(new URL("../package.json", import.meta.url), "utf8"),
);

function currentPlatform() {
	const arch = process.arch === "x64" ? "x64" : process.arch;
	return `${process.platform}-${arch}`;
}

const inputIndex = process.argv.indexOf("--input-dir");
const inputDir = inputIndex >= 0 ? resolve(process.argv[inputIndex + 1]) : null;
const profileIndex = process.argv.indexOf("--profile");
const profile = profileIndex >= 0 ? process.argv[profileIndex + 1] : "release";
if (!profile || !/^[A-Za-z0-9_-]+$/.test(profile)) {
	throw new Error(`Invalid Cargo profile: ${profile ?? "(missing)"}`);
}
const cargoTargetDir = resolve(process.env.CARGO_TARGET_DIR || "target");
const localBinary = resolve(
	cargoTargetDir,
	profile,
	process.platform === "win32" ? "maestro.exe" : "maestro",
);
const binaries = [];

if (inputDir) {
	for (const name of readdirSync(inputDir)) {
		const match = /^maestro-(darwin|linux)-(arm64|x64)$/.exec(name);
		if (match) binaries.push([`${match[1]}-${match[2]}`, join(inputDir, name)]);
	}
} else if (existsSync(localBinary)) {
	binaries.push([currentPlatform(), localBinary]);
}

if (binaries.length === 0) {
	throw new Error(
		inputDir
			? `No maestro-<platform>-<arch> binaries found in ${inputDir}`
			: `Native Maestro binary for profile ${profile} is missing: ${localBinary}`,
	);
}

for (const [platform, source] of binaries) {
	if (!supported.has(platform)) throw new Error(`Unsupported platform ${platform}`);
	const directory = resolve("vendor", "maestro", platform);
	mkdirSync(directory, { recursive: true });
	const destination = join(directory, "maestro");
	copyFileSync(source, destination);
	chmodSync(destination, 0o755);
	console.log(`${basename(source)} -> ${destination}`);
	const helper = inputDir
		? join(inputDir, `deixic-code-device-${platform}.app`)
		: resolve(cargoTargetDir, profile, "DeixicCodeDevice.app");
	if (platform.startsWith("darwin-") && existsSync(helper)) {
		const helperDestination = join(directory, "DeixicCodeDevice.app");
		rmSync(helperDestination, { recursive: true, force: true });
		cpSync(helper, helperDestination, { recursive: true, force: false, errorOnExist: true });
		chmodSync(join(helperDestination, "Contents", "MacOS", "deixic-code-device"), 0o755);
	}

}

mkdirSync(resolve("bin"), { recursive: true });
const launcher = `#!/bin/sh
set -eu
os=$(uname -s | tr '[:upper:]' '[:lower:]')
case "$(uname -m)" in
  x86_64|amd64) arch=x64 ;;
  arm64|aarch64) arch=arm64 ;;
  *) echo "Unsupported Maestro architecture: $(uname -m)" >&2; exit 1 ;;
esac
case "$os" in darwin|linux) ;; *) echo "Unsupported Maestro OS: $os" >&2; exit 1 ;; esac
script=$0
while [ -L "$script" ]; do
  link=$(readlink "$script")
  case "$link" in
    /*) script=$link ;;
    *) script=$(dirname -- "$script")/$link ;;
  esac
done
root=$(CDPATH='' cd -- "$(dirname -- "$script")/.." && pwd -P)
MAESTRO_INSTALL_METHOD=package \
MAESTRO_PACKAGE_NAME='${packageMetadata.name}' \
MAESTRO_PACKAGE_ROOT="$root" \
MAESTRO_VERSION='${packageMetadata.version}' \
  exec "$root/vendor/maestro/$os-$arch/maestro" "$@"
`;
const launcherPath = resolve("bin", "maestro");
writeFileSync(launcherPath, launcher);
chmodSync(launcherPath, 0o755);
console.log(`Compatibility launcher -> ${launcherPath}`);

const canonicalLauncher = `#!/bin/sh
set -eu
script=$0
while [ -L "$script" ]; do
  link=$(readlink "$script")
  case "$link" in
    /*) script=$link ;;
    *) script=$(dirname -- "$script")/$link ;;
  esac
done
bin_dir=$(CDPATH='' cd -- "$(dirname -- "$script")" && pwd -P)
exec "$bin_dir/maestro" "$@"
`;
const canonicalLauncherPath = resolve("bin", "deixic-code");
writeFileSync(canonicalLauncherPath, canonicalLauncher);
chmodSync(canonicalLauncherPath, 0o755);
console.log(`Deixic Code launcher -> ${canonicalLauncherPath}`);

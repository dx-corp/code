#!/usr/bin/env node
// @ts-check

import { existsSync, readFileSync } from "node:fs";
import { isAbsolute, join, posix, relative, resolve, sep } from "node:path";
import { spawnSync } from "node:child_process";

const dockerfilePath = join(process.cwd(), "Dockerfile");
if (!existsSync(dockerfilePath)) {
	console.error("Dockerfile not found.");
	process.exit(1);
}

const dockerfile = readFileSync(dockerfilePath, "utf8");
const stageContents = (stageName) => {
	const match = dockerfile.match(
		new RegExp(
			`(?:^|\\n)FROM\\s+[^\\n]+\\s+AS\\s+${stageName}\\b([\\s\\S]*?)(?=\\nFROM\\s|$)`,
			"i",
		),
	);
	return match?.[1] ?? "";
};
const nativeStageHasRustToolchain =
	/FROM\s+rust:[^\s]+\s+AS\s+native/.test(dockerfile) ||
	(/FROM\s+\S*cargo-chef:[^\s]+\s+AS\s+chef/.test(dockerfile) &&
		/FROM\s+chef\s+AS\s+native/.test(dockerfile));
const required = [
	[/https:\/\/deb\.debian\.org/, "HTTPS Debian package mirror"],
	[
		/COPY\s+--from=native\s+\/app\/target\/release\/maestro\s+\/usr\/local\/bin\/maestro/,
		"native Maestro binary copy",
	],
	[/\bMAESTRO_CONTROL_HOST=0\.0\.0\.0\b/, "Rust server bind environment"],
	[/\bPORT=3000\b/, "Rust server port environment"],
	[/ENTRYPOINT\s+\["maestro"\]/, "native Maestro entrypoint"],
	[/CMD\s+\["serve"\]/, "Rust serve command"],
];

const runtimeFacadeCopy = /COPY\s+packages\/runtime-rs\s+\.\/packages\/runtime-rs/;
const runtimeContractsCopy =
	/COPY\s+packages\/runtime-contracts-rs\s+\.\/packages\/runtime-contracts-rs/;
const plannerStage = stageContents("planner");
const nativeStage = stageContents("native");
const embeddedConfigCopy =
	/COPY\s+config\/vendor-corrections\.json\s+\.\/config\/vendor-corrections\.json/;

const missing = required
	.filter(([pattern]) => !pattern.test(dockerfile))
	.map(([, label]) => label);
if (!nativeStageHasRustToolchain) {
	missing.unshift("Rust native build stage");
}
if (!runtimeFacadeCopy.test(plannerStage)) {
	missing.push("native runtime facade crate in planner Docker stage");
}
if (!runtimeFacadeCopy.test(nativeStage)) {
	missing.push("native runtime facade crate in native Docker stage");
}
if (!runtimeContractsCopy.test(plannerStage)) {
	missing.push("dependency-light runtime contracts crate in planner Docker stage");
}
if (!runtimeContractsCopy.test(nativeStage)) {
	missing.push("dependency-light runtime contracts crate in native Docker stage");
}
const nativeBuild = nativeStage.search(/^RUN\s+cargo\s+build\s+--release\s+--locked\s+-p\s+maestro\b/m);
if (nativeBuild < 0 || !embeddedConfigCopy.test(nativeStage.slice(0, nativeBuild))) {
	missing.push("embedded vendor corrections before native Maestro build");
}
if (missing.length > 0) {
	console.error(`Dockerfile is missing native runtime contracts: ${missing.join(", ")}`);
	process.exit(1);
}

// A workspace member that no build stage copies is invisible until the image
// build runs, and the image build runs only on the publisher. Bind the copy
// list to the checked-in workspace so the next dependency addition fails on
// its own pull request instead.
const cargoManifestPath = join(process.cwd(), "Cargo.toml");
if (!existsSync(cargoManifestPath)) {
	console.error("Cargo.toml not found; cannot check the Docker copy list against the workspace.");
	process.exit(1);
}
const cargoManifest = readFileSync(cargoManifestPath, "utf8");
const membersBlock = cargoManifest.match(/members\s*=\s*\[([\s\S]*?)\]/);
if (!membersBlock) {
	console.error("Cargo.toml declares no workspace members array.");
	process.exit(1);
}
const workspaceMembers = [...membersBlock[1].matchAll(/"([^"]+)"/g)]
	.map((match) => match[1].replace(/\/+$/, ""))
	.filter((member) => member.startsWith("packages/"));
const copiesMember = (stage, member) =>
	new RegExp(`COPY\\s+${member.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")}\\s+\\.\\/${member.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")}(?:\\s|$)`, "m").test(stage);
const uncopied = [];
const vendoredPaths = [...cargoManifest.matchAll(/\bpath\s*=\s*"(vendor\/[^"\n]+)"/g)]
	.map((match) => match[1]);
for (const path of new Set(vendoredPaths)) {
	for (const [name, stage, command] of [
		["planner", plannerStage, "prepare"],
		["native", nativeStage, "cook"],
	]) {
		const marker = new RegExp(`^RUN\\s+cargo\\s+chef\\s+${command}\\b`, "m");
		const boundary = stage.search(marker);
		if (boundary < 0 || !copiesMember(stage.slice(0, boundary), path)) {
			uncopied.push(`${path} before cargo chef ${command} in the ${name} stage`);
		}
	}
}
for (const member of workspaceMembers) {
	if (!copiesMember(plannerStage, member)) {
		uncopied.push(`${member} in the planner stage`);
	}
	if (!copiesMember(nativeStage, member)) {
		uncopied.push(`${member} in the native stage`);
	}
}
if (uncopied.length > 0) {
	console.error(
		`Dockerfile does not copy every Cargo workspace member: ${uncopied.join(", ")}`,
	);
	process.exit(1);
}

// Cargo owns dependency parsing, including workspace inheritance, aliases,
// optional dependencies and target/build/dev tables. --no-deps avoids registry
// resolution and --offline forbids a network fallback; neither builds crates.
const packages = new Map();
const loadPackages = (manifest) => {
	const result = spawnSync("cargo", [
		"metadata", "--offline", "--no-deps", "--format-version", "1",
		"--manifest-path", manifest,
	], { encoding: "utf8", timeout: 10_000, maxBuffer: 16 * 1024 * 1024 });
	if (result.error || result.status !== 0) {
		throw new Error(`Cannot resolve Docker path dependencies: ${result.error?.message ?? result.stderr}`);
	}
	const metadata = JSON.parse(result.stdout);
	for (const pkg of metadata.packages) packages.set(resolve(pkg.manifest_path), pkg);
	return metadata.packages;
};
const within = (parent, child) => {
	const tail = relative(parent, child);
	return tail !== ".." && !tail.startsWith(`..${sep}`) && !isAbsolute(tail);
};
try {
	const maestroRoot = resolve(process.cwd());
	const rustRoot = resolve(maestroRoot, "../../rust");
	const pending = loadPackages(cargoManifestPath);
	const visited = new Set();
	const external = [];
	const rustToMaestro = new Set();
	while (pending.length > 0) {
		const pkg = pending.pop();
		if (visited.has(pkg.manifest_path)) continue;
		visited.add(pkg.manifest_path);
		if (!within(maestroRoot, pkg.manifest_path)) external.push(pkg.manifest_path);
		for (const dependency of pkg.dependencies) {
			if (!dependency.path) continue;
			if (!within(maestroRoot, dependency.path) && !within(rustRoot, dependency.path)) {
				throw new Error(`Unsupported external Docker dependency: ${dependency.path}`);
			}
			if (within(rustRoot, pkg.manifest_path) && within(maestroRoot, dependency.path)) {
				rustToMaestro.add(dependency.path);
			}
			const manifest = resolve(dependency.path, "Cargo.toml");
			if (!packages.has(manifest)) loadPackages(manifest);
			const target = packages.get(manifest);
			if (!target) throw new Error(`Cargo omitted local dependency ${manifest}`);
			pending.push(target);
		}
	}
	if (external.length > 0) {
		const prepare = plannerStage.search(/^RUN\s+cargo\s+chef\s+prepare\b/m);
		const plannerInputs = plannerStage.slice(0, Math.max(0, prepare));
		const copies = [...plannerInputs.matchAll(
			/^COPY --from=dex-loop-workspace (\S+) (\S+)\s*$/gm,
		)];
		const narrowed = plannerInputs.match(/c\\members = (\[[^'\n]+\])' \/rust\/Cargo.toml/);
		const members = narrowed ? JSON.parse(narrowed[1]) : [];
		// Rust's inherited paths point at mono's products/maestro directory;
		// the image puts that same workspace at /app. Require the root-manifest
		// remap before preparation rather than accepting copies alone.
		const regexEscape = (text) => text.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
		const beforePrepare = (command) => new RegExp(
			`^(?:RUN|[ \\t]*&&)[ \\t]+${regexEscape(command)}(?:[ \\t]*\\\\)?[ \\t]*$`, "m",
		).test(plannerInputs);
		const actualSharedMembers = [];
		for (const dependency of rustToMaestro) {
			const source = relative(rustRoot, dependency).split(sep).join("/");
			const member = relative(maestroRoot, dependency).split(sep).join("/");
			const target = `../app/${member}`;
			const remap = `sed -i 's|path = "${regexEscape(source)}"|path = "${target}"|' /rust/Cargo.toml`;
			const workspaceRemap = `sed -i 's#path = "../products/maestro/#path = "/app/#g' /rust/Cargo.toml`;
			if (!beforePrepare(remap) && !beforePrepare(workspaceRemap)) {
				uncopied.push(`Rust-to-Maestro dependency path ${source} before cargo chef prepare`);
			}
			if (workspaceMembers.includes(member)) {
				actualSharedMembers.push(member);
				// Chef dummies only its own workspace. External actual Rust source
				// must retain the APIs of this shared library during the cook.
				const filter = `sed -i '/^    "${member.replaceAll("/", "\\/")}",$/d' Cargo.toml`;
				const exclude = `sed -i 's|"vendor/\\*"|"vendor/*", "${member}"|' Cargo.toml`;
				if (!beforePrepare(filter) || !beforePrepare(exclude)) {
					uncopied.push(`actual shared library ${member} before cargo chef prepare`);
				}
			}
		}
		for (const manifest of external) {
			const path = relative(rustRoot, manifest).split(sep).join("/");
			const source = `/${path}`;
			const copied = copies.some(([, from, to]) => {
				const tail = posix.relative(from, source);
				return tail !== ".." && !tail.startsWith("../") && !posix.isAbsolute(tail)
					&& posix.join(to, tail) === `/rust/${path}`;
			});
			if (!copied) uncopied.push(`${path} before cargo chef prepare in the planner stage`);
			if (!members.includes(posix.dirname(path))) {
				uncopied.push(`${path} in the narrowed Rust workspace`);
			}
		}
		const cook = nativeStage.search(/^RUN\s+cargo\s+chef\s+cook\b/m);
		if (cook < 0 || !/^COPY --from=planner \/rust \/rust\s*$/m.test(nativeStage.slice(0, cook))) {
			uncopied.push("planner Rust dependency tree before cargo chef cook in the native stage");
		}
		for (const dependency of rustToMaestro) {
			const member = relative(maestroRoot, dependency).split(sep).join("/");
			if (cook < 0 || !copiesMember(nativeStage.slice(0, cook), member)) {
				uncopied.push(`shared dependency ${member} before cargo chef cook`);
			}
		}
		if (actualSharedMembers.length > 0 && (cook < 0 || nativeBuild < cook ||
			!/^COPY Cargo.toml Cargo.lock \.\/\s*$/m.test(nativeStage.slice(cook, nativeBuild)))) {
			uncopied.push("original Maestro workspace after cook and before native build");
		}
		if (uncopied.length > 0) throw new Error(`Dockerfile omits reachable local dependencies: ${uncopied.join(", ")}`);
	}
} catch (error) {
	console.error(error.message);
	process.exit(1);
}
if (/Acquire::https::Verify-(?:Peer|Host)=false/.test(dockerfile)) {
	console.error("Dockerfile must not disable HTTPS certificate verification.");
	process.exit(1);
}
if (/ENTRYPOINT\s+\[(?:"node"|"bun")/.test(dockerfile)) {
	console.error("Dockerfile must not use a Node.js or Bun runtime entrypoint.");
	process.exit(1);
}

console.log("Verified native-only Docker runtime contract.");

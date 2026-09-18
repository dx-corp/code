#!/usr/bin/env node

import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { pathToFileURL } from "node:url";

const REQUIRED_TEXT = new Map([
	["README.md", ["# Deixic Code", "deixic-code --version", "@evalops/maestro"]],
	["docs/DEIXIC_CODE_MIGRATION.md", ["## Compatibility matrix", "@evalops/deixic-code", "`maestro`"]],
	["packages/maestro-rs/src/main.rs", ["Deixic Code\\n\\nUsage:", "deixic-code setup", "maestro remains available as an alias"]],
	["packages/presentation-rs/src/components/deixic_logo.rs", ["pub const PRODUCT_TITLE: &str = \"Dex Code\";", "shimmer_spans(PRODUCT_TITLE)"]],
	["CONTRIBUTING.md", ["# Contributing to Deixic Code"]],
	["docs/THREAT_MODEL.md", ["# Deixic Code Threat Model"]],
	["docs/ENTERPRISE.md", ["# Deixic Code Enterprise"]],
	["packages/tui-rs/README.md", ["# Deixic Code TUI (Rust)"]],
	["packages/tui-rs/src/crash_handler.rs", ["=== Deixic Code Crash Report ==="]],
	["packages/runtime-gateway-rs/src/a2a/tasks.rs", ["Deixic Code is working on the A2A task."]],
	["packages/maestro-rs/Cargo.toml", ["description = \"Canonical native Rust CLI for Deixic Code\""]],
	["packages/tui-rs/Cargo.toml", ["description = \"Native terminal UI renderer for Deixic Code\""]],
	["packages/runtime-gateway-rs/Cargo.toml", ["description = \"Native Rust HTTP runtime gateway for Deixic Code\""]],
	["packages/runtime-rs/Cargo.toml", ["Native agent runtime for Deixic Code"]],
	["packages/runtime-contracts-rs/Cargo.toml", ["runtime contracts for Deixic Code"]],
	["packages/ai-rs/Cargo.toml", ["client layer for Deixic Code"]],
	["packages/execpolicy-rs/Cargo.toml", ["policy parser for Deixic Code"]],
	["packages/a2a-ledger-rs/Cargo.toml", ["Deixic Code A2A SQLite task ledger"]],
	["packages/session-history-rs/Cargo.toml", ["authenticated Deixic Code sessions"]],
	["examples/hooks/wasm-plugin/Cargo.toml", ["hook plugin for Deixic Code"]],
	["packages/jetbrains-plugin/src/main/resources/META-INF/plugin.xml", ["<name>Deixic Code</name>", "id=\"Maestro\"", "id=\"Maestro Notifications\""]],
	["scripts/install.sh", ["$install_dir/deixic-code", "$install_dir/maestro", "Installed Deixic Code"]],
	["scripts/materialize-native-package.mjs", ["resolve(\"bin\", \"deixic-code\")", "exec \"$bin_dir/maestro\" \"$@\""]],
]);

const FORBIDDEN_DISPLAY_TEXT = new Map([
	["CONTRIBUTING.md", ["# Contributing to Maestro", "Maestro is developed as"]],
	["docs/THREAT_MODEL.md", ["# Maestro Threat Model", "deploying Maestro in"]],
	["docs/ENTERPRISE.md", ["# Maestro Enterprise", "deploying Maestro against"]],
	["docs/MCP_GUIDE.md", ["with Maestro.", "Configure Maestro", "Register with Maestro", "Maestro MCP Commands"]],
	["docs/SAFETY.md", ["Maestro can execute shell commands"]],
	["docs/mcp-config.md", ["(Maestro)", "approach for Maestro"]],
	["packages/tui-rs/README.md", ["# Maestro TUI", "terminal UI for Maestro"]],
	["packages/tui-rs/docs/user-guide/07-mcp-servers.md", ["Maestro supports the Model Context Protocol", "Launch Maestro"]],
	["packages/tui-rs/docs/user-guide/12-sandbox-and-safety.md", ["Maestro runs tools on your machine"]],
	["packages/tui-rs/docs/user-guide/14-worktrees.md", ["Maestro can run a whole session", "Maestro prints its path"]],
	["packages/tui-rs/src/app/a2a_handoff.rs", ["Maestro is following it in the background"]],
	["packages/tui-rs/src/setup_cli.rs", ["before Maestro can run"]],
	["packages/local-host-rs/src/evalops_cli.rs", ["Maestro EvalOps Login"]],
	["packages/tui-rs/src/connections_cli.rs", ["used by Maestro"]],
	["packages/local-host-rs/src/credential_mode.rs", ["every Maestro session"]],
	["packages/local-host-rs/src/init_cli.rs", ["Registering Maestro with", "\"client_name\": \"Maestro CLI\"", "return to Maestro"]],
	["packages/local-host-rs/src/evalops_cli/platform_tools/config.rs", ["Restart Maestro to load"]],
	["packages/local-host-rs/src/evalops_cli/platform_tools/provision.rs", ["\"client_name\": \"Maestro Platform Tools\"", "return to Maestro"]],
	["packages/tui-rs/src/crash_handler.rs", ["=== Maestro Crash Report ==="]],
	["packages/tui-rs/src/acp_cli.rs", ["\"title\": \"Maestro\"", "failed to launch Maestro agent"]],
	["packages/tui-rs/src/app.rs", ["Maestro TUI - Keyboard Shortcuts"]],
	["packages/tui-rs/src/a2a_cli/mod.rs", ["Maestro A2A Peer"]],
	["packages/tui-rs/src/a2a_cli/pairing.rs", ["Maestro A2A Peer"]],
	["packages/local-host-rs/src/agents_cli.rs", ["Existing Maestro agent instructions"]],
	["packages/local-host-rs/src/evalops_cli/platform_tools.rs", ["from Maestro Platform tools CLI"]],
	["packages/tui-rs/src/keybindings.rs", ["inside Maestro"]],
	["packages/local-host-rs/src/plugins/manager.rs", ["Maestro will not load this plugin"]],
	["packages/local-host-rs/src/hooks/context.rs", ["Maestro dropped this hook"]],
	["packages/local-host-rs/src/tools/bash/mod.rs", ["Blocked by Maestro's native sandbox", "inside Maestro's native OS sandbox"]],
	["packages/tui-rs/src/update_cli.rs", ["signed Maestro installer", "Another Maestro update"]],
	["packages/local-host-rs/src/mcp/client.rs", ["Maestro turn cancelled"]],
	["packages/local-host-rs/src/mcp/http.rs", ["Maestro turn cancelled"]],
	["packages/local-host-rs/src/mailbox.rs", ["Pending Maestro mailbox messages", "another Maestro process"]],
	["packages/local-host-rs/src/subagents/mod.rs", ["Maestro restarted while", "delegated Maestro subagent", "another Maestro process"]],
	["packages/runtime-gateway-rs/src/a2a/tasks.rs", ["this Maestro agent", "Maestro is working on the A2A task", "Maestro accepted the A2A task", "Maestro completed the A2A task"]],
	["packages/runtime-gateway-rs/src/a2a_platform_registration.rs", ["Maestro A2A Peer", "Maestro peer exposing"]],
	["packages/runtime-gateway-rs/src/a2a/native_turn.rs", ["local Maestro Desktop A2A agent"]],
	["packages/runtime-gateway-rs/src/automations.rs", ["durable Maestro automation"]],
	["packages/maestro-rs/Cargo.toml", ["CLI for Maestro"]],
	["packages/tui-rs/Cargo.toml", ["renderer for Maestro"]],
	["packages/runtime-gateway-rs/Cargo.toml", ["gateway for Maestro"]],
	["packages/runtime-rs/Cargo.toml", ["facade for Maestro"]],
	["packages/runtime-contracts-rs/Cargo.toml", ["contracts for Maestro"]],
	["packages/ai-rs/Cargo.toml", ["client layer for Maestro"]],
	["packages/execpolicy-rs/Cargo.toml", ["parser for Maestro"]],
	["packages/a2a-ledger-rs/Cargo.toml", ["Maestro A2A SQLite task ledger"]],
	["packages/session-history-rs/Cargo.toml", ["authenticated Maestro sessions"]],
	["examples/hooks/wasm-plugin/Cargo.toml", ["hook plugin for Maestro"]],
	["packages/maestro-rs/src/main.rs", ["const HELP: &str = \"Maestro", "Usage:\\n  maestro setup"]],
	["packages/presentation-rs/src/components/deixic_logo.rs", ["shimmer_spans(\"Maestro\")"]],
	["packages/jetbrains-plugin/src/main/resources/META-INF/plugin.xml", ["<name>Maestro</name>", "text=\"Focus Maestro\""]],
]);

function contentAt(root, path, overrides) {
	return overrides.get(path) ?? readFileSync(resolve(root, path), "utf8");
}

export function findDeixicCodeNamingProblems(
	root = new URL("..", import.meta.url).pathname,
	overrides = new Map(),
) {
	const problems = [];
	const packageJson = JSON.parse(contentAt(root, "package.json", overrides));
	const binCommands = Object.keys(packageJson.bin ?? {});

	const canonicalPackageName = packageJson.maestro?.canonicalPackageName;
	const packageAliases = packageJson.maestro?.packageAliases;
	if (
		packageJson.name !== canonicalPackageName &&
		!packageAliases?.includes(packageJson.name)
	) {
		problems.push(
			"package.json name must be the canonical package or a declared package alias",
		);
	}
	if (canonicalPackageName !== "@evalops/deixic-code") {
		problems.push("package.json canonical package must be @evalops/deixic-code");
	}
	if (!packageAliases?.includes("@evalops/maestro")) {
		problems.push("package.json must retain @evalops/maestro as a package alias");
	}
	if (binCommands[0] !== "deixic-code" || packageJson.bin?.maestro !== "bin/maestro") {
		problems.push("package.json must declare deixic-code first and retain the maestro binary alias");
	}

	for (const [path, snippets] of REQUIRED_TEXT) {
		const content = contentAt(root, path, overrides);
		for (const snippet of snippets) {
			if (!content.includes(snippet)) problems.push(`${path} is missing ${JSON.stringify(snippet)}`);
		}
	}
	for (const [path, snippets] of FORBIDDEN_DISPLAY_TEXT) {
		const content = contentAt(root, path, overrides);
		for (const snippet of snippets) {
			if (content.includes(snippet)) problems.push(`${path} contains stale display text ${JSON.stringify(snippet)}`);
		}
	}

	return problems;
}

export function main() {
	const problems = findDeixicCodeNamingProblems();
	if (problems.length > 0) {
		console.error("Deixic Code naming check failed:");
		for (const problem of problems) console.error(`- ${problem}`);
		return 1;
	}
	console.log("Deixic Code naming and compatibility check passed.");
	return 0;
}

if (process.argv[1] && pathToFileURL(resolve(process.argv[1])).href === import.meta.url) {
	process.exitCode = main();
}

#!/usr/bin/env node
import { existsSync, readFileSync } from "node:fs";

const files = [
	"README.md",
	"CONTRIBUTING.md",
	"docs/BUILD_TESTING.md",
	"docs/ARCHITECTURE.md",
	"docs/TUI_ARCHITECTURE.md",
	"docs/NATIVE_TUI_PARITY.md",
	"docs/FEATURES.md",
	"docs/QUICKSTART.md",
	"docs/CONTRIBUTOR_RUNBOOK.md",
	"docs/TOOLS_REFERENCE.md",
	"docs/CI_VERSION_PINS.md",
	"docs/THREAT_MODEL.md",
	"docs/SAFETY.md",
	"packages/tui-rs/docs/user-guide/01-getting-started.md",
	"packages/tui-rs/docs/user-guide/12-sandbox-and-safety.md",
	"packages/tui-rs/docs/user-guide/13-headless-mode.md",
];
if (existsSync("AGENTS.md")) files.push("AGENTS.md");
const banned = [
	/dist\/cli\.js/i,
	/vendor\/maestro-tui/i,
	/packages\/ai/i,
	/typescript (agent|runtime) (remains|handles|owns|supports|executes)/i,
	/(bun install(?:\s|$)|bun run(?:\s|$)|npx tsc(?:\s|$)|tsconfig\.build)/i,
	/typescript (path|cli|adapter|surface)/i,
	/src\/safety\/(action-firewall|policy|safe-mode)\.ts/i,
	/src\/agent\/action-approval\.ts/i,
	/src\/security\/directory-access\.ts/i,
	/src\/sandbox\//i,
	/src\/tools\/(background-tasks|webfetch)\.ts/i,
	/src\/utils\/secret-redactor\.ts/i,
	/src\/server\/rate-limiter\.ts/i,
	/src\/(rbac|auth|audit)\//i,
];
const failures = [];
for (const file of files) {
	const text = readFileSync(file, "utf8");
	for (const pattern of banned) if (pattern.test(text)) failures.push(`${file}: ${pattern}`);
}
if (failures.length) {
	console.error(failures.join("\n"));
	process.exit(1);
}
console.log(`Rust-only documentation contract passed (${files.length} canonical documents).`);

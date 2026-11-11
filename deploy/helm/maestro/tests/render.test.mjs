import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { fileURLToPath } from "node:url";
import test from "node:test";

test("rendered Helm runtime uses one port, drain readiness and the native preStop", () => {
	const chart = fileURLToPath(new URL("../", import.meta.url));
	const rendered = execFileSync("helm", ["template", "lifecycle", chart, "--set", "service.port=8123"], { encoding: "utf8" });
	assert.match(rendered, /name: PORT\s+value: "8123"/u);
	assert.match(rendered, /readinessProbe:\s+httpGet:\s+path: \/readyz/u);
	assert.match(rendered, /containerPort: 8123/u);
	assert.match(rendered, /preStop:\s+exec:\s+command:\s+- maestro\s+- drain/u);
	assert.doesNotMatch(rendered, /node <<|MAESTRO_WEB_REQUIRE_KEY: "0"/u);
	assert.match(rendered, /image: "ghcr.io\/dx-corp\/code:main"/u);
	const pinned = execFileSync("helm", ["template", "lifecycle", chart, "--set", "image.tag=sha-owned"], { encoding: "utf8" });
	assert.match(pinned, /image: "ghcr.io\/dx-corp\/code:sha-owned"/u);
	const digest = `sha256:${"a".repeat(64)}`;
	const immutable = execFileSync("helm", ["template", "lifecycle", chart, "--set", `image.digest=${digest}`, "--set", "image.tag=ignored", "--set-string", "env.PORT=9999", "--set", "service.port=8123"], { encoding: "utf8" });
	assert.ok(immutable.includes(`image: "ghcr.io/dx-corp/code@${digest}"`));
	assert.match(immutable, /name: PORT\s+value: "8123"/u);
	assert.doesNotMatch(immutable, /value: "9999"/u);
});

import assert from "node:assert/strict";
import test from "node:test";

import { openAiProtocol } from "./catalog-provider-protocol.mjs";

test("native Daybreak routes require Responses, as documented by OpenAI", () => {
	for (const model of ["gpt-daybreak-blue-latest", "gpt-daybreak-red-latest"]) {
		assert.equal(openAiProtocol(model), "openai-responses", model);
	}
});

test("admitting the documented aliases preserves existing wire selection", () => {
	for (const model of ["gpt-4.1", "gpt-6-sol", "gpt-daybreak-unknown"]) {
		assert.equal(openAiProtocol(model), "openai-chat", model);
	}
	for (const model of ["gpt-5.6", "gpt-6-astra", "gpt-5.3-codex", "o3"]) {
		assert.equal(openAiProtocol(model), "openai-responses", model);
	}
});

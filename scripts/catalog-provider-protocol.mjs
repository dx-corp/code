/** Mirror the native OpenAI wire selection in ai-rs/model_capabilities.rs. */
export function openAiProtocol(modelId) {
	// These exact aliases have Responses support and no Chat Completions:
	// https://developers.openai.com/api/docs/models/gpt-daybreak-blue-latest
	// https://developers.openai.com/api/docs/models/gpt-daybreak-red-latest
	return modelId.includes("codex") || modelId.startsWith("gpt-5") || modelId === "gpt-6-astra" || modelId.startsWith("o3") ||
		modelId === "gpt-daybreak-blue-latest" || modelId === "gpt-daybreak-red-latest"
		? "openai-responses"
		: "openai-chat";
}

/** Keep disputed aggregator output caps unknown until a vendor publishes them. */
export function omitUnverifiedSonarOutput(id, output) {
	// Perplexity documents a 128K context for Sonar Reasoning Pro, but no
	// 4,096-token output ceiling. OpenRouter's derived 90%-of-context value
	// falls back to the models.dev Perplexity row, which supplies 4,096.
	// Scope the omission to this exact model and disputed value.
	// https://docs.perplexity.ai/docs/sonar/models/sonar-reasoning-pro
	if (id === "perplexity/sonar-reasoning-pro" && output === 4096) {
		return undefined;
	}
	return output;
}

//! Canonical native provider registry.
//!
//! Provider identity, aliases, credential precedence, protocol selection, and
//! compatible endpoint defaults live here so native clients and CLI surfaces
//! share one routing contract.

use std::collections::HashMap;

use anyhow::{Result, bail};

use super::op_secret;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderProtocol {
    Anthropic,
    OpenAi,
    OpenAiCompatible,
    Google,
    VertexAi,
    Codex,
    SubscriptionCli,
    AzureOpenAi,
    Bedrock,
    Managed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderDescriptor {
    pub id: &'static str,
    pub aliases: &'static [&'static str],
    pub auth_env: &'static [&'static str],
    pub base_url_env: &'static [&'static str],
    pub default_base_url: Option<&'static str>,
    pub protocol: ProviderProtocol,
}

impl ProviderDescriptor {
    #[must_use]
    pub fn requires_auth(&self) -> bool {
        !self.auth_env.is_empty()
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct ResolvedProvider {
    pub provider: &'static ProviderDescriptor,
    pub auth_source: Option<String>,
    pub credential: Option<String>,
    pub base_url: Option<String>,
}

impl std::fmt::Debug for ResolvedProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResolvedProvider")
            .field("provider", &self.provider)
            .field("auth_source", &self.auth_source)
            .field("credential_present", &self.credential.is_some())
            .field("base_url", &self.base_url)
            .finish()
    }
}

pub struct ProviderRegistry;

impl ProviderRegistry {
    #[must_use]
    pub fn all() -> &'static [ProviderDescriptor] {
        PROVIDERS
    }

    #[must_use]
    pub fn descriptor(id_or_alias: &str) -> Option<&'static ProviderDescriptor> {
        let requested = id_or_alias.trim().to_ascii_lowercase();
        PROVIDERS.iter().find(|provider| {
            provider.id == requested || provider.aliases.iter().any(|alias| *alias == requested)
        })
    }

    /// Resolve only provider identity from a provider name or model route.
    /// This deliberately does not inspect or materialize credential sources.
    pub fn resolve_descriptor(model_or_provider: &str) -> Result<&'static ProviderDescriptor> {
        let requested = model_or_provider.trim();
        if let Some((prefix, _)) = requested.split_once('/') {
            Self::descriptor(prefix)
                .ok_or_else(|| anyhow::anyhow!("unknown provider prefix: {prefix}"))
        } else if let Some(provider) = Self::descriptor(requested) {
            Ok(provider)
        } else {
            Ok(descriptor_for_bare_model(requested))
        }
    }

    pub fn resolve(
        model_or_provider: &str,
        env: &HashMap<String, String>,
    ) -> Result<ResolvedProvider> {
        let descriptor = Self::resolve_descriptor(model_or_provider)?;
        let (auth_source, credential) = match first_env_or_file(env, descriptor.auth_env)? {
            Some((name, value)) => {
                let credential = op_secret::resolve_credential(&name, &value)?;
                (Some(name), Some(credential))
            }
            None => (None, None),
        };
        let base_url = first_env(env, descriptor.base_url_env)
            .map(|(_, value)| normalize_base_url(value))
            .or_else(|| descriptor.default_base_url.map(str::to_string));
        Ok(ResolvedProvider {
            provider: descriptor,
            auth_source,
            credential,
            base_url,
        })
    }

    pub fn require(
        model_or_provider: &str,
        env: &HashMap<String, String>,
    ) -> Result<ResolvedProvider> {
        let resolved = Self::resolve(model_or_provider, env)?;
        if resolved.provider.requires_auth() && resolved.credential.is_none() {
            bail!(
                "provider {} requires one of: {} (or the matching *_FILE path)",
                resolved.provider.id,
                resolved.provider.auth_env.join(", ")
            );
        }
        Ok(resolved)
    }
}

fn first_env<'a>(
    env: &'a HashMap<String, String>,
    names: &[&'static str],
) -> Option<(&'static str, &'a str)> {
    names.iter().find_map(|name| {
        env.get(*name)
            .map(String::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(|value| (*name, value))
    })
}

/// Resolve a provider credential from a direct env value or a sibling
/// `NAME_FILE` path. Platform Runner Host delivers tenant mints only as
/// bootstrap files (`MAESTRO_EVALOPS_ACCESS_TOKEN_FILE`); the raw bearer must
/// not be required in the process environment.
fn first_env_or_file(
    env: &HashMap<String, String>,
    names: &[&'static str],
) -> Result<Option<(String, String)>> {
    if let Some((name, value)) = first_env(env, names) {
        return Ok(Some((name.to_string(), value.to_string())));
    }
    for name in names {
        let file_key = format!("{name}_FILE");
        let Some(path) = env
            .get(&file_key)
            .map(String::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
        else {
            continue;
        };
        let contents = std::fs::read_to_string(path).map_err(|error| {
            anyhow::anyhow!("failed to read provider credential file {file_key}={path}: {error}")
        })?;
        let contents = contents.trim();
        if contents.is_empty() {
            bail!("provider credential file {file_key}={path} is empty");
        }
        return Ok(Some((file_key, contents.to_string())));
    }
    Ok(None)
}

fn normalize_base_url(value: &str) -> String {
    value.trim().trim_end_matches('/').to_string()
}

fn descriptor_for_bare_model(model: &str) -> &'static ProviderDescriptor {
    let model = model.to_ascii_lowercase();
    let id = if model.starts_with("claude") {
        "anthropic"
    } else if model.starts_with("gemini") {
        "google"
    } else if model.starts_with("deepseek-") {
        "deepseek"
    } else if model.starts_with("kimi-") || model.starts_with("moonshot-") {
        "moonshot"
    } else if model.starts_with("qwen") || model.starts_with("qwq-") {
        "dashscope"
    } else if model.starts_with("minimax-") {
        "minimax"
    } else if model.starts_with("glm-") {
        "zai"
    } else if model.contains("mistral") || model.contains("mixtral") || model.contains("codestral")
    {
        "mistral"
    } else if model.starts_with("llama") {
        "groq"
    } else {
        "openai"
    };
    ProviderRegistry::descriptor(id).expect("built-in provider descriptor")
}

const PROVIDERS: &[ProviderDescriptor] = &[
    ProviderDescriptor {
        id: "google",
        aliases: &["gemini"],
        auth_env: &["GEMINI_API_KEY"],
        base_url_env: &["GEMINI_BASE_URL", "GOOGLE_AI_BASE_URL"],
        default_base_url: Some("https://generativelanguage.googleapis.com"),
        protocol: ProviderProtocol::Google,
    },
    ProviderDescriptor {
        id: "vertex-ai",
        aliases: &["vertex"],
        // Vertex supports OAuth access tokens and API keys. Keep the access
        // token first so an explicitly configured OAuth credential wins when
        // both variables are present, matching VertexAiClient's precedence.
        auth_env: &["VERTEX_ACCESS_TOKEN", "GOOGLE_API_KEY"],
        // Vertex's endpoint is derived from project and region rather than a
        // configurable OpenAI-style base URL.
        base_url_env: &[],
        default_base_url: None,
        protocol: ProviderProtocol::VertexAi,
    },
    ProviderDescriptor {
        id: "google-gemini-cli",
        aliases: &[],
        auth_env: &["GOOGLE_GEMINI_CLI_TOKEN"],
        base_url_env: &["GOOGLE_GEMINI_CLI_BASE_URL"],
        default_base_url: Some("https://cloudcode-pa.googleapis.com"),
        protocol: ProviderProtocol::Google,
    },
    ProviderDescriptor {
        id: "google-antigravity",
        aliases: &[],
        auth_env: &["GOOGLE_ANTIGRAVITY_TOKEN"],
        base_url_env: &["GOOGLE_ANTIGRAVITY_BASE_URL"],
        default_base_url: None,
        protocol: ProviderProtocol::Google,
    },
    ProviderDescriptor {
        id: "evalops",
        aliases: &["maestro-managed"],
        auth_env: &["MAESTRO_EVALOPS_ACCESS_TOKEN"],
        base_url_env: &["MAESTRO_EVALOPS_BASE_URL"],
        default_base_url: Some("https://llm-gateway.evalops.dev/v1"),
        protocol: ProviderProtocol::Managed,
    },
    ProviderDescriptor {
        id: "openai",
        aliases: &[],
        auth_env: &["OPENAI_API_KEY"],
        base_url_env: &["OPENAI_BASE_URL"],
        default_base_url: Some("https://api.openai.com/v1"),
        protocol: ProviderProtocol::OpenAi,
    },
    ProviderDescriptor {
        id: "openai-codex",
        aliases: &["codex"],
        auth_env: &[
            "OPENAI_CODEX_TOKEN",
            "OPENAI_CODEX_ACCESS_TOKEN",
            "CODEX_API_KEY",
        ],
        base_url_env: &["OPENAI_CODEX_BASE_URL", "OPENAI_BASE_URL"],
        default_base_url: Some("https://api.openai.com/v1"),
        protocol: ProviderProtocol::Codex,
    },
    ProviderDescriptor {
        id: "claude-code",
        aliases: &[],
        auth_env: &[],
        base_url_env: &[],
        default_base_url: None,
        protocol: ProviderProtocol::SubscriptionCli,
    },
    ProviderDescriptor {
        id: "github-copilot",
        aliases: &["copilot"],
        auth_env: &[],
        base_url_env: &[],
        default_base_url: None,
        protocol: ProviderProtocol::SubscriptionCli,
    },
    ProviderDescriptor {
        id: "azure-openai",
        aliases: &["azure"],
        auth_env: &["AZURE_OPENAI_API_KEY"],
        base_url_env: &["AZURE_OPENAI_ENDPOINT", "AZURE_OPENAI_BASE_URL"],
        default_base_url: None,
        protocol: ProviderProtocol::AzureOpenAi,
    },
    ProviderDescriptor {
        id: "anthropic",
        aliases: &["claude"],
        auth_env: &["ANTHROPIC_API_KEY"],
        base_url_env: &["ANTHROPIC_BASE_URL"],
        default_base_url: Some("https://api.anthropic.com/v1"),
        protocol: ProviderProtocol::Anthropic,
    },
    ProviderDescriptor {
        id: "bedrock",
        aliases: &["aws-bedrock"],
        auth_env: &[
            "AWS_ACCESS_KEY_ID",
            "AWS_SECRET_ACCESS_KEY",
            "AWS_PROFILE",
            "AWS_CONFIG_FILE",
            "AWS_SHARED_CREDENTIALS_FILE",
            "AWS_WEB_IDENTITY_TOKEN_FILE",
            "AWS_CONTAINER_CREDENTIALS_RELATIVE_URI",
            "AWS_CONTAINER_CREDENTIALS_FULL_URI",
        ],
        base_url_env: &["AWS_BEDROCK_ENDPOINT"],
        default_base_url: None,
        protocol: ProviderProtocol::Bedrock,
    },
    ProviderDescriptor {
        id: "writer",
        aliases: &[],
        auth_env: &["WRITER_API_KEY"],
        base_url_env: &["WRITER_BASE_URL"],
        default_base_url: Some("https://api.writer.com/v1"),
        protocol: ProviderProtocol::OpenAiCompatible,
    },
    ProviderDescriptor {
        id: "xai",
        aliases: &["grok"],
        auth_env: &["XAI_API_KEY"],
        base_url_env: &["XAI_BASE_URL"],
        default_base_url: Some("https://api.x.ai/v1"),
        protocol: ProviderProtocol::OpenAiCompatible,
    },
    ProviderDescriptor {
        id: "groq",
        aliases: &[],
        auth_env: &["GROQ_API_KEY"],
        base_url_env: &["GROQ_BASE_URL"],
        default_base_url: Some("https://api.groq.com/openai/v1"),
        protocol: ProviderProtocol::OpenAiCompatible,
    },
    ProviderDescriptor {
        id: "cerebras",
        aliases: &[],
        auth_env: &["CEREBRAS_API_KEY"],
        base_url_env: &["CEREBRAS_BASE_URL"],
        default_base_url: Some("https://api.cerebras.ai/v1"),
        protocol: ProviderProtocol::OpenAiCompatible,
    },
    ProviderDescriptor {
        id: "openrouter",
        aliases: &[],
        auth_env: &["OPENROUTER_API_KEY"],
        base_url_env: &["OPENROUTER_BASE_URL"],
        default_base_url: Some("https://openrouter.ai/api/v1"),
        protocol: ProviderProtocol::OpenAiCompatible,
    },
    ProviderDescriptor {
        id: "llamacpp",
        aliases: &["llama.cpp", "llama-cpp"],
        auth_env: &[],
        base_url_env: &["LLAMA_CPP_BASE_URL"],
        default_base_url: Some("http://127.0.0.1:8080/v1"),
        protocol: ProviderProtocol::OpenAiCompatible,
    },
    ProviderDescriptor {
        id: "lmstudio",
        aliases: &["lm-studio"],
        auth_env: &[],
        base_url_env: &["LM_STUDIO_BASE_URL"],
        default_base_url: Some("http://127.0.0.1:1234/v1"),
        protocol: ProviderProtocol::OpenAiCompatible,
    },
    ProviderDescriptor {
        id: "ollama",
        aliases: &[],
        auth_env: &[],
        base_url_env: &["OLLAMA_BASE_URL"],
        default_base_url: Some("http://127.0.0.1:11434/v1"),
        protocol: ProviderProtocol::OpenAiCompatible,
    },
    ProviderDescriptor {
        id: "zai",
        aliases: &[],
        auth_env: &["ZAI_API_KEY"],
        base_url_env: &["ZAI_BASE_URL"],
        default_base_url: Some("https://api.z.ai/api/coding/paas/v4"),
        protocol: ProviderProtocol::OpenAiCompatible,
    },
    ProviderDescriptor {
        id: "mistral",
        aliases: &[],
        auth_env: &["MISTRAL_API_KEY"],
        base_url_env: &["MISTRAL_BASE_URL"],
        default_base_url: Some("https://api.mistral.ai/v1"),
        protocol: ProviderProtocol::OpenAiCompatible,
    },
    ProviderDescriptor {
        id: "deepseek",
        aliases: &[],
        auth_env: &["DEEPSEEK_API_KEY"],
        base_url_env: &["DEEPSEEK_BASE_URL"],
        default_base_url: Some("https://api.deepseek.com/v1"),
        protocol: ProviderProtocol::OpenAiCompatible,
    },
    ProviderDescriptor {
        id: "moonshot",
        aliases: &["kimi"],
        auth_env: &["MOONSHOT_API_KEY", "KIMI_API_KEY"],
        base_url_env: &["MOONSHOT_BASE_URL", "KIMI_BASE_URL"],
        default_base_url: Some("https://api.moonshot.ai/v1"),
        protocol: ProviderProtocol::OpenAiCompatible,
    },
    ProviderDescriptor {
        id: "dashscope",
        aliases: &["qwen"],
        auth_env: &["DASHSCOPE_API_KEY", "QWEN_API_KEY"],
        base_url_env: &["DASHSCOPE_BASE_URL", "QWEN_BASE_URL"],
        default_base_url: Some("https://dashscope-intl.aliyuncs.com/compatible-mode/v1"),
        protocol: ProviderProtocol::OpenAiCompatible,
    },
    ProviderDescriptor {
        id: "minimax",
        aliases: &[],
        auth_env: &["MINIMAX_API_KEY"],
        base_url_env: &["MINIMAX_BASE_URL"],
        default_base_url: Some("https://api.minimax.io/v1"),
        protocol: ProviderProtocol::OpenAiCompatible,
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolved_provider_debug_never_exposes_credentials() {
        let resolved = ResolvedProvider {
            provider: ProviderRegistry::descriptor("openai").unwrap(),
            auth_source: Some("OPENAI_API_KEY".into()),
            credential: Some("private-credential-fixture".into()),
            base_url: None,
        };
        let debug = format!("{resolved:?}");
        assert!(!debug.contains("private-credential-fixture"));
        assert!(debug.contains("credential_present: true"));
    }

    #[test]
    fn llamacpp_resolves_to_the_local_openai_compatible_server() {
        let resolved = ProviderRegistry::resolve("llamacpp/Qwen3.8-27B", &HashMap::new())
            .expect("llama.cpp provider must resolve without credentials");

        assert_eq!(
            resolved.provider.protocol,
            ProviderProtocol::OpenAiCompatible
        );
        assert_eq!(
            resolved.base_url.as_deref(),
            Some("http://127.0.0.1:8080/v1")
        );
        assert_eq!(resolved.credential, None);
    }

    #[test]
    fn local_openai_compatible_providers_are_authless_and_overridable() {
        for (route, provider, default_url, env_name) in [
            (
                "lmstudio/model-a",
                "lmstudio",
                "http://127.0.0.1:1234/v1",
                "LM_STUDIO_BASE_URL",
            ),
            (
                "ollama/model-b",
                "ollama",
                "http://127.0.0.1:11434/v1",
                "OLLAMA_BASE_URL",
            ),
        ] {
            let resolved = ProviderRegistry::resolve(route, &HashMap::new())
                .expect("local provider must resolve without credentials");
            assert_eq!(resolved.provider.id, provider);
            assert_eq!(
                resolved.provider.protocol,
                ProviderProtocol::OpenAiCompatible
            );
            assert_eq!(resolved.base_url.as_deref(), Some(default_url));
            assert_eq!(resolved.credential, None);

            let env =
                HashMap::from([(env_name.to_owned(), "http://127.0.0.1:9999/v1/".to_owned())]);
            assert_eq!(
                ProviderRegistry::resolve(route, &env)
                    .expect("local provider override must resolve")
                    .base_url
                    .as_deref(),
                Some("http://127.0.0.1:9999/v1")
            );
        }
    }

    #[test]
    fn bedrock_sso_session_marker_is_not_allowlisted() {
        let env = HashMap::from([(
            "AWS_SSO_SESSION_NAME".to_string(),
            "default-sso".to_string(),
        )]);
        let resolved = ProviderRegistry::resolve("bedrock/model", &env).unwrap();
        assert_eq!(resolved.auth_source, None);
        let error = ProviderRegistry::require("bedrock/model", &env)
            .expect_err("a standalone SSO session marker is not a credential source")
            .to_string();
        assert!(error.contains("AWS_PROFILE"));
        assert!(!error.contains("AWS_SSO_SESSION_NAME"));
    }

    #[test]
    fn bare_codex_models_use_openai_credentials() {
        let env = HashMap::from([("OPENAI_API_KEY".to_string(), "secret".to_string())]);
        let resolved = ProviderRegistry::require("gpt-5.1-codex-max", &env).unwrap();
        assert_eq!(resolved.provider.id, "openai");
        assert_eq!(resolved.auth_source.as_deref(), Some("OPENAI_API_KEY"));
    }

    #[test]
    fn op_reference_credentials_resolve_through_op_cli() {
        let _fake = op_secret::test_support::FakeOp::install();
        let env = HashMap::from([(
            "OPENAI_API_KEY".to_string(),
            "op://vault/item/registry".to_string(),
        )]);
        let resolved = ProviderRegistry::require("openai/gpt-4o", &env).unwrap();
        assert_eq!(
            resolved.credential.as_deref(),
            Some("resolved-secret-value")
        );
        assert_eq!(resolved.auth_source.as_deref(), Some("OPENAI_API_KEY"));
    }

    #[test]
    fn explicit_codex_provider_keeps_codex_credentials() {
        let env = HashMap::from([("OPENAI_CODEX_TOKEN".to_string(), "secret".to_string())]);
        let resolved = ProviderRegistry::require("openai-codex/gpt-5.1-codex-max", &env).unwrap();
        assert_eq!(resolved.provider.id, "openai-codex");
    }

    #[test]
    fn evalops_managed_provider_has_gateway_default() {
        let env = HashMap::from([(
            "MAESTRO_EVALOPS_ACCESS_TOKEN".to_string(),
            "delegated-token".to_string(),
        )]);
        let resolved = ProviderRegistry::require("evalops/gpt-4o-mini", &env).unwrap();
        assert_eq!(resolved.provider.protocol, ProviderProtocol::Managed);
        assert_eq!(
            resolved.base_url.as_deref(),
            Some("https://llm-gateway.evalops.dev/v1")
        );
    }

    #[test]
    fn openrouter_accepts_opaque_nested_model_ids() {
        let env = HashMap::from([("OPENROUTER_API_KEY".to_string(), "secret".to_string())]);
        let resolved =
            ProviderRegistry::require("openrouter/anthropic/claude-sonnet-4.5:free", &env).unwrap();

        assert_eq!(resolved.provider.id, "openrouter");
        assert_eq!(
            resolved.provider.protocol,
            ProviderProtocol::OpenAiCompatible
        );
        assert_eq!(
            resolved.base_url.as_deref(),
            Some("https://openrouter.ai/api/v1")
        );
    }

    #[test]
    fn vertex_ai_prefers_access_token_over_api_key() {
        let env = HashMap::from([
            ("VERTEX_ACCESS_TOKEN".to_string(), "oauth-token".to_string()),
            ("GOOGLE_API_KEY".to_string(), "api-key".to_string()),
        ]);
        let resolved = ProviderRegistry::require("vertex-ai/gemini-2.5-pro", &env).unwrap();

        assert_eq!(resolved.provider.id, "vertex-ai");
        assert_eq!(resolved.provider.protocol, ProviderProtocol::VertexAi);
        assert_eq!(resolved.auth_source.as_deref(), Some("VERTEX_ACCESS_TOKEN"));
        assert_eq!(resolved.credential.as_deref(), Some("oauth-token"));
        assert_eq!(
            ProviderRegistry::descriptor("vertex").map(|value| value.id),
            Some("vertex-ai")
        );
    }

    #[test]
    fn evalops_managed_provider_reads_token_from_access_token_file() {
        let dir =
            std::env::temp_dir().join(format!("maestro-ai-rs-token-file-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("evalops-access-token");
        std::fs::write(&path, "tenant-mint-from-bootstrap\n").expect("write token file");
        let env = HashMap::from([(
            "MAESTRO_EVALOPS_ACCESS_TOKEN_FILE".to_string(),
            path.display().to_string(),
        )]);
        let resolved = ProviderRegistry::require("evalops/gpt-4o-mini", &env).unwrap();
        assert_eq!(
            resolved.credential.as_deref(),
            Some("tenant-mint-from-bootstrap")
        );
        assert_eq!(
            resolved.auth_source.as_deref(),
            Some("MAESTRO_EVALOPS_ACCESS_TOKEN_FILE")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn evalops_direct_token_env_wins_over_token_file() {
        let dir = std::env::temp_dir().join(format!(
            "maestro-ai-rs-token-file-pref-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("evalops-access-token");
        std::fs::write(&path, "from-file").expect("write token file");
        let env = HashMap::from([
            (
                "MAESTRO_EVALOPS_ACCESS_TOKEN".to_string(),
                "from-env".to_string(),
            ),
            (
                "MAESTRO_EVALOPS_ACCESS_TOKEN_FILE".to_string(),
                path.display().to_string(),
            ),
        ]);
        let resolved = ProviderRegistry::require("evalops/gpt-4o-mini", &env).unwrap();
        assert_eq!(resolved.credential.as_deref(), Some("from-env"));
        assert_eq!(
            resolved.auth_source.as_deref(),
            Some("MAESTRO_EVALOPS_ACCESS_TOKEN")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}

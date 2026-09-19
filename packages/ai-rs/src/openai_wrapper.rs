//! Provider-aware wrapper around Maestro's shared OpenAI-compatible client.
//!
//! Most providers use the common adapter unchanged. Direct Moonshot Kimi K3
//! requests are dispatched to the K3 codec because K3's always-on reasoning
//! state is carried in `reasoning_content` and must be replayed with tool calls.

use anyhow::Result;
use reqwest::header::AUTHORIZATION;
use tokio::sync::mpsc;

use super::client::{AiClient, AiProvider, CancellableStream, provider_model_name};
use super::kimi::KimiK3Client;
use super::openai_base;
use super::types::{Message, RequestConfig, StreamEvent};

#[derive(Clone)]
pub struct OpenAiClient {
    base: openai_base::OpenAiClient,
    kimi_k3: Option<KimiK3Client>,
    api_key: String,
    base_url: Option<String>,
    route_provider: Option<String>,
    managed_gateway: bool,
}

impl OpenAiClient {
    fn from_base(base: openai_base::OpenAiClient, base_url: Option<String>) -> Result<Self> {
        let api_key = base
            .headers()
            .get(AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "))
            .unwrap_or_default()
            .to_string();
        let kimi_k3 = match base_url.as_deref() {
            Some(url) if url.contains("moonshot.ai") => {
                Some(KimiK3Client::new(api_key.clone(), url)?)
            }
            _ => None,
        };

        Ok(Self {
            base,
            kimi_k3,
            api_key,
            base_url,
            route_provider: None,
            managed_gateway: false,
        })
    }

    pub fn new(api_key: impl Into<String>) -> Result<Self> {
        let base = openai_base::OpenAiClient::new(api_key.into())?;
        Self::from_base(base, None)
    }

    pub fn with_base_url(api_key: impl Into<String>, base_url: impl Into<String>) -> Result<Self> {
        let base_url = base_url.into();
        let base = openai_base::OpenAiClient::with_base_url(api_key.into(), base_url.clone())?;
        Self::from_base(base, Some(base_url))
    }

    /// Exact administrator-approved route, without proxy or redirect discovery.
    pub fn with_disconnected_endpoint(
        api_key: impl Into<String>,
        endpoint: impl Into<String>,
        provider: &str,
    ) -> Result<Self> {
        let api_key = api_key.into();
        let endpoint = endpoint.into();
        let base = openai_base::OpenAiClient::with_disconnected_endpoint(
            api_key.clone(),
            endpoint.clone(),
            provider,
        )?;
        Ok(Self {
            base,
            kimi_k3: None,
            api_key,
            base_url: Some(endpoint),
            route_provider: Some(provider.to_string()),
            managed_gateway: false,
        })
    }

    pub(crate) fn with_route_provider(mut self, provider: &str) -> Self {
        self.base = self.base.with_route_provider(provider);
        let provider = provider.trim();
        self.route_provider = Some(provider.to_string());
        if self.kimi_k3.is_none()
            && (provider.eq_ignore_ascii_case("moonshot") || provider.eq_ignore_ascii_case("kimi"))
        {
            if let Some(base_url) = self.base_url.as_deref() {
                self.kimi_k3 = KimiK3Client::new(self.api_key.clone(), base_url).ok();
            }
        }
        self
    }

    pub(crate) fn routed_provider(&self) -> Option<&str> {
        self.route_provider
            .as_deref()
            .or_else(|| self.base.routed_provider())
    }

    pub(crate) fn is_managed_gateway(&self) -> bool {
        self.managed_gateway || self.base.is_managed_gateway()
    }

    pub(crate) fn managed_gateway_scope(&self) -> Option<(&str, &str)> {
        self.base.managed_gateway_scope()
    }

    pub(crate) fn cache_namespace(&self) -> Result<String> {
        self.base.cache_namespace()
    }

    pub(crate) fn set_managed_request_lineage(&mut self, lineage_id: Option<String>) {
        self.base.set_managed_request_lineage(lineage_id);
    }

    pub(crate) fn set_managed_inference_authorization(&mut self, authorization: Option<String>) {
        self.base.set_managed_inference_authorization(authorization);
    }

    pub(crate) fn set_managed_authorization_provider(
        &mut self,
        provider: std::sync::Arc<dyn crate::managed_authorization::ManagedAuthorizationProvider>,
    ) {
        self.base.set_managed_authorization_provider(provider);
    }

    #[cfg(test)]
    pub(crate) fn headers(&self) -> reqwest::header::HeaderMap {
        self.base.headers()
    }

    #[cfg(test)]
    pub(crate) fn with_response_open_timeout_for_test(
        mut self,
        timeout: std::time::Duration,
    ) -> Self {
        self.base = self.base.with_response_open_timeout_for_test(timeout);
        self
    }

    pub(crate) fn with_managed_gateway_scope(
        mut self,
        organization_id: &str,
        workspace_id: &str,
        provider_ref: serde_json::Value,
    ) -> Result<Self> {
        self.base =
            self.base
                .with_managed_gateway_scope(organization_id, workspace_id, provider_ref)?;
        self.managed_gateway = true;
        Ok(self)
    }

    pub fn from_env() -> Result<Self> {
        Self::from_base(openai_base::OpenAiClient::from_env()?, None)
    }

    pub fn mistral_from_env() -> Result<Self> {
        Self::from_base(
            openai_base::OpenAiClient::mistral_from_env()?,
            Some("https://api.mistral.ai/v1".to_string()),
        )
    }

    pub fn groq_from_env() -> Result<Self> {
        Self::from_base(
            openai_base::OpenAiClient::groq_from_env()?,
            Some("https://api.groq.com/openai/v1".to_string()),
        )
    }

    pub fn deepseek_from_env() -> Result<Self> {
        Self::from_base(
            openai_base::OpenAiClient::deepseek_from_env()?,
            Some("https://api.deepseek.com/v1".to_string()),
        )
    }

    pub fn moonshot_from_env() -> Result<Self> {
        Self::from_base(
            openai_base::OpenAiClient::moonshot_from_env()?,
            Some("https://api.moonshot.ai/v1".to_string()),
        )
    }

    pub fn qwen_from_env() -> Result<Self> {
        Self::from_base(
            openai_base::OpenAiClient::qwen_from_env()?,
            Some("https://dashscope-intl.aliyuncs.com/compatible-mode/v1".to_string()),
        )
    }

    pub fn minimax_from_env() -> Result<Self> {
        Self::from_base(
            openai_base::OpenAiClient::minimax_from_env()?,
            Some("https://api.minimax.io/v1".to_string()),
        )
    }

    pub fn zai_from_env() -> Result<Self> {
        Self::from_base(
            openai_base::OpenAiClient::zai_from_env()?,
            Some("https://api.z.ai/api/coding/paas/v4".to_string()),
        )
    }

    fn uses_native_kimi_k3(&self, model: &str) -> bool {
        if self.is_managed_gateway()
            || !provider_model_name(model).eq_ignore_ascii_case("kimi-k3")
            || self.kimi_k3.is_none()
        {
            return false;
        }

        let route_is_moonshot = self.route_provider.as_deref().is_some_and(|provider| {
            provider.eq_ignore_ascii_case("moonshot") || provider.eq_ignore_ascii_case("kimi")
        });
        let endpoint_is_moonshot = self
            .base_url
            .as_deref()
            .is_some_and(|base_url| base_url.contains("moonshot.ai"));

        route_is_moonshot || endpoint_is_moonshot
    }

    pub(crate) async fn stream_with_producer(
        &self,
        messages: &[Message],
        config: &RequestConfig,
    ) -> Result<CancellableStream> {
        if let Some(client) = self
            .kimi_k3
            .as_ref()
            .filter(|_| self.uses_native_kimi_k3(&config.model))
        {
            return client.stream(messages, config).await.map(Into::into);
        }
        self.base.stream_with_producer(messages, config).await
    }
}

impl AiClient for OpenAiClient {
    async fn stream(
        &self,
        messages: &[Message],
        config: &RequestConfig,
    ) -> Result<mpsc::UnboundedReceiver<StreamEvent>> {
        Ok(self
            .stream_with_producer(messages, config)
            .await?
            .into_receiver())
    }

    fn provider(&self) -> AiProvider {
        self.base.provider()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn direct_moonshot_kimi_k3_uses_the_native_codec() {
        let client = OpenAiClient::with_base_url("test-key", "https://api.moonshot.ai/v1")
            .unwrap()
            .with_route_provider("moonshot");

        assert!(client.uses_native_kimi_k3("kimi-k3"));
        assert!(client.uses_native_kimi_k3("moonshot/kimi-k3"));
        assert!(!client.uses_native_kimi_k3("kimi-k2.6"));
    }

    #[test]
    fn non_moonshot_and_managed_routes_keep_the_shared_adapter() {
        let openrouter = OpenAiClient::with_base_url("test-key", "https://openrouter.ai/api/v1")
            .unwrap()
            .with_route_provider("openrouter");
        assert!(!openrouter.uses_native_kimi_k3("moonshotai/kimi-k3"));

        let managed =
            OpenAiClient::with_base_url("delegated-token", "https://llm-gateway.evalops.dev/v1")
                .unwrap()
                .with_route_provider("moonshot")
                .with_managed_gateway_scope(
                    "org_123",
                    "workspace_456",
                    serde_json::json!({
                        "provider": "moonshot",
                        "environment": "production",
                        "credential_name": "default"
                    }),
                )
                .unwrap();
        assert!(!managed.uses_native_kimi_k3("kimi-k3"));
    }
}

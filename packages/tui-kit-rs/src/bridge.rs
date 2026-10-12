//! Bridge status types for Maestro <-> Conductor integration.
//!
//! These types mirror the JSON payload returned by the Maestro web bridge
//! (`/api/bridge/status`) so Rust consumers can parse or proxy status data.

use serde::{Deserialize, Serialize};

/// Default model/approval settings exposed by the bridge.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct BridgeDefaults {
    #[serde(default)]
    pub approval_mode: Option<String>,
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub model_id: Option<String>,
}

/// Client tool capabilities reported by the bridge.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct BridgeClientTools {
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub clients: Option<Vec<String>>,
    #[serde(default)]
    pub headers: Option<Vec<String>>,
}

/// Bridge status payload from `/api/bridge/status`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct BridgeStatus {
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub server_time: Option<String>,
    #[serde(default)]
    pub defaults: Option<BridgeDefaults>,
    #[serde(default)]
    pub client_tools: Option<BridgeClientTools>,
    #[serde(default)]
    pub approval_modes: Option<Vec<String>>,
}

/// Fetch bridge status from a Maestro web server.
pub async fn fetch_bridge_status(base_url: &str) -> Result<BridgeStatus, reqwest::Error> {
    let base = base_url.trim_end_matches('/');
    let url = format!("{}/api/bridge/status", base);
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()?;
    let response = client.get(url).send().await?;
    response.error_for_status_ref()?;
    response.json::<BridgeStatus>().await
}

//! Read-only validation of a provider-owned ChatGPT subscription sign-in.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::codex_app_server::{CodexAppServerClient, InitializeOptions};

/// Run app-server on a dedicated thread so synchronous connection commands can
/// inspect keyring-backed Codex sign-in without reading or copying its tokens.
pub fn check_chatgpt_profile(profile: Option<&str>, workspace: &Path) -> Result<()> {
    let identity = crate::codex_identity::resolve_codex_identity(profile, workspace)?;
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        runtime.block_on(async move {
            let client = CodexAppServerClient::spawn_with_env(
                None,
                None,
                Some(5_000),
                &identity.child_env(),
            )
            .await
            .context("Codex app-server unavailable")?;
            let result = tokio::time::timeout(Duration::from_secs(10), async {
                client.initialize(InitializeOptions::default()).await?;
                let account = client.read_account(true).await?;
                if account
                    .account
                    .as_ref()
                    .and_then(|account| account.get("type"))
                    .and_then(serde_json::Value::as_str)
                    != Some("chatgpt")
                {
                    bail!(
                        "Codex profile is not signed in with ChatGPT; run `deixic-code codex login`"
                    );
                }
                Ok(())
            })
            .await
            .context("Codex subscription check timed out")
            .and_then(|result| result);
            client.close();
            result
        })
    })
    .join()
    .map_err(|_| anyhow::anyhow!("Codex subscription check failed"))?
}

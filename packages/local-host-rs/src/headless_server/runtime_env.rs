use anyhow::{Context, Result};

pub(super) fn required_governed_runtime_env(name: &str) -> Result<String> {
    std::env::var(name)
        .with_context(|| format!("governed code requires runtime-owned {name}"))
        .and_then(|value| {
            let value = value.trim();
            if value.is_empty() {
                anyhow::bail!("governed code requires non-empty runtime-owned {name}");
            }
            Ok(value.to_string())
        })
}

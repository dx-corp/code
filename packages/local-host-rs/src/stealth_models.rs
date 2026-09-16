//! User-global consent for OpenRouter Stealth models.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

pub const CONSENT_VERSION: u16 = 1;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StealthModelConsent {
    pub enabled: bool,
    pub consent_version: u16,
    pub revision: u32,
    pub granted_at: String,
}

impl StealthModelConsent {
    #[must_use]
    pub fn permits(&self) -> bool {
        self.enabled
            && self.consent_version == CONSENT_VERSION
            && self.revision > 0
            && chrono::DateTime::parse_from_rfc3339(&self.granted_at).is_ok()
    }
}

fn path() -> Option<PathBuf> {
    crate::path_utils::maestro_home_dir().map(|home| home.join("config.toml"))
}

fn load_at(path: &Path) -> Result<StealthModelConsent> {
    if !path.exists() {
        return Ok(StealthModelConsent::default());
    }
    let config: toml::Value = toml::from_str(&std::fs::read_to_string(path)?)?;
    config
        .get("stealth_models")
        .cloned()
        .map(toml::Value::try_into)
        .transpose()
        .map(|value| value.unwrap_or_default())
        .map_err(Into::into)
}

pub fn load() -> Result<StealthModelConsent> {
    load_at(&path().context("User configuration is unavailable")?)
}

fn save_at(path: &Path, enabled: bool) -> Result<StealthModelConsent> {
    let parent = path.parent().context("User configuration has no parent")?;
    std::fs::create_dir_all(parent)?;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(parent.join("config.lock"))?;
    let mut lock = fd_lock::RwLock::new(file);
    let _guard = lock.write()?;
    let mut root: toml::Value = if path.exists() {
        toml::from_str(&std::fs::read_to_string(path)?)?
    } else {
        toml::Value::Table(toml::map::Map::new())
    };
    let (mut consent, consent_was_valid) = match root.get("stealth_models").cloned() {
        Some(value) => match value.try_into() {
            Ok(consent) => (consent, true),
            Err(_) => (StealthModelConsent::default(), false),
        },
        None => (StealthModelConsent::default(), true),
    };
    if consent_was_valid && ((!enabled && !consent.enabled) || (enabled && consent.permits())) {
        return Ok(consent);
    }
    if enabled {
        consent.revision = consent
            .revision
            .checked_add(1)
            .context("Consent revision exhausted")?;
        consent.consent_version = CONSENT_VERSION;
        consent.granted_at =
            chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    }
    consent.enabled = enabled;
    root.as_table_mut()
        .context("User config must be a TOML table")?
        .insert("stealth_models".into(), toml::Value::try_from(&consent)?);
    crate::path_utils::atomic_private_write(path, toml::to_string_pretty(&root)?.as_bytes())?;
    Ok(consent)
}

pub fn set_enabled(enabled: bool) -> Result<StealthModelConsent> {
    save_at(
        &path().context("User configuration is unavailable")?,
        enabled,
    )
}

#[must_use]
pub fn permits_current() -> bool {
    load().is_ok_and(|consent| consent.permits())
}

#[must_use]
pub fn is_openrouter_stealth_route(route: &str) -> bool {
    route
        .trim()
        .split_once('/')
        .is_some_and(|(provider, model_id)| is_openrouter_stealth_model(provider, model_id))
}

#[must_use]
pub fn is_openrouter_stealth_model(provider: &str, model_id: &str) -> bool {
    provider.eq_ignore_ascii_case("openrouter")
        && model_id
            .strip_prefix("stealth/")
            .is_some_and(|model| !model.trim().is_empty())
}

fn blocked_reason_at(path: &Path, route: &str) -> Option<String> {
    if !is_openrouter_stealth_route(route) {
        return None;
    }
    match load_at(path) {
        Ok(consent) if consent.permits() => None,
        Ok(_) => Some(format!(
            "OpenRouter Stealth model \"{}\" is disabled. Review the disclosure and opt in with `/stealth-models on` or `deixic-code stealth-models on`.",
            route.trim()
        )),
        Err(error) => Some(format!(
            "OpenRouter Stealth model \"{}\" is blocked because user consent could not be read: {error}. Repair the user configuration, then opt in again.",
            route.trim()
        )),
    }
}

#[must_use]
pub fn check_model_allowed(route: &str) -> Option<String> {
    if !is_openrouter_stealth_route(route) {
        return None;
    }
    let Some(path) = path() else {
        return Some(format!(
            "OpenRouter Stealth model \"{}\" is blocked because user configuration is unavailable.",
            route.trim()
        ));
    };
    blocked_reason_at(&path, route)
}

fn disclosure(consent: &StealthModelConsent) -> String {
    format!(
        "Stealth models: {}\nOpenRouter Stealth models are experimental and subject to separate OpenRouter Stealth terms.\nThe upstream provider identity is hidden. Prompts and responses may be retained or used for training, evaluation, or improvement under provider-specific terms.\nDo not send confidential, customer, or regulated data unless your organization has authorized that use.\nModels, capabilities, pricing, and availability may change or disappear without notice.\nThis setting only lets Maestro use openrouter/stealth/* routes; it does not change your OpenRouter privacy settings, provider routing, or account guardrails.\nTurn this off to block the next provider request. Existing provider-side data remains governed by the applicable provider and OpenRouter terms.",
        if consent.permits() { "On" } else { "Off" }
    )
}

fn command_at(path: &Path, action: &str) -> Result<String> {
    let consent = match action {
        "on" => save_at(path, true)?,
        "off" => save_at(path, false)?,
        "status" | "" => load_at(path)?,
        _ => bail!("Usage: stealth-models [status|on|off]"),
    };
    Ok(disclosure(&consent))
}

pub fn command(action: &str) -> Result<String> {
    command_at(
        &path().context("User configuration is unavailable")?,
        action,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_openrouter_stealth_routes_require_consent() {
        for route in [
            "openrouter/stealth/union-alphax",
            "OpenRouter/stealth/union-alphax",
            " openrouter/stealth/future-model ",
        ] {
            assert!(is_openrouter_stealth_route(route), "{route}");
        }
        for route in [
            "openrouter/openai/gpt-5.6",
            "stealth/union-alphax",
            "openrouter/stealth/",
            "openrouter/Stealth/union-alphax",
        ] {
            assert!(!is_openrouter_stealth_route(route), "{route}");
        }
        assert!(is_openrouter_stealth_model(
            "openrouter",
            "stealth/union-alphax"
        ));
        assert!(is_openrouter_stealth_model(
            "OpenRouter",
            "stealth/union-alphax"
        ));
        assert!(!is_openrouter_stealth_model("openrouter", "openai/gpt-5.6"));
    }

    #[test]
    fn consent_defaults_off_is_versioned_and_preserves_other_settings() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        assert!(!load_at(&path).unwrap().permits());
        std::fs::write(&path, "model = 'example'\n").unwrap();

        let on = save_at(&path, true).unwrap();
        assert!(on.permits());
        assert_eq!(on.consent_version, CONSENT_VERSION);
        assert_eq!(on.revision, 1);
        assert!(std::fs::read_to_string(&path).unwrap().contains("example"));
        assert_eq!(save_at(&path, true).unwrap().revision, 1);

        assert!(!save_at(&path, false).unwrap().permits());
        let renewed = save_at(&path, true).unwrap();
        assert!(renewed.permits());
        assert_eq!(renewed.revision, 2);

        std::fs::write(
            &path,
            format!(
                "[stealth_models]\nenabled = true\nconsent_version = {CONSENT_VERSION}\nrevision = 2\ngranted_at = 'not-a-timestamp'\n"
            ),
        )
        .unwrap();
        let recovered = save_at(&path, true).unwrap();
        assert!(recovered.permits());
        assert_eq!(recovered.revision, 3);
    }

    #[test]
    fn stale_malformed_or_unreadable_consent_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let mut consent = save_at(&path, true).unwrap();
        assert!(consent.permits());

        consent.consent_version = CONSENT_VERSION + 1;
        assert!(!consent.permits());
        consent.consent_version = CONSENT_VERSION;
        consent.granted_at = "not-a-timestamp".into();
        assert!(!consent.permits());

        std::fs::write(&path, "[stealth_models]\nenabled = true\nunknown = 1\n").unwrap();
        assert!(load_at(&path).is_err());
        assert!(blocked_reason_at(&path, "openrouter/stealth/union-alphax").is_some());
        assert!(blocked_reason_at(&path, "OpenRouter/stealth/union-alphax").is_some());
        assert!(blocked_reason_at(&path, "openrouter/openai/gpt-5.6").is_none());
    }

    #[test]
    fn explicit_mutation_repairs_only_the_malformed_consent_subsection() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");

        std::fs::write(
            &path,
            "model = 'example'\n[stealth_models]\nenabled = true\nunknown = 1\n",
        )
        .unwrap();
        assert!(load_at(&path).is_err());
        let recovered = save_at(&path, true).unwrap();
        assert!(recovered.permits());
        assert_eq!(recovered.revision, 1);
        assert!(std::fs::read_to_string(&path).unwrap().contains("example"));

        std::fs::write(&path, "model = 'example'\nstealth_models = 'not-a-table'\n").unwrap();
        assert!(load_at(&path).is_err());
        let revoked = save_at(&path, false).unwrap();
        assert!(!revoked.permits());
        assert!(std::fs::read_to_string(&path).unwrap().contains("example"));
        assert!(!load_at(&path).unwrap().permits());
    }

    #[test]
    fn command_discloses_risk_and_opt_in_does_not_claim_to_change_openrouter_privacy() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let message = command_at(&path, "on").unwrap();
        for disclosure in [
            "separate OpenRouter Stealth terms",
            "provider identity is hidden",
            "retained",
            "training",
            "confidential, customer, or regulated data",
            "may change or disappear",
            "does not change your OpenRouter privacy settings",
        ] {
            assert!(
                message.contains(disclosure),
                "missing disclosure: {disclosure}"
            );
        }
        assert!(load_at(&path).unwrap().permits());
        assert!(command_at(&path, "yes").is_err());
    }
}

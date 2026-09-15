//! User-global experiment consent. Repository configuration never participates.
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use maestro_runtime_contracts::experiments::{
    CONSENT_VERSION, EnrollmentType, ExperimentAssignment, ExperimentEnrollment, FOLLOW_UP_DAYS,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::telemetry::TelemetryIdentityScope;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ExperimentConsent {
    pub enabled: bool,
    pub consent_version: u16,
    pub revision: u32,
    pub granted_at: String,
    // Remains local; only a tenant-scoped pseudonym is exported.
    pub installation_seed: String,
}

fn path() -> Option<PathBuf> {
    crate::path_utils::maestro_home_dir().map(|home| home.join("config.toml"))
}

fn load_at(path: &Path) -> Result<ExperimentConsent> {
    if !path.exists() {
        return Ok(ExperimentConsent::default());
    }
    let config: toml::Value = toml::from_str(&std::fs::read_to_string(path)?)?;
    config
        .get("experiments")
        .cloned()
        .map(toml::Value::try_into)
        .transpose()
        .map(|value| value.unwrap_or_default())
        .map_err(Into::into)
}

pub fn load() -> Result<ExperimentConsent> {
    load_at(&path().context("User configuration is unavailable")?)
}

fn save_at(path: &Path, enabled: bool) -> Result<ExperimentConsent> {
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
    let mut consent = load_at(path)?;
    if consent.enabled == enabled && (!enabled || consent.consent_version == CONSENT_VERSION) {
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
        if consent.installation_seed.is_empty() {
            consent.installation_seed = Uuid::new_v4().to_string();
        }
    }
    consent.enabled = enabled;
    root.as_table_mut()
        .context("User config must be a TOML table")?
        .insert("experiments".into(), toml::Value::try_from(&consent)?);
    crate::path_utils::atomic_private_write(path, toml::to_string_pretty(&root)?.as_bytes())?;
    Ok(consent)
}

pub fn set_enabled(enabled: bool) -> Result<ExperimentConsent> {
    let consent = save_at(
        &path().context("User configuration is unavailable")?,
        enabled,
    )?;
    if !enabled {
        crate::telemetry::purge_revoked_experiment_events();
    }
    Ok(consent)
}

fn assignment(
    consent: &ExperimentConsent,
    scope: &TelemetryIdentityScope,
) -> Option<ExperimentAssignment> {
    if !consent.enabled
        || consent.consent_version != CONSENT_VERSION
        || consent.revision == 0
        || Uuid::parse_str(&consent.installation_seed).is_err()
        || chrono::DateTime::parse_from_rfc3339(&consent.granted_at).is_err()
        || !scope.is_complete()
    {
        return None;
    }
    Some(scope.experiment_assignment(&consent.installation_seed, consent.revision))
}

pub(crate) fn permits(scope: &TelemetryIdentityScope, expected: &ExperimentAssignment) -> bool {
    !crate::telemetry::experiments_telemetry_disabled()
        && load()
            .ok()
            .and_then(|consent| assignment(&consent, scope))
            .as_ref()
            == Some(expected)
}

/// Return an assignment only after its immutable enrollment is durably queued.
pub(crate) fn enroll(scope: &TelemetryIdentityScope, model: &str) -> Option<ExperimentAssignment> {
    if crate::telemetry::experiments_telemetry_disabled()
        || std::env::var_os("MAESTRO_TOOL_PROFILE").is_some()
        || !matches!(
            maestro_ai::ProviderRegistry::resolve_descriptor(model)
                .ok()?
                .id,
            "evalops" | "maestro-managed"
        )
    {
        return None;
    }
    let consent = load().ok()?;
    let assignment = assignment(&consent, scope)?;
    let event = ExperimentEnrollment {
        event_type: EnrollmentType::ExperimentEnrollment,
        schema_version: 1,
        event_id: assignment.assignment_id,
        timestamp: consent.granted_at,
        assignment: assignment.clone(),
        follow_up_days: FOLLOW_UP_DAYS,
    };
    crate::telemetry::queue_experiment_enrollment(scope, &event).then_some(assignment)
}

pub fn command(action: &str) -> Result<String> {
    let consent = match action {
        "on" => set_enabled(true)?,
        "off" => set_enabled(false)?,
        "status" | "" => load()?,
        _ => bail!("Usage: experiments [status|on|off]"),
    };
    Ok(format!(
        "Experiments: {}\nNative tool profile v1 compares standard tools with a smaller initial set.\nParticipation randomly assigns this installation within your signed-in workspace.\nRecords assignment, tool-surface fingerprints, model/request identifiers, numeric usage, timing and runtime outcomes; no prompts, code or tool contents.\nApplies at the next turn in a supported managed native session. Manual tool profiles and disabled telemetry exclude participation.\nTurn experiments off to stop collection and remove unsent experiment records. Already accepted records follow workspace retention.\nRuntime completion does not establish task correctness.",
        if consent.enabled { "On" } else { "Off" }
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn consent_defaults_off_and_preserves_other_user_settings() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        assert!(!load_at(&path).unwrap().enabled);
        std::fs::write(&path, "model = 'example'\n").unwrap();
        let on = save_at(&path, true).unwrap();
        assert!(on.enabled);
        assert_eq!(on.revision, 1);
        assert!(std::fs::read_to_string(&path).unwrap().contains("example"));
        assert_eq!(save_at(&path, true).unwrap().revision, 1);
        assert!(!save_at(&path, false).unwrap().enabled);
        let renewed = save_at(&path, true).unwrap();
        assert_eq!(renewed.revision, 2);
        assert_eq!(renewed.installation_seed, on.installation_seed);
    }
    #[test]
    fn invalid_consent_and_scope_cannot_assign() {
        let scope = TelemetryIdentityScope::new("org", Some("workspace")).unwrap();
        assert!(assignment(&ExperimentConsent::default(), &scope).is_none());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let mut consent = save_at(&path, true).unwrap();
        assert!(assignment(&consent, &scope).is_some());
        consent.consent_version = 99;
        assert!(assignment(&consent, &scope).is_none());
        std::fs::write(&path, "not = [ valid").unwrap();
        assert!(save_at(&path, true).is_err());
    }
}

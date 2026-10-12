//! Legacy hosted-runner environment keys and the warning metadata the
//! startup path emits for them.

use std::collections::HashMap;

use super::config::env_value;

/// Legacy environment keys still honoured for one deprecation window, paired
/// with the canonical `MAESTRO_*` key that replaces each of them. The
/// canonical key always wins; every legacy key that carries a value is
/// reported on the resolved config so the runner can warn at startup.
pub const DEPRECATED_ENV_ALIASES: &[(&str, &str)] = &[
    ("REMOTE_RUNNER_SESSION_ID", "MAESTRO_RUNNER_SESSION_ID"),
    ("WORKSPACE_ROOT", "MAESTRO_WORKSPACE_ROOT"),
    ("PORT", "MAESTRO_HOSTED_RUNNER_PORT"),
    ("MAESTRO_WEB_API_KEY", "MAESTRO_HOSTED_RUNNER_AUTH_TOKEN"),
    (
        "MAESTRO_WEB_API_KEY_FILE",
        "MAESTRO_HOSTED_RUNNER_AUTH_TOKEN_FILE",
    ),
    (
        "REMOTE_RUNNER_SNAPSHOT_ROOT",
        "MAESTRO_REMOTE_RUNNER_SNAPSHOT_ROOT",
    ),
    (
        "REMOTE_RUNNER_RESTORE_MANIFEST",
        "MAESTRO_REMOTE_RUNNER_RESTORE_MANIFEST",
    ),
    (
        "REMOTE_RUNNER_OWNER_INSTANCE_ID",
        "MAESTRO_REMOTE_RUNNER_OWNER_INSTANCE_ID",
    ),
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeprecatedEnvAlias {
    pub alias: &'static str,
    pub canonical: &'static str,
    /// The canonical key was also set with a different value, so the legacy
    /// value was ignored.
    pub shadowed: bool,
}

impl DeprecatedEnvAlias {
    pub fn warning(&self) -> String {
        let shadowed = if self.shadowed {
            format!(
                "; its value disagrees with {} and was ignored",
                self.canonical
            )
        } else {
            String::new()
        };
        format!(
            "hosted-runner environment key {} is deprecated; set {} instead (the legacy key stops being read after the next Maestro minor release){shadowed}",
            self.alias, self.canonical
        )
    }
}

pub(super) fn deprecated_env_aliases(env: &HashMap<String, String>) -> Vec<DeprecatedEnvAlias> {
    DEPRECATED_ENV_ALIASES
        .iter()
        .filter_map(|(alias, canonical)| {
            let legacy_value = env_value(env, alias)?;
            let shadowed = env_value(env, canonical).is_some_and(|value| value != legacy_value);
            Some(DeprecatedEnvAlias {
                alias,
                canonical,
                shadowed,
            })
        })
        .collect()
}

use std::collections::BTreeSet;
use std::fs;

use tempfile::tempdir;

use super::*;

#[test]
fn shadowed_legacy_web_api_key_is_reported_and_ignored() {
    let workspace = tempdir().expect("workspace");
    let mut env = base_hosted_runner_env(workspace.path());
    env.insert(
        "MAESTRO_HOSTED_RUNNER_AUTH_TOKEN".to_string(),
        "runner-secret".to_string(),
    );
    env.insert(
        "MAESTRO_WEB_API_KEY".to_string(),
        "legacy-secret".to_string(),
    );

    let config = HostedRunnerConfig::from_env_map(&env).expect("config");

    assert_eq!(config.auth_token.as_deref(), Some("runner-secret"));
    assert_eq!(
        config.deprecated_env_aliases,
        vec![DeprecatedEnvAlias {
            alias: "MAESTRO_WEB_API_KEY",
            canonical: "MAESTRO_HOSTED_RUNNER_AUTH_TOKEN",
            shadowed: true,
        }]
    );
    assert!(
        config.deprecated_env_aliases[0]
            .warning()
            .contains("disagrees with MAESTRO_HOSTED_RUNNER_AUTH_TOKEN and was ignored")
    );
}

#[test]
fn canonical_hosted_runner_env_reports_no_deprecated_aliases() {
    let workspace = tempdir().expect("workspace");
    let mut env = base_hosted_runner_env(workspace.path());
    env.insert(
        "MAESTRO_HOSTED_RUNNER_AUTH_TOKEN".to_string(),
        "runner-secret".to_string(),
    );
    env.insert("MAESTRO_HOSTED_RUNNER_PORT".to_string(), "9091".to_string());

    let config = HostedRunnerConfig::from_env_map(&env).expect("config");

    assert!(config.deprecated_env_aliases.is_empty());
}

#[test]
fn every_deprecated_hosted_runner_env_alias_resolves_like_its_canonical_key() {
    let workspace = tempdir().expect("workspace");
    let snapshot_root = workspace.path().join("snapshots");
    let manifest = workspace.path().join("restore.json");
    let token_file = workspace.path().join("token");
    fs::write(&token_file, "file-secret").expect("token file");
    let values: HashMap<&str, String> = HashMap::from([
        ("MAESTRO_RUNNER_SESSION_ID", "mrs_alias".to_string()),
        (
            "MAESTRO_WORKSPACE_ROOT",
            workspace.path().display().to_string(),
        ),
        ("MAESTRO_HOSTED_RUNNER_PORT", "9093".to_string()),
        (
            "MAESTRO_HOSTED_RUNNER_AUTH_TOKEN",
            "inline-secret".to_string(),
        ),
        (
            "MAESTRO_HOSTED_RUNNER_AUTH_TOKEN_FILE",
            token_file.display().to_string(),
        ),
        (
            "MAESTRO_REMOTE_RUNNER_SNAPSHOT_ROOT",
            snapshot_root.display().to_string(),
        ),
        (
            "MAESTRO_REMOTE_RUNNER_RESTORE_MANIFEST",
            manifest.display().to_string(),
        ),
        (
            "MAESTRO_REMOTE_RUNNER_OWNER_INSTANCE_ID",
            "owner_alias".to_string(),
        ),
    ]);
    let canonical_keys: BTreeSet<&str> = DEPRECATED_ENV_ALIASES
        .iter()
        .map(|(_, canonical)| *canonical)
        .collect();
    assert_eq!(
        canonical_keys,
        values.keys().copied().collect::<BTreeSet<_>>(),
        "alias table changed; extend this resolution matrix"
    );

    for (alias, canonical) in DEPRECATED_ENV_ALIASES {
        let mut canonical_env = base_hosted_runner_env(workspace.path());
        let mut alias_env = base_hosted_runner_env(workspace.path());
        // Inline and file bearer forms are mutually exclusive, so only the
        // form under test (or the inline form otherwise) is present.
        let excluded_auth_key = if *canonical == "MAESTRO_HOSTED_RUNNER_AUTH_TOKEN_FILE" {
            "MAESTRO_HOSTED_RUNNER_AUTH_TOKEN"
        } else {
            "MAESTRO_HOSTED_RUNNER_AUTH_TOKEN_FILE"
        };
        for (key, value) in &values {
            if *key == excluded_auth_key {
                continue;
            }
            canonical_env.insert((*key).to_string(), value.clone());
            alias_env.insert((*key).to_string(), value.clone());
        }
        alias_env.remove(*canonical);
        alias_env.insert((*alias).to_string(), values[canonical].clone());

        let expected = HostedRunnerConfig::from_env_map(&canonical_env)
            .unwrap_or_else(|error| panic!("canonical {canonical}: {error}"));
        let actual = HostedRunnerConfig::from_env_map(&alias_env)
            .unwrap_or_else(|error| panic!("alias {alias}: {error}"));

        assert!(expected.deprecated_env_aliases.is_empty(), "{canonical}");
        assert_eq!(
            actual.deprecated_env_aliases,
            vec![DeprecatedEnvAlias {
                alias,
                canonical,
                shadowed: false,
            }],
            "{alias}"
        );
        assert_eq!(
            actual.runner_session_id, expected.runner_session_id,
            "{alias}"
        );
        assert_eq!(actual.workspace_root, expected.workspace_root, "{alias}");
        assert_eq!(actual.bind_addr, expected.bind_addr, "{alias}");
        assert_eq!(actual.auth_token, expected.auth_token, "{alias}");
        assert_eq!(actual.snapshot_root, expected.snapshot_root, "{alias}");
        assert_eq!(
            actual.restore_manifest_path, expected.restore_manifest_path,
            "{alias}"
        );
        assert_eq!(
            actual.owner_instance_id, expected.owner_instance_id,
            "{alias}"
        );
    }
}

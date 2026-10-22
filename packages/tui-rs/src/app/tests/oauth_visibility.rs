use super::*;

#[tokio::test]
async fn configuration_visibility_uses_live_oauth_scope_and_rejects_stale_origins() {
    let _guard = crate::config::test_process_env_lock_async().await;
    struct Restore(Vec<(&'static str, Option<std::ffi::OsString>)>);
    impl Drop for Restore {
        fn drop(&mut self) {
            for (name, value) in &self.0 {
                match value {
                    Some(value) => std::env::set_var(name, value),
                    None => std::env::remove_var(name),
                }
            }
        }
    }
    let names = [
        "MAESTRO_HOME",
        "MAESTRO_OAUTH_STORAGE_MODE",
        "MAESTRO_HOSTED_RUNNER_MODE",
        "MAESTRO_EVALOPS_ACCESS_TOKEN",
        "MAESTRO_EVALOPS_ACCESS_TOKEN_FILE",
        "MAESTRO_EVALOPS_ORG_ID",
        "MAESTRO_EVALOPS_WORKSPACE_ID",
        "MAESTRO_IDENTITY_URL",
        crate::init_cli::TEST_IDENTITY_AUTHORITY_ENV,
        "MAESTRO_MANAGED_SETUP_URL",
        "MAESTRO_INTERNAL_TELEMETRY_DISABLED",
        "EVALOPS_INTERNAL_TELEMETRY_DISABLED",
        "MAESTRO_TELEMETRY",
    ];
    let _restore = Restore(
        names
            .iter()
            .map(|name| (*name, std::env::var_os(name)))
            .collect(),
    );
    for name in names {
        std::env::remove_var(name);
    }
    let home = tempdir().unwrap();
    std::env::set_var("MAESTRO_HOME", home.path());
    std::env::set_var("MAESTRO_OAUTH_STORAGE_MODE", "file");
    std::env::set_var("MAESTRO_TELEMETRY", "1");
    std::env::set_var(
        "MAESTRO_IDENTITY_URL",
        crate::credential_mode::test_identity_base_url(),
    );
    std::env::set_var(crate::init_cli::TEST_IDENTITY_AUTHORITY_ENV, "1");
    // A real loopback response rejects the managed lookup; no external service
    // or existing user's configuration participates in this boundary test.
    std::env::set_var(
        "MAESTRO_MANAGED_SETUP_URL",
        crate::credential_mode::test_identity_base_url(),
    );
    // A concurrent account reader can cache absence before this fixture writes
    // the OAuth file. Exercise that ordering without relying on thread timing.
    assert!(crate::init_cli::load_evalops_snapshot().unwrap().is_none());
    let oauth = serde_json::to_vec(&serde_json::json!({"evalops": {
        "type": "oauth", "access": "fixture-token", "refresh": "fixture-refresh",
        "expires": chrono::Utc::now().timestamp_millis() + 3_600_000,
        "metadata": {"organizationId": "mutable-local-org", "userId": "user-test",
            "identityBaseUrl": crate::credential_mode::test_identity_base_url()}
    }}))
    .unwrap();
    std::fs::write(home.path().join("oauth.json"), &oauth).unwrap();
    crate::init_cli::invalidate_evalops_credentials_cache();
    let snapshot = crate::init_cli::load_evalops_snapshot().unwrap().unwrap();
    assert!(snapshot.agent_mcp.is_none());
    let mut app = new_test_app_with_platform_resolution(PlatformSessionResolution::Detect);
    let origin = crate::telemetry::TelemetryIdentityScope::new("org-test", Some("workspace-test"));
    assert_eq!(app.managed_setup_identity_scope, origin);
    assert_eq!(app.managed_setup.setup().organization_id, "org-test");
    assert_eq!(app.managed_setup.setup().workspace_id, "workspace-test");
    assert_eq!(
        std::fs::read(home.path().join("oauth.json")).unwrap(),
        oauth
    );
    let event = crate::telemetry::VisibilityEvent::configuration(
        crate::telemetry::ConfigurationReceipt::from_managed_setup(
            &app.managed_setup,
            chrono::Utc::now().timestamp(),
        ),
    );
    let session = crate::credential_mode::current_verified_identity_session().unwrap();
    // Background runtime telemetry uses MAESTRO_HOME. Keep this recorder's
    // exact-count assertions on its own outbox when the suite runs in parallel.
    let isolated_outbox = tempdir().unwrap();
    let outbox = isolated_outbox.path().join("outbox");
    assert_eq!(
        crate::telemetry::test_record_visibility_with_session(
            &event,
            origin.clone(),
            session.clone(),
            &outbox,
        ),
        crate::telemetry::OnboardingCollectionStatus::Queued
    );
    let queued_paths = std::fs::read_dir(&outbox)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "json")
        })
        .collect::<Vec<_>>();
    assert_eq!(queued_paths.len(), 1);
    let persisted = std::fs::read_to_string(&queued_paths[0]).unwrap();
    assert!(persisted.contains("org-test"));
    assert!(persisted.contains("workspace-test"));
    assert!(!persisted.contains("fixture-token"));
    let wrong_origin =
        crate::telemetry::TelemetryIdentityScope::new("other-org", Some("other-workspace"));
    assert_eq!(
        crate::telemetry::test_record_visibility_with_session(
            &event,
            wrong_origin,
            session,
            &outbox
        ),
        crate::telemetry::OnboardingCollectionStatus::Unavailable
    );

    std::env::set_var("MAESTRO_EVALOPS_ORG_ID", "new-local-org");
    for token in ["inactive-token", "unscoped-token"] {
        std::env::set_var("MAESTRO_EVALOPS_ACCESS_TOKEN", token);
        app.managed_setup_identity_scope = origin.clone();
        assert!(app.refresh_managed_setup_after_identity_login().is_err());
        assert!(app.managed_setup_identity_scope.is_none());
        let denied = new_test_app_with_platform_resolution(PlatformSessionResolution::Detect);
        assert!(denied.managed_setup_identity_scope.is_none());
        assert_eq!(
            *denied.managed_setup.mcp_policy(),
            crate::managed_setup::McpPolicy::deny_all()
        );
    }
    std::env::set_var("MAESTRO_EVALOPS_ACCESS_TOKEN", "missing-workspace-token");
    let incomplete = new_test_app_with_platform_resolution(PlatformSessionResolution::Detect);
    assert!(incomplete.managed_setup_identity_scope.is_none());
    let session = crate::credential_mode::current_verified_identity_session().unwrap();
    assert_eq!(
        crate::telemetry::test_record_visibility_with_session(&event, origin, session, &outbox),
        crate::telemetry::OnboardingCollectionStatus::Unavailable
    );
    assert_eq!(
        std::fs::read_dir(outbox)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path
                .extension()
                .is_some_and(|extension| extension == "json"))
            .count(),
        1
    );
}

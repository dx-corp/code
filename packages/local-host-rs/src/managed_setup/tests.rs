use std::time::Duration;

use super::*;
use crate::sandbox_policy::NetworkAction;

fn session() -> PlatformSession {
    session_for("org-a", Some("workspace-a"))
}

fn session_for(organization_id: &str, workspace_id: Option<&str>) -> PlatformSession {
    PlatformSession {
        access_token: "access-token".to_string(),
        organization_id: organization_id.to_string(),
        workspace_id: workspace_id.map(str::to_string),
        provider_ref: serde_json::Value::Null,
        email: None,
        user_id: None,
    }
}

fn allowlist_setup() -> ManagedSetup {
    ManagedSetup {
        version: 7,
        issued_at: None,
        organization_id: "org-a".to_string(),
        workspace_id: "workspace-a".to_string(),
        rules: vec![ManagedRule {
            id: "runbook".to_string(),
            title: "Follow the runbook".to_string(),
            body_markdown: "Always link the incident ticket.".to_string(),
            scope: RuleScope::Organization,
        }],
        skills: vec![
            ManagedSkillRef {
                id: "incident-triage".to_string(),
                source: "deixic-marketplace".to_string(),
                version: "1.2.0".to_string(),
                required: true,
            },
            ManagedSkillRef {
                id: "optional-helper".to_string(),
                source: String::new(),
                version: String::new(),
                required: false,
            },
        ],
        mcp: McpPolicy {
            mode: McpPolicyMode::Allowlist,
            servers: vec![McpServerRef {
                name: "approved".to_string(),
                url_pattern: "https://mcp.example.com/*".to_string(),
                transport: "http".to_string(),
            }],
        },
        sandbox_policy_toml: "[network]\ndefault = \"deny\"\n".to_string(),
    }
}

// MCP enforcement

#[test]
fn allowlist_refuses_a_server_it_does_not_name() {
    let policy = allowlist_setup().mcp;
    assert_eq!(
        policy.decide("approved", Some("https://mcp.example.com/v1"), "http"),
        McpDecision::Allowed
    );
    assert_eq!(
        policy.decide(
            "some-other-server",
            Some("https://mcp.example.com/v1"),
            "http"
        ),
        McpDecision::RefusedNotAllowlisted
    );
}

#[test]
fn allowlist_requires_every_populated_selector_to_match() {
    let policy = allowlist_setup().mcp;
    assert_eq!(
        policy.decide("approved", Some("https://mcp.attacker.test/v1/sse"), "http"),
        McpDecision::RefusedNotAllowlisted
    );
    assert_eq!(
        policy.decide("approved", Some("https://mcp.example.com/v1/sse"), "stdio"),
        McpDecision::RefusedNotAllowlisted
    );
}

#[test]
fn allowlist_can_match_an_entry_with_only_a_url_pattern() {
    let policy = McpPolicy {
        mode: McpPolicyMode::Allowlist,
        servers: vec![McpServerRef {
            url_pattern: "https://mcp.example.com/*".to_string(),
            ..McpServerRef::default()
        }],
    };
    assert_eq!(
        policy.decide("unnamed", Some("https://mcp.example.com/v1/sse"), "http"),
        McpDecision::Allowed
    );
}

#[test]
fn url_allowlist_wildcards_cannot_cross_the_authority_boundary() {
    let policy = McpPolicy {
        mode: McpPolicyMode::Allowlist,
        servers: vec![McpServerRef {
            url_pattern: "https://*.example.com/*".to_string(),
            ..McpServerRef::default()
        }],
    };

    assert_eq!(
        policy.decide("managed", Some("https://mcp.example.com/v1"), "http"),
        McpDecision::Allowed
    );
    assert_eq!(
        policy.decide(
            "managed",
            Some("https://attacker.test/.example.com/v1"),
            "http"
        ),
        McpDecision::RefusedNotAllowlisted
    );
}

#[test]
fn denylist_refuses_a_server_it_names_and_permits_the_rest() {
    let policy = McpPolicy {
        mode: McpPolicyMode::Denylist,
        servers: vec![McpServerRef {
            name: "banned".to_string(),
            ..McpServerRef::default()
        }],
    };
    assert_eq!(
        policy.decide("banned", None, "stdio"),
        McpDecision::RefusedDenylisted
    );
    assert_eq!(
        policy.decide("anything-else", None, "stdio"),
        McpDecision::Allowed
    );
}

#[test]
fn an_unset_mode_is_treated_as_an_allowlist_not_as_open() {
    let policy = McpPolicy::default();
    assert_eq!(policy.mode, McpPolicyMode::Unspecified);
    assert_eq!(
        policy.decide("any-server", None, "stdio"),
        McpDecision::RefusedNotAllowlisted
    );
}

#[test]
fn open_mode_permits_every_server() {
    let policy = McpPolicy {
        mode: McpPolicyMode::Open,
        servers: Vec::new(),
    };
    assert_eq!(
        policy.decide("anything", None, "stdio"),
        McpDecision::Allowed
    );
}

// Fetch, cache, and fail-closed

#[test]
fn a_successful_fetch_is_written_to_the_cache() {
    let home = tempfile::tempdir().expect("tempdir");
    let cache = home.path().join(CACHE_FILE_NAME);
    let client = ManagedSetupClient::resolve_with(
        Some(&session()),
        Some(cache.as_path()),
        1_000,
        DEFAULT_CACHE_TTL,
        |_| Ok(allowlist_setup()),
    );
    assert_eq!(client.origin(), ManagedSetupOrigin::Fetched);
    assert_eq!(client.version(), 7);
    assert!(client.notices().is_empty());

    let written = read_cache(cache.as_path()).expect("cache written");
    assert_eq!(written.version, 7);
    assert_eq!(written.fetched_at, 1_000);
    assert_eq!(written.tenant_organization_id, "org-a");
    assert_eq!(written.tenant_workspace_id, "workspace-a");
    assert_eq!(written.setup, allowlist_setup());
}

#[test]
fn a_fetched_document_from_another_organization_is_rejected_and_not_cached() {
    let home = tempfile::tempdir().expect("tempdir");
    let cache = home.path().join(CACHE_FILE_NAME);
    let mut mismatched = allowlist_setup();
    mismatched.organization_id = "org-b".to_string();

    let client = ManagedSetupClient::resolve_with(
        Some(&session()),
        Some(cache.as_path()),
        1_000,
        DEFAULT_CACHE_TTL,
        |_| Ok(mismatched),
    );

    assert_eq!(client.origin(), ManagedSetupOrigin::FailedClosed);
    assert!(!cache.exists());
}

#[test]
fn a_fetched_document_from_another_workspace_is_rejected_and_not_cached() {
    let home = tempfile::tempdir().expect("tempdir");
    let cache = home.path().join(CACHE_FILE_NAME);
    let mut mismatched = allowlist_setup();
    mismatched.workspace_id = "workspace-b".to_string();

    let client = ManagedSetupClient::resolve_with(
        Some(&session()),
        Some(cache.as_path()),
        1_000,
        DEFAULT_CACHE_TTL,
        |_| Ok(mismatched),
    );

    assert_eq!(client.origin(), ManagedSetupOrigin::FailedClosed);
    assert!(!cache.exists());
}

#[test]
fn an_organization_wide_document_is_valid_for_a_requested_workspace() {
    let mut organization_setup = allowlist_setup();
    organization_setup.workspace_id.clear();
    let client =
        ManagedSetupClient::resolve_with(Some(&session()), None, 1_000, DEFAULT_CACHE_TTL, |_| {
            Ok(organization_setup)
        });
    assert_eq!(client.origin(), ManagedSetupOrigin::Fetched);
}

#[test]
fn a_stale_cache_is_used_when_the_fetch_fails() {
    let home = tempfile::tempdir().expect("tempdir");
    let cache = home.path().join(CACHE_FILE_NAME);
    write_cache(cache.as_path(), &session(), &allowlist_setup(), 1_000).expect("seed cache");

    let client = ManagedSetupClient::resolve_with(
        Some(&session()),
        Some(cache.as_path()),
        // Far past the TTL, so a fetch is attempted and then fails.
        1_000 + 10_000,
        DEFAULT_CACHE_TTL,
        |_| Err(ManagedSetupError::Request("connection refused".to_string())),
    );
    assert_eq!(client.origin(), ManagedSetupOrigin::Cache);
    assert_eq!(client.version(), 7);
    assert_eq!(
        client
            .mcp_policy()
            .decide("approved", Some("https://mcp.example.com/v1"), "http"),
        McpDecision::Allowed,
        "the cached allowlist is still enforced"
    );
    assert_eq!(client.notices().len(), 1);
    assert!(client.notices()[0].contains("cached policy"));
}

#[test]
fn a_fresh_cache_short_circuits_the_fetch() {
    let home = tempfile::tempdir().expect("tempdir");
    let cache = home.path().join(CACHE_FILE_NAME);
    write_cache(cache.as_path(), &session(), &allowlist_setup(), 1_000).expect("seed cache");

    let client = ManagedSetupClient::resolve_with(
        Some(&session()),
        Some(cache.as_path()),
        1_000 + 60,
        DEFAULT_CACHE_TTL,
        |_| panic!("a fresh cache must not trigger a fetch"),
    );
    assert_eq!(client.version(), 7);
}

#[test]
fn a_fresh_cache_from_another_tenant_does_not_short_circuit_the_fetch() {
    let home = tempfile::tempdir().expect("tempdir");
    let cache = home.path().join(CACHE_FILE_NAME);
    write_cache(cache.as_path(), &session(), &allowlist_setup(), 1_000).expect("seed cache");
    let other_session = session_for("org-b", Some("workspace-b"));
    let mut other_setup = allowlist_setup();
    other_setup.organization_id = "org-b".to_string();
    other_setup.workspace_id = "workspace-b".to_string();
    other_setup.version = 11;

    let client = ManagedSetupClient::resolve_with(
        Some(&other_session),
        Some(cache.as_path()),
        1_000 + 60,
        DEFAULT_CACHE_TTL,
        move |_| Ok(other_setup.clone()),
    );

    assert_eq!(client.origin(), ManagedSetupOrigin::Fetched);
    assert_eq!(client.version(), 11);
    assert_eq!(client.setup().organization_id, "org-b");
    assert_eq!(client.setup().workspace_id, "workspace-b");
}

#[test]
fn a_failed_fetch_cannot_apply_a_previous_tenants_cached_policy() {
    let home = tempfile::tempdir().expect("tempdir");
    let cache = home.path().join(CACHE_FILE_NAME);
    write_cache(cache.as_path(), &session(), &allowlist_setup(), 1_000).expect("seed cache");
    let other_session = session_for("org-a", Some("workspace-b"));

    let client = ManagedSetupClient::resolve_with(
        Some(&other_session),
        Some(cache.as_path()),
        1_000 + 60,
        DEFAULT_CACHE_TTL,
        |_| Err(ManagedSetupError::Request("offline".to_string())),
    );

    assert_eq!(client.origin(), ManagedSetupOrigin::FailedClosed);
    assert_eq!(client.setup().organization_id, "org-a");
    assert_eq!(client.setup().workspace_id, "workspace-b");
    assert_eq!(
        client.mcp_policy().decide("approved", None, "stdio"),
        McpDecision::RefusedNotAllowlisted,
        "the previous workspace's allowlist must not become an offline fallback"
    );
    assert!(client.notices()[0].contains("no cached policy"));
}

#[test]
fn no_cache_and_no_network_fails_closed_for_a_platform_bound_session() {
    let home = tempfile::tempdir().expect("tempdir");
    let cache = home.path().join(CACHE_FILE_NAME);
    let client = ManagedSetupClient::resolve_with(
        Some(&session()),
        Some(cache.as_path()),
        1_000,
        DEFAULT_CACHE_TTL,
        |_| Err(ManagedSetupError::Request("connection refused".to_string())),
    );
    assert_eq!(client.origin(), ManagedSetupOrigin::FailedClosed);
    assert_eq!(client.mcp_policy().mode, McpPolicyMode::Allowlist);
    assert!(client.mcp_policy().servers.is_empty());
    assert_eq!(
        client.mcp_policy().decide("anything", None, "stdio"),
        McpDecision::RefusedNotAllowlisted,
        "failing closed must refuse every MCP server, never open the session"
    );
    assert_eq!(client.notices().len(), 1);
    assert!(client.notices()[0].contains("MCP servers are refused"));
}

#[test]
fn shared_platform_base_url_configures_managed_setup() {
    let _lock = crate::config::test_process_env_lock();
    let names = [
        "MAESTRO_MANAGED_SETUP_URL",
        "MAESTRO_PLATFORM_BASE_URL",
        "MAESTRO_EVALOPS_BASE_URL",
    ];
    let previous = names
        .iter()
        .map(|name| (*name, std::env::var_os(name)))
        .collect::<Vec<_>>();
    struct EnvRestore(Vec<(&'static str, Option<std::ffi::OsString>)>);
    impl Drop for EnvRestore {
        fn drop(&mut self) {
            for (name, value) in self.0.drain(..) {
                if let Some(value) = value {
                    std::env::set_var(name, value);
                } else {
                    std::env::remove_var(name);
                }
            }
        }
    }
    let _restore = EnvRestore(previous);
    for name in names {
        std::env::remove_var(name);
    }

    std::env::set_var("MAESTRO_EVALOPS_BASE_URL", "https://gateway.example/v1");
    std::env::set_var("MAESTRO_PLATFORM_BASE_URL", " https://platform.example/ ");
    assert_eq!(
        platform_base_url().as_deref(),
        Some("https://platform.example")
    );

    std::env::set_var("MAESTRO_MANAGED_SETUP_URL", "https://managed.example/");
    assert_eq!(
        platform_base_url().as_deref(),
        Some("https://managed.example")
    );
}

#[test]
fn default_platform_base_url_is_used_without_an_override() {
    let _lock = crate::config::test_process_env_lock();
    let names = [
        "MAESTRO_MANAGED_SETUP_URL",
        "MAESTRO_PLATFORM_BASE_URL",
        "MAESTRO_EVALOPS_BASE_URL",
    ];
    let previous = names
        .iter()
        .map(|name| (*name, std::env::var_os(name)))
        .collect::<Vec<_>>();
    struct EnvRestore(Vec<(&'static str, Option<std::ffi::OsString>)>);
    impl Drop for EnvRestore {
        fn drop(&mut self) {
            for (name, value) in self.0.drain(..) {
                if let Some(value) = value {
                    std::env::set_var(name, value);
                } else {
                    std::env::remove_var(name);
                }
            }
        }
    }
    let _restore = EnvRestore(previous);
    for name in names {
        std::env::remove_var(name);
    }

    assert_eq!(
        platform_base_url().as_deref(),
        Some("https://app.deixic.com")
    );
}

#[test]
fn a_session_with_no_platform_binding_is_unmanaged() {
    let client = ManagedSetupClient::resolve_with(None, None, 1_000, DEFAULT_CACHE_TTL, |_| {
        panic!("BYOK sessions must not fetch a managed setup")
    });
    assert_eq!(client.origin(), ManagedSetupOrigin::Unmanaged);
    assert!(!client.is_managed());
    assert_eq!(
        client.mcp_policy().decide("anything", None, "stdio"),
        McpDecision::Allowed
    );
    assert!(client.notices().is_empty());
}

#[test]
fn offline_local_startup_is_quiet_but_keeps_stored_tenant_mcp_fail_closed() {
    let client = ManagedSetupClient::offline_local(Some(&session()));
    assert_eq!(client.origin(), ManagedSetupOrigin::FailedClosed);
    assert!(client.is_managed());
    assert_eq!(
        client.mcp_policy().decide("anything", None, "stdio"),
        McpDecision::RefusedNotAllowlisted
    );
    assert!(client.notices().is_empty());

    let standalone = ManagedSetupClient::offline_local(None);
    assert_eq!(standalone.origin(), ManagedSetupOrigin::Unmanaged);
    assert!(!standalone.is_managed());
}

#[test]
fn a_cache_written_by_a_future_schema_is_ignored() {
    let home = tempfile::tempdir().expect("tempdir");
    let cache = home.path().join(CACHE_FILE_NAME);
    std::fs::write(
        &cache,
        serde_json::json!({
            "schemaVersion": CACHE_SCHEMA_VERSION + 1,
            "fetchedAt": 1_000,
            "version": 7,
            "setup": {},
        })
        .to_string(),
    )
    .expect("write cache");
    assert!(read_cache(cache.as_path()).is_none());

    let client = ManagedSetupClient::resolve_with(
        Some(&session()),
        Some(cache.as_path()),
        1_000,
        DEFAULT_CACHE_TTL,
        |_| Err(ManagedSetupError::Request("offline".to_string())),
    );
    assert_eq!(client.origin(), ManagedSetupOrigin::FailedClosed);
}

// Wire decoding

#[test]
fn a_connect_json_response_decodes_including_a_string_encoded_version() {
    let body = serde_json::json!({
        "version": "12",
        "organizationId": "org-a",
        "workspaceId": "workspace-a",
        "rules": [{
            "id": "runbook",
            "title": "Runbook",
            "bodyMarkdown": "Do the thing.",
            "scope": "RULE_SCOPE_WORKSPACE",
        }],
        "skills": [{
            "id": "incident-triage",
            "source": "deixic-marketplace",
            "version": "1.2.0",
            "required": true,
        }],
        "mcp": {
            "mode": "MCP_POLICY_MODE_DENYLIST",
            "servers": [{ "name": "banned", "urlPattern": "", "transport": "stdio" }],
        },
        "sandboxPolicyToml": "[network]\ndefault = \"deny\"\n",
    })
    .to_string();
    let setup = serde_json::from_str::<ManagedSetup>(&body).expect("decode");
    assert_eq!(setup.version, 12);
    assert_eq!(setup.rules[0].scope, RuleScope::Workspace);
    assert_eq!(setup.mcp.mode, McpPolicyMode::Denylist);
    assert!(setup.skills[0].required);
    assert!(setup.sandbox_policy_toml.contains("[network]"));
}

#[test]
fn an_absent_version_field_decodes_as_zero() {
    let setup = serde_json::from_str::<ManagedSetup>("{}").expect("decode");
    assert_eq!(setup.version, 0);
    assert_eq!(setup.mcp.mode, McpPolicyMode::Unspecified);
}

// Rules and skills

#[test]
fn the_rule_block_names_the_policy_version_and_every_rule() {
    let section = rules_prompt_section(&allowlist_setup()).expect("section");
    assert!(section.contains("Deixic managed setup version 7"));
    assert!(section.contains("Follow the runbook"));
    assert!(section.contains("Always link the incident ticket."));
    assert!(section.contains("(organization)"));
}

#[test]
fn a_document_with_no_rules_contributes_no_prompt_block() {
    assert!(rules_prompt_section(&ManagedSetup::default()).is_none());
}

#[test]
fn only_required_skills_that_are_absent_are_reported() {
    let client =
        ManagedSetupClient::resolve_with(Some(&session()), None, 1_000, DEFAULT_CACHE_TTL, |_| {
            Ok(allowlist_setup())
        });
    assert!(
        client
            .missing_required_skills_notice(["incident-triage"])
            .is_none(),
        "an installed required skill is not reported"
    );
    let notice = client
        .missing_required_skills_notice(["optional-helper"])
        .expect("an absent required skill is reported");
    assert!(notice.contains("incident-triage"));
    assert!(!notice.contains("optional-helper"));
    assert!(notice.contains("does not"));
}

// Sandbox policy provider

#[test]
fn the_provider_returns_the_parsed_administrator_document() {
    let client =
        ManagedSetupClient::resolve_with(Some(&session()), None, 1_000, DEFAULT_CACHE_TTL, |_| {
            Ok(allowlist_setup())
        });
    let document = client.team_policy().expect("team policy");
    let network = document.network.expect("network section");
    assert_eq!(network.default, NetworkAction::Deny);
}

#[test]
fn an_empty_sandbox_policy_is_no_administrator_opinion() {
    let mut setup = allowlist_setup();
    setup.sandbox_policy_toml = String::new();
    let client = ManagedSetupClient::resolve_with(
        Some(&session()),
        None,
        1_000,
        DEFAULT_CACHE_TTL,
        move |_| Ok(setup.clone()),
    );
    assert!(client.team_policy().is_none());
}

#[test]
fn an_unparseable_sandbox_policy_denies_rather_than_disappearing() {
    let mut setup = allowlist_setup();
    setup.sandbox_policy_toml = "this is not = [ toml".to_string();
    let client = ManagedSetupClient::resolve_with(
        Some(&session()),
        None,
        1_000,
        DEFAULT_CACHE_TTL,
        move |_| Ok(setup.clone()),
    );
    let document = client
        .team_policy()
        .expect("an unparseable policy still constrains");
    let network = document.network.expect("network section");
    assert_eq!(network.default, NetworkAction::Deny);
    assert_eq!(document.additional_read_paths, Some(Vec::new()));
}

// URL glob

#[test]
fn the_url_glob_matches_prefix_suffix_and_exact_patterns() {
    assert!(url_pattern_matches(
        "https://a.test/*",
        "https://a.test/x/y"
    ));
    assert!(url_pattern_matches("*.a.test", "https://mcp.a.test"));
    assert!(url_pattern_matches(
        "https://a.test/v1",
        "https://a.test/v1"
    ));
    assert!(!url_pattern_matches(
        "https://a.test/v1",
        "https://a.test/v2"
    ));
    assert!(!url_pattern_matches("https://a.test/*", "https://b.test/x"));
    assert!(url_pattern_matches("*", "https://anything.test/path"));
}

#[test]
fn a_zero_ttl_always_refetches() {
    let home = tempfile::tempdir().expect("tempdir");
    let cache = home.path().join(CACHE_FILE_NAME);
    write_cache(cache.as_path(), &session(), &allowlist_setup(), 1_000).expect("seed cache");
    let client = ManagedSetupClient::resolve_with(
        Some(&session()),
        Some(cache.as_path()),
        1_000,
        Duration::from_secs(0),
        |_| Ok(allowlist_setup()),
    );
    assert_eq!(client.origin(), ManagedSetupOrigin::Fetched);
}

#[test]
fn the_client_is_usable_as_the_team_source_of_the_effective_policy() {
    let workspace = tempfile::tempdir().expect("workspace");
    let client =
        ManagedSetupClient::resolve_with(Some(&session()), None, 1_000, DEFAULT_CACHE_TTL, |_| {
            Ok(allowlist_setup())
        });
    let provider: &dyn TeamPolicyProvider = &client;
    let sources =
        crate::sandbox_policy::load_policy_sources_at(None, None, provider).expect("load sources");
    assert!(
        sources
            .iter()
            .any(|(source, _)| *source == crate::sandbox_policy::PolicySource::TeamAdmin),
        "the managed setup client supplies the team-admin source"
    );
    let effective = crate::sandbox_policy::resolve_effective_policy(workspace.path(), provider)
        .expect("resolve effective policy");
    let network = effective.network.expect("network section");
    assert_eq!(
        network.default,
        NetworkAction::Deny,
        "the administrator's deny survives the merge"
    );
    assert!(matches!(
        client
            .native_sandbox_policy(workspace.path(), None)
            .expect("native policy"),
        Some(crate::sandbox::SandboxPolicy::WorkspaceWrite {
            network_access: false,
            ..
        })
    ));
}

#[test]
fn managed_sandbox_policy_only_restricts_the_native_baseline() {
    use std::path::PathBuf;

    use crate::sandbox::SandboxPolicy;

    let baseline = SandboxPolicy::WorkspaceWrite {
        writable_roots: vec![PathBuf::from("/shared"), PathBuf::from("/user-only")],
        network_access: true,
        exclude_tmpdir_env_var: true,
        exclude_slash_tmp: false,
    };
    let network_only_restriction = SandboxPolicy::WorkspaceWrite {
        writable_roots: vec![PathBuf::from("/shared"), PathBuf::from("/user-only")],
        network_access: false,
        exclude_tmpdir_env_var: false,
        exclude_slash_tmp: false,
    };
    let managed = SandboxPolicy::WorkspaceWrite {
        writable_roots: vec![PathBuf::from("/shared"), PathBuf::from("/managed-only")],
        network_access: false,
        exclude_tmpdir_env_var: false,
        exclude_slash_tmp: true,
    };

    assert_eq!(
        restrict_native_sandbox_policy(Some(baseline.clone()), network_only_restriction),
        Some(SandboxPolicy::WorkspaceWrite {
            writable_roots: vec![PathBuf::from("/shared"), PathBuf::from("/user-only")],
            network_access: false,
            exclude_tmpdir_env_var: true,
            exclude_slash_tmp: false,
        })
    );
    assert_eq!(
        restrict_native_sandbox_policy(Some(baseline), managed.clone()),
        Some(SandboxPolicy::WorkspaceWrite {
            writable_roots: vec![PathBuf::from("/shared")],
            network_access: false,
            exclude_tmpdir_env_var: true,
            exclude_slash_tmp: true,
        })
    );
    assert_eq!(
        restrict_native_sandbox_policy(Some(SandboxPolicy::DangerFullAccess), managed.clone()),
        Some(managed)
    );
    assert_eq!(
        restrict_native_sandbox_policy(
            Some(SandboxPolicy::ReadOnly),
            SandboxPolicy::DangerFullAccess
        ),
        Some(SandboxPolicy::ReadOnly)
    );
}

fn protobuf_fixture() -> wire::ManagedSetup {
    wire::ManagedSetup {
        version: u64::MAX,
        issued_at: Some(prost_types::Timestamp {
            seconds: 1_800_000_000,
            nanos: 123,
        }),
        scope: Some(crate::public_protocol::Scope {
            organization_id: "org-a".into(),
            workspace_id: "workspace-a".into(),
        }),
        rules: vec![wire::ManagedRule {
            id: "rule".into(),
            title: "Review".into(),
            body_markdown: "Read the runbook.".into(),
            scope: 2,
        }],
        skills: vec![wire::ManagedSkillRef {
            id: "triage".into(),
            source: "catalog".into(),
            version: "3".into(),
            required: true,
        }],
        mcp: Some(wire::McpPolicy {
            mode: 2,
            servers: vec![wire::McpServerRef {
                name: "docs".into(),
                url_pattern: "https://docs.example.com/*".into(),
                transport: "http".into(),
            }],
        }),
        sandbox_policy_toml: "[network]\ndefault = \"deny\"\n".into(),
    }
}

fn protobuf_server(body: Vec<u8>) -> (String, std::thread::JoinHandle<()>) {
    protobuf_server_for(body, true)
}

fn protobuf_server_for(
    body: Vec<u8>,
    workspace_bound: bool,
) -> (String, std::thread::JoinHandle<()>) {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("listener");
    let address = listener.local_addr().expect("address");
    let task = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("request");
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("timeout");
        let mut request = Vec::new();
        let header_end = loop {
            let mut buffer = [0; 1024];
            let count = stream.read(&mut buffer).expect("read");
            assert!(count > 0);
            request.extend_from_slice(&buffer[..count]);
            if let Some(end) = request.windows(4).position(|part| part == b"\r\n\r\n") {
                break end + 4;
            }
        };
        let headers = String::from_utf8_lossy(&request[..header_end]).to_ascii_lowercase();
        assert!(headers.starts_with("post /deixicpublic.v1.deixicpublicservice/getclientsetup "));
        assert!(headers.contains("content-type: application/proto\r\n"));
        assert!(headers.contains("accept: application/proto\r\n"));
        assert!(headers.contains("authorization: bearer access-token\r\n"));
        assert!(headers.contains("x-organization-id: org-a\r\n"));
        if workspace_bound {
            assert!(headers.contains("x-workspace-id: workspace-a\r\n"));
        } else {
            assert!(!headers.contains("x-workspace-id:"));
        }
        let length: usize = headers
            .lines()
            .find_map(|line| line.strip_prefix("content-length: "))
            .expect("length")
            .parse()
            .expect("number");
        while request.len() < header_end + length {
            let mut buffer = [0; 1024];
            let count = stream.read(&mut buffer).expect("body");
            assert!(count > 0);
            request.extend_from_slice(&buffer[..count]);
        }
        let expected: &[u8] = if workspace_bound {
            b"\x0a\x14\x0a\x05org-a\x12\x0bworkspace-a"
        } else {
            b"\x0a\x07\x0a\x05org-a"
        };
        assert_eq!(&request[header_end..], expected);
        // Decode the actual bytes crossing the HTTP boundary, not the request builder.
        let decoded =
            wire::GetManagedSetupRequest::decode(&request[header_end..]).expect("protobuf request");
        assert_eq!(decoded.scope.as_ref().unwrap().organization_id, "org-a");
        assert_eq!(
            decoded.scope.as_ref().unwrap().workspace_id,
            if workspace_bound { "workspace-a" } else { "" }
        );
        write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/proto\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).expect("headers");
        stream.write_all(&body).expect("response");
    });
    (format!("http://{address}"), task)
}

#[test]
fn protobuf_transport_preserves_every_policy_field_and_cache_timestamp() {
    let (base, server) = protobuf_server(protobuf_fixture().encode_to_vec());
    let setup = fetch_managed_setup_from(&session(), &base).expect("policy");
    server.join().expect("server");
    assert_eq!(setup.version, u64::MAX);
    assert_eq!(
        setup.issued_at,
        Some(ManagedSetupTimestamp {
            seconds: 1_800_000_000,
            nanos: 123
        })
    );
    assert_eq!(setup.organization_id, "org-a");
    assert_eq!(setup.workspace_id, "workspace-a");
    assert_eq!(
        setup.rules,
        vec![ManagedRule {
            id: "rule".into(),
            title: "Review".into(),
            body_markdown: "Read the runbook.".into(),
            scope: RuleScope::Workspace
        }]
    );
    assert_eq!(
        setup.skills,
        vec![ManagedSkillRef {
            id: "triage".into(),
            source: "catalog".into(),
            version: "3".into(),
            required: true
        }]
    );
    assert_eq!(
        setup.mcp,
        McpPolicy {
            mode: McpPolicyMode::Allowlist,
            servers: vec![McpServerRef {
                name: "docs".into(),
                url_pattern: "https://docs.example.com/*".into(),
                transport: "http".into()
            }]
        }
    );
    assert_eq!(setup.sandbox_policy_toml, "[network]\ndefault = \"deny\"\n");
    let temp = tempfile::tempdir().expect("directory");
    let path = temp.path().join("cache.json");
    write_cache(&path, &session(), &setup, 42).expect("cache");
    assert_eq!(read_cache(&path).expect("cached").setup, setup);
}

#[test]
fn protobuf_transport_rejects_malformed_foreign_and_unknown_policy() {
    let mut foreign_org = protobuf_fixture();
    foreign_org.scope.as_mut().unwrap().organization_id = "other".into();
    let mut foreign_workspace = protobuf_fixture();
    foreign_workspace.scope.as_mut().unwrap().workspace_id = "other".into();
    let mut unknown_mode = protobuf_fixture();
    unknown_mode.mcp.as_mut().unwrap().mode = 99;
    let mut unknown_scope = protobuf_fixture();
    unknown_scope.rules[0].scope = 99;
    let mut missing_scope = protobuf_fixture();
    missing_scope.scope = None;
    let mut invalid_timestamp = protobuf_fixture();
    invalid_timestamp.issued_at.as_mut().unwrap().nanos = -1;
    for body in [
        vec![0x80],
        missing_scope.encode_to_vec(),
        foreign_org.encode_to_vec(),
        foreign_workspace.encode_to_vec(),
        unknown_mode.encode_to_vec(),
        unknown_scope.encode_to_vec(),
        invalid_timestamp.encode_to_vec(),
    ] {
        let (base, server) = protobuf_server(body);
        let client = ManagedSetupClient::resolve_with(
            Some(&session()),
            None,
            42,
            Duration::ZERO,
            |session| fetch_managed_setup_from(session, &base),
        );
        server.join().expect("server");
        assert_eq!(client.origin(), ManagedSetupOrigin::FailedClosed);
        assert_eq!(client.mcp_policy(), &McpPolicy::deny_all());
    }
}

#[test]
fn protobuf_transport_accepts_organization_policy_for_both_selectors() {
    for workspace_bound in [false, true] {
        let mut policy = protobuf_fixture();
        policy.scope.as_mut().unwrap().workspace_id.clear();
        let (base, server) = protobuf_server_for(policy.encode_to_vec(), workspace_bound);
        let session = session_for("org-a", workspace_bound.then_some("workspace-a"));
        let setup = fetch_managed_setup_from(&session, &base).expect("organization policy");
        server.join().expect("server");
        assert_eq!(setup.organization_id, "org-a");
        assert!(setup.workspace_id.is_empty());
    }
}

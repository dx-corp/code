//! Explicit, bounded onboarding checks. Reports contain fixed summaries, never
//! provider responses, credentials, workspace contents, or configuration values.

use std::collections::HashMap;
use std::path::Path;
use std::time::{Duration, Instant};

use crate::ai::{Message, MessageContent, RequestConfig, Role, StreamEvent, UnifiedClient};
use crate::doctor::CheckStatus;
use crate::telemetry::OnboardingCheckId;

const CHECK_TIMEOUT: Duration = Duration::from_secs(30);
const PROBE_TIMEOUT: Duration = Duration::from_secs(30);
const FIXTURE: &str = "maestro-onboarding-read-probe";

/// One local readiness result. The ID is from the onboarding check vocabulary.
#[derive(Debug, Clone)]
pub struct OnboardingCheck {
    pub id: OnboardingCheckId,
    pub status: CheckStatus,
    pub summary: String,
    pub repair: Option<String>,
}

/// Readiness requires every necessary check to pass, including live probes.
#[derive(Debug, Clone)]
pub struct OnboardingReadiness {
    pub checks: Vec<OnboardingCheck>,
    pub ready: bool,
    pub elapsed_ms: u64,
}

fn check(
    id: OnboardingCheckId,
    status: CheckStatus,
    summary: &str,
    repair: &str,
) -> OnboardingCheck {
    OnboardingCheck {
        id,
        status,
        summary: summary.to_owned(),
        repair: (status != CheckStatus::Pass).then(|| repair.to_owned()),
    }
}

fn finish(checks: Vec<OnboardingCheck>, start: Instant) -> OnboardingReadiness {
    let required = [
        OnboardingCheckId::Config,
        OnboardingCheckId::Identity,
        OnboardingCheckId::Provider,
        OnboardingCheckId::Model,
        OnboardingCheckId::ManagedSetup,
        OnboardingCheckId::Workspace,
        OnboardingCheckId::ModelProbe,
        OnboardingCheckId::ToolProbe,
    ];
    let ready = checks.len() == required.len()
        && required
            .iter()
            .all(|id| checks.iter().filter(|c| c.id == *id).count() == 1)
        && checks.iter().all(|c| c.status == CheckStatus::Pass);
    OnboardingReadiness {
        checks,
        ready,
        elapsed_ms: u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX),
    }
}

/// Check the selected route and read a disposable fixture using the native tool
/// executor. The caller must explain that the model probe can incur usage before
/// starting this function. No login flow is started and no user files are read.
pub async fn run_checks(model: Option<&str>, cwd: &Path) -> OnboardingReadiness {
    let start = Instant::now();
    let doctor =
        tokio::time::timeout(CHECK_TIMEOUT, crate::doctor::build_report(model, true, cwd)).await;
    let mut checks = Vec::new();
    let Ok(doctor) = doctor else {
        checks.push(check(
            OnboardingCheckId::Identity,
            CheckStatus::Fail,
            "Setup verification timed out",
            "Check your connection and retry the readiness check.",
        ));
        return finish(checks, start);
    };
    checks.extend(project_doctor_checks(&doctor.checks));
    checks.push(managed_policy_check(&doctor.selected_model.requested, cwd).await);
    let workspace_ok = cwd.is_dir() && std::fs::read_dir(cwd).is_ok();
    checks.push(check(
        OnboardingCheckId::Workspace,
        if workspace_ok {
            CheckStatus::Pass
        } else {
            CheckStatus::Fail
        },
        if workspace_ok {
            "Workspace directory is accessible"
        } else {
            "Workspace directory is inaccessible"
        },
        "Open an accessible workspace directory and retry.",
    ));
    let delegated_codex = crate::ai::ProviderRegistry::descriptor(&doctor.selected_model.provider)
        .is_some_and(|provider| provider.protocol == crate::ai::ProviderProtocol::Codex);
    complete_probe_checks(
        checks,
        delegated_codex,
        model_probe(&doctor.selected_model.requested),
        tool_probe(),
        PROBE_TIMEOUT,
        start,
    )
    .await
}

async fn complete_probe_checks(
    mut checks: Vec<OnboardingCheck>,
    delegated_codex: bool,
    model: impl std::future::Future<Output = anyhow::Result<()>>,
    tool: impl std::future::Future<Output = anyhow::Result<()>>,
    timeout: Duration,
    start: Instant,
) -> OnboardingReadiness {
    let prerequisites = [
        OnboardingCheckId::Config,
        OnboardingCheckId::Identity,
        OnboardingCheckId::Provider,
        OnboardingCheckId::Model,
        OnboardingCheckId::ManagedSetup,
        OnboardingCheckId::Workspace,
    ];
    let prerequisites_ok = checks.len() == prerequisites.len()
        && prerequisites.iter().all(|id| {
            checks
                .iter()
                .filter(|c| c.id == *id && c.status == CheckStatus::Pass)
                .count()
                == 1
        });
    let model_status = if delegated_codex {
        CheckStatus::Skipped
    } else if prerequisites_ok {
        match tokio::time::timeout(timeout, model).await {
            Ok(Ok(())) => CheckStatus::Pass,
            _ => CheckStatus::Fail,
        }
    } else {
        CheckStatus::Skipped
    };
    checks.push(check(
        OnboardingCheckId::ModelProbe,
        model_status,
        if delegated_codex {
            "Codex app-server live model verification is not available in this onboarding check"
        } else if model_status == CheckStatus::Pass {
            "Selected model completed a small test request"
        } else if model_status == CheckStatus::Skipped {
            "Model test waits for setup repairs"
        } else {
            "Model test failed or timed out"
        },
        if delegated_codex { "Run a small prompt in your Codex session to verify the app-server route. Onboarding cannot yet mark that route verified." } else { "Check model access and provider connectivity with deixic-code doctor --live, then retry." },
    ));
    let tool_status = match tokio::time::timeout(timeout.min(Duration::from_secs(10)), tool).await {
        Ok(Ok(())) => CheckStatus::Pass,
        _ => CheckStatus::Fail,
    };
    checks.push(check(OnboardingCheckId::ToolProbe, tool_status, if tool_status == CheckStatus::Pass { "Native read tool passed an isolated read-only fixture test; session tools are not verified" } else { "Native read tool test failed or was denied" }, "Check local file permissions and tool policy, then retry. Other tools and integrations require separate verification."));
    finish(checks, start)
}

fn project_doctor_checks(doctor_checks: &[crate::doctor::DoctorCheck]) -> Vec<OnboardingCheck> {
    let mut checks = Vec::new();
    for (id, source, good, bad, repair) in [
        (
            OnboardingCheckId::Config,
            "config",
            "Configuration is valid",
            "Configuration needs attention",
            "Run deixic-code config validate.",
        ),
        (
            OnboardingCheckId::Identity,
            "credential_mode",
            "Identity and credential route verified",
            "Identity or credential route could not be verified",
            "Run deixic-code evalops login, then retry.",
        ),
        (
            OnboardingCheckId::Provider,
            "provider",
            "Provider route resolved",
            "Provider route could not be resolved",
            "Run deixic-code setup --byok or sign in for managed inference.",
        ),
        (
            OnboardingCheckId::Model,
            "model_catalog",
            "Model route selected",
            "Model route needs attention",
            "Run deixic-code models and select a supported route.",
        ),
    ] {
        let matching: Vec<_> = doctor_checks.iter().filter(|c| c.id == source).collect();
        let mut status = if matching.is_empty() {
            CheckStatus::Skipped
        } else if matching.iter().any(|c| c.status == CheckStatus::Fail) {
            CheckStatus::Fail
        } else if matching.iter().any(|c| c.status == CheckStatus::Warning) {
            CheckStatus::Warning
        } else if matching.iter().any(|c| c.status == CheckStatus::Skipped) {
            CheckStatus::Skipped
        } else {
            CheckStatus::Pass
        };
        // A custom model need not be in the catalog; the live model probe below
        // is the authority for whether that route can actually answer.
        if id == OnboardingCheckId::Model && status == CheckStatus::Warning {
            status = CheckStatus::Pass;
        }
        // Missing optional config files use the runtime defaults.
        if id == OnboardingCheckId::Config
            && !matching.is_empty()
            && matching
                .iter()
                .all(|c| matches!(c.status, CheckStatus::Pass | CheckStatus::Skipped))
        {
            status = CheckStatus::Pass;
        }
        checks.push(check(
            id,
            status,
            if status == CheckStatus::Pass {
                good
            } else {
                bad
            },
            repair,
        ));
    }
    checks
}

async fn managed_policy_check(model: &str, cwd: &Path) -> OnboardingCheck {
    let model = model.to_owned();
    let cwd = cwd.to_owned();
    let task = tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        // Preserve the session which authorized either typed route. Inference
        // BYOK is not an exemption from the workspace's managed setup policy.
        let (_mode, identity) = crate::credential_mode::require_ready_with_identity(&model)?;
        let policy = crate::managed_setup::ManagedSetupClient::resolve_with(
            identity.as_ref(),
            None,
            0,
            Duration::ZERO,
            crate::managed_setup::fetch_managed_setup,
        );
        anyhow::ensure!(
            policy.origin() == crate::managed_setup::ManagedSetupOrigin::Fetched,
            "managed policy unavailable"
        );
        policy.native_sandbox_policy(&cwd, Some(crate::sandbox::SandboxPolicy::ReadOnly))?;
        Ok(())
    });
    let passed = matches!(
        tokio::time::timeout(CHECK_TIMEOUT, task).await,
        Ok(Ok(Ok(())))
    );
    check(
        OnboardingCheckId::ManagedSetup,
        if passed {
            CheckStatus::Pass
        } else {
            CheckStatus::Fail
        },
        if passed {
            "Workspace managed setup and sandbox policy verified"
        } else {
            "Workspace managed setup or sandbox policy could not be verified"
        },
        "Check your Deixic address, workspace access, and managed policy with deixic-code doctor --live, then retry.",
    )
}

async fn model_probe(model: &str) -> anyhow::Result<()> {
    if let Some(client) = crate::credential_mode::disconnected_client(model)? {
        return probe_client(&client, model, PROBE_TIMEOUT).await;
    }
    let source_env: HashMap<String, String> = std::env::vars().collect();
    let mode = crate::credential_mode::require_ready(model)?;
    let (route, mut env) = match mode {
        crate::credential_mode::DetectedMode::Platform(session) => (
            session.managed_model_route(model),
            session.managed_env(model, &source_env)?,
        ),
        crate::credential_mode::DetectedMode::Byok => (model.to_owned(), source_env),
    };
    crate::service_connections::ConnectionBroker::merge_default_for_model(&route, &mut env)?;
    let _ = crate::codex_auth::merge_codex_auth_snapshot_into_env(
        &mut env,
        crate::codex_auth::read_codex_auth(),
        false,
    );
    let client = UnifiedClient::from_model_with_env(&route, &env)?;
    probe_client(&client, &route, PROBE_TIMEOUT).await
}

async fn probe_client(
    client: &UnifiedClient,
    route: &str,
    timeout: Duration,
) -> anyhow::Result<()> {
    let messages = [Message {
        role: Role::User,
        content: MessageContent::text("Reply with the single word ready."),
    }];
    let config = RequestConfig {
        model: crate::ai::provider_model_name(route),
        max_tokens: 32,
        ..RequestConfig::default()
    };
    let mut stream = client.stream_owned_config(&messages, config).await?;
    let result = tokio::time::timeout(timeout, async {
        let mut text_received = false;
        while let Some(event) = stream.recv().await {
            match event {
                StreamEvent::TextDelta { text, .. } => text_received |= !text.trim().is_empty(),
                StreamEvent::MessageStop { stop_reason } => {
                    anyhow::ensure!(
                        text_received
                            && matches!(
                                stop_reason,
                                None | Some(
                                    crate::ai::StopReason::EndTurn
                                        | crate::ai::StopReason::StopSequence
                                )
                            ),
                        "incomplete model probe"
                    );
                    return Ok(());
                }
                StreamEvent::Error { .. } | StreamEvent::ProviderError { .. } => {
                    anyhow::bail!("provider probe failed")
                }
                _ => {}
            }
        }
        anyhow::bail!("provider stream ended without completion")
    })
    .await;
    // Close the receiver even on timeout to cancel the provider producer.
    let _ = tokio::time::timeout(Duration::from_secs(2), stream.cancel_and_wait()).await;
    result.map_err(|_| anyhow::anyhow!("model probe timed out"))?
}

async fn read_fixture(root: &Path) -> anyhow::Result<()> {
    let executor = crate::tools::ToolExecutor::new(root.display().to_string())
        .with_sandbox_policy(crate::sandbox::SandboxPolicy::ReadOnly)
        .unattended();
    let args = serde_json::json!({"file_path": root.join("probe.txt")});
    anyhow::ensure!(
        !executor.requires_approval("read", &args),
        "read requires approval"
    );
    anyhow::ensure!(
        matches!(
            executor.firewall_verdict("read", &args),
            crate::safety::FirewallVerdict::Allow
        ),
        "read denied by policy"
    );
    let result = executor
        .execute("read", &args, None, "onboarding-read-probe")
        .await;
    anyhow::ensure!(
        result.success && result.output.contains(FIXTURE),
        "read probe failed"
    );
    Ok(())
}

async fn tool_probe() -> anyhow::Result<()> {
    let fixture = tempfile::Builder::new()
        .prefix("maestro-onboarding-")
        .tempdir()?;
    std::fs::write(fixture.path().join("probe.txt"), FIXTURE)?;
    read_fixture(fixture.path()).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn required_skipped_warning_and_failure_never_claim_ready() {
        for status in [
            CheckStatus::Skipped,
            CheckStatus::Warning,
            CheckStatus::Fail,
        ] {
            assert!(
                !finish(
                    vec![check(
                        OnboardingCheckId::ModelProbe,
                        status,
                        "test",
                        "repair"
                    )],
                    Instant::now()
                )
                .ready
            );
        }
        assert!(!finish(vec![], Instant::now()).ready);
    }

    fn doctor_check(id: &str, status: CheckStatus) -> crate::doctor::DoctorCheck {
        crate::doctor::DoctorCheck {
            id: id.to_owned(),
            status,
            summary: "untrusted detail must not reach onboarding".to_owned(),
            detail: Some("sensitive detail".to_owned()),
            live: true,
            duration_ms: None,
            remediation: None,
        }
    }

    fn prerequisites(catalog: CheckStatus) -> Vec<OnboardingCheck> {
        let mut checks = project_doctor_checks(&[
            doctor_check("config", CheckStatus::Skipped),
            doctor_check("credential_mode", CheckStatus::Pass),
            doctor_check("provider", CheckStatus::Pass),
            doctor_check("model_catalog", catalog),
        ]);
        checks.extend([
            check(
                OnboardingCheckId::ManagedSetup,
                CheckStatus::Pass,
                "verified",
                "repair",
            ),
            check(
                OnboardingCheckId::Workspace,
                CheckStatus::Pass,
                "accessible",
                "repair",
            ),
        ]);
        checks
    }

    #[test]
    fn projection_distinguishes_absent_checks_from_missing_optional_files() {
        let missing = project_doctor_checks(&[]);
        assert!(missing.iter().all(|c| c.status == CheckStatus::Skipped));
        let valid = prerequisites(CheckStatus::Pass);
        assert_eq!(
            valid
                .iter()
                .find(|c| c.id == OnboardingCheckId::Config)
                .unwrap()
                .status,
            CheckStatus::Pass
        );
        assert!(
            valid
                .iter()
                .all(|c| !c.summary.contains("untrusted") && !c.summary.contains("sensitive"))
        );
    }

    #[test]
    fn projection_preserves_failures_across_duplicate_config_or_identity_results() {
        for source in ["config", "credential_mode", "provider", "model_catalog"] {
            let result = project_doctor_checks(&[
                doctor_check(source, CheckStatus::Pass),
                doctor_check(source, CheckStatus::Fail),
                doctor_check(source, CheckStatus::Skipped),
            ]);
            assert!(result.iter().any(|c| c.status == CheckStatus::Fail));
        }
        let result = project_doctor_checks(&[doctor_check("config", CheckStatus::Warning)]);
        assert_eq!(result[0].status, CheckStatus::Warning);
    }

    #[tokio::test]
    async fn catalog_warning_requires_successful_live_probe_and_complete_unique_results() {
        let successful = complete_probe_checks(
            prerequisites(CheckStatus::Warning),
            false,
            async { Ok(()) },
            async { Ok(()) },
            Duration::from_secs(1),
            Instant::now(),
        )
        .await;
        assert!(successful.ready);
        let mut missing = successful.checks.clone();
        missing.pop();
        assert!(!finish(missing, Instant::now()).ready);
        let mut duplicate = successful.checks;
        let last = duplicate.len() - 1;
        duplicate[last] = duplicate[0].clone();
        assert!(!finish(duplicate, Instant::now()).ready);
        let failed = complete_probe_checks(
            prerequisites(CheckStatus::Warning),
            false,
            async { anyhow::bail!("failed") },
            async { Ok(()) },
            Duration::from_secs(1),
            Instant::now(),
        )
        .await;
        assert!(!failed.ready);
        assert_eq!(
            failed
                .checks
                .iter()
                .find(|c| c.id == OnboardingCheckId::ModelProbe)
                .unwrap()
                .status,
            CheckStatus::Fail
        );
    }

    #[tokio::test]
    async fn failed_missing_and_duplicate_prerequisites_do_not_poll_model_probe() {
        for scenario in 0..3 {
            let mut checks = prerequisites(CheckStatus::Pass);
            match scenario {
                0 => checks[0].status = CheckStatus::Fail,
                1 => {
                    checks.pop();
                }
                _ => checks.push(checks[0].clone()),
            }
            let result = complete_probe_checks(
                checks,
                false,
                async { panic!("model must not be polled before prerequisites pass") },
                async { Ok(()) },
                Duration::from_secs(1),
                Instant::now(),
            )
            .await;
            assert!(!result.ready);
            assert_eq!(
                result
                    .checks
                    .iter()
                    .find(|c| c.id == OnboardingCheckId::ModelProbe)
                    .unwrap()
                    .status,
                CheckStatus::Skipped
            );
        }
    }

    #[tokio::test]
    async fn coordinator_times_out_model_and_tool_operations_without_environment_access() {
        let result = complete_probe_checks(
            prerequisites(CheckStatus::Pass),
            false,
            std::future::pending(),
            std::future::pending(),
            Duration::from_millis(1),
            Instant::now(),
        )
        .await;
        assert!(!result.ready);
        for id in [OnboardingCheckId::ModelProbe, OnboardingCheckId::ToolProbe] {
            assert_eq!(
                result.checks.iter().find(|c| c.id == id).unwrap().status,
                CheckStatus::Fail
            );
        }
    }

    #[tokio::test]
    async fn delegated_codex_remains_unverified_without_polling_direct_provider() {
        let result = complete_probe_checks(
            prerequisites(CheckStatus::Pass),
            true,
            async { panic!("delegated Codex must not use direct provider probe") },
            async { Ok(()) },
            Duration::from_secs(1),
            Instant::now(),
        )
        .await;
        assert!(!result.ready);
        let model = result
            .checks
            .iter()
            .find(|c| c.id == OnboardingCheckId::ModelProbe)
            .unwrap();
        assert_eq!(model.status, CheckStatus::Skipped);
        assert!(model.summary.contains("Codex app-server"));
    }

    async fn mock_model(
        body: &'static str,
        delay: Duration,
    ) -> (UnifiedClient, tokio::task::JoinHandle<String>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buffer = [0_u8; 4096];
            loop {
                let count = socket.read(&mut buffer).await.unwrap();
                if count == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..count]);
                if let Some(index) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&request[..index]).to_lowercase();
                    let length = headers
                        .lines()
                        .find_map(|line| {
                            line.strip_prefix("content-length:")
                                .and_then(|v| v.trim().parse::<usize>().ok())
                        })
                        .unwrap_or(0);
                    if request.len() >= index + 4 + length {
                        break;
                    }
                }
            }
            tokio::time::sleep(delay).await;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = socket.write_all(response.as_bytes()).await;
            String::from_utf8(request).unwrap()
        });
        (
            UnifiedClient::OpenAI(
                crate::ai::OpenAiClient::with_base_url("test-key", format!("http://{address}/v1"))
                    .unwrap(),
            ),
            server,
        )
    }

    #[tokio::test]
    async fn model_probe_requires_completed_transport_response_and_sends_only_fixture_prompt() {
        let (client, server) = mock_model(
            "data: {\"id\":\"probe\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"ready\"},\"finish_reason\":null}]}\n\ndata: {\"id\":\"probe\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n",
            Duration::ZERO,
        ).await;
        assert!(
            probe_client(&client, "openai/gpt-4o", Duration::from_secs(2))
                .await
                .is_ok()
        );
        let request = server.await.unwrap();
        let body: serde_json::Value =
            serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap();
        assert_eq!(body["messages"].as_array().unwrap().len(), 1);
        assert_eq!(
            body["messages"][0]["content"],
            "Reply with the single word ready."
        );
        assert_eq!(body["max_tokens"], 32);
        assert!(
            body.get("tools")
                .is_none_or(|tools| tools.as_array().is_some_and(Vec::is_empty))
        );
    }

    #[tokio::test]
    async fn model_probe_rejects_empty_terminal_response() {
        let (client, server) = mock_model(
            "data: {\"id\":\"probe\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n",
            Duration::ZERO,
        ).await;
        assert!(
            probe_client(&client, "openai/gpt-4o", Duration::from_secs(2))
                .await
                .is_err()
        );
        let _ = server.await;
    }

    #[tokio::test]
    async fn model_probe_times_out_without_claiming_success() {
        let (client, server) = mock_model("", Duration::from_millis(100)).await;
        assert!(
            probe_client(&client, "openai/gpt-4o", Duration::from_millis(10))
                .await
                .is_err()
        );
        let _ = server.await;
    }

    #[tokio::test]
    async fn native_read_probe_requires_a_readable_matching_fixture() {
        assert!(tool_probe().await.is_ok());
        let dir = tempfile::tempdir().unwrap();
        assert!(read_fixture(dir.path()).await.is_err());
        std::fs::write(dir.path().join("probe.txt"), "wrong contents").unwrap();
        assert!(read_fixture(dir.path()).await.is_err());
    }
}

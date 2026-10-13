use super::*;
use crate::agent::NativeAgent;
use crate::ai::{ScriptedBlock, ScriptedClient, ScriptedResponse, StopReason, UnifiedClient};
use crate::state::ApprovalMode;
use maestro_runtime::agent::FromAgent;
use serde_json::json;
use std::sync::Mutex;
use std::time::Duration;

/// Give each host its own executor root while retaining the real admission,
/// hooks and receipt implementation.
struct RootedToolHost {
    cwd: String,
    identity_verification_skipped: Mutex<Vec<bool>>,
}

impl ToolHost for RootedToolHost {
    fn build_local_host(
        self: Arc<Self>,
        build: LocalHostBuild<'_>,
    ) -> Result<NativeExecutionHostHandle> {
        self.identity_verification_skipped
            .lock()
            .unwrap()
            .push(build.skip_identity_verification);
        let config = NativeAgentConfig {
            cwd: self.cwd.clone(),
            ..build.config.clone()
        };
        super::super::build_local_host(
            self,
            &config,
            build.credential_vault,
            build.skip_identity_verification,
            build.subagent_parent_scope_id,
            build.mailbox_identity,
            build.telemetry_identity_scope,
        )
    }

    fn registry_tool_definitions(&self) -> Vec<ToolDefinition> {
        BuiltInToolHost.registry_tool_definitions()
    }

    fn executor_tool_definitions(&self, cwd: &Path) -> Vec<ToolDefinition> {
        BuiltInToolHost.executor_tool_definitions(cwd)
    }
}

fn scripted_client(model: &str) -> UnifiedClient {
    UnifiedClient::Scripted(ScriptedClient::new(
        model,
        vec![
            ScriptedResponse {
                blocks: vec![ScriptedBlock::ToolUse {
                    id: "read-marker".to_owned(),
                    name: "read".to_owned(),
                    input: json!({"path": "marker.txt"}),
                }],
                stop_reason: StopReason::ToolUse,
                error: None,
            },
            ScriptedResponse::text("done"),
        ],
    ))
}

async fn read_marker(
    agent: &NativeAgent,
    events: &mut tokio::sync::mpsc::UnboundedReceiver<FromAgent>,
) -> String {
    agent
        .prompt("read the marker".to_owned(), vec![])
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        let mut output = None;
        loop {
            match events.recv().await.expect("agent event") {
                FromAgent::ToolEnd {
                    success,
                    result: Some(result),
                    ..
                } => {
                    assert!(success, "{result:?}");
                    output = Some(result.output);
                }
                FromAgent::TurnCompleted { .. } => return output.expect("tool result"),
                FromAgent::Error { message, .. } | FromAgent::ProviderError { message, .. } => {
                    panic!("{message}")
                }
                _ => {}
            }
        }
    })
    .await
    .expect("marker turn")
}

#[tokio::test]
async fn agents_use_independent_tool_hosts_in_one_process() {
    let _env = crate::config::test_process_env_lock_async().await;
    let first_root = tempfile::tempdir().unwrap();
    let second_root = tempfile::tempdir().unwrap();
    std::fs::write(first_root.path().join("marker.txt"), "first-host-marker").unwrap();
    std::fs::write(second_root.path().join("marker.txt"), "second-host-marker").unwrap();
    let first_host = Arc::new(RootedToolHost {
        cwd: first_root.path().display().to_string(),
        identity_verification_skipped: Mutex::new(Vec::new()),
    });
    let second_host = Arc::new(RootedToolHost {
        cwd: second_root.path().display().to_string(),
        identity_verification_skipped: Mutex::new(Vec::new()),
    });
    let config = NativeAgentConfig {
        model: "scripted-replay/maestro-replay-v1".to_owned(),
        cwd: first_root.path().display().to_string(),
        approval_mode: ApprovalMode::Yolo,
        ..NativeAgentConfig::default()
    };
    let (first, mut first_events) = NativeAgent::new_with_test_client(
        first_host.clone(),
        config.clone(),
        scripted_client(&config.model),
    )
    .unwrap();
    let (second, mut second_events) = NativeAgent::new_with_test_client(
        second_host.clone(),
        config.clone(),
        scripted_client(&config.model),
    )
    .unwrap();
    let (first_output, second_output) = tokio::join!(
        read_marker(&first, &mut first_events),
        read_marker(&second, &mut second_events),
    );
    first.shutdown().await;
    second.shutdown().await;
    assert!(first_output.contains("first-host-marker"));
    assert!(!first_output.contains("second-host-marker"));
    assert!(second_output.contains("second-host-marker"));
    assert!(!second_output.contains("first-host-marker"));
    assert_eq!(
        *first_host.identity_verification_skipped.lock().unwrap(),
        [true]
    );
    assert_eq!(
        *second_host.identity_verification_skipped.lock().unwrap(),
        [true]
    );

    // The dex-loop construction path keeps Identity verification enabled.
    crate::agent::dex_loop_execution_host(first_host.clone(), &config, CredentialVault::new())
        .unwrap();
    assert_eq!(
        *first_host.identity_verification_skipped.lock().unwrap(),
        [true, false]
    );
}

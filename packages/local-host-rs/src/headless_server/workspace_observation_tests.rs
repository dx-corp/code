//! Native capability advertisement, snapshots and queued activation wire contracts.
use super::*;
#[test]
fn native_server_capabilities_match_the_registry_and_request_surface() {
    let capabilities = crate::headless::native_server_capabilities();
    assert_eq!(
        capabilities.utility_operations,
        vec![
            crate::headless::UtilityOperation::CommandExec,
            crate::headless::UtilityOperation::FileSearch,
            crate::headless::UtilityOperation::FileRead,
            crate::headless::UtilityOperation::FileWatch,
        ]
    );
    assert!(capabilities.raw_agent_events);
    assert_eq!(
        capabilities.server_requests,
        vec![
            crate::headless::ServerRequestType::Approval,
            crate::headless::ServerRequestType::ClientTool,
            crate::headless::ServerRequestType::UserInput,
            crate::headless::ServerRequestType::ToolRetry,
        ]
    );

    let mut expected = crate::tools::ToolRegistry::new()
        .tools()
        .map(|definition| {
            let name = definition.tool.name.clone();
            crate::headless::NativeToolCapability {
                name: name.clone(),
                requires_approval: definition.requires_approval,
                version: crate::tools::versions::is_version_managed(&name)
                    .then(|| "current".to_string()),
            }
        })
        .collect::<Vec<_>>();
    expected.sort_unstable_by(|left, right| left.name.cmp(&right.name));
    assert_eq!(capabilities.native_tools, expected);

    let bash = capabilities
        .native_tools
        .iter()
        .find(|tool| tool.name == "bash")
        .expect("registry must advertise bash");
    assert!(bash.requires_approval);
    assert_eq!(bash.version.as_deref(), Some("current"));
}

#[test]
fn coarser_transcripts_coalesce_text_and_drop_thinking() {
    let mut chunks = vec![
        ("reasoning".to_string(), true),
        ("hello ".to_string(), false),
        ("world".to_string(), false),
    ];
    assert_eq!(coalesce_response_chunks(&mut chunks), "hello world");
    assert!(chunks.is_empty());
}

#[tokio::test]
async fn native_semantic_snapshot_keeps_processed_queue_ids_on_headless_wire() {
    const FIXTURE: &str = "MAESTRO_HEADLESS_SNAPSHOT_IDS_FIXTURE";
    if std::env::var_os(FIXTURE).is_some() {
        let meta = Arc::new(Mutex::new(RuntimeMeta::default()));
        let (tool_tx, _tool_rx) = mpsc::unbounded_channel();
        handle_agent_event(
            FromAgent::ConversationSnapshot {
                protocol_version: crate::headless::messages::SEMANTIC_CONVERSATION_PROTOCOL
                    .to_owned(),
                messages: vec![],
                processed_queue_ids: vec![7, 9],
            },
            &meta,
            &tool_tx,
            "test-model",
            None,
        )
        .await
        .expect("emit snapshot");
        return;
    }
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .arg("headless_server::workspace_observation_tests::native_semantic_snapshot_keeps_processed_queue_ids_on_headless_wire")
        .args(["--exact", "--nocapture", "--format", "terse"])
        .env(FIXTURE, "1")
        .output().expect("run snapshot fixture");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let snapshot = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .find(|event| event["type"] == "conversation_snapshot")
        .expect("private snapshot on headless wire");
    assert_eq!(snapshot["processed_queue_ids"], serde_json::json!([7, 9]));
}

#[tokio::test]
async fn queued_activation_observations_require_exact_installed_config_and_are_consumed_once() {
    use crate::headless::workspace_capabilities::WorkspaceCapabilitySetApplied;
    let receipt: WorkspaceCapabilitySetApplied = serde_json::from_value(serde_json::json!({
        "schema_version": "workspace-capability-set/v1", "organization_id": "org-1",
        "workspace_id": "workspace-1", "runner_session_id": "runner-1", "runtime_generation": 7,
        "activation_generation": 2, "effective_catalog_digest": "sha256:catalog2",
        "accepted_entry_digests": ["sha256:entry2"], "rejected_entries": [],
        "replay_cursor": "receipt-2", "applied_at": 1, "controller_binding_sha256": "sha256:binding",
        "provider_prompt_sha256": "sha256:provider", "staged_for_next_turn": true,
        "idempotent": false, "current_activation_generation": 1, "current_catalog_digest": "sha256:catalog1"
    })).unwrap();
    let mut meta = RuntimeMeta::default();
    for id in 1..=4 {
        meta.queued_workspace_receipts
            .insert(id, ("sha256:raw-config".to_owned(), receipt.clone()));
    }
    const FIXTURE: &str = "MAESTRO_HEADLESS_QUEUED_CONFIG_FIXTURE";
    if std::env::var_os(FIXTURE).is_some() {
        let meta = Arc::new(Mutex::new(meta));
        let (tool_tx, _tool_rx) = mpsc::unbounded_channel();
        for (queue_ids, hash) in [
            (vec![1], None),
            (vec![2], Some("sha256:superseding-config")),
            (vec![3, 4], Some("sha256:raw-config")),
            (vec![3], Some("sha256:raw-config")),
        ] {
            handle_agent_event(
                FromAgent::QueuedPromptConfiguration {
                    queue_ids,
                    system_prompt_sha256: hash.map(str::to_owned),
                },
                &meta,
                &tool_tx,
                "test-model",
                None,
            )
            .await
            .unwrap();
        }
        return;
    }
    assert!(
        meta.take_queued_workspace_receipts(&[1], None).is_empty(),
        "discarded or blocked prompt has no activation"
    );
    assert!(
        meta.take_queued_workspace_receipts(&[2], Some("sha256:superseding-config"))
            .is_empty()
    );
    let observed = meta.take_queued_workspace_receipts(&[3, 4], Some("sha256:raw-config"));
    assert_eq!(
        observed.len(),
        1,
        "batched admission of one config emits one receipt"
    );
    assert!(!observed[0].staged_for_next_turn);
    assert!(observed[0].idempotent);
    assert_eq!(observed[0].current_activation_generation, Some(2));
    assert_eq!(
        observed[0].current_catalog_digest.as_deref(),
        Some("sha256:catalog2")
    );
    assert_eq!(
        observed[0].provider_prompt_sha256,
        receipt.provider_prompt_sha256
    );
    assert!(
        meta.take_queued_workspace_receipts(&[3], Some("sha256:raw-config"))
            .is_empty()
    );
    assert!(meta.queued_workspace_receipts.is_empty());
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .arg("headless_server::workspace_observation_tests::queued_activation_observations_require_exact_installed_config_and_are_consumed_once")
        .args(["--exact", "--nocapture", "--format", "terse"])
        .env(FIXTURE, "1").output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let events: Vec<serde_json::Value> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .filter(|event: &serde_json::Value| event["type"] == "workspace_capability_set_applied")
        .collect();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["receipt"]["current_activation_generation"], 2);
    assert_eq!(
        events[0]["receipt"]["current_catalog_digest"],
        "sha256:catalog2"
    );
    assert_eq!(events[0]["receipt"]["staged_for_next_turn"], false);
}
#[test]
fn client_tool_content_preserves_text_and_images() {
    let result = client_content_to_agent_result(
        vec![
            ClientToolResultContent::Text {
                text: "done".to_string(),
            },
            ClientToolResultContent::Image {
                data: "AAAA".to_string(),
                mime_type: "image/png".to_string(),
            },
        ],
        false,
    );
    assert!(result.success);
    assert_eq!(result.output, "done\ndata:image/png;base64,AAAA");
    assert_eq!(result.error, None);
}
#[test]
fn request_resolution_maps_each_response_shape() {
    assert_eq!(
        server_request_resolution(ServerRequestType::Approval, Some(false), None, None, None,),
        ServerRequestResolutionStatus::Denied
    );
    assert_eq!(
        server_request_resolution(
            ServerRequestType::ToolRetry,
            None,
            None,
            None,
            Some(ToolRetryDecisionAction::Skip),
        ),
        ServerRequestResolutionStatus::Skipped
    );
}

//! Session persistence and admitted workspace recovery contracts.
use super::*;
use crate::ai::{ContentBlock, Message, MessageContent, Role};
use crate::headless::{CodeMode, GovernedToolGrant};
use tempfile::TempDir;

fn governed_grant_fixture() -> GovernedToolGrant {
    serde_json::from_value(serde_json::json!({
        "envelope_version": 2,
        "grant_id": "grant-reconnect",
        "grant_version": 8,
        "issuer": "evalops.platform",
        "audience": "evalops.maestro",
        "organization_id": "org-1",
        "workspace_id": "workspace-1",
        "thread_id": "thread-1",
        "turn_id": "turn-1",
        "run_id": "run-1",
        "runtime_generation": 5,
        "grant_epoch": 3,
        "issued_at_ms": 100,
        "not_before_ms": 100,
        "expires_at_ms": 10_000,
        "grant_hash": "sha256:immutable",
        "signing_key_id": "key-1",
        "grant_signature": "hmac-sha256:signed",
        "identity_authorization": {
            "schemaVersion": "identity.tool_authorization.v1",
            "organizationId": "org-1",
            "workspaceId": "workspace-1",
            "applicationId": "deixic",
            "subjectId": "user-1",
            "actorChainDigest": "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "decisionId": "decision-1",
            "authorizationLineageId": "lineage-1",
            "policyId": "policy-1",
            "policyVersion": "v1",
            "policyDigest": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            "authorizationFingerprint": "authz_fingerprint_v1_cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc",
            "capabilityDigest": "sha256:dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd",
            "actionDigest": "sha256:eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee",
            "audience": "evalops.maestro",
            "issuedAtMs": 100,
            "expiresAtMs": 10000,
            "revocationEpoch": 3
        },
        "native_tool_ids": ["read"],
        "external_tools": []
    }))
    .unwrap()
}

fn workspace_capability_request(generation: u64) -> ApplyWorkspaceCapabilitySet {
    ApplyWorkspaceCapabilitySet {
        organization_id: "org-1".to_string(),
        workspace_id: "workspace-1".to_string(),
        runner_session_id: "runner-1".to_string(),
        runtime_generation: 7,
        activation_generation: generation,
        workspace_snapshot_digest: format!("sha256:snapshot-{generation}"),
        workspace_skill_set_digest: format!("sha256:skills-{generation}"),
        capability_set_digest: format!("sha256:set-{generation}"),
        workspace_instructions: vec![format!("generation {generation}")],
        admitted_catalog: Vec::new(),
        admission_receipt_id: format!("admission-{generation}"),
    }
}

fn workspace_capability_receipt(
    request: &ApplyWorkspaceCapabilitySet,
) -> WorkspaceCapabilitySetApplied {
    WorkspaceCapabilitySetApplied {
        schema_version: "evalops.maestro.workspace-prompt-capability-set.v1".to_string(),
        organization_id: request.organization_id.clone(),
        workspace_id: request.workspace_id.clone(),
        runner_session_id: request.runner_session_id.clone(),
        runtime_generation: request.runtime_generation,
        activation_generation: request.activation_generation,
        effective_catalog_digest: request.capability_set_digest.clone(),
        accepted_entry_digests: request
            .admitted_catalog
            .iter()
            .map(|entry| entry.entry_digest.clone())
            .collect(),
        rejected_entries: Vec::new(),
        replay_cursor: workspace_capability_replay_cursor(request),
        applied_at: 123,
        controller_binding_sha256: "sha256:binding".to_string(),
        provider_prompt_sha256: "sha256:provider-prompt".to_string(),
        staged_for_next_turn: false,
        current_activation_generation: None,
        current_catalog_digest: None,
        idempotent: false,
    }
}

fn process_budget_fixture() -> ProcessBudgetState {
    let mut budget = ProcessBudgetState::new(crate::agent::process_budget::ProcessBudgetLimits {
        event_id: "event-session-replay".to_owned(),
        max_requests: 4,
        max_total_tokens: 100,
        max_cost_micros: 1_000,
        cost_micros_per_token: 2,
    })
    .expect("valid process budget limits");
    budget.admit_request().expect("first request admission");
    budget
        .observe_usage(3, 4, Some(41))
        .expect("provider usage");
    budget.admit_tools(1).expect("tool admission");
    budget
        .charge_tool("execution-session-1", 13)
        .expect("acknowledged tool cost");
    budget.admit_request().expect("pending second request");
    budget
}

fn legacy_sidecar_without_process_budget(path: &std::path::Path) {
    let json = fs::read_to_string(path).expect("read replay sidecar");
    let mut value: serde_json::Value = serde_json::from_str(&json).expect("parse sidecar");
    value
        .as_object_mut()
        .expect("sidecar object")
        .remove("last_process_budget");
    fs::write(
        path,
        serde_json::to_string(&value).expect("serialize legacy sidecar"),
    )
    .expect("write legacy sidecar");
}

#[test]
fn process_budget_evidence_survives_journal_sidecar_periodic_and_resume_paths() {
    let temp = TempDir::new().expect("session root");
    let mut recorder =
        SessionRecorder::with_id(temp.path(), "budget-evidence").expect("session recorder");
    let expected = process_budget_fixture();
    recorder
        .record_received(&FromAgentMessage::ProcessBudgetCheckpoint {
            budget: expected.clone(),
        })
        .expect("record budget checkpoint");

    // Budget observations are evidence only. The active state snapshot is
    // unchanged, so replay cannot install a grant or reset accounting.
    assert_eq!(
        recorder.replay().last_process_budget,
        Some(expected.clone())
    );
    assert_eq!(
        AgentStateCheckpoint::from_state(recorder.replay_state()),
        AgentStateCheckpoint::from_state(&AgentState::default())
    );

    // Cross the ordinary checkpoint interval to exercise the periodic
    // checkpoint path after the budget receipt's forced sync checkpoint.
    for index in 0..CHECKPOINT_INTERVAL {
        recorder
            .record_received(&FromAgentMessage::Status {
                message: format!("tail status {index}"),
            })
            .expect("record tail status");
    }
    recorder.flush().expect("flush budget journal");

    let journal = fs::read_to_string(recorder.path()).expect("read budget journal");
    assert!(journal.contains("process_budget_checkpoint"));
    assert!(journal.contains("\"last_process_budget\""));
    let sidecar_path = temp.path().join("budget-evidence.replay.json");
    let sidecar = fs::read_to_string(&sidecar_path).expect("read budget sidecar");
    assert!(sidecar.contains("\"last_process_budget\""));

    // Full journal replay and sidecar-plus-tail resume must retain exact
    // pending, spent, and acknowledged cost evidence.
    let full_replay = SessionReader::load(temp.path(), "budget-evidence")
        .expect("load full journal")
        .replay();
    assert_eq!(full_replay.last_process_budget, Some(expected.clone()));
    drop(recorder);
    let resumed = SessionRecorder::resume(temp.path(), "budget-evidence")
        .expect("resume recorder")
        .replay();
    assert_eq!(resumed.last_process_budget, Some(expected));
}

#[test]
fn legacy_sidecar_recovers_budget_prefix_without_dropping_semantic_history() {
    let temp = TempDir::new().expect("session root");
    let session_id = "legacy-budget-sidecar";
    let prefix_budget = process_budget_fixture();
    let mut tail_budget = prefix_budget.clone();
    tail_budget
        .observe_usage(5, 6, Some(23))
        .expect("resolve pending usage");
    tail_budget.admit_tools(1).expect("tail tool admission");
    tail_budget
        .charge_tool("execution-session-2", 17)
        .expect("second acknowledged tool cost");
    let semantic_messages = semantic_tool_pair_fixture();

    let mut recorder = SessionRecorder::with_id(temp.path(), session_id).expect("session recorder");
    recorder
        .record_semantic_conversation(semantic_messages.clone())
        .expect("record semantic history");
    recorder
        .flush_checkpoint()
        .expect("persist semantic history");
    recorder
        .record_received(&FromAgentMessage::ProcessBudgetCheckpoint {
            budget: prefix_budget.clone(),
        })
        .expect("record prefix budget");
    recorder
        .record_received(&FromAgentMessage::Status {
            message: "tail before crash".to_owned(),
        })
        .expect("record tail status");
    recorder.flush().expect("flush legacy sidecar fixture");
    let sidecar_path = temp.path().join(format!("{session_id}.replay.json"));
    let journal_path = temp.path().join(format!("{session_id}.jsonl"));
    drop(recorder);

    // Model a pre-budget sidecar whose tail already skips the prefix
    // receipt. Append a later receipt after that anchor so recovery must
    // combine journal-prefix evidence with sidecar-tail replay.
    legacy_sidecar_without_process_budget(&sidecar_path);
    let (legacy_sidecar, has_process_budget_field) =
        load_replay_sidecar(&sidecar_path).expect("load legacy sidecar");
    let prefix_replay =
        replay_from_sidecar_tail(&journal_path, legacy_sidecar, has_process_budget_field)
            .expect("replay legacy sidecar prefix");
    assert!(!has_process_budget_field);
    assert_eq!(prefix_replay.last_process_budget, Some(prefix_budget));
    assert_eq!(
        serde_json::to_value(&prefix_replay.semantic_conversation).expect("replayed messages"),
        serde_json::to_value(Some(sanitize_semantic_conversation(&semantic_messages)))
            .expect("expected messages")
    );
    let tail_entry = SessionEntry::received(FromAgentMessage::ProcessBudgetCheckpoint {
        budget: tail_budget.clone(),
    });
    let mut journal = OpenOptions::new()
        .append(true)
        .open(&journal_path)
        .expect("open journal tail");
    writeln!(
        journal,
        "{}",
        serde_json::to_string(&tail_entry).expect("serialize tail budget")
    )
    .expect("append tail budget");
    journal.sync_all().expect("sync journal tail");

    let replay = SessionRecorder::resume(temp.path(), session_id)
        .expect("resume legacy sidecar")
        .replay();
    assert_eq!(replay.last_process_budget, Some(tail_budget));
    assert_eq!(
        serde_json::to_value(&replay.semantic_conversation).expect("replayed messages"),
        serde_json::to_value(Some(sanitize_semantic_conversation(&semantic_messages)))
            .expect("expected messages")
    );
}

#[test]
fn legacy_checkpoint_without_budget_does_not_erase_prior_evidence() {
    let temp = TempDir::new().expect("session root");
    let session_id = "legacy-budget-checkpoint";
    let expected = process_budget_fixture();
    let received = SessionEntry::received(FromAgentMessage::ProcessBudgetCheckpoint {
        budget: expected.clone(),
    });
    let checkpoint = SessionEntry::Checkpoint {
        timestamp: 2,
        state: Box::new(AgentStateCheckpoint::from_state(&AgentState::default())),
        last_init: None,
        semantic_conversation: None,
        last_workspace_capability_set: None,
        last_process_budget: None,
    };
    let mut legacy_checkpoint = serde_json::to_value(checkpoint).expect("serialize checkpoint");
    legacy_checkpoint
        .as_object_mut()
        .expect("checkpoint object")
        .remove("last_process_budget");
    let journal = format!(
        "{}\n{}\n",
        serde_json::to_string(&received).expect("serialize budget"),
        serde_json::to_string(&legacy_checkpoint).expect("serialize legacy checkpoint")
    );
    fs::write(temp.path().join(format!("{session_id}.jsonl")), journal)
        .expect("write legacy journal");

    let replay = SessionReader::load(temp.path(), session_id)
        .expect("load legacy journal")
        .replay();
    assert_eq!(replay.last_process_budget, Some(expected));
    assert_eq!(
        AgentStateCheckpoint::from_state(&replay.state),
        AgentStateCheckpoint::from_state(&AgentState::default())
    );
}

#[test]
fn legacy_sidecar_without_budget_does_not_fabricate_a_zero_checkpoint() {
    let temp = TempDir::new().expect("session root");
    let session_id = "legacy-empty-budget-sidecar";
    let mut recorder = SessionRecorder::with_id(temp.path(), session_id).expect("session recorder");
    recorder
        .flush_checkpoint()
        .expect("persist empty checkpoint");
    let sidecar_path = temp.path().join(format!("{session_id}.replay.json"));
    drop(recorder);
    legacy_sidecar_without_process_budget(&sidecar_path);

    let replay = SessionRecorder::resume(temp.path(), session_id)
        .expect("resume legacy sidecar")
        .replay();
    assert!(replay.last_process_budget.is_none());
}

#[test]
fn session_restart_replays_only_the_last_matching_accepted_capability_set() {
    let temp = TempDir::new().expect("session root");
    let mut recorder = SessionRecorder::new(temp.path()).expect("session recorder");
    let session_id = recorder.id().to_string();
    let accepted = workspace_capability_request(1);
    recorder
        .record_sent(&ToAgentMessage::ApplyWorkspaceCapabilitySet {
            request: accepted.clone(),
        })
        .expect("record accepted candidate");

    let mut mismatched = workspace_capability_receipt(&accepted);
    mismatched.organization_id = "org-wrong".to_string();
    recorder
        .record_received(&FromAgentMessage::WorkspaceCapabilitySetApplied {
            receipt: mismatched,
        })
        .expect("record mismatched receipt");
    recorder
        .record_received(&FromAgentMessage::WorkspaceCapabilitySetApplied {
            receipt: workspace_capability_receipt(&accepted),
        })
        .expect("record matching receipt");

    recorder
        .record_sent(&ToAgentMessage::ApplyWorkspaceCapabilitySet {
            request: workspace_capability_request(2),
        })
        .expect("record unacknowledged successor");
    recorder.flush().expect("flush journal");
    drop(recorder);

    let replay = SessionRecorder::resume(temp.path(), &session_id)
        .expect("resume recorder")
        .replay();
    assert_eq!(replay.last_workspace_capability_set, Some(accepted));
}

#[test]
fn session_restart_ignores_partial_and_late_older_capability_receipts() {
    let temp = TempDir::new().expect("session root");
    let mut recorder = SessionRecorder::new(temp.path()).expect("session recorder");
    let session_id = recorder.id().to_string();
    let first = workspace_capability_request(1);
    let second = workspace_capability_request(2);
    for request in [&first, &second] {
        recorder
            .record_sent(&ToAgentMessage::ApplyWorkspaceCapabilitySet {
                request: request.clone(),
            })
            .expect("record candidate");
    }
    let mut partial = workspace_capability_receipt(&second);
    partial.rejected_entries.push("skill.review".to_string());
    recorder
        .record_received(&FromAgentMessage::WorkspaceCapabilitySetApplied { receipt: partial })
        .expect("record partial receipt");
    assert!(recorder.replay().last_workspace_capability_set.is_none());

    recorder
        .record_received(&FromAgentMessage::WorkspaceCapabilitySetApplied {
            receipt: workspace_capability_receipt(&second),
        })
        .expect("record accepted receipt");
    recorder
        .record_received(&FromAgentMessage::WorkspaceCapabilitySetApplied {
            receipt: workspace_capability_receipt(&first),
        })
        .expect("record late older receipt");
    recorder.flush().expect("flush journal");
    drop(recorder);

    let replay = SessionRecorder::resume(temp.path(), &session_id)
        .expect("resume recorder")
        .replay();
    assert_eq!(replay.last_workspace_capability_set, Some(second));
}

#[test]
fn governed_init_grant_identity_survives_session_replay() {
    let temp = TempDir::new().unwrap();
    let mut recorder = SessionRecorder::new(temp.path()).unwrap();
    let session_id = recorder.id().to_string();
    let grant = governed_grant_fixture();
    recorder
        .record_sent(&ToAgentMessage::GovernedInit {
            system_prompt: None,
            append_system_prompt: None,
            thinking_level: None,
            approval_mode: None,
            history: None,
            code_mode: CodeMode::GovernedCode,
            tool_grant: grant.clone(),
        })
        .unwrap();
    recorder.flush().unwrap();
    drop(recorder);

    let replay = SessionReader::load(temp.path(), &session_id)
        .unwrap()
        .replay();
    let restored = replay
        .last_init
        .and_then(|init| init.tool_grant)
        .expect("governed grant restored");
    assert_eq!(restored.identity(), grant.identity());
    assert_eq!(restored, grant);
}

#[path = "tests/managed_gateway_renewal.rs"]
mod managed_gateway_renewal;

fn semantic_tool_pair_fixture() -> Vec<Message> {
    vec![
        Message {
            role: Role::User,
            content: MessageContent::Text("inspect the workspace".to_string()),
        },
        Message {
            role: Role::Assistant,
            content: MessageContent::Blocks(vec![ContentBlock::ToolUse {
                id: "tool-call-1".to_string(),
                name: "bash".to_string(),
                input: serde_json::json!({ "command": "pwd" }),
                gemini_context: None,
            }]),
        },
        Message {
            role: Role::User,
            content: MessageContent::Blocks(vec![ContentBlock::ToolResult {
                tool_use_id: "tool-call-1".to_string(),
                content: "/workspace".to_string(),
                is_error: None,
            }]),
        },
        Message {
            role: Role::Assistant,
            content: MessageContent::Text("The workspace is ready.".to_string()),
        },
    ]
}

fn credential_tool_fixture() -> Vec<Message> {
    vec![Message {
        role: Role::Assistant,
        content: MessageContent::Blocks(vec![ContentBlock::ToolUse {
            id: "tool-call-1".to_string(),
            name: "http".to_string(),
            input: serde_json::json!({ "api_key": "sk-secret-value" }),
            gemini_context: None,
        }]),
    }]
}

#[test]
fn test_session_replay_restores_semantic_provider_conversation_with_tool_pairs() {
    let tmp = TempDir::new().unwrap();
    let messages = semantic_tool_pair_fixture();
    let mut recorder = SessionRecorder::new(tmp.path()).unwrap();
    let id = recorder.id().to_string();
    recorder
        .record_semantic_conversation(messages.clone())
        .unwrap();
    recorder.flush_checkpoint().unwrap();

    let replay = SessionRecorder::resume(tmp.path(), &id).unwrap().replay();
    assert_eq!(
        serde_json::to_value(replay.semantic_conversation).unwrap(),
        serde_json::to_value(Some(sanitize_semantic_conversation(&messages))).unwrap()
    );
}

#[test]
fn test_semantic_snapshot_checkpoint_redacts_credentials() {
    let tmp = TempDir::new().unwrap();
    let mut recorder = SessionRecorder::new(tmp.path()).unwrap();
    recorder
        .record_semantic_conversation(credential_tool_fixture())
        .unwrap();
    recorder.flush_checkpoint().unwrap();

    let jsonl = std::fs::read_to_string(recorder.path()).unwrap();
    let sidecar =
        std::fs::read_to_string(tmp.path().join(format!("{}.replay.json", recorder.id()))).unwrap();
    assert!(!jsonl.contains("sk-secret-value"));
    assert!(!jsonl.contains("tool-call-1"));
    assert!(!sidecar.contains("sk-secret-value"));
    assert!(sidecar.contains("[REDACTED]"));
    assert!(sidecar.contains("tool-call-1"));
}

#[test]
fn direct_semantic_snapshot_preserves_processed_queue_ids_across_resume() {
    let tmp = TempDir::new().unwrap();
    let mut recorder = SessionRecorder::new(tmp.path()).unwrap();
    let id = recorder.id().to_owned();
    for ids in [vec![7, 9], vec![9, 7, 11]] {
        let message: FromAgentMessage = serde_json::from_value(serde_json::json!({
            "type": "conversation_snapshot",
            "protocol_version": crate::headless::messages::SEMANTIC_CONVERSATION_PROTOCOL,
            "messages": semantic_tool_pair_fixture(),
            "processed_queue_ids": ids,
        }))
        .unwrap();
        recorder.record_received(&message).unwrap();
    }
    recorder.flush().unwrap();
    drop(recorder);

    let resumed = SessionRecorder::resume(tmp.path(), &id).unwrap();
    assert_eq!(
        resumed.semantic_processed_queue_ids(),
        &HashSet::from([7, 9, 11])
    );
    assert!(resumed.replay().semantic_conversation.is_some());
    assert!(
        SessionReader::load(tmp.path(), &id)
            .unwrap()
            .received_messages()
            .is_empty()
    );
    let sidecar: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(tmp.path().join(format!("{id}.replay.json"))).unwrap(),
    )
    .unwrap();
    assert_eq!(
        sidecar["semantic_processed_queue_ids"],
        serde_json::json!([7, 9, 11])
    );
}

#[test]
fn missing_or_corrupt_semantic_sidecar_does_not_invent_processed_queue_ids() {
    for corrupt in [false, true] {
        let tmp = TempDir::new().unwrap();
        let mut recorder = SessionRecorder::new(tmp.path()).unwrap();
        let id = recorder.id().to_owned();
        recorder
            .record_received(&FromAgentMessage::Status {
                message: "journal survives".to_owned(),
            })
            .unwrap();
        let snapshot: FromAgentMessage = serde_json::from_value(serde_json::json!({
            "type": "conversation_snapshot",
            "protocol_version": crate::headless::messages::SEMANTIC_CONVERSATION_PROTOCOL,
            "messages": semantic_tool_pair_fixture(),
            "processed_queue_ids": [7, 9],
        }))
        .unwrap();
        recorder.record_received(&snapshot).unwrap();
        recorder.flush().unwrap();
        drop(recorder);
        let sidecar = tmp.path().join(format!("{id}.replay.json"));
        if corrupt {
            fs::write(&sidecar, "invalid json").unwrap();
        } else {
            fs::remove_file(&sidecar).unwrap();
        }
        let resumed = SessionRecorder::resume(tmp.path(), &id).unwrap();
        // New journals deliberately omit private snapshots; this fallback
        // cannot recover their provider history or processed-id metadata.
        assert!(resumed.semantic_processed_queue_ids().is_empty());
        assert!(resumed.replay().semantic_conversation.is_none());
        assert_eq!(
            SessionReader::load(tmp.path(), &id)
                .unwrap()
                .received_messages()
                .len(),
            1
        );
    }
}

#[test]
fn legacy_semantic_snapshot_without_queue_ids_remains_readable() {
    let snapshot: FromAgentMessage = serde_json::from_value(serde_json::json!({
        "type": "conversation_snapshot",
        "protocol_version": crate::headless::messages::SEMANTIC_CONVERSATION_PROTOCOL,
        "messages": semantic_tool_pair_fixture(),
    }))
    .unwrap();
    let tmp = TempDir::new().unwrap();
    let mut recorder = SessionRecorder::new(tmp.path()).unwrap();
    let id = recorder.id().to_owned();
    recorder.record_received(&snapshot).unwrap();
    recorder.flush().unwrap();
    drop(recorder);
    let resumed = SessionRecorder::resume(tmp.path(), &id).unwrap();
    assert!(resumed.semantic_processed_queue_ids().is_empty());
    assert!(resumed.replay().semantic_conversation.is_some());
}

#[test]
fn explicit_semantic_snapshot_metadata_preserves_caller_queue_ids() {
    let snapshot: FromAgentMessage = serde_json::from_value(serde_json::json!({
        "type": "conversation_snapshot",
        "protocol_version": crate::headless::messages::SEMANTIC_CONVERSATION_PROTOCOL,
        "messages": semantic_tool_pair_fixture(),
        "processed_queue_ids": [7, 9],
    }))
    .unwrap();
    let tmp = TempDir::new().unwrap();
    let mut recorder = SessionRecorder::new(tmp.path()).unwrap();
    let id = recorder.id().to_owned();
    recorder
        .record_received_preserving_credential_references_with_snapshot_metadata(
            &snapshot,
            Some(3),
            &[21, 21, 23],
        )
        .unwrap();
    recorder.flush().unwrap();
    drop(recorder);
    let resumed = SessionRecorder::resume(tmp.path(), &id).unwrap();
    assert_eq!(
        resumed.semantic_processed_queue_ids(),
        &HashSet::from([21, 23])
    );
    assert_eq!(resumed.semantic_conversation_attempt, Some(3));
}

#[test]
fn received_semantic_snapshot_is_checkpoint_only_and_strips_private_content() {
    let tmp = TempDir::new().unwrap();
    let mut recorder = SessionRecorder::new(tmp.path()).unwrap();
    let id = recorder.id().to_owned();
    recorder
        .record_received(&FromAgentMessage::ConversationSnapshot {
            processed_queue_ids: vec![7, 9],
            protocol_version: crate::headless::messages::SEMANTIC_CONVERSATION_PROTOCOL.to_owned(),
            messages: vec![
                Message {
                    role: Role::Assistant,
                    content: MessageContent::Blocks(vec![
                        ContentBlock::Thinking {
                            thinking: "secret reasoning".to_owned(),
                            signature: None,
                        },
                        ContentBlock::ToolUse {
                            id: "call-private".to_owned(),
                            name: "read".to_owned(),
                            input: serde_json::json!({}),
                            gemini_context: None,
                        },
                    ]),
                },
                Message {
                    role: Role::User,
                    content: MessageContent::Blocks(vec![ContentBlock::ToolResult {
                        tool_use_id: "call-private".to_owned(),
                        content: "private tool output".to_owned(),
                        is_error: None,
                    }]),
                },
            ],
        })
        .unwrap();
    recorder.flush().unwrap();
    drop(recorder);

    let reader = SessionReader::load(tmp.path(), &id).unwrap();
    assert!(reader.received_messages().is_empty());
    let jsonl = fs::read_to_string(tmp.path().join(format!("{id}.jsonl"))).unwrap();
    let sidecar = fs::read_to_string(tmp.path().join(format!("{id}.replay.json"))).unwrap();
    assert!(!jsonl.contains("secret reasoning"));
    assert!(!jsonl.contains("private tool output"));
    assert!(!jsonl.contains("call-private"));
    assert!(!sidecar.contains("secret reasoning"));
    assert!(!sidecar.contains("private tool output"));
    assert!(sidecar.contains("call-private"));
}

#[test]
fn unsupported_semantic_checkpoint_clears_stale_provider_history() {
    let tmp = TempDir::new().unwrap();
    let mut recorder = SessionRecorder::new(tmp.path()).unwrap();
    let id = recorder.id().to_owned();
    recorder
        .record_received(&FromAgentMessage::ConversationSnapshot {
            protocol_version: crate::headless::messages::SEMANTIC_CONVERSATION_PROTOCOL.to_owned(),
            messages: semantic_tool_pair_fixture(),
            processed_queue_ids: vec![7, 9],
        })
        .unwrap();
    assert_eq!(
        recorder.semantic_processed_queue_ids(),
        &HashSet::from([7, 9])
    );
    recorder.flush_checkpoint().unwrap();
    recorder
        .record_received(&FromAgentMessage::ConversationSnapshot {
            processed_queue_ids: vec![7, 9],
            protocol_version: "evalops.maestro.semantic-conversation.v999".to_owned(),
            messages: vec![],
        })
        .unwrap();
    recorder.flush().unwrap();
    drop(recorder);

    assert!(
        SessionRecorder::resume(tmp.path(), &id)
            .unwrap()
            .semantic_processed_queue_ids()
            .is_empty()
    );
    assert!(
        SessionReader::load(tmp.path(), &id)
            .unwrap()
            .replay()
            .semantic_conversation
            .is_none()
    );
}

#[test]
fn replay_sidecar_bounds_semantic_persistence_as_history_grows() {
    fn persisted_bytes(turns: usize) -> u64 {
        let tmp = TempDir::new().unwrap();
        let mut recorder = SessionRecorder::new(tmp.path()).unwrap();
        let id = recorder.id().to_owned();
        for turn in 0..turns {
            let messages = (0..=turn)
                .map(|index| Message {
                    role: Role::User,
                    content: MessageContent::Text(format!("turn-{index}: {}", "x".repeat(256))),
                })
                .collect();
            recorder.record_semantic_conversation(messages).unwrap();
            recorder.flush_checkpoint().unwrap();
        }
        let journal = fs::metadata(recorder.path()).unwrap().len();
        let sidecar = fs::metadata(tmp.path().join(format!("{id}.replay.json")))
            .unwrap()
            .len();
        let journal_text = fs::read_to_string(recorder.path()).unwrap();
        assert!(
            !journal_text.contains("semantic_conversation"),
            "semantic history must not be duplicated in append-only checkpoints"
        );
        journal + sidecar
    }

    let eight_turns = persisted_bytes(8);
    let sixteen_turns = persisted_bytes(16);
    assert!(
        sixteen_turns < eight_turns * 3,
        "latest-sidecar persistence should grow linearly, not quadratically: {eight_turns} -> {sixteen_turns}"
    );
}

#[test]
fn resume_uses_latest_sidecar_anchor_and_applies_only_the_tail() {
    let tmp = TempDir::new().unwrap();
    let mut recorder = SessionRecorder::new(tmp.path()).unwrap();
    let id = recorder.id().to_owned();
    recorder
        .record_sent(&ToAgentMessage::Init {
            system_prompt: Some("historical init".to_owned()),
            append_system_prompt: None,
            thinking_level: None,
            approval_mode: None,
            history: None,
        })
        .unwrap();
    recorder.flush_checkpoint().unwrap();
    for index in 0..10 {
        recorder
            .record_received(&FromAgentMessage::Status {
                message: format!("historical status {index}"),
            })
            .unwrap();
    }
    recorder
        .record_sent(&ToAgentMessage::Init {
            system_prompt: Some("tail init".to_owned()),
            append_system_prompt: None,
            thinking_level: None,
            approval_mode: None,
            history: None,
        })
        .unwrap();
    recorder.flush().unwrap();
    drop(recorder);

    let replay = SessionRecorder::resume(tmp.path(), &id).unwrap().replay();
    assert_eq!(
        replay.last_init.and_then(|init| init.system_prompt),
        Some("tail init".to_owned())
    );
}

#[test]
fn sidecar_publish_before_checkpoint_preserves_new_semantic_snapshot_on_interruption() {
    let tmp = TempDir::new().unwrap();
    let mut recorder = SessionRecorder::new(tmp.path()).unwrap();
    let id = recorder.id().to_owned();
    let messages = semantic_tool_pair_fixture();
    recorder
        .record_semantic_conversation(messages.clone())
        .unwrap();

    // Simulate termination after the pre-checkpoint atomic publication and
    // before the semantic-free journal checkpoint can be appended.
    recorder.write_replay_sidecar().unwrap();
    drop(recorder);

    let replay = SessionRecorder::resume(tmp.path(), &id).unwrap().replay();
    assert_eq!(
        serde_json::to_value(replay.semantic_conversation).unwrap(),
        serde_json::to_value(Some(sanitize_semantic_conversation(&messages))).unwrap()
    );
}

#[test]
fn restore_manifest_uses_checkpoint_after_process_death() {
    let tmp = TempDir::new().unwrap();
    let messages = semantic_tool_pair_fixture();
    let mut recorder = SessionRecorder::new(tmp.path()).unwrap();
    let id = recorder.id().to_string();
    recorder
        .record_semantic_conversation(messages.clone())
        .unwrap();
    recorder.flush_checkpoint().unwrap();

    // A later facade-only snapshot is the state saved in a drain manifest;
    // it must not erase the durable provider checkpoint before restart.
    recorder
        .apply_snapshot(AgentState::default(), None)
        .unwrap();
    recorder.flush_checkpoint().unwrap();
    drop(recorder);

    let replay = SessionRecorder::resume(tmp.path(), &id).unwrap().replay();
    assert_eq!(
        serde_json::to_value(replay.semantic_conversation).unwrap(),
        serde_json::to_value(Some(sanitize_semantic_conversation(&messages))).unwrap()
    );
}

#[test]
fn test_session_record_and_load() {
    let tmp = TempDir::new().unwrap();
    let sessions_dir = tmp.path();

    // Create and record a session
    let mut recorder = SessionRecorder::new(sessions_dir).unwrap();
    let id = recorder.id().to_string();

    // Record a prompt
    recorder
        .record_sent(&ToAgentMessage::Prompt {
            content: "Hello, world!".to_string(),
            attachments: None,
            managed_inference_authorization: None,
        })
        .unwrap();

    // Record a response
    recorder
        .record_received(&FromAgentMessage::Ready {
            protocol_version: Some("2026-03-30".to_string()),
            model: "claude-3-opus".to_string(),
            provider: "anthropic".to_string(),
            session_id: Some("sess_123".to_string()),
        })
        .unwrap();

    recorder.flush().unwrap();
    drop(recorder);

    // Load the session
    let reader = SessionReader::load(sessions_dir, &id).unwrap();
    assert_eq!(reader.entries().len(), 2);
    assert_eq!(reader.prompts().len(), 1);
    assert_eq!(reader.prompts()[0], "Hello, world!");
    assert_eq!(reader.metadata().title.as_deref(), Some("Hello, world!"));
    assert_eq!(reader.metadata().model.as_deref(), Some("claude-3-opus"));
    assert_eq!(
        reader.metadata().protocol_version.as_deref(),
        Some("2026-03-30")
    );
    assert_eq!(
        reader.metadata().agent_session_id.as_deref(),
        Some("sess_123")
    );
}

#[test]
fn recorder_entry_points_create_missing_session_directories() {
    let tmp = TempDir::new().unwrap();
    let new_sessions = tmp.path().join("new").join("sessions");
    let resumed_sessions = tmp.path().join("resumed").join("sessions");

    let new_recorder = SessionRecorder::with_id(&new_sessions, "new-session").unwrap();
    assert!(new_sessions.is_dir());
    assert!(new_recorder.path().exists());

    let resumed_recorder = SessionRecorder::resume(&resumed_sessions, "resumed-session").unwrap();
    assert!(resumed_sessions.is_dir());
    assert!(resumed_recorder.path().exists());
}

#[test]
fn received_executable_args_are_portable_redacted_before_persistence() {
    let tmp = TempDir::new().unwrap();
    let sessions_dir = tmp.path();
    let client_secret = "sk-ant-abcdefghijklmnopqrstuvwxyz123456";
    let server_secret = "correct-horse-battery-staple";

    let mut recorder = SessionRecorder::with_id(sessions_dir, "redacted-session").unwrap();
    recorder
        .record_received(&FromAgentMessage::ClientToolRequest {
            call_id: "client-call".to_string(),
            tool_execution_id: None,
            tool: "bash".to_string(),
            args: serde_json::json!({
                "command": format!(
                    "curl -H 'Authorization: Bearer {client_secret}' example.test"
                )
            }),
        })
        .unwrap();
    recorder
        .record_received(&FromAgentMessage::ServerRequest {
            request_id: "server-request".to_string(),
            request_type: ServerRequestType::ClientTool,
            call_id: "server-call".to_string(),
            tool_execution_id: None,
            tool: "http".to_string(),
            args: serde_json::json!({
                "payload": format!("password={server_secret}"),
            }),
            reason: "execute on client".to_string(),
            started_at_ms: None,
        })
        .unwrap();

    let live_args = serde_json::to_string(&recorder.replay_state().pending_client_tools)
        .expect("serialize live pending client tools");
    assert!(live_args.contains(client_secret));
    assert!(live_args.contains(server_secret));
    recorder.maybe_write_checkpoint(true).unwrap();
    recorder.flush().unwrap();
    drop(recorder);

    let jsonl =
        fs::read_to_string(sessions_dir.join("redacted-session.jsonl")).expect("read JSONL");
    assert!(!jsonl.contains(client_secret), "{jsonl}");
    assert!(!jsonl.contains(server_secret), "{jsonl}");
    assert!(
        jsonl.contains("[REDACTED:token:portable-export]"),
        "{jsonl}"
    );
    assert!(
        jsonl.contains("[REDACTED:password:portable-export]"),
        "{jsonl}"
    );

    let reader = SessionReader::load(sessions_dir, "redacted-session").unwrap();
    let recorded =
        serde_json::to_string(reader.received_messages().as_slice()).expect("serialize replay");
    let replayed =
        serde_json::to_string(&reader.replay_state().pending_client_tools).expect("replay state");
    for persisted in [&recorded, &replayed] {
        assert!(!persisted.contains(client_secret), "{persisted}");
        assert!(!persisted.contains(server_secret), "{persisted}");
        assert!(persisted.contains("[REDACTED:"), "{persisted}");
    }
}

#[test]
fn test_session_reader_replay_restores_state_and_init() {
    let tmp = TempDir::new().unwrap();
    let sessions_dir = tmp.path();

    let mut recorder = SessionRecorder::new(sessions_dir).unwrap();
    let id = recorder.id().to_string();

    recorder
        .record_sent(&ToAgentMessage::Init {
            system_prompt: Some("You are Maestro".to_string()),
            append_system_prompt: Some("Stay concise".to_string()),
            thinking_level: Some(super::super::messages::ThinkingLevel::High),
            approval_mode: Some(super::super::messages::ApprovalMode::Prompt),
            history: None,
        })
        .unwrap();

    recorder
        .record_received(&FromAgentMessage::Ready {
            protocol_version: Some("2026-03-30".to_string()),
            model: "claude-3-opus".to_string(),
            provider: "anthropic".to_string(),
            session_id: Some("sess_ready".to_string()),
        })
        .unwrap();
    recorder
        .record_received(&FromAgentMessage::SessionInfo {
            session_id: Some("sess_info".to_string()),
            cwd: "/tmp/project".to_string(),
            git_branch: Some("main".to_string()),
        })
        .unwrap();
    recorder
        .record_received(&FromAgentMessage::ResponseStart {
            response_id: "resp_1".to_string(),
        })
        .unwrap();
    recorder
        .record_received(&FromAgentMessage::ResponseChunk {
            response_id: "resp_1".to_string(),
            content: "Partial reply".to_string(),
            is_thinking: false,
        })
        .unwrap();
    recorder
        .record_received(&FromAgentMessage::ToolCall {
            call_id: "call_1".to_string(),
            tool_execution_id: None,
            tool: "bash".to_string(),
            args: serde_json::json!({ "cmd": "git status" }),
            requires_approval: true,
        })
        .unwrap();
    recorder
        .record_received(&FromAgentMessage::UtilityCommandStarted {
            command_id: "cmd_owned".to_string(),
            command: "echo hi".to_string(),
            cwd: Some("/tmp/project".to_string()),
            shell_mode: super::super::messages::UtilityCommandShellMode::Direct,
            terminal_mode: super::super::messages::UtilityCommandTerminalMode::Pipe,
            pid: Some(1234),
            columns: None,
            rows: None,
            owner_connection_id: Some("conn_owned".to_string()),
        })
        .unwrap();
    recorder
        .record_received(&FromAgentMessage::UtilityFileWatchStarted {
            watch_id: "watch_owned".to_string(),
            root_dir: "/tmp/project".to_string(),
            include_patterns: Some(vec!["src/**".to_string()]),
            exclude_patterns: Some(vec!["dist/**".to_string()]),
            debounce_ms: 50,
            owner_connection_id: Some("conn_owned".to_string()),
        })
        .unwrap();
    recorder.flush().unwrap();
    drop(recorder);

    let reader = SessionReader::load(sessions_dir, &id).unwrap();
    let replay = reader.replay();

    assert_eq!(
        replay.last_init,
        Some(InitConfig {
            system_prompt: Some("You are Maestro".to_string()),
            append_system_prompt: Some("Stay concise".to_string()),
            thinking_level: Some(super::super::messages::ThinkingLevel::High),
            approval_mode: Some(super::super::messages::ApprovalMode::Prompt),
            history: None,
            code_mode: None,
            tool_grant: None,
        })
    );
    assert_eq!(replay.state.protocol_version.as_deref(), Some("2026-03-30"));
    assert_eq!(replay.state.session_id.as_deref(), Some("sess_info"));
    assert_eq!(replay.state.cwd.as_deref(), Some("/tmp/project"));
    assert_eq!(replay.state.git_branch.as_deref(), Some("main"));
    assert!(replay.state.is_ready);
    assert!(replay.state.is_responding);
    assert_eq!(
        replay
            .state
            .current_response
            .as_ref()
            .map(|response| response.text.as_str()),
        Some("Partial reply")
    );
    assert_eq!(replay.state.pending_approvals.len(), 1);
    assert_eq!(replay.state.pending_approvals[0].tool, "bash");
    assert_eq!(
        replay
            .state
            .active_utility_commands
            .get("cmd_owned")
            .and_then(|command| command.owner_connection_id.as_deref()),
        Some("conn_owned")
    );
    assert_eq!(
        replay
            .state
            .active_file_watches
            .get("watch_owned")
            .and_then(|watch| watch.owner_connection_id.as_deref()),
        Some("conn_owned")
    );
}

#[test]
fn test_last_init_returns_most_recent_init_message() {
    let tmp = TempDir::new().unwrap();
    let sessions_dir = tmp.path();

    let mut recorder = SessionRecorder::new(sessions_dir).unwrap();
    let id = recorder.id().to_string();

    recorder
        .record_sent(&ToAgentMessage::Init {
            system_prompt: Some("First".to_string()),
            append_system_prompt: None,
            thinking_level: Some(super::super::messages::ThinkingLevel::Low),
            approval_mode: Some(super::super::messages::ApprovalMode::Auto),
            history: None,
        })
        .unwrap();
    recorder
        .record_sent(&ToAgentMessage::Prompt {
            content: "Hello".to_string(),
            attachments: None,
            managed_inference_authorization: None,
        })
        .unwrap();
    recorder
        .record_sent(&ToAgentMessage::Init {
            system_prompt: Some("Second".to_string()),
            append_system_prompt: None,
            thinking_level: Some(super::super::messages::ThinkingLevel::Ultra),
            approval_mode: Some(super::super::messages::ApprovalMode::Fail),
            history: None,
        })
        .unwrap();
    recorder.flush().unwrap();
    drop(recorder);

    let reader = SessionReader::load(sessions_dir, &id).unwrap();
    assert_eq!(
        reader.last_init(),
        Some(InitConfig {
            system_prompt: Some("Second".to_string()),
            append_system_prompt: None,
            thinking_level: Some(super::super::messages::ThinkingLevel::Ultra),
            approval_mode: Some(super::super::messages::ApprovalMode::Fail),
            history: None,
            code_mode: None,
            tool_grant: None,
        })
    );
}

#[test]
fn test_list_sessions() {
    let tmp = TempDir::new().unwrap();
    let sessions_dir = tmp.path();

    // Create a few sessions
    let mut r1 = SessionRecorder::new(sessions_dir).unwrap();
    r1.record_sent(&ToAgentMessage::Prompt {
        content: "First session".to_string(),
        attachments: None,
        managed_inference_authorization: None,
    })
    .unwrap();
    r1.flush().unwrap();

    let mut r2 = SessionRecorder::new(sessions_dir).unwrap();
    r2.record_sent(&ToAgentMessage::Prompt {
        content: "Second session".to_string(),
        attachments: None,
        managed_inference_authorization: None,
    })
    .unwrap();
    r2.flush().unwrap();

    // List sessions
    let sessions = list_sessions(sessions_dir).unwrap();
    assert_eq!(sessions.len(), 2);
}

#[test]
fn test_delete_session() {
    let tmp = TempDir::new().unwrap();
    let sessions_dir = tmp.path();

    // Create a session
    let mut recorder = SessionRecorder::new(sessions_dir).unwrap();
    let id = recorder.id().to_string();
    recorder
        .record_sent(&ToAgentMessage::Prompt {
            content: "Test".to_string(),
            attachments: None,
            managed_inference_authorization: None,
        })
        .unwrap();
    recorder.flush().unwrap();
    drop(recorder);

    // Verify files exist
    assert!(sessions_dir.join(format!("{}.jsonl", id)).exists());
    assert!(sessions_dir.join(format!("{}.meta.json", id)).exists());
    let spill_dir = sessions_dir.join("tool-output").join(&id);
    fs::create_dir_all(&spill_dir).unwrap();
    fs::write(spill_dir.join("large.txt"), "output").unwrap();

    // Delete the session
    delete_session(sessions_dir, &id).unwrap();

    // Verify files are gone
    assert!(!sessions_dir.join(format!("{}.jsonl", id)).exists());
    assert!(!sessions_dir.join(format!("{}.meta.json", id)).exists());
    assert!(!spill_dir.exists());
}

#[test]
fn delete_session_succeeds_when_post_commit_spill_cleanup_fails() {
    let tmp = TempDir::new().unwrap();
    let sessions_dir = tmp.path();
    let id = "spill-cleanup-failure";
    fs::write(sessions_dir.join(format!("{id}.jsonl")), "{}\n").unwrap();
    fs::write(sessions_dir.join(format!("{id}.meta.json")), "{}").unwrap();
    fs::write(sessions_dir.join("tool-output"), "blocks spill directory").unwrap();

    delete_session(sessions_dir, id).expect("transcript deletion is the commit point");

    assert!(!sessions_dir.join(format!("{id}.jsonl")).exists());
    assert!(!sessions_dir.join(format!("{id}.meta.json")).exists());
    assert!(sessions_dir.join("tool-output").exists());
}

#[test]
fn test_resume_session() {
    let tmp = TempDir::new().unwrap();
    let sessions_dir = tmp.path();

    // Create initial session
    let mut recorder = SessionRecorder::new(sessions_dir).unwrap();
    let id = recorder.id().to_string();
    recorder
        .record_sent(&ToAgentMessage::Prompt {
            content: "First message".to_string(),
            attachments: None,
            managed_inference_authorization: None,
        })
        .unwrap();
    recorder.flush().unwrap();
    drop(recorder);

    // Resume the session
    let mut recorder = SessionRecorder::resume(sessions_dir, &id).unwrap();
    recorder
        .record_sent(&ToAgentMessage::Prompt {
            content: "Second message".to_string(),
            attachments: None,
            managed_inference_authorization: None,
        })
        .unwrap();
    recorder.flush().unwrap();
    drop(recorder);

    // Load and verify
    let reader = SessionReader::load(sessions_dir, &id).unwrap();
    assert_eq!(reader.prompts().len(), 2);
}

/// Regression test for a torn `.meta.json`: before the fix, both
/// `SessionRecorder::resume` and `SessionReader::load` hard-failed with
/// an `InvalidData` IO error on a corrupt metadata file, even though the
/// JSONL log (the actual conversation history) was fully intact. A user
/// hitting a crash right as metadata flushed would be unable to resume
/// a session that was otherwise perfectly recoverable.
#[test]
fn resume_tolerates_corrupt_metadata_instead_of_hard_failing() {
    let tmp = TempDir::new().unwrap();
    let sessions_dir = tmp.path();

    let mut recorder = SessionRecorder::new(sessions_dir).unwrap();
    let id = recorder.id().to_string();
    recorder
        .record_sent(&ToAgentMessage::Prompt {
            content: "Before the crash".to_string(),
            attachments: None,
            managed_inference_authorization: None,
        })
        .unwrap();
    recorder.flush().unwrap();
    drop(recorder);

    // Simulate a crash mid-`fs::write` of the metadata file: truncated,
    // invalid JSON.
    let meta_path = sessions_dir.join(format!("{id}.meta.json"));
    fs::write(&meta_path, "{\"id\":\"partial").unwrap();

    // resume() must succeed rather than propagating a parse error.
    let mut recorder = SessionRecorder::resume(sessions_dir, &id)
        .expect("resume must tolerate corrupt metadata, not hard-fail");
    recorder
        .record_sent(&ToAgentMessage::Prompt {
            content: "After the resume".to_string(),
            attachments: None,
            managed_inference_authorization: None,
        })
        .unwrap();
    recorder.flush().unwrap();
    drop(recorder);

    // The corrupt file must be preserved as forensic evidence, not
    // silently overwritten in place.
    let rotated: Vec<_> = fs::read_dir(sessions_dir)
        .unwrap()
        .flatten()
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .contains(".meta.json.corrupt.")
        })
        .collect();
    assert_eq!(
        rotated.len(),
        1,
        "corrupt metadata should be rotated aside, not discarded"
    );

    // The conversation history survived the crash even though metadata
    // did not: both prompts (before and after) must still be present.
    let reader = SessionReader::load(sessions_dir, &id).unwrap();
    assert_eq!(reader.prompts().len(), 2);

    // list_sessions must not hide a session just because its metadata
    // was corrupt -- that would make a crash look identical to the
    // session never having existed.
    let sessions = list_sessions(sessions_dir).unwrap();
    assert!(sessions.iter().any(|meta| meta.id == id));
}

/// Regression test: a `.meta.json` torn mid-write can just as easily be
/// torn in the middle of a multi-byte UTF-8 character as in the middle
/// of a JSON token. Before the fix, `load_metadata_tolerant` used
/// `fs::read_to_string`, which failed with `InvalidData` before the
/// tolerant JSON-parsing branch (exercised by
/// `resume_tolerates_corrupt_metadata_instead_of_hard_failing` above)
/// ever ran -- so this exact kind of corruption still hard-failed
/// `resume` and silently dropped the session from `list_sessions`,
/// even though the equivalent ASCII-corruption case was already fixed.
#[test]
fn resume_and_list_sessions_tolerate_invalid_utf8_metadata() {
    let tmp = TempDir::new().unwrap();
    let sessions_dir = tmp.path();

    let mut recorder = SessionRecorder::new(sessions_dir).unwrap();
    let id = recorder.id().to_string();
    recorder
        .record_sent(&ToAgentMessage::Prompt {
            content: "Before the crash".to_string(),
            attachments: None,
            managed_inference_authorization: None,
        })
        .unwrap();
    recorder.flush().unwrap();
    drop(recorder);

    // Simulate a crash mid-write torn in the middle of a multi-byte
    // UTF-8 character: a valid JSON prefix followed by a lone
    // continuation byte, which is invalid UTF-8 on its own.
    let meta_path = sessions_dir.join(format!("{id}.meta.json"));
    let mut torn = br#"{"id":"partial","title":"caf"#.to_vec();
    torn.push(0xE9); // incomplete multi-byte sequence, not valid UTF-8
    fs::write(&meta_path, &torn).unwrap();
    assert!(
        std::str::from_utf8(&torn).is_err(),
        "test fixture must actually be invalid UTF-8"
    );

    let mut recorder = SessionRecorder::resume(sessions_dir, &id)
        .expect("resume must tolerate invalid-UTF-8 metadata, not hard-fail");
    recorder
        .record_sent(&ToAgentMessage::Prompt {
            content: "After the resume".to_string(),
            attachments: None,
            managed_inference_authorization: None,
        })
        .unwrap();
    recorder.flush().unwrap();
    drop(recorder);

    let rotated: Vec<_> = fs::read_dir(sessions_dir)
        .unwrap()
        .flatten()
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .contains(".meta.json.corrupt.")
        })
        .collect();
    assert_eq!(
        rotated.len(),
        1,
        "invalid-UTF-8 metadata should be rotated aside, not discarded"
    );

    let reader = SessionReader::load(sessions_dir, &id).unwrap();
    assert_eq!(reader.prompts().len(), 2);

    let sessions = list_sessions(sessions_dir).unwrap();
    assert!(sessions.iter().any(|meta| meta.id == id));
}

/// When corrupt metadata is rotated aside on resume, the lost fields
/// (title, message count) are rebuilt from the still-intact JSONL log
/// instead of being permanently reset to defaults on the next flush.
#[test]
fn resume_rebuilds_corrupt_metadata_from_jsonl_log() {
    let tmp = TempDir::new().unwrap();
    let sessions_dir = tmp.path();

    let mut recorder = SessionRecorder::new(sessions_dir).unwrap();
    let id = recorder.id().to_string();
    recorder
        .record_sent(&ToAgentMessage::Prompt {
            content: "Recovered title".to_string(),
            attachments: None,
            managed_inference_authorization: None,
        })
        .unwrap();
    recorder.flush().unwrap();
    drop(recorder);

    let meta_path = sessions_dir.join(format!("{id}.meta.json"));
    fs::write(&meta_path, "{\"id\":\"partial").unwrap();

    let mut recorder = SessionRecorder::resume(sessions_dir, &id).unwrap();
    assert_eq!(
        recorder.metadata().title.as_deref(),
        Some("Recovered title"),
        "title must be rebuilt from the JSONL log, not reset"
    );
    assert_eq!(recorder.metadata().message_count, 1);
    recorder.flush().unwrap();
    drop(recorder);

    let reader = SessionReader::load(sessions_dir, &id).unwrap();
    assert_eq!(reader.metadata().title.as_deref(), Some("Recovered title"));
}

/// Regression test: calling `SessionReader::load` directly (not through
/// `SessionRecorder::resume`) on a session with corrupt metadata must
/// also rebuild the lost fields from the JSONL log, not silently reset
/// them to defaults -- and must persist that rebuild, since this
/// read-only path has no later `flush()` to do so, and without it a
/// later `list_sessions` scan would find no `.meta.json` for this
/// session at all (it was rotated aside above, with nothing to replace
/// it) and treat a fully recoverable session as if it never existed.
#[test]
fn direct_session_reader_load_rebuilds_and_persists_corrupt_metadata() {
    let tmp = TempDir::new().unwrap();
    let sessions_dir = tmp.path();

    let mut recorder = SessionRecorder::new(sessions_dir).unwrap();
    let id = recorder.id().to_string();
    recorder
        .record_sent(&ToAgentMessage::Prompt {
            content: "Loaded directly".to_string(),
            attachments: None,
            managed_inference_authorization: None,
        })
        .unwrap();
    recorder.flush().unwrap();
    drop(recorder);

    let meta_path = sessions_dir.join(format!("{id}.meta.json"));
    fs::write(&meta_path, "{\"id\":\"partial").unwrap();

    let reader = SessionReader::load(sessions_dir, &id)
        .expect("load must tolerate corrupt metadata, not hard-fail");
    assert_eq!(
        reader.metadata().title.as_deref(),
        Some("Loaded directly"),
        "title must be rebuilt from the JSONL log via a direct load, not reset"
    );
    assert_eq!(reader.metadata().message_count, 1);

    // The rebuild must be persisted, not just returned in memory: a
    // later listing has to be able to find this session again.
    assert!(
        meta_path.exists(),
        "rebuilt metadata must be persisted after a direct load, not left absent"
    );
    let sessions = list_sessions(sessions_dir).unwrap();
    let found = sessions
        .iter()
        .find(|meta| meta.id == id)
        .expect("session must still be discoverable after a direct load rebuilt its metadata");
    assert_eq!(found.title.as_deref(), Some("Loaded directly"));
}

#[test]
fn rebuilt_metadata_persistence_errors_are_propagated() {
    let tmp = TempDir::new().unwrap();
    let target = tmp.path().join("session.meta.json");
    fs::create_dir(&target).unwrap();
    fs::write(target.join("occupied"), "keep").unwrap();

    let metadata = SessionMetadata::new("session");
    let err = persist_rebuilt_metadata(&target, &metadata)
        .expect_err("replacing a non-empty directory must fail");

    assert!(
        !err.to_string().is_empty(),
        "the replacement error must reach the caller"
    );
    assert!(target.join("occupied").exists());
}

/// After `list_sessions` rotates a corrupt metadata file aside, it
/// persists the rebuilt metadata so subsequent listings keep showing
/// the session instead of hiding it (no `.meta.json` left to match).
#[test]
fn list_sessions_persists_rebuilt_metadata_after_rotation() {
    let tmp = TempDir::new().unwrap();
    let sessions_dir = tmp.path();

    let mut recorder = SessionRecorder::new(sessions_dir).unwrap();
    let id = recorder.id().to_string();
    recorder
        .record_sent(&ToAgentMessage::Prompt {
            content: "Still here".to_string(),
            attachments: None,
            managed_inference_authorization: None,
        })
        .unwrap();
    recorder.flush().unwrap();
    drop(recorder);

    let meta_path = sessions_dir.join(format!("{id}.meta.json"));
    fs::write(&meta_path, "{\"id\":\"partial").unwrap();

    let first = list_sessions(sessions_dir).unwrap();
    assert!(first.iter().any(|meta| meta.id == id));
    assert!(
        meta_path.exists(),
        "rebuilt metadata must be persisted after rotation"
    );

    let second = list_sessions(sessions_dir).unwrap();
    let meta = second
        .iter()
        .find(|meta| meta.id == id)
        .expect("session must stay discoverable in subsequent listings");
    assert_eq!(meta.title.as_deref(), Some("Still here"));
    assert_eq!(meta.message_count, 1);
}

#[test]
fn test_session_metadata_usage() {
    let mut metadata = SessionMetadata::new("test");

    // Add some usage
    metadata.add_usage(&TokenUsage {
        input_tokens: 100,
        output_tokens: 200,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
        cost: None,
        total_tokens: None,
        model_id: None,
        provider: None,
    });

    metadata.add_usage(&TokenUsage {
        input_tokens: 150,
        output_tokens: 300,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
        cost: None,
        total_tokens: None,
        model_id: None,
        provider: None,
    });

    assert_eq!(metadata.total_input_tokens, 250);
    assert_eq!(metadata.total_output_tokens, 500);
}

#[test]
fn test_title_truncation() {
    let mut metadata = SessionMetadata::new("test");

    let long_message = "a".repeat(200);
    metadata.set_title_from_prompt(&long_message);

    assert!(metadata.title.as_ref().unwrap().len() <= 80);
    assert!(metadata.title.as_ref().unwrap().ends_with("..."));
}

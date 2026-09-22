use super::*;
use crate::agent::{ExecutionReceipt, ExecutionSource, ExecutionStatus, ToolReceiptDetails};
use maestro_runtime_contracts::tool_wire;

#[test]
fn headless_receipt_event_contains_record_lineage_and_status_only() {
    let message = FromAgentMessage::ManagedGatewayReceipt {
        request_id: "request-1".to_string(),
        record_id: "record-1".to_string(),
        lineage_id: "lineage-1".to_string(),
        record_status: "planned".to_string(),
        prompt_experiment: None,
    };

    let encoded = serde_json::to_value(message).expect("serialize managed Gateway receipt");
    assert_eq!(
        encoded,
        serde_json::json!({
            "type": "managed_gateway_receipt",
            "request_id": "request-1",
            "record_id": "record-1",
            "lineage_id": "lineage-1",
            "record_status": "planned"
        })
    );
}

#[test]
fn managed_gateway_receipt_decodes_as_known_and_rejects_malformed_payloads() {
    let raw = r#"{"type":"managed_gateway_receipt","request_id":"request-1","record_id":"record-1","lineage_id":"lineage-1","record_status":"planned"}"#;
    assert!(matches!(
        decode_from_agent_message(raw).expect("valid producer receipt"),
        maestro_runtime::TaggedMessageDecode::Known(FromAgentMessage::ManagedGatewayReceipt { record_id, .. })
            if record_id == "record-1"
    ));
    assert!(matches!(
        decode_from_agent_message(r#"{"type":"managed_gateway_receipt","request_id":"request-1"}"#),
        Err(maestro_runtime::TaggedMessageDecodeError::InvalidKnownMessage { .. })
    ));
}

#[test]
fn prompt_experiment_assignment_and_exposure_round_trip() {
    let assignment = PromptExperimentAssignment {
        experiment_id: "causal-debugging-v1".to_string(),
        assignment_id: "assignment-1".to_string(),
        arm: PromptExperimentArm::Candidate,
        artifact_id: "skill-causal-debugging".to_string(),
        artifact_version: "1".to_string(),
        artifact_sha256: format!("sha256:{}", "a".repeat(64)),
        artifact_content: "State the causal path.".to_string(),
    };
    let configured = serde_json::to_value(ToAgentMessage::ConfigurePromptExperiment {
        assignment: assignment.clone(),
    })
    .expect("serialize assignment");
    assert_eq!(configured["type"], "configure_prompt_experiment");
    assert_eq!(configured["assignment"]["arm"], "candidate");

    let receipt = FromAgentMessage::ManagedGatewayReceipt {
        request_id: "request-1".to_string(),
        record_id: "record-1".to_string(),
        lineage_id: "lineage-1".to_string(),
        record_status: "planned".to_string(),
        prompt_experiment: Some(PromptExperimentExposure {
            experiment_id: assignment.experiment_id,
            assignment_id: assignment.assignment_id,
            arm: assignment.arm,
            artifact_id: assignment.artifact_id,
            artifact_version: assignment.artifact_version,
            artifact_sha256: assignment.artifact_sha256,
            provider_prompt_sha256: format!("sha256:{}", "b".repeat(64)),
            applied: true,
        }),
    };
    let encoded = serde_json::to_value(receipt).expect("serialize exposure");
    assert_eq!(
        encoded["prompt_experiment"]["assignment_id"],
        "assignment-1"
    );
    assert_eq!(encoded["prompt_experiment"]["applied"], true);
}

#[test]
fn headless_prompt_rejects_malformed_managed_authorization_before_dispatch() {
    let message = ToAgentMessage::Prompt {
        content: "managed prompt".to_string(),
        attachments: None,
        managed_inference_authorization: Some(crate::agent::ManagedInferenceAuthorization::new(
            "signed\ncapability",
        )),
    };

    assert_eq!(
        message.validate_managed_inference_authorization(),
        Err("managedInferenceAuthorization contains control characters")
    );
}

#[test]
fn managed_gateway_receipt_survives_headless_state_conversion() {
    let event = AgentState::default().handle_message(FromAgentMessage::ManagedGatewayReceipt {
        request_id: "request-1".to_string(),
        record_id: "record-1".to_string(),
        lineage_id: "lineage-1".to_string(),
        record_status: "planned".to_string(),
        prompt_experiment: None,
    });

    assert!(matches!(
        event,
        Some(AgentEvent::ManagedGatewayReceipt {
            request_id,
            record_id,
            lineage_id,
            record_status,
        }) if request_id == "request-1"
            && record_id == "record-1"
            && lineage_id == "lineage-1"
            && record_status == "planned"
    ));
}

#[test]
fn delegation_event_round_trips_as_typed_headless_protocol_state() {
    let projection = maestro_runtime::DelegationEvent::from_subagent_lifecycle(
        "event-1",
        "delegation-1",
        1,
        "approval_required",
        Some("Approval is needed."),
        None,
    );
    let message = FromAgentMessage::DelegationEvent {
        event: projection.clone(),
    };
    let encoded = serde_json::to_value(&message).expect("delegation event serializes");
    assert_eq!(encoded["type"], "delegation_event");
    assert_eq!(encoded["event"]["lifecycleState"], "approval_required");
    assert_eq!(encoded["event"]["summary"], "Approval is needed.");

    let decoded =
        decode_from_agent_message(&serde_json::to_string(&message).expect("delegation event JSON"))
            .expect("delegation event decodes");
    assert!(matches!(
        decoded,
        maestro_runtime::TaggedMessageDecode::Known(FromAgentMessage::DelegationEvent { event })
            if event == projection
    ));

    let mut state = AgentState::default();
    let event = state.handle_message(message);
    assert!(matches!(
        event,
        Some(AgentEvent::DelegationEvent { event })
            if event.lifecycle_state == maestro_runtime::DelegationLifecycleState::ApprovalRequired
    ));
}

#[test]
fn parse_ready_message() {
    let json = r#"{"type":"ready","protocol_version":"2026-03-30","model":"claude-3-opus","provider":"anthropic","session_id":"sess_123"}"#;
    let msg: FromAgentMessage = serde_json::from_str(json).unwrap();
    match msg {
        FromAgentMessage::Ready {
            protocol_version,
            model,
            provider,
            session_id,
        } => {
            assert_eq!(protocol_version.as_deref(), Some("2026-03-30"));
            assert_eq!(model, "claude-3-opus");
            assert_eq!(provider, "anthropic");
            assert_eq!(session_id.as_deref(), Some("sess_123"));
        }
        _ => panic!("Expected Ready message"),
    }
}

#[test]
fn tool_end_serializes_typed_receipt_additively() {
    let message = FromAgentMessage::ToolEnd(tool_wire::ToolEnd {
        call_id: "call-1".to_string(),
        tool_execution_id: None,
        success: true,
        tool: Some("read".to_string()),
        details: None,
        receipt: Some(ExecutionReceipt {
            code_authority: None,
            call_id: "call-1".to_string(),
            tool_name: "read".to_string(),
            source: ExecutionSource::Native,
            status: ExecutionStatus::Succeeded,
            duration_ms: Some(4),
            policy: None,
            details: ToolReceiptDetails::None,
        }),
    });

    let json = serde_json::to_value(message).unwrap();
    assert_eq!(json["receipt"]["call_id"], "call-1");
    assert_eq!(json["receipt"]["source"], "native");
}

#[test]
fn indeterminate_receipt_survives_headless_wire_and_state() {
    let message = FromAgentMessage::ToolEnd(tool_wire::ToolEnd {
        call_id: "call-gh-write".to_string(),
        tool_execution_id: Some("execution-gh-write".to_string()),
        success: false,
        tool: Some("gh_issue".to_string()),
        details: None,
        receipt: Some(ExecutionReceipt {
            code_authority: None,
            call_id: "call-gh-write".to_string(),
            tool_name: "gh_issue".to_string(),
            source: ExecutionSource::Native,
            status: ExecutionStatus::Indeterminate,
            duration_ms: Some(2_000),
            policy: None,
            details: ToolReceiptDetails::None,
        }),
    });

    let encoded = serde_json::to_string(&message).expect("serialize indeterminate receipt");
    let decoded: FromAgentMessage =
        serde_json::from_str(&encoded).expect("deserialize indeterminate receipt");
    let event = AgentState::default().handle_message(decoded);

    assert!(matches!(
        event,
        Some(AgentEvent::ToolEnd {
            tool_execution_id: Some(tool_execution_id),
            receipt: Some(ExecutionReceipt {
                code_authority: None,
                status: ExecutionStatus::Indeterminate,
                ..
            }),
            ..
        }) if tool_execution_id == "execution-gh-write"
    ));
}

#[test]
fn tool_response_round_trips_governed_execution_id() {
    let message = ToAgentMessage::ToolResponse {
        call_id: "call-1".to_string(),
        tool_execution_id: Some("tool-execution-1".to_string()),
        approved: true,
        result: None,
    };

    let encoded = serde_json::to_value(&message).expect("serialize tool response");
    assert_eq!(encoded["tool_execution_id"], "tool-execution-1");

    let decoded: ToAgentMessage =
        serde_json::from_value(encoded).expect("deserialize tool response");
    assert!(matches!(
        decoded,
        ToAgentMessage::ToolResponse {
            call_id,
            tool_execution_id: Some(tool_execution_id),
            approved: true,
            result: None,
        } if call_id == "call-1" && tool_execution_id == "tool-execution-1"
    ));
}

#[test]
fn tool_response_omits_absent_execution_id() {
    let message = ToAgentMessage::ToolResponse {
        call_id: "call-1".to_string(),
        tool_execution_id: None,
        approved: false,
        result: None,
    };

    let encoded = serde_json::to_value(&message).expect("serialize tool response");
    assert_eq!(encoded.get("tool_execution_id"), None);
}

#[test]
fn state_preserves_tool_execution_id_on_tool_end_event() {
    let mut state = AgentState::default();

    let event = state.handle_message(FromAgentMessage::ToolEnd(tool_wire::ToolEnd {
        call_id: "call-1".to_string(),
        tool_execution_id: Some("tool-execution-1".to_string()),
        success: true,
        tool: Some("read".to_string()),
        details: None,
        receipt: None,
    }));

    assert!(matches!(
        event,
        Some(AgentEvent::ToolEnd {
            call_id,
            tool_execution_id: Some(tool_execution_id),
            success: true,
            ..
        }) if call_id == "call-1" && tool_execution_id == "tool-execution-1"
    ));
}

#[test]
fn parse_response_chunk() {
    let json =
        r#"{"type":"response_chunk","response_id":"abc","content":"Hello","is_thinking":false}"#;
    let msg: FromAgentMessage = serde_json::from_str(json).unwrap();
    match msg {
        FromAgentMessage::ResponseChunk {
            response_id,
            content,
            is_thinking,
        } => {
            assert_eq!(response_id, "abc");
            assert_eq!(content, "Hello");
            assert!(!is_thinking);
        }
        _ => panic!("Expected ResponseChunk message"),
    }
}

#[test]
fn parse_response_end_with_tools_summary() {
    let json = r#"{"type":"response_end","response_id":"abc","usage":{"input_tokens":1,"output_tokens":2,"cache_read_tokens":0,"cache_write_tokens":0,"total_tokens":3,"total_cost_usd":0.25,"model_id":"claude-sonnet","provider":"anthropic"},"tools_summary":{"tools_used":["read","bash"],"calls_succeeded":1,"calls_failed":1,"summary_labels":["Read package.json","Ran cargo test"]},"duration_ms":2500,"ttft_ms":120}"#;
    let msg: FromAgentMessage = serde_json::from_str(json).unwrap();
    match msg {
        FromAgentMessage::ResponseEnd {
            response_id,
            usage,
            tools_summary,
            duration_ms,
            ttft_ms,
            ..
        } => {
            assert_eq!(response_id, "abc");
            let usage = usage.expect("expected usage");
            assert_eq!(usage.total_tokens(), 3);
            assert_eq!(usage.cost, Some(0.25));
            assert_eq!(usage.model_id.as_deref(), Some("claude-sonnet"));
            assert_eq!(usage.provider.as_deref(), Some("anthropic"));
            let tools_summary = tools_summary.expect("expected tools summary");
            assert_eq!(tools_summary.tools_used, vec!["read", "bash"]);
            assert_eq!(tools_summary.calls_succeeded, 1);
            assert_eq!(tools_summary.calls_failed, 1);
            assert_eq!(duration_ms, Some(2500));
            assert_eq!(ttft_ms, Some(120));
            assert_eq!(
                tools_summary.summary_labels,
                vec!["Read package.json", "Ran cargo test"]
            );
        }
        _ => panic!("Expected ResponseEnd message"),
    }
}

#[test]
fn parse_compaction_message() {
    let json = r###"{"type":"compaction","summary":"## Conversation Summary","first_kept_entry_index":3,"tokens_before":9000,"auto":true,"timestamp":"2026-03-31T12:00:00Z"}"###;
    let msg: FromAgentMessage = serde_json::from_str(json).unwrap();
    match msg {
        FromAgentMessage::Compaction {
            first_kept_entry_index,
            tokens_before,
            auto,
            ..
        } => {
            assert_eq!(first_kept_entry_index, 3);
            assert_eq!(tokens_before, 9000);
            assert!(auto);
        }
        _ => panic!("Expected Compaction message"),
    }
}

#[test]
fn parse_server_request_message() {
    let json = r#"{"type":"server_request","request_id":"call_approval","request_type":"approval","call_id":"call_approval","tool":"bash","args":{"command":"git push --force"},"reason":"Force push requires approval"}"#;
    let msg: FromAgentMessage = serde_json::from_str(json).unwrap();
    match msg {
        FromAgentMessage::ServerRequest {
            request_id,
            request_type,
            call_id,
            tool,
            args,
            reason,
            ..
        } => {
            assert_eq!(request_id, "call_approval");
            assert_eq!(request_type, ServerRequestType::Approval);
            assert_eq!(call_id, "call_approval");
            assert_eq!(tool, "bash");
            assert_eq!(args["command"], "git push --force");
            assert_eq!(reason, "Force push requires approval");
        }
        _ => panic!("Expected ServerRequest message"),
    }
}

#[test]
fn parse_client_tool_server_request_message() {
    let json = r#"{"type":"server_request","request_id":"call_client","request_type":"client_tool","call_id":"call_client","tool":"artifacts","args":{"command":"create","filename":"report.txt"},"reason":"Client tool artifacts requires local execution"}"#;
    let msg: FromAgentMessage = serde_json::from_str(json).unwrap();
    match msg {
        FromAgentMessage::ServerRequest {
            request_id,
            request_type,
            call_id,
            tool,
            args,
            reason,
            ..
        } => {
            assert_eq!(request_id, "call_client");
            assert_eq!(request_type, ServerRequestType::ClientTool);
            assert_eq!(call_id, "call_client");
            assert_eq!(tool, "artifacts");
            assert_eq!(args["command"], "create");
            assert_eq!(reason, "Client tool artifacts requires local execution");
        }
        _ => panic!("Expected ServerRequest message"),
    }
}

#[test]
fn parse_user_input_server_request_message() {
    let json = r#"{"type":"server_request","request_id":"call_user_input","request_type":"user_input","call_id":"call_user_input","tool":"ask_user","args":{"questions":[{"header":"Stack","question":"Which schema library should we use?","options":[{"label":"Zod","description":"Use Zod schemas"}]}]},"reason":"Agent requested structured user input"}"#;
    let msg: FromAgentMessage = serde_json::from_str(json).unwrap();
    match msg {
        FromAgentMessage::ServerRequest {
            request_id,
            request_type,
            call_id,
            tool,
            args,
            reason,
            ..
        } => {
            assert_eq!(request_id, "call_user_input");
            assert_eq!(request_type, ServerRequestType::UserInput);
            assert_eq!(call_id, "call_user_input");
            assert_eq!(tool, "ask_user");
            assert_eq!(args["questions"][0]["header"], "Stack");
            assert_eq!(reason, "Agent requested structured user input");
        }
        _ => panic!("Expected ServerRequest message"),
    }
}

#[test]
fn parse_client_tool_request_message() {
    let json = r#"{"type":"client_tool_request","call_id":"call_client","tool":"artifacts","args":{"command":"create","filename":"report.txt"}}"#;
    let msg: FromAgentMessage = serde_json::from_str(json).unwrap();
    match msg {
        FromAgentMessage::ClientToolRequest {
            call_id,
            tool,
            args,
            ..
        } => {
            assert_eq!(call_id, "call_client");
            assert_eq!(tool, "artifacts");
            assert_eq!(args["command"], "create");
            assert_eq!(args["filename"], "report.txt");
        }
        _ => panic!("Expected ClientToolRequest message"),
    }
}

#[test]
fn parse_connection_info_message() {
    let json = r#"{"type":"connection_info","connection_id":"conn_remote","client_protocol_version":"2026-03-30","client_info":{"name":"maestro-web","version":"1.2.3"},"capabilities":{"server_requests":["approval","client_tool"]},"opt_out_notifications":["status","heartbeat"],"role":"controller","connection_count":1,"controller_connection_id":"conn_remote","connections":[{"connection_id":"conn_remote","role":"controller","client_protocol_version":"2026-03-30","client_info":{"name":"maestro-web","version":"1.2.3"},"capabilities":{"server_requests":["approval","client_tool"]},"opt_out_notifications":["status","heartbeat"],"subscription_count":1,"attached_subscription_count":1,"controller_lease_granted":true}]}"#;
    let msg: FromAgentMessage = serde_json::from_str(json).unwrap();
    match msg {
        FromAgentMessage::ConnectionInfo {
            connection_id,
            client_protocol_version,
            client_info,
            capabilities,
            opt_out_notifications,
            role,
            connection_count,
            controller_connection_id,
            connections,
            ..
        } => {
            assert_eq!(connection_id.as_deref(), Some("conn_remote"));
            assert_eq!(client_protocol_version.as_deref(), Some("2026-03-30"));
            assert_eq!(
                client_info.as_ref().map(|info| info.name.as_str()),
                Some("maestro-web")
            );
            assert_eq!(
                capabilities
                    .as_ref()
                    .and_then(|caps| caps.server_requests.as_ref())
                    .map(|caps| caps.len()),
                Some(2)
            );
            assert_eq!(
                opt_out_notifications.as_ref().map(|items| items.len()),
                Some(2)
            );
            assert_eq!(role, Some(ConnectionRole::Controller));
            assert_eq!(connection_count, Some(1));
            assert_eq!(controller_connection_id.as_deref(), Some("conn_remote"));
            assert_eq!(connections.as_ref().map(Vec::len), Some(1));
        }
        _ => panic!("Expected ConnectionInfo message"),
    }
}

#[test]
fn parse_hello_ok_message() {
    let json = r#"{"type":"hello_ok","protocol_version":"2026-08-01","connection_id":"conn_remote","client_protocol_version":"2026-03-30","client_info":{"name":"maestro-web","version":"1.2.3"},"capabilities":{"server_requests":["approval"]},"opt_out_notifications":["status"],"role":"controller","controller_connection_id":"conn_remote"}"#;
    let msg: FromAgentMessage = serde_json::from_str(json).unwrap();
    match msg {
        FromAgentMessage::HelloOk {
            protocol_version,
            connection_id,
            client_protocol_version,
            client_info,
            capabilities,
            server_capabilities,
            opt_out_notifications,
            role,
            controller_connection_id,
            lease_expires_at,
            controller_binding_version,
            controller_binding_sha256,
        } => {
            assert_eq!(controller_binding_version, None);
            assert_eq!(controller_binding_sha256, None);
            assert_eq!(protocol_version, "2026-08-01");
            assert_eq!(connection_id.as_deref(), Some("conn_remote"));
            assert_eq!(client_protocol_version.as_deref(), Some("2026-03-30"));
            assert_eq!(
                client_info.as_ref().map(|info| info.name.as_str()),
                Some("maestro-web")
            );
            assert_eq!(
                capabilities
                    .as_ref()
                    .and_then(|caps| caps.server_requests.as_ref())
                    .map(|caps| caps.len()),
                Some(1)
            );
            assert!(server_capabilities.is_none());
            assert_eq!(opt_out_notifications, Some(vec!["status".to_string()]));
            assert_eq!(role, Some(ConnectionRole::Controller));
            assert_eq!(controller_connection_id.as_deref(), Some("conn_remote"));
            assert!(lease_expires_at.is_none());
        }
        _ => panic!("Expected HelloOk message"),
    }
}

#[test]
fn hello_ok_keeps_client_and_server_capabilities_separate() {
    let message = FromAgentMessage::HelloOk {
        protocol_version: HEADLESS_PROTOCOL_VERSION.to_string(),
        controller_binding_version: None,
        controller_binding_sha256: None,
        connection_id: Some("conn_native".to_string()),
        client_protocol_version: Some("2026-08-08".to_string()),
        client_info: None,
        capabilities: Some(ClientCapabilities {
            server_requests: Some(vec![ServerRequestType::Approval]),
            ..ClientCapabilities::default()
        }),
        server_capabilities: Some(ServerCapabilities {
            native_tools: vec![NativeToolCapability {
                name: "bash".to_string(),
                requires_approval: true,
                version: Some("current".to_string()),
            }],
            ..ServerCapabilities::default()
        }),
        opt_out_notifications: None,
        role: Some(ConnectionRole::Controller),
        controller_connection_id: None,
        lease_expires_at: None,
    };

    let json = serde_json::to_value(message).expect("serialize hello acknowledgement");
    assert_eq!(json["capabilities"]["server_requests"][0], "approval");
    assert_eq!(
        json["server_capabilities"]["native_tools"][0]["name"],
        "bash"
    );
    assert_eq!(
        json["server_capabilities"]["native_tools"][0]["requires_approval"],
        true
    );
}

#[test]
fn hello_ok_tolerates_partial_and_unknown_server_capabilities() {
    let json = r#"{
        "type":"hello_ok",
        "protocol_version":"2026-08-08",
        "server_capabilities":{
            "server_requests":["approval","mcp_elicitation","future_request"],
            "native_tools":[]
        }
    }"#;
    let message: FromAgentMessage =
        serde_json::from_str(json).expect("parse hello acknowledgement");

    let FromAgentMessage::HelloOk {
        server_capabilities: Some(capabilities),
        ..
    } = message
    else {
        panic!("expected hello acknowledgement with server capabilities");
    };
    assert_eq!(
        capabilities.server_requests,
        vec![ServerRequestType::Approval]
    );
    assert!(capabilities.utility_operations.is_empty());
    assert!(!capabilities.raw_agent_events);
    assert!(capabilities.connection_roles.is_empty());
    assert!(capabilities.native_tools.is_empty());

    let empty_json = serde_json::to_value(ServerCapabilities::default())
        .expect("serialize empty server capabilities");
    assert_eq!(empty_json["native_tools"], serde_json::json!([]));
}

#[test]
fn parse_raw_agent_event_message() {
    let json = r#"{"type":"raw_agent_event","event_type":"status","event":{"type":"status","status":"Working","details":{}}}"#;
    let msg: FromAgentMessage = serde_json::from_str(json).unwrap();
    match msg {
        FromAgentMessage::RawAgentEvent { event_type, event } => {
            assert_eq!(event_type, "status");
            assert_eq!(event["type"], "status");
            assert_eq!(event["status"], "Working");
        }
        _ => panic!("Expected RawAgentEvent message"),
    }
}

#[test]
fn serialize_prompt_message() {
    let msg = ToAgentMessage::Prompt {
        content: "Hello".to_string(),
        attachments: None,
        managed_inference_authorization: None,
    };
    let json = serde_json::to_string(&msg).unwrap();
    assert!(json.contains(r#""type":"prompt""#));
    assert!(json.contains(r#""content":"Hello""#));
}

#[test]
fn serialize_init_message() {
    let msg = ToAgentMessage::Init {
        system_prompt: Some("You are Maestro".to_string()),
        append_system_prompt: None,
        thinking_level: Some(ThinkingLevel::High),
        approval_mode: Some(ApprovalMode::Prompt),
        history: None,
    };
    let json = serde_json::to_string(&msg).unwrap();
    assert!(json.contains(r#""type":"init""#));
    assert!(json.contains(r#""system_prompt":"You are Maestro""#));
    assert!(json.contains(r#""thinking_level":"high""#));
    assert!(json.contains(r#""approval_mode":"prompt""#));
}

#[test]
fn parse_init_message_with_history() {
    let json = r#"{
        "type": "init",
        "system_prompt": "You are Maestro",
        "history": [
            {"role": "user", "content": "What is 2+2?"},
            {"role": "assistant", "content": "4"},
            {"role": "system", "content": "Stay concise"}
        ]
    }"#;
    let msg: ToAgentMessage = serde_json::from_str(json).unwrap();
    match msg {
        ToAgentMessage::Init {
            system_prompt,
            append_system_prompt,
            thinking_level,
            approval_mode,
            history,
        } => {
            assert_eq!(system_prompt.as_deref(), Some("You are Maestro"));
            assert!(append_system_prompt.is_none());
            assert!(thinking_level.is_none());
            assert!(approval_mode.is_none());
            let history = history.expect("history present");
            assert_eq!(history.len(), 3);
            assert_eq!(history[0].role, HistoryRole::User);
            assert_eq!(history[0].content, "What is 2+2?");
            assert_eq!(history[1].role, HistoryRole::Assistant);
            assert_eq!(history[1].content, "4");
            assert_eq!(history[2].role, HistoryRole::System);
            assert_eq!(history[2].content, "Stay concise");
        }
        _ => panic!("Expected Init message"),
    }
}

#[test]
fn parse_init_message_without_history_defaults_none() {
    let json = r#"{"type":"init","system_prompt":"hi"}"#;
    let msg: ToAgentMessage = serde_json::from_str(json).unwrap();
    match msg {
        ToAgentMessage::Init { history, .. } => assert!(history.is_none()),
        _ => panic!("Expected Init message"),
    }
}

#[test]
fn serialize_init_message_with_history_roundtrip() {
    let msg = ToAgentMessage::Init {
        system_prompt: None,
        append_system_prompt: None,
        thinking_level: None,
        approval_mode: None,
        history: Some(vec![
            HistoryMessage {
                role: HistoryRole::User,
                content: "hello".to_string(),
            },
            HistoryMessage {
                role: HistoryRole::Assistant,
                content: "hi there".to_string(),
            },
        ]),
    };
    let json = serde_json::to_string(&msg).unwrap();
    assert!(json.contains(r#""type":"init""#));
    assert!(json.contains(r#""role":"user""#));
    assert!(json.contains(r#""content":"hello""#));
    assert!(!json.contains("system_prompt"));
    let parsed: ToAgentMessage = serde_json::from_str(&json).unwrap();
    match parsed {
        ToAgentMessage::Init { history, .. } => {
            let history = history.expect("history");
            assert_eq!(history.len(), 2);
            assert_eq!(history[1].role, HistoryRole::Assistant);
            assert_eq!(history[1].content, "hi there");
        }
        _ => panic!("Expected Init"),
    }
}

#[test]
fn history_message_converts_to_ai_message() {
    let seeded = HistoryMessage {
        role: HistoryRole::User,
        content: "prior turn".to_string(),
    };
    let ai = seeded.to_ai_message();
    assert!(matches!(ai.role, crate::ai::Role::User));
    assert_eq!(ai.content.as_text(), Some("prior turn"));

    let messages = history_to_ai_messages(Some(&[HistoryMessage {
        role: HistoryRole::Assistant,
        content: "ok".to_string(),
    }]));
    assert_eq!(messages.len(), 1);
    assert!(matches!(messages[0].role, crate::ai::Role::Assistant));
}

#[test]
fn serialize_hello_message() {
    let msg = ToAgentMessage::Hello {
        protocol_version: Some(HEADLESS_PROTOCOL_VERSION.to_string()),
        client_info: Some(ClientInfo {
            name: "maestro-tui-rs".to_string(),
            version: Some("0.1.0".to_string()),
        }),
        capabilities: Some(ClientCapabilities {
            server_requests: Some(vec![ServerRequestType::Approval]),
            utility_operations: Some(vec![UtilityOperation::CommandExec]),
            raw_agent_events: Some(true),
            transcript_grade: None,
            governed_code_mode: None,
        }),
        role: Some(ConnectionRole::Controller),
        opt_out_notifications: Some(vec!["status".to_string()]),
        controller_binding: None,
    };
    let json = serde_json::to_string(&msg).unwrap();
    assert!(json.contains(r#""type":"hello""#));
    assert!(json.contains(&format!(
        r#""protocol_version":"{}""#,
        HEADLESS_PROTOCOL_VERSION
    )));
    assert!(json.contains(r#""name":"maestro-tui-rs""#));
    assert!(json.contains(r#""role":"controller""#));
    assert!(json.contains(r#""opt_out_notifications":["status"]"#));
}

#[test]
fn serialize_server_request_response_message() {
    let msg = ToAgentMessage::ServerRequestResponse {
        request_id: "call_user_input".to_string(),
        request_type: ServerRequestType::UserInput,
        approved: None,
        result: None,
        content: Some(vec![ClientToolResultContent::Text {
            text: "Use Zod".to_string(),
        }]),
        is_error: Some(false),
        decision_action: None,
        reason: None,
    };
    let json = serde_json::to_string(&msg).unwrap();
    assert!(json.contains(r#""type":"server_request_response""#));
    assert!(json.contains(r#""request_id":"call_user_input""#));
    assert!(json.contains(r#""request_type":"user_input""#));
}

#[test]
fn serialize_utility_command_start_message_with_stdin() {
    let msg = ToAgentMessage::UtilityCommandStart {
        command_id: "cmd_stdin".to_string(),
        command: "cat".to_string(),
        cwd: None,
        env: None,
        shell_mode: Some(UtilityCommandShellMode::Direct),
        terminal_mode: Some(UtilityCommandTerminalMode::Pipe),
        allow_stdin: Some(true),
        columns: None,
        rows: None,
    };
    let json = serde_json::to_string(&msg).unwrap();
    assert!(json.contains(r#""type":"utility_command_start""#));
    assert!(json.contains(r#""command_id":"cmd_stdin""#));
    assert!(json.contains(r#""allow_stdin":true"#));
}

#[test]
fn serialize_utility_command_stdin_message() {
    let msg = ToAgentMessage::UtilityCommandStdin {
        command_id: "cmd_stdin".to_string(),
        content: "hello maestro".to_string(),
        eof: Some(true),
    };
    let json = serde_json::to_string(&msg).unwrap();
    assert!(json.contains(r#""type":"utility_command_stdin""#));
    assert!(json.contains(r#""command_id":"cmd_stdin""#));
    assert!(json.contains(r#""content":"hello maestro""#));
    assert!(json.contains(r#""eof":true"#));
}

#[test]
fn serialize_utility_command_resize_message() {
    let msg = ToAgentMessage::UtilityCommandResize {
        command_id: "cmd_stdin".to_string(),
        columns: 120,
        rows: 40,
    };
    let json = serde_json::to_string(&msg).unwrap();
    assert!(json.contains(r#""type":"utility_command_resize""#));
    assert!(json.contains(r#""columns":120"#));
    assert!(json.contains(r#""rows":40"#));
}

#[test]
fn serialize_utility_file_search_message() {
    let msg = ToAgentMessage::UtilityFileSearch {
        search_id: "search_src".to_string(),
        query: "headless".to_string(),
        cwd: Some("/tmp/project".to_string()),
        limit: Some(25),
    };
    let json = serde_json::to_string(&msg).unwrap();
    assert!(json.contains(r#""type":"utility_file_search""#));
    assert!(json.contains(r#""search_id":"search_src""#));
    assert!(json.contains(r#""query":"headless""#));
    assert!(json.contains(r#""limit":25"#));
}

#[test]
fn serialize_utility_file_read_message() {
    let msg = ToAgentMessage::UtilityFileRead {
        read_id: "read_src".to_string(),
        path: "src/headless/mod.rs".to_string(),
        cwd: Some("/tmp/project".to_string()),
        offset: Some(25),
        limit: Some(40),
    };
    let json = serde_json::to_string(&msg).unwrap();
    assert!(json.contains(r#""type":"utility_file_read""#));
    assert!(json.contains(r#""read_id":"read_src""#));
    assert!(json.contains(r#""path":"src/headless/mod.rs""#));
    assert!(json.contains(r#""offset":25"#));
    assert!(json.contains(r#""limit":40"#));
}

#[test]
fn parse_utility_file_watch_event_message() {
    let json = r#"{"type":"utility_file_watch_event","watch_id":"watch_src","change_type":"modify","path":"/tmp/project/src/app.ts","relative_path":"src/app.ts","timestamp":1234,"is_directory":false}"#;
    let msg: FromAgentMessage = serde_json::from_str(json).unwrap();
    match msg {
        FromAgentMessage::UtilityFileWatchEvent {
            watch_id,
            change_type,
            path,
            relative_path,
            timestamp,
            is_directory,
        } => {
            assert_eq!(watch_id, "watch_src");
            assert_eq!(change_type, UtilityFileWatchChangeType::Modify);
            assert_eq!(path, "/tmp/project/src/app.ts");
            assert_eq!(relative_path, "src/app.ts");
            assert_eq!(timestamp, 1234);
            assert!(!is_directory);
        }
        _ => panic!("Expected UtilityFileWatchEvent message"),
    }
}

#[test]
fn parse_utility_file_read_result_message() {
    let json = r#"{"type":"utility_file_read_result","read_id":"read_src","path":"/tmp/project/src/main.rs","relative_path":"src/main.rs","cwd":"/tmp/project","content":"fn main() {}","start_line":1,"end_line":1,"total_lines":1,"truncated":false}"#;
    let msg: FromAgentMessage = serde_json::from_str(json).unwrap();
    match msg {
        FromAgentMessage::UtilityFileReadResult {
            read_id,
            path,
            relative_path,
            cwd,
            content,
            start_line,
            end_line,
            total_lines,
            truncated,
        } => {
            assert_eq!(read_id, "read_src");
            assert_eq!(path, "/tmp/project/src/main.rs");
            assert_eq!(relative_path, "src/main.rs");
            assert_eq!(cwd, "/tmp/project");
            assert_eq!(content, "fn main() {}");
            assert_eq!(start_line, 1);
            assert_eq!(end_line, 1);
            assert_eq!(total_lines, 1);
            assert!(!truncated);
        }
        _ => panic!("Expected UtilityFileReadResult message"),
    }
}

#[test]
fn state_handles_response_stream() {
    let mut state = AgentState::default();
    state.handle_message(FromAgentMessage::Ready {
        protocol_version: Some("2026-03-30".to_string()),
        model: "claude-3-opus".to_string(),
        provider: "anthropic".to_string(),
        session_id: Some("sess_123".to_string()),
    });

    assert_eq!(state.protocol_version.as_deref(), Some("2026-03-30"));
    assert_eq!(state.session_id.as_deref(), Some("sess_123"));

    // Start response
    state.handle_message(FromAgentMessage::ResponseStart {
        response_id: "resp1".to_string(),
    });
    assert!(state.is_responding);
    assert!(state.current_response.is_some());

    // Add chunks
    state.handle_message(FromAgentMessage::ResponseChunk {
        response_id: "resp1".to_string(),
        content: "Hello ".to_string(),
        is_thinking: false,
    });
    state.handle_message(FromAgentMessage::ResponseChunk {
        response_id: "resp1".to_string(),
        content: "world".to_string(),
        is_thinking: false,
    });

    assert_eq!(state.current_response.as_ref().unwrap().text, "Hello world");

    // End response
    state.handle_message(FromAgentMessage::ResponseEnd {
        response_id: "resp1".to_string(),
        usage: None,
        tools_summary: Some(ResponseToolsSummary {
            tools_used: vec!["read".to_string()],
            calls_succeeded: 1,
            calls_failed: 0,
            summary_labels: vec!["Read package.json".to_string()],
        }),
        duration_ms: Some(2300),
        ttft_ms: Some(150),
    });
    assert!(state.is_responding);
    assert!(state.current_response.is_none());
    assert_eq!(state.last_response_duration_ms, Some(2300));
    assert_eq!(state.last_ttft_ms, Some(150));

    state.handle_message(FromAgentMessage::TurnCompleted {
        response_id: "done".to_string(),
        coding_completion: None,
        coding_child_records: Vec::new(),
    });
    assert!(!state.is_responding);
}

#[test]
fn state_tracks_structured_errors() {
    let mut state = AgentState::default();
    state.handle_message(FromAgentMessage::ResponseStart {
        response_id: "resp1".to_string(),
    });
    let event = state.handle_message(FromAgentMessage::Error {
        request_id: Some("read_missing".to_string()),
        message: "Cancelled by user".to_string(),
        fatal: false,
        terminal: true,
        error_type: Some(HeadlessErrorType::Cancelled),
    });

    assert_eq!(state.last_error.as_deref(), Some("Cancelled by user"));
    assert_eq!(state.last_error_type, Some(HeadlessErrorType::Cancelled));
    assert!(!state.is_responding);
    assert!(state.current_response.is_none());
    assert!(matches!(
        event,
        Some(AgentEvent::Error {
            request_id: Some(ref request_id),
            terminal: true,
            error_type: Some(HeadlessErrorType::Cancelled),
            ..
        }) if request_id == "read_missing"
    ));

    state.handle_message(FromAgentMessage::ResponseStart {
        response_id: "resp2".to_string(),
    });
    state.handle_message(FromAgentMessage::Error {
        request_id: None,
        message: "Recoverable warning".to_string(),
        fatal: false,
        terminal: false,
        error_type: Some(HeadlessErrorType::Transient),
    });
    assert!(state.is_responding);
    assert_eq!(
        state
            .current_response
            .as_ref()
            .map(|response| response.response_id.as_str()),
        Some("resp2")
    );
}

#[test]
fn state_tracks_and_clears_file_watches() {
    let mut state = AgentState::default();
    state.handle_message(FromAgentMessage::UtilityCommandStarted {
        command_id: "cmd_owned".to_string(),
        command: "echo hi".to_string(),
        cwd: Some("/tmp/project".to_string()),
        shell_mode: UtilityCommandShellMode::Direct,
        terminal_mode: UtilityCommandTerminalMode::Pipe,
        pid: Some(42),
        columns: None,
        rows: None,
        owner_connection_id: Some("conn_owned".to_string()),
    });
    state.handle_message(FromAgentMessage::UtilityFileWatchStarted {
        watch_id: "watch_src".to_string(),
        root_dir: "/tmp/project".to_string(),
        include_patterns: Some(vec!["src/**".to_string()]),
        exclude_patterns: Some(vec!["dist/**".to_string()]),
        debounce_ms: 50,
        owner_connection_id: Some("conn_owned".to_string()),
    });

    assert_eq!(state.active_utility_commands.len(), 1);
    assert_eq!(
        state
            .active_utility_commands
            .get("cmd_owned")
            .and_then(|command| command.owner_connection_id.as_deref()),
        Some("conn_owned")
    );
    assert_eq!(state.active_file_watches.len(), 1);
    assert_eq!(
        state
            .active_file_watches
            .get("watch_src")
            .map(|watch| watch.root_dir.as_str()),
        Some("/tmp/project")
    );
    assert_eq!(
        state
            .active_file_watches
            .get("watch_src")
            .and_then(|watch| watch.owner_connection_id.as_deref()),
        Some("conn_owned")
    );

    state.handle_message(FromAgentMessage::UtilityFileWatchStopped {
        watch_id: "watch_src".to_string(),
        reason: Some("Stopped by controller".to_string()),
    });

    assert!(state.active_file_watches.is_empty());
}

#[test]
fn state_updates_active_utility_command_dimensions_after_resize() {
    let mut state = AgentState::default();
    state.handle_message(FromAgentMessage::UtilityCommandStarted {
        command_id: "cmd_pty".to_string(),
        command: "node app.js".to_string(),
        cwd: Some("/tmp/project".to_string()),
        shell_mode: UtilityCommandShellMode::Direct,
        terminal_mode: UtilityCommandTerminalMode::Pty,
        pid: Some(321),
        columns: Some(90),
        rows: Some(30),
        owner_connection_id: Some("conn_pty".to_string()),
    });

    state.handle_message(FromAgentMessage::UtilityCommandResized {
        command_id: "cmd_pty".to_string(),
        columns: 120,
        rows: 40,
    });

    let command = state
        .active_utility_commands
        .get("cmd_pty")
        .expect("active utility command");
    assert_eq!(command.terminal_mode, UtilityCommandTerminalMode::Pty);
    assert_eq!(command.columns, Some(120));
    assert_eq!(command.rows, Some(40));
    assert_eq!(command.owner_connection_id.as_deref(), Some("conn_pty"));
}

#[test]
fn state_caps_active_utility_command_output() {
    let mut state = AgentState::default();
    state.handle_message(FromAgentMessage::UtilityCommandStarted {
        command_id: "cmd_cap".to_string(),
        command: "node app.js".to_string(),
        cwd: Some("/tmp/project".to_string()),
        shell_mode: UtilityCommandShellMode::Direct,
        terminal_mode: UtilityCommandTerminalMode::Pipe,
        pid: Some(321),
        columns: None,
        rows: None,
        owner_connection_id: None,
    });

    state.handle_message(FromAgentMessage::UtilityCommandOutput {
        command_id: "cmd_cap".to_string(),
        stream: UtilityCommandStream::Stdout,
        content: "a".repeat(HEADLESS_OUTPUT_LIMIT),
    });
    state.handle_message(FromAgentMessage::UtilityCommandOutput {
        command_id: "cmd_cap".to_string(),
        stream: UtilityCommandStream::Stdout,
        content: "bcdef".to_string(),
    });

    let command = state
        .active_utility_commands
        .get("cmd_cap")
        .expect("active utility command");
    assert_eq!(command.output.len(), HEADLESS_OUTPUT_LIMIT);
    assert!(command.output.ends_with("bcdef"));
}

#[test]
fn state_handles_compaction_event() {
    let mut state = AgentState::default();
    let event = state.handle_message(FromAgentMessage::Compaction {
        summary: "## Conversation Summary".to_string(),
        first_kept_entry_index: 2,
        tokens_before: 7000,
        auto: false,
        custom_instructions: None,
        continuation: None,
        timestamp: "2026-03-31T12:00:00Z".to_string(),
    });

    assert!(matches!(
        event,
        Some(AgentEvent::Compaction {
            first_kept_entry_index,
            tokens_before,
            auto,
            ..
        }) if first_kept_entry_index == 2 && tokens_before == 7000 && !auto
    ));
}

#[test]
fn explicit_turn_terminals_close_an_unfinished_headless_response() {
    for terminal in [
        FromAgentMessage::TurnCompleted {
            response_id: "done".to_string(),
            coding_completion: None,
            coding_child_records: Vec::new(),
        },
        FromAgentMessage::TurnInterrupted {
            response_id: "done".to_string(),
            reason: "cancelled".to_string(),
        },
        FromAgentMessage::ProviderError {
            kind: maestro_ai::ProviderStreamErrorKind::TransientProtocol,
            message: "unexpected eof".to_string(),
        },
    ] {
        let mut state = AgentState::default();
        state.handle_message(FromAgentMessage::ResponseStart {
            response_id: "resp-1".to_string(),
        });
        assert!(state.is_responding);
        assert!(state.current_response.is_some());

        assert!(state.handle_message(terminal).is_some());
        assert!(!state.is_responding);
        assert!(state.current_response.is_none());
    }
}

#[test]
fn state_preserves_tool_name_for_nonapproval_runs() {
    let mut state = AgentState::default();

    let tool_call = state.handle_message(FromAgentMessage::ToolCall {
        call_id: "call_read".to_string(),
        tool_execution_id: None,
        tool: "read".to_string(),
        args: serde_json::json!({ "file_path": "package.json" }),
        requires_approval: false,
    });
    assert!(matches!(
        tool_call,
        Some(AgentEvent::ToolCall { ref tool, .. }) if tool == "read"
    ));

    let tool_start = state.handle_message(FromAgentMessage::ToolStart {
        call_id: "call_read".to_string(),
    });
    assert!(matches!(
        tool_start,
        Some(AgentEvent::ToolStart { ref tool, .. }) if tool == "read"
    ));
    assert_eq!(
        state
            .active_tools
            .get("call_read")
            .map(|tool| tool.tool.as_str()),
        Some("read")
    );
}

#[test]
fn state_tracks_and_clears_server_request_approvals() {
    let mut state = AgentState::default();

    let event = state.handle_message(FromAgentMessage::ServerRequest {
        request_id: "call_approval".to_string(),
        request_type: ServerRequestType::Approval,
        call_id: "call_approval".to_string(),
        tool_execution_id: None,
        tool: "bash".to_string(),
        args: serde_json::json!({ "command": "git push --force" }),
        reason: "Force push requires approval".to_string(),
        started_at_ms: Some(1_771_000_000_000),
    });

    assert!(event.is_none());
    assert_eq!(state.pending_approvals.len(), 1);
    assert_eq!(state.pending_approvals[0].tool, "bash");
    assert!(state.tracked_tools.contains_key("call_approval"));

    let resolved = state.handle_message(FromAgentMessage::ServerRequestResolved {
        request_id: "call_approval".to_string(),
        request_type: ServerRequestType::Approval,
        call_id: "call_approval".to_string(),
        resolution: ServerRequestResolutionStatus::Denied,
        reason: Some("Denied by user".to_string()),
        resolved_by: ServerRequestResolvedBy::User,
        started_at_ms: Some(1_771_000_000_000),
        resolved_at_ms: Some(1_771_000_000_123),
    });

    assert!(resolved.is_none());
    assert!(state.pending_approvals.is_empty());
    assert!(!state.tracked_tools.contains_key("call_approval"));
}

#[test]
fn state_tracks_connection_metadata_from_hello_and_connection_info() {
    let mut state = AgentState::default();

    state.handle_sent_message(&ToAgentMessage::Hello {
        protocol_version: Some(HEADLESS_PROTOCOL_VERSION.to_string()),
        client_info: Some(ClientInfo {
            name: "maestro-tui-rs".to_string(),
            version: Some("0.1.0".to_string()),
        }),
        capabilities: Some(ClientCapabilities {
            server_requests: Some(vec![ServerRequestType::Approval]),
            utility_operations: Some(vec![UtilityOperation::CommandExec]),
            raw_agent_events: None,
            transcript_grade: None,
            governed_code_mode: None,
        }),
        role: Some(ConnectionRole::Controller),
        opt_out_notifications: Some(vec!["status".to_string()]),
        controller_binding: None,
    });
    let event = state.handle_message(FromAgentMessage::ConnectionInfo {
        connection_id: Some("conn_remote".to_string()),
        client_protocol_version: Some(HEADLESS_PROTOCOL_VERSION.to_string()),
        client_info: Some(ClientInfo {
            name: "maestro-web".to_string(),
            version: Some("1.2.3".to_string()),
        }),
        capabilities: Some(ClientCapabilities {
            server_requests: Some(vec![
                ServerRequestType::Approval,
                ServerRequestType::ClientTool,
            ]),
            utility_operations: Some(vec![UtilityOperation::CommandExec]),
            raw_agent_events: Some(true),
            transcript_grade: None,
            governed_code_mode: None,
        }),
        opt_out_notifications: Some(vec!["status".to_string(), "connection_info".to_string()]),
        role: Some(ConnectionRole::Viewer),
        connection_count: Some(1),
        controller_connection_id: Some("conn_remote".to_string()),
        lease_expires_at: None,
        connections: Some(vec![ConnectionState {
            connection_id: "conn_remote".to_string(),
            role: ConnectionRole::Viewer,
            client_protocol_version: Some(HEADLESS_PROTOCOL_VERSION.to_string()),
            client_info: Some(ClientInfo {
                name: "maestro-web".to_string(),
                version: Some("1.2.3".to_string()),
            }),
            capabilities: Some(ClientCapabilities {
                server_requests: Some(vec![
                    ServerRequestType::Approval,
                    ServerRequestType::ClientTool,
                ]),
                utility_operations: Some(vec![UtilityOperation::CommandExec]),
                raw_agent_events: Some(true),
                transcript_grade: None,
                governed_code_mode: None,
            }),
            opt_out_notifications: Some(vec!["status".to_string(), "connection_info".to_string()]),
            subscription_count: 1,
            attached_subscription_count: 1,
            controller_lease_granted: false,
            lease_expires_at: None,
        }]),
    });

    assert!(event.is_none());
    assert_eq!(
        state.client_protocol_version.as_deref(),
        Some(HEADLESS_PROTOCOL_VERSION)
    );
    assert_eq!(
        state.client_info.as_ref().map(|info| info.name.as_str()),
        Some("maestro-web")
    );
    assert_eq!(state.connection_role, Some(ConnectionRole::Viewer));
    assert_eq!(state.connection_count, 1);
    assert_eq!(
        state
            .opt_out_notifications
            .as_ref()
            .map(|items| items.len()),
        Some(2)
    );
    assert_eq!(
        state.controller_connection_id.as_deref(),
        Some("conn_remote")
    );
    assert_eq!(state.connections.len(), 1);
    assert_eq!(
        state
            .capabilities
            .as_ref()
            .and_then(|caps| caps.server_requests.as_ref())
            .map(|caps| caps.len()),
        Some(2)
    );
}

#[test]
fn state_emits_raw_agent_events() {
    let mut state = AgentState::default();
    let event = state.handle_message(FromAgentMessage::RawAgentEvent {
        event_type: "status".to_string(),
        event: serde_json::json!({
            "type": "status",
            "status": "Working",
            "details": {},
        }),
    });

    match event {
        Some(AgentEvent::RawAgentEvent { event_type, event }) => {
            assert_eq!(event_type, "status");
            assert_eq!(event["status"], "Working");
        }
        _ => panic!("Expected raw agent event"),
    }
}

#[test]
fn state_tracks_protocol_version_from_hello_ok() {
    let mut state = AgentState::default();

    let event = state.handle_message(FromAgentMessage::HelloOk {
        protocol_version: HEADLESS_PROTOCOL_VERSION.to_string(),
        controller_binding_version: None,
        controller_binding_sha256: None,
        connection_id: Some("conn_remote".to_string()),
        client_protocol_version: Some("2026-08-01".to_string()),
        client_info: Some(ClientInfo {
            name: "maestro-web".to_string(),
            version: Some("1.2.3".to_string()),
        }),
        capabilities: Some(ClientCapabilities {
            server_requests: Some(vec![ServerRequestType::Approval]),
            utility_operations: Some(vec![UtilityOperation::FileRead]),
            raw_agent_events: None,
            transcript_grade: None,
            governed_code_mode: None,
        }),
        server_capabilities: Some(ServerCapabilities {
            native_tools: vec![NativeToolCapability {
                name: "bash".to_string(),
                requires_approval: true,
                version: Some("current".to_string()),
            }],
            ..ServerCapabilities::default()
        }),
        opt_out_notifications: Some(vec!["connection_info".to_string()]),
        role: Some(ConnectionRole::Controller),
        controller_connection_id: Some("conn_remote".to_string()),
        lease_expires_at: None,
    });

    assert!(event.is_none());
    assert_eq!(
        state.protocol_version.as_deref(),
        Some(HEADLESS_PROTOCOL_VERSION)
    );
    assert_eq!(state.client_protocol_version.as_deref(), Some("2026-08-01"));
    assert_eq!(state.connection_role, Some(ConnectionRole::Controller));
    assert_eq!(
        state.controller_connection_id.as_deref(),
        Some("conn_remote")
    );
    assert_eq!(
        state
            .server_capabilities
            .as_ref()
            .and_then(|capabilities| capabilities.native_tools.first())
            .map(|tool| (
                tool.name.as_str(),
                tool.requires_approval,
                tool.version.as_deref()
            )),
        Some(("bash", true, Some("current")))
    );
}

#[test]
fn state_tracks_and_clears_client_tool_requests() {
    let mut state = AgentState::default();

    let event = state.handle_message(FromAgentMessage::ClientToolRequest {
        call_id: "call_client".to_string(),
        tool_execution_id: None,
        tool: "artifacts".to_string(),
        args: serde_json::json!({ "command": "create", "filename": "report.txt" }),
    });

    assert!(event.is_none());
    assert_eq!(state.pending_client_tools.len(), 1);
    assert_eq!(state.pending_client_tools[0].tool, "artifacts");
    assert!(state.tracked_tools.contains_key("call_client"));

    state.handle_sent_message(&ToAgentMessage::ClientToolResult {
        call_id: "call_client".to_string(),
        content: vec![ClientToolResultContent::Text {
            text: "created".to_string(),
        }],
        is_error: false,
    });

    assert!(state.pending_client_tools.is_empty());
    assert!(state.tracked_tools.contains_key("call_client"));
}

#[test]
fn state_tracks_and_clears_generic_client_tool_server_requests() {
    let mut state = AgentState::default();

    let event = state.handle_message(FromAgentMessage::ServerRequest {
        request_id: "call_client".to_string(),
        request_type: ServerRequestType::ClientTool,
        call_id: "call_client".to_string(),
        tool_execution_id: None,
        tool: "artifacts".to_string(),
        args: serde_json::json!({ "command": "create", "filename": "report.txt" }),
        reason: "Client tool artifacts requires local execution".to_string(),
        started_at_ms: Some(1_771_000_000_000),
    });

    assert!(event.is_none());
    assert_eq!(state.pending_client_tools.len(), 1);
    assert_eq!(state.pending_client_tools[0].tool, "artifacts");
    assert!(state.tracked_tools.contains_key("call_client"));

    let resolved = state.handle_message(FromAgentMessage::ServerRequestResolved {
        request_id: "call_client".to_string(),
        request_type: ServerRequestType::ClientTool,
        call_id: "call_client".to_string(),
        resolution: ServerRequestResolutionStatus::Completed,
        reason: None,
        resolved_by: ServerRequestResolvedBy::Client,
        started_at_ms: Some(1_771_000_000_000),
        resolved_at_ms: Some(1_771_000_000_123),
    });

    assert!(resolved.is_none());
    assert!(state.pending_client_tools.is_empty());
    assert!(state.tracked_tools.contains_key("call_client"));
}

#[test]
fn state_tracks_and_clears_user_input_requests() {
    let mut state = AgentState::default();

    let event = state.handle_message(FromAgentMessage::ClientToolRequest {
        call_id: "call_user_input".to_string(),
        tool_execution_id: None,
        tool: "ask_user".to_string(),
        args: serde_json::json!({
            "questions": [{
                "header": "Stack",
                "question": "Which schema library should we use?",
                "options": [{
                    "label": "Zod",
                    "description": "Use Zod schemas"
                }]
            }]
        }),
    });

    assert!(event.is_none());
    assert_eq!(state.pending_user_inputs.len(), 1);
    assert_eq!(state.pending_user_inputs[0].tool, "ask_user");
    assert!(state.tracked_tools.contains_key("call_user_input"));

    let resolved = state.handle_message(FromAgentMessage::ServerRequestResolved {
        request_id: "call_user_input".to_string(),
        request_type: ServerRequestType::UserInput,
        call_id: "call_user_input".to_string(),
        resolution: ServerRequestResolutionStatus::Answered,
        reason: None,
        resolved_by: ServerRequestResolvedBy::Client,
        started_at_ms: Some(1_771_000_000_000),
        resolved_at_ms: Some(1_771_000_000_123),
    });

    assert!(resolved.is_none());
    assert!(state.pending_user_inputs.is_empty());
    assert!(state.tracked_tools.contains_key("call_user_input"));
}

#[test]
fn state_clears_user_input_on_sent_generic_server_request_response() {
    let mut state = AgentState::default();

    state.handle_message(FromAgentMessage::ServerRequest {
        request_id: "call_user_input".to_string(),
        request_type: ServerRequestType::UserInput,
        call_id: "call_user_input".to_string(),
        tool_execution_id: None,
        tool: "ask_user".to_string(),
        args: serde_json::json!({
            "questions": [{
                "header": "Stack",
                "question": "Which schema library should we use?",
                "options": [{
                    "label": "Zod",
                    "description": "Use Zod schemas"
                }]
            }]
        }),
        reason: "Agent requested structured user input".to_string(),
        started_at_ms: Some(1_771_000_000_000),
    });

    state.handle_sent_message(&ToAgentMessage::ServerRequestResponse {
        request_id: "call_user_input".to_string(),
        request_type: ServerRequestType::UserInput,
        approved: None,
        result: None,
        content: Some(vec![ClientToolResultContent::Text {
            text: "Use Zod".to_string(),
        }]),
        is_error: Some(false),
        decision_action: None,
        reason: None,
    });

    assert!(state.pending_user_inputs.is_empty());
    assert!(state.tracked_tools.contains_key("call_user_input"));
}

#[test]
fn state_tracks_and_clears_tool_retry_requests_by_request_id() {
    let mut state = AgentState::default();

    state.handle_message(FromAgentMessage::ToolCall {
        call_id: "call_bash".to_string(),
        tool_execution_id: None,
        tool: "bash".to_string(),
        args: serde_json::json!({ "command": "ls" }),
        requires_approval: false,
    });

    let event = state.handle_message(FromAgentMessage::ServerRequest {
        request_id: "retry_1".to_string(),
        request_type: ServerRequestType::ToolRetry,
        call_id: "call_bash".to_string(),
        tool_execution_id: None,
        tool: "bash".to_string(),
        args: serde_json::json!({
            "tool_call_id": "call_bash",
            "args": { "command": "ls" },
            "error_message": "Command failed",
            "attempt": 1
        }),
        reason: "Retry bash command".to_string(),
        started_at_ms: Some(1_771_000_000_000),
    });

    assert!(event.is_none());
    assert_eq!(state.pending_tool_retries.len(), 1);
    assert_eq!(state.pending_tool_retries[0].call_id, "call_bash");
    assert_eq!(
        state.pending_tool_retries[0].request_id.as_deref(),
        Some("retry_1")
    );
    assert_eq!(
        state
            .tracked_tools
            .get("call_bash")
            .and_then(|tool| tool.args.get("command"))
            .and_then(serde_json::Value::as_str),
        Some("ls")
    );

    state.handle_sent_message(&ToAgentMessage::ServerRequestResponse {
        request_id: "retry_1".to_string(),
        request_type: ServerRequestType::ToolRetry,
        approved: None,
        result: None,
        content: None,
        is_error: None,
        decision_action: Some(ToolRetryDecisionAction::Retry),
        reason: Some("Try again".to_string()),
    });

    assert!(state.pending_tool_retries.is_empty());
    assert!(state.tracked_tools.contains_key("call_bash"));
}

#[test]
fn state_clears_tracked_client_tool_on_cancelled_server_request() {
    let mut state = AgentState::default();

    state.handle_message(FromAgentMessage::ServerRequest {
        request_id: "call_client".to_string(),
        request_type: ServerRequestType::ClientTool,
        call_id: "call_client".to_string(),
        tool_execution_id: None,
        tool: "artifacts".to_string(),
        args: serde_json::json!({ "command": "create", "filename": "report.txt" }),
        reason: "Client tool artifacts requires local execution".to_string(),
        started_at_ms: Some(1_771_000_000_000),
    });

    let resolved = state.handle_message(FromAgentMessage::ServerRequestResolved {
        request_id: "call_client".to_string(),
        request_type: ServerRequestType::ClientTool,
        call_id: "call_client".to_string(),
        resolution: ServerRequestResolutionStatus::Cancelled,
        reason: Some("Interrupted before request completed".to_string()),
        resolved_by: ServerRequestResolvedBy::Runtime,
        started_at_ms: Some(1_771_000_000_000),
        resolved_at_ms: Some(1_771_000_000_123),
    });

    assert!(resolved.is_none());
    assert!(state.pending_client_tools.is_empty());
    assert!(!state.tracked_tools.contains_key("call_client"));
}

#[test]
fn state_preserves_codex_subagent_edges_across_runtime_reset_messages() {
    let mut state = AgentState::default();
    let edge = CodexSubagentContinuityEdge {
        spawn_tool_call_id: Some("collab-spawn-reset".to_string()),
        spawn_tool_execution_id: None,
        wait_tool_call_id: None,
        wait_tool_execution_id: None,
        child_run_id: Some("agent-run-child-reset".to_string()),
        thread_id: Some("child-thread-reset".to_string()),
        operation: "spawn_agent".to_string(),
        status: "waiting_for_restore".to_string(),
    };
    state.codex_subagent_edges = vec![edge.clone()];

    state.clear_pending_request_state();
    assert_eq!(state.codex_subagent_edges, vec![edge.clone()]);

    state.handle_sent_message(&ToAgentMessage::Interrupt);
    assert_eq!(state.codex_subagent_edges, vec![edge.clone()]);

    state.handle_sent_message(&ToAgentMessage::Cancel);
    assert_eq!(state.codex_subagent_edges, vec![edge.clone()]);

    state.handle_sent_message(&ToAgentMessage::Shutdown);
    assert_eq!(state.codex_subagent_edges, vec![edge]);
}

#[test]
fn state_marks_denied_restored_codex_subagent_edges_failed_without_tracked_source() {
    let mut state = AgentState {
        codex_subagent_edges: vec![CodexSubagentContinuityEdge {
            spawn_tool_call_id: Some("collab-spawn-denied".to_string()),
            spawn_tool_execution_id: None,
            wait_tool_call_id: None,
            wait_tool_execution_id: None,
            child_run_id: Some("agent-run-child-denied".to_string()),
            thread_id: Some("child-thread-denied".to_string()),
            operation: "spawn_agent".to_string(),
            status: "waiting_for_restore".to_string(),
        }],
        ..Default::default()
    };

    let resolved = state.handle_message(FromAgentMessage::ServerRequestResolved {
        request_id: "approval-denied".to_string(),
        request_type: ServerRequestType::Approval,
        call_id: "collab-spawn-denied".to_string(),
        resolution: ServerRequestResolutionStatus::Denied,
        reason: Some("Denied by policy".to_string()),
        resolved_by: ServerRequestResolvedBy::Policy,
        started_at_ms: Some(1_771_000_000_000),
        resolved_at_ms: Some(1_771_000_000_123),
    });

    assert!(resolved.is_none());
    assert_eq!(
        state.codex_subagent_edges,
        vec![CodexSubagentContinuityEdge {
            spawn_tool_call_id: Some("collab-spawn-denied".to_string()),
            spawn_tool_execution_id: None,
            wait_tool_call_id: None,
            wait_tool_execution_id: None,
            child_run_id: Some("agent-run-child-denied".to_string()),
            thread_id: Some("child-thread-denied".to_string()),
            operation: "spawn_agent".to_string(),
            status: "failed".to_string(),
        }]
    );
}

#[test]
fn state_uses_codex_subagent_tool_end_details_for_child_targets() {
    let mut state = AgentState::default();

    state.handle_message(FromAgentMessage::ToolCall {
        call_id: "collab-spawn-complete".to_string(),
        tool_execution_id: Some("texec-collab-spawn-complete".to_string()),
        tool: "codex.subagent.spawnAgent".to_string(),
        args: serde_json::json!({ "receiverThreadIds": [] }),
        requires_approval: false,
    });
    state.handle_message(FromAgentMessage::ToolEnd(tool_wire::ToolEnd {
        call_id: "collab-spawn-complete".to_string(),
        tool_execution_id: None,
        success: true,
        tool: Some("codex.subagent.spawnAgent".to_string()),
        details: Some(serde_json::json!({
            "receiverThreadIds": ["child-thread-complete"],
            "childRunIds": ["agent-run-child-complete"],
            "codexWorkGraph": {
                "schemaVersion": CODEX_SUBAGENT_WORK_GRAPH_SCHEMA,
                "childRuns": [{
                    "threadId": "child-thread-complete",
                    "childRunId": "agent-run-child-complete",
                    "operation": "spawnAgent"
                }]
            },
            "prompt": "Sensitive child task prompt"
        })),
        receipt: None,
    }));

    assert_eq!(
        state.codex_subagent_edges,
        vec![CodexSubagentContinuityEdge {
            spawn_tool_call_id: Some("collab-spawn-complete".to_string()),
            spawn_tool_execution_id: Some("texec-collab-spawn-complete".to_string()),
            wait_tool_call_id: None,
            wait_tool_execution_id: None,
            child_run_id: Some("agent-run-child-complete".to_string()),
            thread_id: Some("child-thread-complete".to_string()),
            operation: "spawn_agent".to_string(),
            status: "spawned".to_string(),
        }]
    );
    let encoded = serde_json::to_string(&state.codex_subagent_edges).unwrap();
    assert!(!encoded.contains("Sensitive child task prompt"));
}

#[test]
fn state_keeps_governed_codex_subagent_id_on_partial_server_request_update() {
    let mut state = AgentState::default();
    let args = serde_json::json!({
        "codexWorkGraph": {
            "schemaVersion": CODEX_SUBAGENT_WORK_GRAPH_SCHEMA,
            "childRuns": [{
                "threadId": "child-thread-governed",
                "childRunId": "agent-run-child-governed",
                "operation": "spawnAgent"
            }]
        }
    });

    state.handle_message(FromAgentMessage::ToolCall {
        call_id: "collab-spawn-governed".to_string(),
        tool_execution_id: Some("texec-spawn-governed".to_string()),
        tool: "codex.subagent.spawnAgent".to_string(),
        args: args.clone(),
        requires_approval: false,
    });
    state.handle_message(FromAgentMessage::ServerRequest {
        request_id: "approval-spawn-governed".to_string(),
        request_type: ServerRequestType::Approval,
        call_id: "collab-spawn-governed".to_string(),
        tool_execution_id: None,
        tool: "codex.subagent.spawnAgent".to_string(),
        args,
        reason: "Policy approval required".to_string(),
        started_at_ms: None,
    });

    assert_eq!(
        state.codex_subagent_edges,
        vec![CodexSubagentContinuityEdge {
            spawn_tool_call_id: Some("collab-spawn-governed".to_string()),
            spawn_tool_execution_id: Some("texec-spawn-governed".to_string()),
            wait_tool_call_id: None,
            wait_tool_execution_id: None,
            child_run_id: Some("agent-run-child-governed".to_string()),
            thread_id: Some("child-thread-governed".to_string()),
            operation: "spawn_agent".to_string(),
            status: "waiting_for_restore".to_string(),
        }]
    );
    assert_eq!(
        state
            .pending_approvals
            .first()
            .and_then(|pending| pending.tool_execution_id.as_deref()),
        Some("texec-spawn-governed")
    );
}

#[test]
fn state_persists_governed_codex_subagent_id_from_retry_request() {
    let mut state = AgentState::default();
    let args = serde_json::json!({
        "codexWorkGraph": {
            "schemaVersion": CODEX_SUBAGENT_WORK_GRAPH_SCHEMA,
            "childRuns": [{
                "threadId": "child-thread-retry-governed",
                "childRunId": "agent-run-child-retry-governed",
                "operation": "spawnAgent"
            }]
        }
    });

    state.handle_message(FromAgentMessage::ToolCall {
        call_id: "collab-spawn-retry-governed".to_string(),
        tool_execution_id: None,
        tool: "codex.subagent.spawnAgent".to_string(),
        args: args.clone(),
        requires_approval: false,
    });
    state.handle_message(FromAgentMessage::ServerRequest {
        request_id: "retry-spawn-governed".to_string(),
        request_type: ServerRequestType::ToolRetry,
        call_id: "collab-spawn-retry-governed".to_string(),
        tool_execution_id: Some("texec-spawn-retry-governed".to_string()),
        tool: "codex.subagent.spawnAgent".to_string(),
        args,
        reason: "Retry governed spawn".to_string(),
        started_at_ms: None,
    });

    assert_eq!(
        state
            .tracked_tools
            .get("collab-spawn-retry-governed")
            .and_then(|pending| pending.tool_execution_id.as_deref()),
        Some("texec-spawn-retry-governed")
    );

    state.handle_message(FromAgentMessage::ToolEnd(tool_wire::ToolEnd {
        call_id: "collab-spawn-retry-governed".to_string(),
        tool_execution_id: None,
        success: true,
        tool: None,
        details: None,
        receipt: None,
    }));

    assert_eq!(
        state.codex_subagent_edges,
        vec![CodexSubagentContinuityEdge {
            spawn_tool_call_id: Some("collab-spawn-retry-governed".to_string()),
            spawn_tool_execution_id: Some("texec-spawn-retry-governed".to_string()),
            wait_tool_call_id: None,
            wait_tool_execution_id: None,
            child_run_id: Some("agent-run-child-retry-governed".to_string()),
            thread_id: Some("child-thread-retry-governed".to_string()),
            operation: "spawn_agent".to_string(),
            status: "spawned".to_string(),
        }]
    );
}

#[test]
fn state_preserves_codex_subagent_child_target_status_from_work_graph_edges() {
    let mut state = AgentState::default();

    state.handle_message(FromAgentMessage::ToolCall {
        call_id: "collab-spawn-status".to_string(),
        tool_execution_id: None,
        tool: "codex.subagent.spawnAgent".to_string(),
        args: serde_json::json!({
            "codexWorkGraph": {
                "schemaVersion": CODEX_SUBAGENT_WORK_GRAPH_SCHEMA,
                "childRuns": [{
                    "edgeId": "collab-spawn-status:0:spawnAgent:agent-run-child-status",
                    "targetIndex": 0,
                    "threadId": "child-thread-status",
                    "childRunId": "agent-run-child-status",
                    "operation": "spawnAgent",
                    "status": "running"
                }]
            },
            "prompt": "Sensitive child task prompt"
        }),
        requires_approval: false,
    });

    assert_eq!(
        state.codex_subagent_edges,
        vec![CodexSubagentContinuityEdge {
            spawn_tool_call_id: Some("collab-spawn-status".to_string()),
            spawn_tool_execution_id: None,
            wait_tool_call_id: None,
            wait_tool_execution_id: None,
            child_run_id: Some("agent-run-child-status".to_string()),
            thread_id: Some("child-thread-status".to_string()),
            operation: "spawn_agent".to_string(),
            status: "running".to_string(),
        }]
    );
    let encoded = serde_json::to_string(&state.codex_subagent_edges).unwrap();
    assert!(!encoded.contains("Sensitive child task prompt"));
}

#[test]
fn codex_subagent_terminal_statuses_keep_spawned_and_resumed_active() {
    assert!(codex_subagent_status_is_terminal("acknowledged"));
    assert!(codex_subagent_status_is_terminal("Acknowledged"));
    assert!(codex_subagent_status_is_terminal("completed"));
    assert!(codex_subagent_status_is_terminal("closed"));
    assert!(!codex_subagent_status_is_terminal("spawned"));
    assert!(!codex_subagent_status_is_terminal("Spawned"));
    assert!(!codex_subagent_status_is_terminal("resumed"));
    assert!(!codex_subagent_status_is_terminal("reSumed"));
}

#[test]
fn hello_accepts_every_client_version_this_build_serves() {
    for version in SUPPORTED_CLIENT_PROTOCOL_VERSIONS {
        assert!(client_protocol_version_is_supported(Some(version)));
    }
    assert!(
        client_protocol_version_is_supported(None),
        "a client that announces nothing keeps the pre-negotiation behavior"
    );
    assert!(
        SUPPORTED_CLIENT_PROTOCOL_VERSIONS.contains(&HEADLESS_PROTOCOL_VERSION),
        "this build must accept its own protocol version"
    );
}

#[test]
fn hello_rejects_protocol_versions_this_build_does_not_speak() {
    for version in [
        "",
        "2025-01-01",
        "2026-04-02",
        "2026-08-01",
        "2026-08-07",
        "2027-01-01",
        "latest",
        " 2026-08-07",
    ] {
        assert!(
            !client_protocol_version_is_supported(Some(version)),
            "{version:?} is not a version this build serves"
        );
        let message = unsupported_client_protocol_version_message(version);
        assert!(message.contains(HEADLESS_PROTOCOL_VERSION));
    }
}

#[test]
fn coding_completion_survives_wire_and_headless_event_translation() {
    let wire = serde_json::json!({
        "type": "turn_completed", "response_id": "response-1",
        "coding_completion": {
            "taskId": "task-1", "workId": "work-1", "repositoryId": "repository-1",
            "contractDigest": "digest-1", "generation": 1, "revision": "revision-1",
            "implementationSessionId": "session-1", "commands": [], "readiness": [],
            "review": null, "behavior": null, "handoffItems": []
        },
        "coding_child_records": [{
            "organizationId": "org-1", "workspaceId": "workspace-1", "workId": "work-1",
            "parentSessionId": "session-1", "childId": "child-1", "sessionId": "child-session",
            "role": "review", "revision": "revision-1", "completedSuccessfully": true,
            "reportDigest": "report-1"
        }]
    });
    let message: FromAgentMessage = serde_json::from_value(wire.clone()).unwrap();
    assert_eq!(serde_json::to_value(&message).unwrap(), wire);
    let event = AgentState::default().handle_message(message).unwrap();
    let AgentEvent::TurnCompleted {
        coding_completion,
        coding_child_records,
        ..
    } = event
    else {
        panic!("expected terminal event");
    };
    assert_eq!(coding_completion.unwrap().work_id, "work-1");
    assert_eq!(coding_child_records[0].child_id, "child-1");
    let legacy: FromAgentMessage = serde_json::from_value(serde_json::json!({
        "type": "turn_completed", "response_id": "legacy"
    }))
    .unwrap();
    assert!(matches!(legacy, FromAgentMessage::TurnCompleted {
        coding_completion: None, coding_child_records, ..
    } if coding_child_records.is_empty()));
}

#[test]
fn process_budget_checkpoint_round_trips_without_becoming_a_turn_terminal() {
    let wire = serde_json::json!({
        "type":"process_budget_checkpoint",
        "budget": {
            "limits":{"event_id":"event-1","max_requests":2,"max_total_tokens":10,
                "max_cost_micros":20,"cost_micros_per_token":2},
            "requests":1,"tool_calls":0,"total_tokens":5,"cost_micros":10,"awaiting_usage":false,"tool_costs":{}
        }
    });
    let message: FromAgentMessage = serde_json::from_value(wire.clone()).unwrap();
    assert!(message.terminal_event().is_none());
    assert_eq!(serde_json::to_value(&message).unwrap(), wire);
    assert!(AgentState::default().handle_message(message).is_none());
}

#[test]
fn governed_payloads_round_trip_across_runtime_and_platform_envelopes() {
    use tool_wire::{ToolClientMessage, ToolServerMessage};
    let request = serde_json::json!({
        "type":"governed_client_tool_request", "call_id":"call-shared",
        "tool_execution_id":"execution-shared", "tool":"test.read", "args":{},
        "provider_tool_name":"read", "tool_id":"read", "client_instance_id":"client",
        "grant_id":"grant", "grant_version":1, "grant_hash":"hash", "turn_digest":"turn",
        "definition_digest":"definition", "args_digest":"args", "owner_lease_epoch":2,
        "idempotency_key":"result"
    });
    let runtime: FromAgentMessage = serde_json::from_value(request.clone()).unwrap();
    let platform: ToolServerMessage =
        serde_json::from_value(serde_json::to_value(runtime).unwrap()).unwrap();
    assert_eq!(serde_json::to_value(platform).unwrap(), request);
    let mut result = request;
    for field in ["tool", "args", "provider_tool_name", "tool_id"] {
        result.as_object_mut().unwrap().remove(field);
    }
    result["type"] = serde_json::json!("governed_client_tool_result");
    result["is_error"] = serde_json::json!(false);
    result["content"] = serde_json::json!([{"type":"text","text":"result"}]);
    let platform: ToolClientMessage = serde_json::from_value(result.clone()).unwrap();
    let runtime: ToAgentMessage =
        serde_json::from_value(serde_json::to_value(platform).unwrap()).unwrap();
    assert_eq!(serde_json::to_value(runtime).unwrap(), result);
    let ack = serde_json::json!({"type":"tool_end","call_id":"call-shared", "success":true});
    let runtime: FromAgentMessage = serde_json::from_value(ack.clone()).unwrap();
    let platform: ToolServerMessage =
        serde_json::from_value(serde_json::to_value(runtime).unwrap()).unwrap();
    assert_eq!(serde_json::to_value(platform).unwrap(), ack);
}

#[test]
fn acceptance_round_trips_from_native_producer_to_shared_contract() {
    let wire = serde_json::json!({"type":"response_accepted", "request_id":"execution-1"});
    let runtime: FromAgentMessage = serde_json::from_value(wire.clone()).unwrap();
    let shared: tool_wire::ToolServerMessage =
        serde_json::from_value(serde_json::to_value(runtime).unwrap()).unwrap();
    assert!(
        matches!(&shared, tool_wire::ToolServerMessage::ResponseAccepted(ack) if ack.request_id == "execution-1")
    );
    assert_eq!(serde_json::to_value(shared).unwrap(), wire);
}

#[test]
fn thinking_levels_match_the_generated_protocol_constant() {
    // HEADLESS_THINKING_LEVELS is the wire contract mirrored from
    // proto/maestro/v1/headless.proto. Nothing checked it against the enum
    // before, which is how the enum and the proto could drift apart.
    use crate::headless::generated_protocol::HEADLESS_THINKING_LEVELS;

    let encoded: Vec<String> = [
        ThinkingLevel::Off,
        ThinkingLevel::Minimal,
        ThinkingLevel::Low,
        ThinkingLevel::Medium,
        ThinkingLevel::High,
        ThinkingLevel::XHigh,
        ThinkingLevel::Ultra,
    ]
    .into_iter()
    .map(|level| {
        serde_json::to_value(level)
            .unwrap()
            .as_str()
            .unwrap()
            .to_owned()
    })
    .collect();

    let mut declared: Vec<String> = HEADLESS_THINKING_LEVELS
        .iter()
        .map(|value| (*value).to_owned())
        .collect();
    let mut encoded_sorted = encoded.clone();
    declared.sort();
    encoded_sorted.sort();
    assert_eq!(
        encoded_sorted, declared,
        "every ThinkingLevel variant must appear in HEADLESS_THINKING_LEVELS"
    );

    // snake_case would spell this "x_high"; the wire name is "xhigh".
    assert!(encoded.contains(&"xhigh".to_owned()));
    assert_eq!(
        serde_json::from_value::<ThinkingLevel>(serde_json::json!("xhigh")).unwrap(),
        ThinkingLevel::XHigh
    );
}

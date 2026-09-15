#![allow(clippy::all)]
#![allow(dead_code)]
#![allow(missing_docs)]

// Generated protobuf types for the headless protocol live under a separate
// namespace so they can coexist with the current serde-based JSON transport.
pub mod maestro {
    pub mod v1 {
        include!(concat!(env!("OUT_DIR"), "/maestro.v1.rs"));
    }
}

#[cfg(test)]
mod tests {
    use prost::Message;
    use std::collections::BTreeSet;

    use crate::headless::messages::{
        PromptExperimentArm, PromptExperimentAssignment, ResponseToolsSummary,
        ServerRequestResolutionStatus, ServerRequestResolvedBy, ToolRetryDecisionAction,
        UtilityCommandTerminalMode, UtilityFileSearchMatch, UtilityFileWatchChangeType,
    };
    use crate::headless::workspace_capabilities::{
        ApplyWorkspaceCapabilitySet, WorkspaceCapabilitySetApplied, WorkspacePromptCapability,
    };
    use crate::headless::{
        ClientToolResultContent, FromAgentMessage, GovernedToolGrant as RuntimeGovernedToolGrant,
        HeadlessErrorType, ServerRequestType, ToAgentMessage, TokenUsage, ToolResult,
        UtilityCommandShellMode, UtilityCommandStream,
    };

    use super::maestro::v1::from_agent_envelope::Payload as FromPayload;
    use super::maestro::v1::to_agent_envelope::Payload;
    use super::maestro::v1::{
        ApplyWorkspaceCapabilitySetMessage, CodeMode, FromAgentEnvelope, GovernedInitMessage,
        GovernedToolGrant as ProtoGovernedToolGrant, HelloMessage, NativeToolCapability,
        ProviderErrorMessage, ProviderStreamErrorKind, ResponseAcceptedMessage, ServerCapabilities,
        ToAgentEnvelope, ToolEndMessage, ToolResponseMessage, TurnCompletedMessage,
        TurnInterruptedMessage,
    };

    fn test_grant() -> RuntimeGovernedToolGrant {
        RuntimeGovernedToolGrant {
            process_budget: None,
            process_system_prompt: None,
            envelope_version: 2,
            grant_id: "grant-1".into(),
            grant_version: 1,
            issuer: "platform".into(),
            audience: "maestro".into(),
            organization_id: "org-1".into(),
            workspace_id: "workspace-1".into(),
            thread_id: "thread-1".into(),
            turn_id: "turn-1".into(),
            run_id: "run-1".into(),
            runtime_generation: 1,
            grant_epoch: 1,
            issued_at_ms: 1,
            not_before_ms: 1,
            expires_at_ms: 2,
            grant_hash: "hash".into(),
            signing_key_id: "key".into(),
            grant_signature: "signature".into(),
            identity_authorization: None,
            native_tool_ids: vec!["read".into()],
            external_tools: vec![],
            connection_bindings: vec![],
        }
    }

    fn live_to_agent_messages() -> Vec<ToAgentMessage> {
        vec![
            ToAgentMessage::Hello {
                protocol_version: Some("2026-08-08".into()),
                client_info: None,
                capabilities: None,
                role: None,
                opt_out_notifications: None,
                controller_binding: None,
            },
            ToAgentMessage::Init {
                system_prompt: None,
                append_system_prompt: None,
                thinking_level: None,
                approval_mode: None,
                history: None,
            },
            ToAgentMessage::GovernedInit {
                system_prompt: None,
                append_system_prompt: None,
                thinking_level: None,
                approval_mode: None,
                history: None,
                code_mode: crate::headless::CodeMode::GovernedCode,
                tool_grant: test_grant(),
            },
            ToAgentMessage::Prompt {
                content: "prompt".into(),
                attachments: None,
                managed_inference_authorization: None,
            },
            ToAgentMessage::GovernedPrompt {
                content: "governed prompt".into(),
                attachments: None,
                code_mode: crate::headless::CodeMode::GovernedCode,
                tool_grant: test_grant(),
                managed_inference_authorization: None,
            },
            ToAgentMessage::GovernedSteer {
                content: "governed steer".into(),
                attachments: None,
                code_mode: crate::headless::CodeMode::GovernedCode,
                tool_grant: test_grant(),
                managed_inference_authorization: None,
            },
            ToAgentMessage::ApplyWorkspaceCapabilitySet {
                request: ApplyWorkspaceCapabilitySet {
                    organization_id: "org-1".into(),
                    workspace_id: "workspace-1".into(),
                    runner_session_id: "runner-1".into(),
                    runtime_generation: 1,
                    activation_generation: 1,
                    workspace_snapshot_digest: "sha256:snapshot".into(),
                    workspace_skill_set_digest: "sha256:skills".into(),
                    capability_set_digest: "sha256:set".into(),
                    workspace_instructions: vec!["Use the selected workspace guidance.".into()],
                    admitted_catalog: vec![WorkspacePromptCapability {
                        qualified_id: "workspace.review".into(),
                        name: "review".into(),
                        scope: "workspace".into(),
                        revision_digest: "sha256:revision".into(),
                        body_digest: "sha256:body".into(),
                        trigger_patterns: vec!["review".into()],
                        user_invocable: true,
                        pinned_prompt_only: true,
                        title: "Review".into(),
                        description: "Review workspace changes.".into(),
                        instructions: vec!["Apply the review checklist.".into()],
                        body: "Apply the review checklist.".into(),
                        entry_digest: "sha256:entry".into(),
                    }],
                    admission_receipt_id: "admission-1".into(),
                },
            },
            ToAgentMessage::ConfigurePromptExperiment {
                assignment: PromptExperimentAssignment {
                    experiment_id: "experiment-1".into(),
                    assignment_id: "assignment-1".into(),
                    arm: PromptExperimentArm::Candidate,
                    artifact_id: "skill-1".into(),
                    artifact_version: "v1".into(),
                    artifact_sha256: format!("sha256:{}", "0".repeat(64)),
                    artifact_content: "Use the focused skill.".into(),
                },
            },
            ToAgentMessage::ManagedAuthorizationResult {
                request_id: "invocation-1".into(),
                authorization: maestro_runtime_contracts::ManagedInferenceAuthorization::new(
                    "opaque-capability",
                ),
            },
            ToAgentMessage::Interrupt,
            ToAgentMessage::ToolResponse {
                call_id: "call-1".into(),
                tool_execution_id: Some("execution-1".into()),
                approved: true,
                result: Some(ToolResult::default()),
            },
            ToAgentMessage::ClientToolResult {
                call_id: "call-1".into(),
                content: vec![ClientToolResultContent::Text { text: "ok".into() }],
                is_error: false,
            },
            ToAgentMessage::GovernedClientToolResult {
                process_tool_cost_micros: None,
                call_id: "call-1".into(),
                content: vec![],
                is_error: false,
                tool_execution_id: "execution-1".into(),
                client_instance_id: "client-1".into(),
                grant_id: "grant-1".into(),
                grant_version: 1,
                grant_hash: "hash".into(),
                turn_digest: "turn-digest".into(),
                definition_digest: "definition-digest".into(),
                args_digest: "args-digest".into(),
                owner_lease_epoch: 1,
                idempotency_key: "idempotency-1".into(),
            },
            ToAgentMessage::ServerRequestResponse {
                request_id: "request-1".into(),
                request_type: ServerRequestType::Approval,
                approved: Some(true),
                result: Some(ToolResult::default()),
                content: Some(vec![]),
                is_error: Some(false),
                decision_action: Some(ToolRetryDecisionAction::Skip),
                reason: Some("done".into()),
            },
            ToAgentMessage::UtilityCommandStart {
                command_id: "command-1".into(),
                command: "true".into(),
                cwd: None,
                env: None,
                shell_mode: Some(UtilityCommandShellMode::Direct),
                terminal_mode: Some(UtilityCommandTerminalMode::Pipe),
                allow_stdin: Some(false),
                columns: Some(80),
                rows: Some(24),
            },
            ToAgentMessage::UtilityCommandTerminate {
                command_id: "command-1".into(),
                force: Some(false),
            },
            ToAgentMessage::UtilityCommandStdin {
                command_id: "command-1".into(),
                content: "input".into(),
                eof: Some(true),
            },
            ToAgentMessage::UtilityCommandResize {
                command_id: "command-1".into(),
                columns: 80,
                rows: 24,
            },
            ToAgentMessage::UtilityFileSearch {
                search_id: "search-1".into(),
                query: "README".into(),
                cwd: None,
                limit: Some(10),
            },
            ToAgentMessage::UtilityFileRead {
                read_id: "read-1".into(),
                path: "README.md".into(),
                cwd: None,
                offset: Some(0),
                limit: Some(10),
            },
            ToAgentMessage::UtilityFileWatchStart {
                watch_id: "watch-1".into(),
                root_dir: None,
                include_patterns: Some(vec!["**/*.rs".into()]),
                exclude_patterns: Some(vec!["target/**".into()]),
                debounce_ms: Some(10),
            },
            ToAgentMessage::UtilityFileWatchStop {
                watch_id: "watch-1".into(),
            },
            ToAgentMessage::Cancel,
            ToAgentMessage::Shutdown,
            ToAgentMessage::RestoreConversation {
                protocol_version: "restore-v1".into(),
                messages: vec![],
            },
            ToAgentMessage::Steer {
                content: "steer".into(),
                attachments: None,
                managed_inference_authorization: None,
            },
        ]
    }

    fn assert_live_to_variant_is_enumerated(message: &ToAgentMessage) {
        match message {
            ToAgentMessage::Hello { .. }
            | ToAgentMessage::Init { .. }
            | ToAgentMessage::GovernedInit { .. }
            | ToAgentMessage::RestoreConversation { .. }
            | ToAgentMessage::Prompt { .. }
            | ToAgentMessage::GovernedPrompt { .. }
            | ToAgentMessage::Steer { .. }
            | ToAgentMessage::GovernedSteer { .. }
            | ToAgentMessage::ApplyWorkspaceCapabilitySet { .. }
            | ToAgentMessage::ConfigurePromptExperiment { .. }
            | ToAgentMessage::Interrupt
            | ToAgentMessage::ToolResponse { .. }
            | ToAgentMessage::ClientToolResult { .. }
            | ToAgentMessage::GovernedClientToolResult { .. }
            | ToAgentMessage::ManagedAuthorizationResult { .. }
            | ToAgentMessage::ServerRequestResponse { .. }
            | ToAgentMessage::UtilityCommandStart { .. }
            | ToAgentMessage::UtilityCommandTerminate { .. }
            | ToAgentMessage::UtilityCommandStdin { .. }
            | ToAgentMessage::UtilityCommandResize { .. }
            | ToAgentMessage::UtilityFileSearch { .. }
            | ToAgentMessage::UtilityFileRead { .. }
            | ToAgentMessage::UtilityFileWatchStart { .. }
            | ToAgentMessage::UtilityFileWatchStop { .. }
            | ToAgentMessage::Cancel
            | ToAgentMessage::Shutdown => {}
        }
    }

    fn live_from_agent_messages() -> Vec<FromAgentMessage> {
        vec![
            FromAgentMessage::ManagedAuthorizationRequest {
                request_id: "invocation-1".into(),
            },
            FromAgentMessage::WorkspaceCapabilitySetApplied {
                receipt: WorkspaceCapabilitySetApplied {
                    schema_version: "evalops.maestro.workspace-prompt-capability-set.v1".into(),
                    organization_id: "org-1".into(),
                    workspace_id: "workspace-1".into(),
                    runner_session_id: "runner-1".into(),
                    runtime_generation: 1,
                    activation_generation: 1,
                    effective_catalog_digest: "sha256:capabilities".into(),
                    accepted_entry_digests: vec!["sha256:capability".into()],
                    rejected_entries: vec![],
                    replay_cursor: "1:sha256:capabilities".into(),
                    applied_at: 1,
                    controller_binding_sha256: "sha256:binding".into(),
                    provider_prompt_sha256: "sha256:prompt".into(),
                    staged_for_next_turn: false,
                    idempotent: false,
                },
            },
            FromAgentMessage::HelloOk {
                protocol_version: "2026-08-08".into(),
                controller_binding_version: None,
                controller_binding_sha256: None,
                connection_id: None,
                client_protocol_version: None,
                client_info: None,
                capabilities: None,
                server_capabilities: None,
                opt_out_notifications: None,
                role: None,
                controller_connection_id: None,
                lease_expires_at: None,
            },
            FromAgentMessage::Ready {
                protocol_version: Some("2026-08-08".into()),
                model: "model".into(),
                provider: "provider".into(),
                session_id: Some("session-1".into()),
            },
            FromAgentMessage::ResponseStart {
                response_id: "response-1".into(),
            },
            FromAgentMessage::ResponseChunk {
                response_id: "response-1".into(),
                content: "chunk".into(),
                is_thinking: false,
            },
            FromAgentMessage::ResponseEnd {
                response_id: "response-1".into(),
                usage: Some(TokenUsage::default()),
                tools_summary: Some(ResponseToolsSummary::default()),
                duration_ms: Some(1),
                ttft_ms: Some(1),
            },
            FromAgentMessage::ToolCall {
                call_id: "call-1".into(),
                tool_execution_id: Some("execution-1".into()),
                tool: "read".into(),
                args: serde_json::json!({"path":"README.md"}),
                requires_approval: false,
            },
            FromAgentMessage::ToolStart {
                call_id: "call-1".into(),
            },
            FromAgentMessage::ToolOutput {
                call_id: "call-1".into(),
                content: "output".into(),
            },
            FromAgentMessage::ToolEnd {
                call_id: "call-1".into(),
                tool_execution_id: Some("execution-1".into()),
                success: true,
                tool: Some("read".into()),
                details: None,
                receipt: None,
            },
            FromAgentMessage::ClientToolRequest {
                call_id: "call-1".into(),
                tool_execution_id: Some("execution-1".into()),
                tool: "browser".into(),
                args: serde_json::json!({"url":"https://example.test"}),
            },
            FromAgentMessage::GovernedClientToolRequest {
                call_id: "call-1".into(),
                tool_execution_id: "execution-1".into(),
                tool: "browser".into(),
                args: serde_json::json!({}),
                provider_tool_name: "browser".into(),
                tool_id: "tool-1".into(),
                connection_binding_id: None,
                client_instance_id: "client-1".into(),
                grant_id: "grant-1".into(),
                grant_version: 1,
                grant_hash: "hash".into(),
                turn_digest: "turn-digest".into(),
                definition_digest: "definition-digest".into(),
                args_digest: "args-digest".into(),
                owner_lease_epoch: 1,
                idempotency_key: "idempotency-1".into(),
            },
            FromAgentMessage::ServerRequest {
                request_id: "request-1".into(),
                request_type: ServerRequestType::Approval,
                call_id: "call-1".into(),
                tool_execution_id: Some("execution-1".into()),
                tool: "read".into(),
                args: serde_json::json!({}),
                reason: "approval".into(),
                started_at_ms: Some(1),
            },
            FromAgentMessage::ServerRequestResolved {
                request_id: "request-1".into(),
                request_type: ServerRequestType::Approval,
                call_id: "call-1".into(),
                resolution: ServerRequestResolutionStatus::Approved,
                reason: Some("approved".into()),
                resolved_by: ServerRequestResolvedBy::User,
                started_at_ms: Some(1),
                resolved_at_ms: Some(2),
            },
            FromAgentMessage::RawAgentEvent {
                event_type: "future".into(),
                event: serde_json::json!({"safe":true}),
            },
            FromAgentMessage::UtilityCommandStarted {
                command_id: "command-1".into(),
                command: "true".into(),
                cwd: None,
                shell_mode: UtilityCommandShellMode::Direct,
                terminal_mode: UtilityCommandTerminalMode::Pipe,
                pid: Some(1),
                columns: Some(80),
                rows: Some(24),
                owner_connection_id: None,
            },
            FromAgentMessage::UtilityCommandResized {
                command_id: "command-1".into(),
                columns: 80,
                rows: 24,
            },
            FromAgentMessage::UtilityCommandOutput {
                command_id: "command-1".into(),
                stream: UtilityCommandStream::Stdout,
                content: "output".into(),
            },
            FromAgentMessage::UtilityCommandExited {
                command_id: "command-1".into(),
                success: true,
                exit_code: Some(0),
                signal: None,
                reason: None,
            },
            FromAgentMessage::UtilityFileSearchResults {
                search_id: "search-1".into(),
                query: "README".into(),
                cwd: "/workspace".into(),
                results: vec![UtilityFileSearchMatch {
                    path: "README.md".into(),
                    score: 1,
                }],
                truncated: false,
            },
            FromAgentMessage::UtilityFileReadResult {
                read_id: "read-1".into(),
                path: "README.md".into(),
                relative_path: "README.md".into(),
                cwd: "/workspace".into(),
                content: "read".into(),
                start_line: 1,
                end_line: 1,
                total_lines: 1,
                truncated: false,
            },
            FromAgentMessage::UtilityFileWatchStarted {
                watch_id: "watch-1".into(),
                root_dir: "/workspace".into(),
                include_patterns: Some(vec!["**/*.rs".into()]),
                exclude_patterns: Some(vec![]),
                debounce_ms: 10,
                owner_connection_id: None,
            },
            FromAgentMessage::UtilityFileWatchEvent {
                watch_id: "watch-1".into(),
                change_type: UtilityFileWatchChangeType::Modify,
                path: "/workspace/README.md".into(),
                relative_path: "README.md".into(),
                timestamp: 1,
                is_directory: false,
            },
            FromAgentMessage::UtilityFileWatchStopped {
                watch_id: "watch-1".into(),
                reason: Some("done".into()),
            },
            FromAgentMessage::Error {
                request_id: Some("request-1".into()),
                message: "error".into(),
                fatal: false,
                terminal: false,
                error_type: Some(HeadlessErrorType::Transient),
            },
            FromAgentMessage::Status {
                message: "status".into(),
            },
            FromAgentMessage::Compaction {
                summary: "summary".into(),
                first_kept_entry_index: 0,
                tokens_before: 1,
                auto: true,
                custom_instructions: None,
                continuation: None,
                timestamp: "2026-01-01T00:00:00Z".into(),
            },
            FromAgentMessage::SessionInfo {
                session_id: Some("session-1".into()),
                cwd: "/workspace".into(),
                git_branch: Some("main".into()),
            },
            FromAgentMessage::ConnectionInfo {
                connection_id: Some("connection-1".into()),
                client_protocol_version: Some("2026-08-08".into()),
                client_info: None,
                capabilities: None,
                opt_out_notifications: None,
                role: None,
                connection_count: Some(1),
                controller_connection_id: None,
                lease_expires_at: None,
                connections: None,
            },
            FromAgentMessage::ResponseAccepted {
                request_id: "request-1".into(),
            },
            FromAgentMessage::TurnCompleted {
                response_id: "response-1".into(),
                coding_completion: None,
                coding_child_records: Vec::new(),
            },
            FromAgentMessage::TurnInterrupted {
                response_id: "response-1".into(),
                reason: "cancelled".into(),
            },
            FromAgentMessage::ProviderError {
                kind: maestro_ai::ProviderStreamErrorKind::TransientProtocol,
                message: "provider".into(),
            },
            FromAgentMessage::DelegationEvent {
                event: maestro_runtime::DelegationEvent::from_subagent_lifecycle(
                    "event-1",
                    "delegation-1",
                    1,
                    "completed",
                    Some("done"),
                    None,
                ),
            },
            FromAgentMessage::ConversationSnapshot {
                processed_queue_ids: vec![7, 9],
                protocol_version: "snapshot-v1".into(),
                messages: vec![],
            },
            FromAgentMessage::CodexSessionState {
                state: "ready".into(),
                thread_id: "thread-1".into(),
                profile: "default".into(),
            },
            FromAgentMessage::CodexTurnState {
                state: "running".into(),
                thread_id: "thread-1".into(),
                turn_id: Some("turn-1".into()),
            },
            FromAgentMessage::CodexUsageState {
                source: "provider".into(),
                usage: Some(TokenUsage::default()),
            },
            FromAgentMessage::CodexCompatibility {
                protocol_version: "codex-v1".into(),
                resume: true,
                steering: true,
            },
            FromAgentMessage::ManagedGatewayReceipt {
                request_id: "request-1".into(),
                record_id: "record-1".into(),
                lineage_id: "lineage-1".into(),
                record_status: "planned".into(),
                prompt_experiment: None,
            },
            FromAgentMessage::ProcessBudgetCheckpoint {
                budget: crate::agent::process_budget::ProcessBudgetState::new(
                    crate::agent::process_budget::ProcessBudgetLimits {
                        event_id: "event-1".into(),
                        max_requests: 1,
                        max_total_tokens: 1,
                        max_cost_micros: 1,
                        cost_micros_per_token: 1,
                    },
                )
                .expect("valid process budget limits"),
            },
        ]
    }

    fn assert_live_from_variant_is_enumerated(message: &FromAgentMessage) {
        match message {
            FromAgentMessage::ConversationSnapshot { .. }
            | FromAgentMessage::HelloOk { .. }
            | FromAgentMessage::ResponseAccepted { .. }
            | FromAgentMessage::ProcessBudgetCheckpoint { .. }
            | FromAgentMessage::ManagedGatewayReceipt { .. }
            | FromAgentMessage::WorkspaceCapabilitySetApplied { .. }
            | FromAgentMessage::Ready { .. }
            | FromAgentMessage::ResponseStart { .. }
            | FromAgentMessage::ResponseChunk { .. }
            | FromAgentMessage::ResponseEnd { .. }
            | FromAgentMessage::TurnCompleted { .. }
            | FromAgentMessage::TurnInterrupted { .. }
            | FromAgentMessage::CodexSessionState { .. }
            | FromAgentMessage::CodexTurnState { .. }
            | FromAgentMessage::CodexUsageState { .. }
            | FromAgentMessage::CodexCompatibility { .. }
            | FromAgentMessage::ToolCall { .. }
            | FromAgentMessage::ToolStart { .. }
            | FromAgentMessage::ToolOutput { .. }
            | FromAgentMessage::ToolEnd { .. }
            | FromAgentMessage::ClientToolRequest { .. }
            | FromAgentMessage::GovernedClientToolRequest { .. }
            | FromAgentMessage::ManagedAuthorizationRequest { .. }
            | FromAgentMessage::ServerRequest { .. }
            | FromAgentMessage::ServerRequestResolved { .. }
            | FromAgentMessage::Error { .. }
            | FromAgentMessage::ProviderError { .. }
            | FromAgentMessage::DelegationEvent { .. }
            | FromAgentMessage::Status { .. }
            | FromAgentMessage::Compaction { .. }
            | FromAgentMessage::SessionInfo { .. }
            | FromAgentMessage::ConnectionInfo { .. }
            | FromAgentMessage::RawAgentEvent { .. }
            | FromAgentMessage::UtilityCommandStarted { .. }
            | FromAgentMessage::UtilityCommandResized { .. }
            | FromAgentMessage::UtilityCommandOutput { .. }
            | FromAgentMessage::UtilityCommandExited { .. }
            | FromAgentMessage::UtilityFileSearchResults { .. }
            | FromAgentMessage::UtilityFileReadResult { .. }
            | FromAgentMessage::UtilityFileWatchStarted { .. }
            | FromAgentMessage::UtilityFileWatchEvent { .. }
            | FromAgentMessage::UtilityFileWatchStopped { .. } => {}
        }
    }

    #[test]
    fn generated_headless_proto_types_compile() {
        let hello = HelloMessage {
            protocol_version: Some("2026-08-08".to_string()),
            ..HelloMessage::default()
        };

        let envelope = ToAgentEnvelope {
            payload: Some(Payload::Hello(hello)),
        };

        assert!(matches!(envelope.payload, Some(Payload::Hello(_))));

        let tool_end = ToolEndMessage {
            call_id: "call-1".to_string(),
            success: true,
            receipt: Some(prost_types::Value::default()),
            ..ToolEndMessage::default()
        };
        assert!(tool_end.receipt.is_some());

        let governed = ToAgentEnvelope {
            payload: Some(Payload::GovernedInit(GovernedInitMessage {
                code_mode: CodeMode::GovernedCode.into(),
                tool_grant: Some(ProtoGovernedToolGrant {
                    envelope_version: 2,
                    grant_id: "grant-1".to_string(),
                    ..ProtoGovernedToolGrant::default()
                }),
                ..GovernedInitMessage::default()
            })),
        };
        assert!(matches!(governed.payload, Some(Payload::GovernedInit(_))));

        let hello_ok = super::maestro::v1::HelloOkMessage {
            server_capabilities: Some(ServerCapabilities {
                native_tools: vec![NativeToolCapability {
                    name: "bash".to_string(),
                    requires_approval: true,
                    version: Some("current".to_string()),
                }],
                governed_tool_grant_algorithms: vec!["ed25519".to_string()],
                ..ServerCapabilities::default()
            }),
            ..super::maestro::v1::HelloOkMessage::default()
        };
        let encoded = hello_ok.encode_to_vec();
        let decoded = super::maestro::v1::HelloOkMessage::decode(encoded.as_slice())
            .expect("decode hello acknowledgement");
        let capabilities = decoded.server_capabilities.expect("server capabilities");
        assert_eq!(capabilities.governed_tool_grant_algorithms, ["ed25519"]);
        let tool = capabilities
            .native_tools
            .into_iter()
            .next()
            .expect("native tool capability");
        assert_eq!(tool.name, "bash");
        assert!(tool.requires_approval);
        assert_eq!(tool.version.as_deref(), Some("current"));
    }

    #[test]
    fn explicit_turn_terminals_are_in_the_authoritative_proto_envelope() {
        use super::maestro::v1::from_agent_envelope::Payload;

        for payload in [
            Payload::TurnCompleted(TurnCompletedMessage {
                response_id: "done".to_string(),
            }),
            Payload::TurnInterrupted(TurnInterruptedMessage {
                response_id: "done".to_string(),
                reason: "cancelled".to_string(),
            }),
            Payload::ProviderError(ProviderErrorMessage {
                kind: ProviderStreamErrorKind::TransientProtocol.into(),
                message: "unexpected eof".to_string(),
            }),
        ] {
            let encoded = FromAgentEnvelope {
                payload: Some(payload),
            }
            .encode_to_vec();
            let decoded = FromAgentEnvelope::decode(encoded.as_slice()).expect("decode terminal");
            assert!(matches!(
                decoded.payload,
                Some(
                    Payload::TurnCompleted(_)
                        | Payload::TurnInterrupted(_)
                        | Payload::ProviderError(_)
                )
            ));
        }

        for message_type in ["turn_completed", "turn_interrupted", "provider_error"] {
            assert!(
                super::super::generated_protocol::HEADLESS_FROM_AGENT_MESSAGE_TYPES
                    .contains(&message_type),
                "generated JSON contract is missing {message_type}"
            );
        }
    }

    #[test]
    fn generated_json_projection_matches_runtime_owned_contract() {
        let contract = maestro_runtime::headless_protocol_contract();
        let to_runtime = contract
            .to_runtime_messages
            .iter()
            .map(|message| serde_json::to_string(message).expect("message type serializes"))
            .map(|message| message.trim_matches('"').to_string())
            .collect::<Vec<_>>();
        let from_runtime = contract
            .from_runtime_messages
            .iter()
            .map(|message| serde_json::to_string(message).expect("message type serializes"))
            .map(|message| message.trim_matches('"').to_string())
            .collect::<Vec<_>>();
        let runtime_only_to_runtime = contract
            .runtime_only_to_runtime_messages
            .iter()
            .map(|message| serde_json::to_string(message).expect("message type serializes"))
            .map(|message| message.trim_matches('"').to_string())
            .collect::<Vec<_>>();
        let runtime_only_from_runtime = contract
            .runtime_only_from_runtime_messages
            .iter()
            .map(|message| serde_json::to_string(message).expect("message type serializes"))
            .map(|message| message.trim_matches('"').to_string())
            .collect::<Vec<_>>();

        let public_to_runtime = to_runtime
            .iter()
            .filter(|message| !runtime_only_to_runtime.contains(message))
            .cloned()
            .collect::<Vec<_>>();
        let public_from_runtime = from_runtime
            .iter()
            .filter(|message| !runtime_only_from_runtime.contains(message))
            .cloned()
            .collect::<Vec<_>>();

        assert_eq!(
            contract.protocol_version,
            super::super::generated_protocol::HEADLESS_PROTOCOL_VERSION
        );
        assert_eq!(
            public_to_runtime,
            super::super::generated_protocol::HEADLESS_TO_AGENT_MESSAGE_TYPES
                .iter()
                .map(|value| (*value).to_string())
                .collect::<Vec<_>>()
        );
        assert_eq!(
            public_from_runtime,
            super::super::generated_protocol::HEADLESS_FROM_AGENT_MESSAGE_TYPES
                .iter()
                .map(|value| (*value).to_string())
                .collect::<Vec<_>>()
        );
        let accepted_server_requests = contract
            .capabilities
            .server_requests
            .iter()
            .map(|value| serde_json::to_string(value).expect("capability serializes"))
            .map(|value| value.trim_matches('"').to_string())
            .collect::<Vec<_>>();
        let schema_only_server_requests = contract
            .capabilities
            .schema_only_server_requests
            .iter()
            .map(|value| serde_json::to_string(value).expect("capability serializes"))
            .map(|value| value.trim_matches('"').to_string())
            .collect::<Vec<_>>();
        assert!(!accepted_server_requests.contains(&"mcp_elicitation".to_string()));
        assert_eq!(schema_only_server_requests, ["mcp_elicitation"]);
        let mut typed_server_request_union = accepted_server_requests;
        typed_server_request_union.extend(schema_only_server_requests);
        let mut generated_server_requests =
            super::super::generated_protocol::HEADLESS_SERVER_REQUEST_TYPES
                .iter()
                .map(|value| (*value).to_string())
                .collect::<Vec<_>>();
        typed_server_request_union.sort();
        generated_server_requests.sort();
        assert_eq!(typed_server_request_union, generated_server_requests);
        assert_eq!(
            contract
                .capabilities
                .utility_operations
                .iter()
                .map(|value| serde_json::to_string(value).expect("capability serializes"))
                .map(|value| value.trim_matches('"').to_string())
                .collect::<Vec<_>>(),
            super::super::generated_protocol::HEADLESS_UTILITY_OPERATIONS
                .iter()
                .map(|value| (*value).to_string())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn live_serde_models_match_runtime_contract_and_preserve_unknown_events() {
        let live_to_names = live_to_agent_messages()
            .into_iter()
            .map(|message| {
                assert_live_to_variant_is_enumerated(&message);
                serde_json::to_value(message).expect("live client message serializes")["type"]
                    .as_str()
                    .expect("live client message has a type")
                    .to_string()
            })
            .collect::<Vec<_>>();
        let live_from_names = live_from_agent_messages()
            .into_iter()
            .map(|message| {
                assert_live_from_variant_is_enumerated(&message);
                serde_json::to_value(message).expect("live runtime message serializes")["type"]
                    .as_str()
                    .expect("live runtime message has a type")
                    .to_string()
            })
            .collect::<Vec<_>>();

        assert_eq!(
            live_to_names.len(),
            maestro_runtime::HEADLESS_TO_RUNTIME_MESSAGE_NAMES.len()
        );
        assert_eq!(
            live_from_names.len(),
            maestro_runtime::HEADLESS_FROM_RUNTIME_MESSAGE_NAMES.len()
        );
        assert_eq!(
            live_to_names.into_iter().collect::<BTreeSet<_>>(),
            maestro_runtime::HEADLESS_TO_RUNTIME_MESSAGE_NAMES
                .iter()
                .map(|value| (*value).to_string())
                .collect::<BTreeSet<_>>()
        );
        assert_eq!(
            live_from_names.into_iter().collect::<BTreeSet<_>>(),
            maestro_runtime::HEADLESS_FROM_RUNTIME_MESSAGE_NAMES
                .iter()
                .map(|value| (*value).to_string())
                .collect::<BTreeSet<_>>()
        );

        let unknown = crate::headless::decode_from_agent_message(
            r#"{"type":"future_runtime_event","receipt":{"id":"receipt-1"}}"#,
        )
        .expect("unknown additive event is an explicit decode outcome");
        assert!(matches!(
            unknown,
            maestro_runtime::TaggedMessageDecode::Unknown(maestro_runtime::UnknownWireMessage {
                type_name,
                raw,
            }) if type_name == "future_runtime_event"
                && raw == serde_json::json!({
                    "type": "future_runtime_event",
                    "receipt": {"id": "receipt-1"}
                })
        ));

        assert!(matches!(
            crate::headless::decode_from_agent_message(
                r#"{"type":"response_start","response_id":"response-1"}"#
            )
            .expect("known live event decodes"),
            maestro_runtime::TaggedMessageDecode::Known(FromAgentMessage::ResponseStart {
                response_id
            }) if response_id == "response-1"
        ));
        assert!(matches!(
            crate::headless::decode_from_agent_message(r#"{"type":"response_start"}"#),
            Err(maestro_runtime::TaggedMessageDecodeError::InvalidKnownMessage {
                type_name, ..
            }) if type_name == "response_start"
        ));
        assert!(matches!(
            maestro_runtime::decode_tagged_message::<ToAgentMessage>(
                r#"{"type":"future_client_command","receipt":{"id":"receipt-2"}}"#,
                maestro_runtime::HEADLESS_TO_RUNTIME_MESSAGE_NAMES,
            )
            .expect("unknown client additive event is representable"),
            maestro_runtime::TaggedMessageDecode::Unknown(maestro_runtime::UnknownWireMessage {
                type_name,
                ..
            }) if type_name == "future_client_command"
        ));
    }

    #[test]
    fn live_terminal_messages_feed_the_runtime_reducer_without_accepting_response_end() {
        let mut reducer = maestro_runtime::TerminalReducer::new();
        for message in [
            FromAgentMessage::ResponseStart {
                response_id: "response-1".into(),
            },
            FromAgentMessage::ResponseEnd {
                response_id: "response-1".into(),
                usage: None,
                tools_summary: None,
                duration_ms: None,
                ttft_ms: None,
            },
            FromAgentMessage::TurnCompleted {
                response_id: "response-1".into(),
                coding_completion: None,
                coding_child_records: Vec::new(),
            },
        ] {
            let event = message.terminal_event().expect("terminal event projection");
            reducer.apply(event);
        }
        assert_eq!(reducer.status(), maestro_runtime::TerminalStatus::Completed);

        let mut interrupted = maestro_runtime::TerminalReducer::new();
        interrupted.apply(
            FromAgentMessage::ResponseStart {
                response_id: "response-2".into(),
            }
            .terminal_event()
            .expect("response start projection"),
        );
        interrupted.apply(
            FromAgentMessage::TurnInterrupted {
                response_id: "response-2".into(),
                reason: "cancelled".into(),
            }
            .terminal_event()
            .expect("turn interruption projection"),
        );
        assert_eq!(
            interrupted.status(),
            maestro_runtime::TerminalStatus::Interrupted
        );
    }

    #[test]
    fn live_direct_provider_tool_turn_accepts_rotated_response_ids_and_done_terminal() {
        // Mirrors NativeAgent::run_loop: each provider request gets a UUID,
        // tool work occurs between response segments, and the turn-level
        // completion uses the synthetic "done" correlation label.
        let messages = [
            FromAgentMessage::ResponseStart {
                response_id: "generated-response-1".into(),
            },
            FromAgentMessage::ResponseEnd {
                response_id: "generated-response-1".into(),
                usage: None,
                tools_summary: None,
                duration_ms: None,
                ttft_ms: None,
            },
            FromAgentMessage::ToolCall {
                call_id: "call-1".into(),
                tool_execution_id: Some("execution-1".into()),
                tool: "read".into(),
                args: serde_json::json!({"path": "README.md"}),
                requires_approval: false,
            },
            FromAgentMessage::ResponseStart {
                response_id: "generated-response-2".into(),
            },
            FromAgentMessage::ResponseEnd {
                response_id: "generated-response-2".into(),
                usage: None,
                tools_summary: None,
                duration_ms: None,
                ttft_ms: None,
            },
            FromAgentMessage::ResponseEnd {
                response_id: "done".into(),
                usage: None,
                tools_summary: None,
                duration_ms: None,
                ttft_ms: None,
            },
            FromAgentMessage::TurnCompleted {
                response_id: "done".into(),
                coding_completion: None,
                coding_child_records: Vec::new(),
            },
        ];
        let mut reducer = maestro_runtime::TerminalReducer::new();
        for message in messages {
            if let Some(event) = message.terminal_event() {
                reducer.apply(event);
            }
        }
        assert_eq!(reducer.status(), maestro_runtime::TerminalStatus::Completed);
        assert_eq!(reducer.response_id(), Some("generated-response-2"));
    }

    #[test]
    fn live_native_done_sentinel_closes_uuid_response_before_turn_completion() {
        let messages = [
            FromAgentMessage::ResponseStart {
                response_id: "native-generated-uuid".into(),
            },
            FromAgentMessage::ResponseEnd {
                response_id: "done".into(),
                usage: None,
                tools_summary: None,
                duration_ms: None,
                ttft_ms: None,
            },
            FromAgentMessage::TurnCompleted {
                response_id: "done".into(),
                coding_completion: None,
                coding_child_records: Vec::new(),
            },
        ];
        let mut reducer = maestro_runtime::TerminalReducer::new();
        for message in messages {
            if let Some(event) = message.terminal_event() {
                reducer.apply(event);
            }
        }
        assert_eq!(reducer.status(), maestro_runtime::TerminalStatus::Completed);
        assert_eq!(reducer.response_id(), Some("native-generated-uuid"));
    }

    #[test]
    fn provider_terminal_messages_preserve_each_machine_readable_error_kind() {
        for (provider_kind, runtime_kind) in [
            (
                maestro_ai::ProviderStreamErrorKind::TransientProtocol,
                maestro_runtime::TerminalErrorKind::TransientProtocol,
            ),
            (
                maestro_ai::ProviderStreamErrorKind::OutputTokenExhaustion,
                maestro_runtime::TerminalErrorKind::OutputTokenExhaustion,
            ),
            (
                maestro_ai::ProviderStreamErrorKind::IncompleteResponse,
                maestro_runtime::TerminalErrorKind::IncompleteResponse,
            ),
            (
                maestro_ai::ProviderStreamErrorKind::ProviderDeclaredFailure,
                maestro_runtime::TerminalErrorKind::ProviderDeclaredFailure,
            ),
        ] {
            let event = FromAgentMessage::ProviderError {
                kind: provider_kind,
                message: "provider failure".into(),
            }
            .terminal_event()
            .expect("provider errors are terminal events");
            assert!(matches!(
                event,
                maestro_runtime::TerminalEvent::ProviderFailed { kind, .. }
                    if kind == runtime_kind
            ));
        }
    }

    #[test]
    fn fatal_error_messages_preserve_existing_interruption_classification() {
        let mut reducer = maestro_runtime::TerminalReducer::new();
        let event = FromAgentMessage::Error {
            request_id: None,
            message: "fatal runtime error".into(),
            fatal: true,
            terminal: true,
            error_type: Some(crate::headless::HeadlessErrorType::Fatal),
        }
        .terminal_event()
        .expect("fatal errors are terminal events");

        assert_eq!(
            reducer.apply(event),
            maestro_runtime::TerminalTransition::Applied {
                previous: maestro_runtime::TerminalStatus::Idle,
                current: maestro_runtime::TerminalStatus::Interrupted,
            }
        );
    }

    fn proto_to_type(payload: &Payload) -> &'static str {
        match payload {
            Payload::Hello(_) => "hello",
            Payload::Init(_) => "init",
            Payload::Prompt(_) => "prompt",
            Payload::Interrupt(_) => "interrupt",
            Payload::ToolResponse(_) => "tool_response",
            Payload::ClientToolResult(_) => "client_tool_result",
            Payload::ServerRequestResponse(_) => "server_request_response",
            Payload::UtilityCommandStart(_) => "utility_command_start",
            Payload::UtilityCommandTerminate(_) => "utility_command_terminate",
            Payload::UtilityCommandStdin(_) => "utility_command_stdin",
            Payload::UtilityCommandResize(_) => "utility_command_resize",
            Payload::UtilityFileSearch(_) => "utility_file_search",
            Payload::UtilityFileRead(_) => "utility_file_read",
            Payload::UtilityFileWatchStart(_) => "utility_file_watch_start",
            Payload::UtilityFileWatchStop(_) => "utility_file_watch_stop",
            Payload::Cancel(_) => "cancel",
            Payload::Shutdown(_) => "shutdown",
            Payload::GovernedInit(_) => "governed_init",
            Payload::GovernedPrompt(_) => "governed_prompt",
            Payload::GovernedSteer(_) => "governed_steer",
            Payload::GovernedClientToolResult(_) => "governed_client_tool_result",
            Payload::ManagedAuthorizationResult(_) => "managed_authorization_result",
            Payload::ApplyWorkspaceCapabilitySet(_) => "apply_workspace_capability_set",
            Payload::ConfigurePromptExperiment(_) => "configure_prompt_experiment",
        }
    }

    fn proto_from_type(payload: &FromPayload) -> &'static str {
        match payload {
            FromPayload::HelloOk(_) => "hello_ok",
            FromPayload::Ready(_) => "ready",
            FromPayload::ResponseStart(_) => "response_start",
            FromPayload::ResponseChunk(_) => "response_chunk",
            FromPayload::ResponseEnd(_) => "response_end",
            FromPayload::ToolCall(_) => "tool_call",
            FromPayload::ToolStart(_) => "tool_start",
            FromPayload::ToolOutput(_) => "tool_output",
            FromPayload::ToolEnd(_) => "tool_end",
            FromPayload::ClientToolRequest(_) => "client_tool_request",
            FromPayload::ServerRequest(_) => "server_request",
            FromPayload::ServerRequestResolved(_) => "server_request_resolved",
            FromPayload::RawAgentEvent(_) => "raw_agent_event",
            FromPayload::UtilityCommandStarted(_) => "utility_command_started",
            FromPayload::UtilityCommandResized(_) => "utility_command_resized",
            FromPayload::UtilityCommandOutput(_) => "utility_command_output",
            FromPayload::UtilityCommandExited(_) => "utility_command_exited",
            FromPayload::UtilityFileSearchResults(_) => "utility_file_search_results",
            FromPayload::UtilityFileReadResult(_) => "utility_file_read_result",
            FromPayload::UtilityFileWatchStarted(_) => "utility_file_watch_started",
            FromPayload::UtilityFileWatchEvent(_) => "utility_file_watch_event",
            FromPayload::UtilityFileWatchStopped(_) => "utility_file_watch_stopped",
            FromPayload::Error(_) => "error",
            FromPayload::Status(_) => "status",
            FromPayload::Compaction(_) => "compaction",
            FromPayload::SessionInfo(_) => "session_info",
            FromPayload::ConnectionInfo(_) => "connection_info",
            FromPayload::ResponseAccepted(_) => "response_accepted",
            FromPayload::TurnCompleted(_) => "turn_completed",
            FromPayload::TurnInterrupted(_) => "turn_interrupted",
            FromPayload::ProviderError(_) => "provider_error",
            FromPayload::GovernedClientToolRequest(_) => "governed_client_tool_request",
            FromPayload::ManagedAuthorizationRequest(_) => "managed_authorization_request",
            FromPayload::DelegationEvent(_) => "delegation_event",
            FromPayload::WorkspaceCapabilitySetApplied(_) => "workspace_capability_set_applied",
        }
    }

    #[test]
    fn generated_protobuf_envelopes_match_runtime_public_projection() {
        let to_payloads = vec![
            Payload::Hello(Default::default()),
            Payload::Init(Default::default()),
            Payload::Prompt(Default::default()),
            Payload::Interrupt(Default::default()),
            Payload::ToolResponse(Default::default()),
            Payload::ClientToolResult(Default::default()),
            Payload::ServerRequestResponse(Default::default()),
            Payload::UtilityCommandStart(Default::default()),
            Payload::UtilityCommandTerminate(Default::default()),
            Payload::UtilityCommandStdin(Default::default()),
            Payload::UtilityCommandResize(Default::default()),
            Payload::UtilityFileSearch(Default::default()),
            Payload::UtilityFileRead(Default::default()),
            Payload::UtilityFileWatchStart(Default::default()),
            Payload::UtilityFileWatchStop(Default::default()),
            Payload::Cancel(Default::default()),
            Payload::Shutdown(Default::default()),
            Payload::GovernedInit(Default::default()),
            Payload::GovernedPrompt(Default::default()),
            Payload::GovernedSteer(Default::default()),
            Payload::GovernedClientToolResult(Default::default()),
            Payload::ManagedAuthorizationResult(Default::default()),
            Payload::ApplyWorkspaceCapabilitySet(ApplyWorkspaceCapabilitySetMessage::default()),
            Payload::ConfigurePromptExperiment(Default::default()),
        ];
        let to_names = to_payloads
            .into_iter()
            .map(|payload| {
                let encoded = ToAgentEnvelope {
                    payload: Some(payload),
                }
                .encode_to_vec();
                let decoded = ToAgentEnvelope::decode(encoded.as_slice())
                    .expect("generated client envelope round-trips");
                proto_to_type(decoded.payload.as_ref().expect("client payload"))
            })
            .collect::<Vec<_>>();
        assert_eq!(
            to_names.iter().collect::<BTreeSet<_>>(),
            super::super::generated_protocol::HEADLESS_TO_AGENT_MESSAGE_TYPES
                .iter()
                .collect::<BTreeSet<_>>()
        );

        let from_payloads = vec![
            FromPayload::HelloOk(Default::default()),
            FromPayload::Ready(Default::default()),
            FromPayload::ResponseStart(Default::default()),
            FromPayload::ResponseChunk(Default::default()),
            FromPayload::ResponseEnd(Default::default()),
            FromPayload::ToolCall(Default::default()),
            FromPayload::ToolStart(Default::default()),
            FromPayload::ToolOutput(Default::default()),
            FromPayload::ToolEnd(Default::default()),
            FromPayload::ClientToolRequest(Default::default()),
            FromPayload::ServerRequest(Default::default()),
            FromPayload::ServerRequestResolved(Default::default()),
            FromPayload::RawAgentEvent(Default::default()),
            FromPayload::UtilityCommandStarted(Default::default()),
            FromPayload::UtilityCommandResized(Default::default()),
            FromPayload::UtilityCommandOutput(Default::default()),
            FromPayload::UtilityCommandExited(Default::default()),
            FromPayload::UtilityFileSearchResults(Default::default()),
            FromPayload::UtilityFileReadResult(Default::default()),
            FromPayload::UtilityFileWatchStarted(Default::default()),
            FromPayload::UtilityFileWatchEvent(Default::default()),
            FromPayload::UtilityFileWatchStopped(Default::default()),
            FromPayload::Error(Default::default()),
            FromPayload::Status(Default::default()),
            FromPayload::Compaction(Default::default()),
            FromPayload::SessionInfo(Default::default()),
            FromPayload::ConnectionInfo(Default::default()),
            FromPayload::ResponseAccepted(Default::default()),
            FromPayload::TurnCompleted(Default::default()),
            FromPayload::TurnInterrupted(Default::default()),
            FromPayload::ProviderError(Default::default()),
            FromPayload::GovernedClientToolRequest(Default::default()),
            FromPayload::ManagedAuthorizationRequest(Default::default()),
            FromPayload::DelegationEvent(Default::default()),
            FromPayload::WorkspaceCapabilitySetApplied(Default::default()),
        ];
        let from_names = from_payloads
            .into_iter()
            .map(|payload| {
                let encoded = FromAgentEnvelope {
                    payload: Some(payload),
                }
                .encode_to_vec();
                let decoded = FromAgentEnvelope::decode(encoded.as_slice())
                    .expect("generated runtime envelope round-trips");
                proto_from_type(decoded.payload.as_ref().expect("runtime payload"))
            })
            .collect::<Vec<_>>();
        assert_eq!(
            from_names.iter().collect::<BTreeSet<_>>(),
            super::super::generated_protocol::HEADLESS_FROM_AGENT_MESSAGE_TYPES
                .iter()
                .collect::<BTreeSet<_>>()
        );
    }

    #[test]
    fn tool_response_round_trips_tool_execution_id() {
        let response = ToolResponseMessage {
            call_id: "call-1".to_string(),
            approved: false,
            tool_execution_id: Some("tool-execution-1".to_string()),
            ..ToolResponseMessage::default()
        };

        let encoded = response.encode_to_vec();
        let decoded =
            ToolResponseMessage::decode(encoded.as_slice()).expect("decode tool response");

        assert_eq!(
            decoded.tool_execution_id.as_deref(),
            Some("tool-execution-1")
        );
    }

    #[test]
    fn response_acceptance_is_in_the_authoritative_proto_envelope() {
        let envelope = FromAgentEnvelope {
            payload: Some(
                super::maestro::v1::from_agent_envelope::Payload::ResponseAccepted(
                    ResponseAcceptedMessage {
                        request_id: "call-1".to_string(),
                    },
                ),
            ),
        };
        let encoded = envelope.encode_to_vec();
        let decoded = FromAgentEnvelope::decode(encoded.as_slice()).expect("decode envelope");
        assert!(matches!(
            decoded.payload,
            Some(super::maestro::v1::from_agent_envelope::Payload::ResponseAccepted(
                ResponseAcceptedMessage { request_id }
            )) if request_id == "call-1"
        ));
    }
}

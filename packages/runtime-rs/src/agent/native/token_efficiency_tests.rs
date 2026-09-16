use super::tests::*;
use super::{ExternalToolSchemaPolicy, NativeAgentConfig};
use crate::agent::{FromAgent, NativeContextEffect, NativeExecutionHost};
use crate::ai::{ContentBlock, Message, MessageContent, Role, UnifiedClient};
use serde_json::{Value, json};
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;

#[test]
fn sdk_hosts_default_to_opaque_context_effects() {
    let host = RuntimeTestHost::new(
        "/workspace",
        UnifiedClient::Scripted(crate::ai::ScriptedClient::new("sdk-default", vec![])),
    );

    let effect = host.tool_context_effect("custom_read", &json!({"id": "42"}));

    assert_eq!(effect, Option::<NativeContextEffect>::None);
}

#[test]
fn provider_request_ids_are_stable_and_change_with_identity_inputs() {
    let messages = vec![Message {
        role: Role::User,
        content: MessageContent::text("same logical request"),
    }];
    let first = super::provider_request_id("primary", "model-a", &messages).expect("request id");
    assert_eq!(
        first,
        super::provider_request_id("primary", "model-a", &messages).expect("same request id")
    );
    assert_ne!(
        first,
        super::provider_request_id("side_question", "model-a", &messages).expect("kind request id")
    );
    assert_ne!(
        first,
        super::provider_request_id("primary", "model-b", &messages).expect("model request id")
    );
    let changed_messages = vec![Message {
        role: Role::User,
        content: MessageContent::text("different logical request"),
    }];
    assert_ne!(
        first,
        super::provider_request_id("primary", "model-a", &changed_messages)
            .expect("history request id")
    );
}

#[tokio::test]
async fn recall_output_recovers_a_prior_native_tool_result_by_call_id() {
    let workspace = tempfile::tempdir().expect("temporary workspace");
    std::fs::write(workspace.path().join("evidence.txt"), "retained evidence")
        .expect("write fixture");
    let scripted = crate::ai::ScriptedClient::new(
        "runtime-test/recall-output",
        vec![
            crate::ai::ScriptedResponse {
                blocks: vec![crate::ai::ScriptedBlock::ToolUse {
                    id: "original-read".to_owned(),
                    name: "read".to_owned(),
                    input: json!({"path":"evidence.txt"}),
                }],
                stop_reason: crate::ai::StopReason::ToolUse,
                error: None,
            },
            crate::ai::ScriptedResponse {
                blocks: vec![crate::ai::ScriptedBlock::ToolUse {
                    id: "recall-call".to_owned(),
                    name: "recall_output".to_owned(),
                    input: json!({"id":"original-read"}),
                }],
                stop_reason: crate::ai::StopReason::ToolUse,
                error: None,
            },
            crate::ai::ScriptedResponse::text("done"),
        ],
    );
    let config = NativeAgentConfig {
        model: "scripted/recall-output".to_owned(),
        cwd: workspace.path().display().to_string(),
        approval_mode: crate::agent::ApprovalMode::Yolo,
        max_turn_steps: 4,
        ..NativeAgentConfig::default()
    };
    let (agent, mut events) =
        new_runtime_test_agent(config, UnifiedClient::Scripted(scripted)).expect("test agent");

    agent
        .prompt("read and recall the evidence".to_owned(), Vec::new())
        .await
        .expect("prompt queued");
    let recalled = tokio::time::timeout(Duration::from_secs(10), async {
        let mut recalled = None;
        loop {
            match events.recv().await {
                Some(FromAgent::ToolEnd {
                    call_id,
                    result: Some(result),
                    ..
                }) if call_id == "recall-call" => recalled = Some(result.output),
                Some(FromAgent::TurnCompleted { .. }) => break recalled,
                Some(FromAgent::Error { message, .. })
                | Some(FromAgent::ProviderError { message, .. }) => panic!("{message}"),
                Some(_) => {}
                None => panic!("agent event channel closed"),
            }
        }
    })
    .await
    .expect("turn timeout")
    .expect("recall result");
    assert_eq!(recalled, "retained evidence");
    agent.shutdown().await;
}

fn observation_turn(
    prompt: &str,
    call_id: &str,
    tool_name: &str,
    input: serde_json::Value,
    output: &str,
) -> Vec<Message> {
    observation_turn_with_status(prompt, call_id, tool_name, input, output, false)
}

fn observation_turn_with_status(
    prompt: &str,
    call_id: &str,
    tool_name: &str,
    input: serde_json::Value,
    output: &str,
    is_error: bool,
) -> Vec<Message> {
    vec![
        Message {
            role: Role::User,
            content: MessageContent::text(prompt),
        },
        Message {
            role: Role::Assistant,
            content: MessageContent::Blocks(vec![ContentBlock::ToolUse {
                id: call_id.to_owned(),
                name: tool_name.to_owned(),
                input,
                gemini_context: None,
            }]),
        },
        Message {
            role: Role::User,
            content: MessageContent::Blocks(vec![ContentBlock::ToolResult {
                tool_use_id: call_id.to_owned(),
                content: output.to_owned(),
                is_error: Some(is_error),
            }]),
        },
    ]
}

fn test_file_context_effect(name: &str, input: &Value) -> Option<NativeContextEffect> {
    let resource_key = input
        .get("path")
        .or_else(|| input.get("file_path"))?
        .as_str()?
        .to_owned();
    match name.to_ascii_lowercase().as_str() {
        "read" => Some(NativeContextEffect::Observe { resource_key }),
        "edit" | "write" => Some(NativeContextEffect::Mutate { resource_key }),
        _ => None,
    }
}

fn tool_result_content<'a>(messages: &'a [Message], call_id: &str) -> Option<&'a str> {
    messages.iter().find_map(|message| match &message.content {
        MessageContent::Blocks(blocks) => blocks.iter().find_map(|block| match block {
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                ..
            } if tool_use_id == call_id => Some(content.as_str()),
            _ => None,
        }),
        MessageContent::Text(_) => None,
    })
}

fn total_tool_result_chars(messages: &[Message]) -> usize {
    messages
        .iter()
        .filter_map(|message| match &message.content {
            MessageContent::Blocks(blocks) => Some(blocks),
            MessageContent::Text(_) => None,
        })
        .flatten()
        .filter_map(|block| match block {
            ContentBlock::ToolResult { content, .. } => Some(content.chars().count()),
            _ => None,
        })
        .sum()
}

#[test]
fn provider_history_masks_only_aged_verbose_observations() {
    let mut messages = observation_turn(
        "turn one",
        "read-old",
        "read",
        json!({"path":"src/lib.rs"}),
        "old read output",
    );
    messages.extend(observation_turn(
        "turn two",
        "bash-recent",
        "bash",
        json!({"command":"cargo check"}),
        "recent bash output",
    ));
    messages.push(Message {
        role: Role::User,
        content: MessageContent::text("turn three"),
    });
    let durable = Arc::new(messages);

    let projected = super::provider_history::project_observation_history(
        &durable,
        2,
        true,
        test_file_context_effect,
    );

    assert!(
        tool_result_content(&projected, "read-old").is_some_and(|content| content
            .contains("recall_output")
            && content.contains("read-old"))
    );
    assert_eq!(
        tool_result_content(&projected, "bash-recent"),
        Some("recent bash output")
    );
    assert_eq!(
        tool_result_content(&durable, "read-old"),
        Some("old read output"),
        "masking must never mutate durable session history"
    );
}

#[test]
fn provider_history_stays_full_when_the_sdk_host_does_not_offer_recall() {
    let durable = Arc::new(observation_turn(
        "turn one",
        "read-old",
        "read",
        json!({"path":"src/lib.rs"}),
        "old read output",
    ));
    let projected = super::provider_history::project_observation_history(
        &durable,
        0,
        false,
        test_file_context_effect,
    );

    assert!(Arc::ptr_eq(&durable, &projected));
    assert_eq!(
        tool_result_content(&projected, "read-old"),
        Some("old read output")
    );
}

#[test]
fn provider_history_defers_same_turn_read_supersession_until_a_boundary() {
    let mut messages = observation_turn(
        "turn one",
        "read-first",
        "read",
        json!({"path":"src/lib.rs"}),
        "first version",
    );
    messages.extend([
        Message {
            role: Role::Assistant,
            content: MessageContent::Blocks(vec![ContentBlock::ToolUse {
                id: "read-second".into(),
                name: "read".into(),
                input: json!({"path":"src/lib.rs"}),
                gemini_context: None,
            }]),
        },
        Message {
            role: Role::User,
            content: MessageContent::Blocks(vec![ContentBlock::ToolResult {
                tool_use_id: "read-second".into(),
                content: "second version".into(),
                is_error: Some(false),
            }]),
        },
    ]);
    let within_turn = Arc::new(messages.clone());
    let projected_within_turn = super::provider_history::project_observation_history(
        &within_turn,
        10,
        true,
        test_file_context_effect,
    );
    assert_eq!(
        tool_result_content(&projected_within_turn, "read-first"),
        Some("first version"),
        "a provider-cache prefix must stay stable within a user turn"
    );

    messages.push(Message {
        role: Role::User,
        content: MessageContent::text("turn two"),
    });
    let after_boundary = super::provider_history::project_observation_history(
        &Arc::new(messages),
        10,
        true,
        test_file_context_effect,
    );
    assert!(
        tool_result_content(&after_boundary, "read-first").is_some_and(|content| content
            .contains("superseded")
            && content.contains("read-second"))
    );
    assert_eq!(
        tool_result_content(&after_boundary, "read-second"),
        Some("second version")
    );
}

#[test]
fn causal_context_frontier_invalidates_an_observation_after_a_successful_mutation() {
    let mut messages = observation_turn(
        "inspect",
        "read-before",
        "read",
        json!({"path":"src/lib.rs"}),
        "stale source",
    );
    messages.extend(observation_turn(
        "change it",
        "edit-after",
        "edit",
        json!({"path":"src/lib.rs"}),
        "updated",
    ));
    messages.push(Message {
        role: Role::User,
        content: MessageContent::text("verify"),
    });
    let durable = Arc::new(messages);

    let projected = super::provider_history::project_observation_history(
        &durable,
        10,
        true,
        test_file_context_effect,
    );

    assert!(
        tool_result_content(&projected, "read-before").is_some_and(|content| {
            content.contains("invalidated")
                && content.contains("edit-after")
                && content.contains("read-before")
        })
    );
    assert_eq!(
        tool_result_content(&projected, "edit-after"),
        Some("updated"),
        "mutation outputs are not observations and must remain untouched"
    );
    assert_eq!(
        tool_result_content(&durable, "read-before"),
        Some("stale source"),
        "provider projection must not mutate durable history"
    );
}

#[test]
fn causal_context_frontier_ignores_failed_mutations() {
    let mut messages = observation_turn(
        "inspect",
        "read-before",
        "read",
        json!({"path":"src/lib.rs"}),
        "still current",
    );
    messages.extend(observation_turn_with_status(
        "attempt change",
        "edit-failed",
        "edit",
        json!({"path":"src/lib.rs"}),
        "permission denied",
        true,
    ));
    messages.push(Message {
        role: Role::User,
        content: MessageContent::text("continue"),
    });

    let projected = super::provider_history::project_observation_history(
        &Arc::new(messages),
        10,
        true,
        test_file_context_effect,
    );

    assert_eq!(
        tool_result_content(&projected, "read-before"),
        Some("still current")
    );
}

#[test]
fn causal_context_frontier_keeps_a_fresh_observation_after_mutation() {
    let mut messages = observation_turn(
        "inspect",
        "read-old",
        "read",
        json!({"path":"src/lib.rs"}),
        "old source",
    );
    messages.extend(observation_turn(
        "change it",
        "edit-source",
        "edit",
        json!({"path":"src/lib.rs"}),
        "updated",
    ));
    messages.extend(observation_turn(
        "inspect again",
        "read-fresh",
        "read",
        json!({"path":"src/lib.rs"}),
        "fresh source",
    ));
    messages.push(Message {
        role: Role::User,
        content: MessageContent::text("continue"),
    });

    let projected = super::provider_history::project_observation_history(
        &Arc::new(messages),
        10,
        true,
        test_file_context_effect,
    );

    assert!(
        tool_result_content(&projected, "read-old")
            .is_some_and(|content| content.contains("invalidated"))
    );
    assert_eq!(
        tool_result_content(&projected, "read-fresh"),
        Some("fresh source")
    );
}

#[test]
fn causal_context_frontier_is_scoped_to_one_resource() {
    let mut messages = observation_turn(
        "inspect source",
        "read-source",
        "read",
        json!({"path":"src/lib.rs"}),
        "source state",
    );
    messages.extend(observation_turn(
        "inspect config",
        "read-config",
        "read",
        json!({"path":"config.toml"}),
        "config state",
    ));
    messages.extend(observation_turn(
        "change source",
        "edit-source",
        "edit",
        json!({"path":"src/lib.rs"}),
        "updated",
    ));
    messages.push(Message {
        role: Role::User,
        content: MessageContent::text("continue"),
    });

    let projected = super::provider_history::project_observation_history(
        &Arc::new(messages),
        10,
        true,
        test_file_context_effect,
    );

    assert!(
        tool_result_content(&projected, "read-source")
            .is_some_and(|content| content.contains("invalidated"))
    );
    assert_eq!(
        tool_result_content(&projected, "read-config"),
        Some("config state")
    );
}

#[test]
fn causal_context_frontier_waits_for_a_user_turn_boundary() {
    let mut messages = observation_turn(
        "inspect",
        "read-before",
        "read",
        json!({"path":"src/lib.rs"}),
        "cache prefix",
    );
    messages.extend([
        Message {
            role: Role::Assistant,
            content: MessageContent::Blocks(vec![ContentBlock::ToolUse {
                id: "edit-same-turn".into(),
                name: "edit".into(),
                input: json!({"path":"src/lib.rs"}),
                gemini_context: None,
            }]),
        },
        Message {
            role: Role::User,
            content: MessageContent::Blocks(vec![ContentBlock::ToolResult {
                tool_use_id: "edit-same-turn".into(),
                content: "updated".into(),
                is_error: Some(false),
            }]),
        },
    ]);

    let projected = super::provider_history::project_observation_history(
        &Arc::new(messages),
        10,
        true,
        test_file_context_effect,
    );

    assert_eq!(
        tool_result_content(&projected, "read-before"),
        Some("cache prefix")
    );
}

#[test]
fn causal_context_frontier_reclaims_stale_observation_bytes() {
    let stale_output = "x".repeat(24_000);
    let mut messages = observation_turn(
        "inspect",
        "read-large",
        "read",
        json!({"path":"src/generated.rs"}),
        &stale_output,
    );
    messages.extend(observation_turn(
        "replace it",
        "write-generated",
        "write",
        json!({"path":"src/generated.rs"}),
        "written",
    ));
    messages.push(Message {
        role: Role::User,
        content: MessageContent::text("continue"),
    });
    let durable = Arc::new(messages);

    let projected = super::provider_history::project_observation_history(
        &durable,
        10,
        true,
        test_file_context_effect,
    );
    let durable_chars = total_tool_result_chars(&durable);
    let projected_chars = total_tool_result_chars(&projected);
    println!(
        "causal-context-frontier durable_chars={durable_chars} projected_chars={projected_chars} saved_chars={}",
        durable_chars.saturating_sub(projected_chars)
    );

    assert!(projected_chars < durable_chars);
    assert_eq!(
        tool_result_content(&durable, "read-large"),
        Some(stale_output.as_str())
    );
}

#[test]
fn deferred_external_tools_require_the_search_escape_hatch() {
    let client = UnifiedClient::Scripted(crate::ai::ScriptedClient::new(
        "runtime-test/deferred-tool-validation",
        Vec::new(),
    ));
    let config = NativeAgentConfig {
        external_tool_schema_policy: ExternalToolSchemaPolicy::Deferred,
        ..NativeAgentConfig::default()
    };
    let allowed = HashSet::from([String::from("read")]);
    let error = match NativeAgent::new_with_external_tools(
        config,
        vec![external_tool_definition("client_calendar")],
        Some(&allowed),
        client,
    ) {
        Ok(_) => panic!("deferred tools without tool_search must fail closed"),
        Err(error) => error.to_string(),
    };
    assert!(error.contains("tool_search"), "{error}");
}

#[tokio::test]
async fn deferred_governed_replacement_requires_the_search_escape_hatch() {
    let client = UnifiedClient::Scripted(crate::ai::ScriptedClient::new(
        "runtime-test/deferred-governed-tool-validation",
        Vec::new(),
    ));
    let config = NativeAgentConfig {
        external_tool_schema_policy: ExternalToolSchemaPolicy::Deferred,
        ..NativeAgentConfig::default()
    };
    let (agent, _events) = NativeAgent::new_with_external_tools(config, Vec::new(), None, client)
        .expect("an initially empty deferred catalog should be valid");
    let error = agent
        .replace_governed_tools(
            HashSet::from([String::from("read")]),
            vec![external_tool_definition("client_calendar")],
        )
        .expect_err("a replacement without tool_search must fail closed");
    assert!(error.to_string().contains("tool_search"), "{error}");
    agent.shutdown().await;
}

#[test]
fn deferred_external_schemas_keep_search_visible_without_shipping_the_tools() {
    let mut definitions = super::deferred_tool_tests::profile_fixture_definitions();
    definitions.insert(
        "client_calendar".into(),
        external_tool_definition("client_calendar"),
    );
    let external = HashSet::from([String::from("client_calendar")]);
    let active = super::deferred_tool_schemas::initial_active_tool_names(
        super::deferred_tool_schemas::ToolProfile::Fast,
        &definitions,
        &external,
        None,
        ExternalToolSchemaPolicy::Deferred,
    );
    assert!(active.contains("tool_search"));
    assert!(!active.contains("client_calendar"));
    assert!(definitions.contains_key("client_calendar"));
}

#[tokio::test]
async fn tool_search_materializes_a_deferred_external_schema_on_the_next_request() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let mut requests = Vec::new();
        for index in 0..2 {
            let (mut stream, _) = listener.accept().await.unwrap();
            requests.push(read_scripted_provider_request(&mut stream).await);
            let body = if index == 0 {
                let start = json!({
                    "id":"deferred-tools","object":"chat.completion.chunk","created":0,
                    "model":"gpt-4o","choices":[{"index":0,
                    "delta":{"role":"assistant","content":""},"finish_reason":null}]
                });
                let tool = json!({
                    "id":"deferred-tools","object":"chat.completion.chunk","created":0,
                    "model":"gpt-4o","choices":[{"index":0,
                    "delta":{"tool_calls":[{"index":0,"id":"call-search","type":"function",
                    "function":{"name":"tool_search","arguments":"{\"names\":[\"client_calendar\"]}"}}]},
                    "finish_reason":"tool_calls"}]
                });
                format!("data: {start}\n\ndata: {tool}\n\ndata: [DONE]\n\n")
            } else {
                chat_sse_response("deferred-tools-done", "Done.", false)
            };
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream.write_all(response.as_bytes()).await.unwrap();
        }
        requests
    });
    let workspace = tempfile::tempdir().unwrap();
    let config = NativeAgentConfig {
        model: "openai/gpt-4o".into(),
        cwd: workspace.path().display().to_string(),
        external_tool_schema_policy: ExternalToolSchemaPolicy::Deferred,
        ..NativeAgentConfig::default()
    };
    let client = UnifiedClient::OpenAI(
        crate::ai::OpenAiClient::with_base_url("test-key", format!("http://{address}/v1")).unwrap(),
    );
    let (agent, mut events) = NativeAgent::new_with_external_tools(
        config,
        vec![external_tool_definition("client_calendar")],
        None,
        client,
    )
    .unwrap();
    agent
        .prompt("Check my calendar.".into(), vec![])
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match events.recv().await.unwrap() {
                FromAgent::TurnCompleted { .. } => break,
                FromAgent::Error { message, .. } | FromAgent::ProviderError { message, .. } => {
                    panic!("{message}")
                }
                _ => {}
            }
        }
    })
    .await
    .unwrap();
    agent.shutdown().await;

    let requests = server.await.unwrap();
    let tool_names = |request: &Value| {
        request["tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|tool| tool["function"]["name"].as_str())
            .map(str::to_owned)
            .collect::<HashSet<String>>()
    };
    assert!(tool_names(&requests[0]).contains("tool_search"));
    assert!(!tool_names(&requests[0]).contains("client_calendar"));
    let search_description = requests[0]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["function"]["name"] == "tool_search")
        .and_then(|tool| tool["function"]["description"].as_str())
        .expect("tool_search should carry the deferred catalog");
    assert!(search_description.contains("client_calendar: Caller-owned test tool"));
    assert!(tool_names(&requests[1]).contains("client_calendar"));
}

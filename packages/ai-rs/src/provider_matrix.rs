//! Provider request-builder conformance matrix.
//!
//! One canonical multi-turn script (system prompt, user turn, assistant tool
//! call, tool result, assistant text, follow-up user turn, plus an interrupted
//! tool call) is rendered through every provider request builder. Each wire
//! shape is reduced to the same observation record and checked against
//! invariants that hold for all providers, so a regression in one adapter's
//! tool-history handling fails here instead of at the provider.

use crate::anthropic::AnthropicClient;
use crate::google::GoogleClient;
use crate::kimi::KimiK3Client;
use crate::openai_base::OpenAiClient;
use crate::types::{ContentBlock, Message, MessageContent, RequestConfig, Role, Tool};
use crate::vertex::VertexAiClient;
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::sync::Arc;

const SYSTEM_PROMPT: &str = "You are the matrix system prompt.";
const USER_ONE: &str = "matrix user turn one";
const ASSISTANT_TEXT: &str = "matrix assistant text after tool";
const USER_TWO: &str = "matrix user turn two";
const TOOL_A: &str = "read_file";
const TOOL_B: &str = "run_shell";
const CALL_A: &str = "toolu_matrix_call_a";
const CALL_B: &str = "toolu_matrix_call_b";
const RESULT_A: &str = "matrix tool result a";
/// Fragment of the text every adapter must hand the model for a call that never completed.
const INTERRUPTED_MARKER: &str = "interrupted";

/// Provider-neutral view of a rendered request.
#[derive(Debug, Default)]
struct Observed {
    model: String,
    tool_names: BTreeSet<String>,
    /// Tool calls in wire order as (call identity, tool name).
    calls: Vec<(String, String)>,
    /// Tool results in wire order.
    results: Vec<ObservedResult>,
    text: String,
}

#[derive(Debug)]
struct ObservedResult {
    /// Call identity (tool name on wires without call ids).
    id: String,
    /// Explicit error flag where the wire has one.
    is_error: Option<bool>,
    /// Result payload rendered as text.
    content: String,
}

impl ObservedResult {
    fn new(id: &str, is_error: Option<bool>, content: &Value) -> Self {
        let mut text = String::new();
        collect_text(content, &mut text);
        Self {
            id: id.to_string(),
            is_error,
            content: text,
        }
    }
}

fn script() -> Vec<Message> {
    vec![
        Message {
            role: Role::User,
            content: MessageContent::Text(USER_ONE.to_string()),
        },
        Message {
            role: Role::Assistant,
            content: MessageContent::Blocks(vec![ContentBlock::ToolUse {
                id: CALL_A.to_string(),
                name: TOOL_A.to_string(),
                input: json!({"path": "README.md"}),
                gemini_context: None,
            }]),
        },
        Message {
            role: Role::User,
            content: MessageContent::Blocks(vec![ContentBlock::ToolResult {
                tool_use_id: CALL_A.to_string(),
                content: RESULT_A.to_string(),
                is_error: None,
            }]),
        },
        Message {
            role: Role::Assistant,
            content: MessageContent::Blocks(vec![
                ContentBlock::Text {
                    text: ASSISTANT_TEXT.to_string(),
                },
                ContentBlock::ToolUse {
                    id: CALL_B.to_string(),
                    name: TOOL_B.to_string(),
                    input: json!({"command": "ls"}),
                    gemini_context: None,
                },
            ]),
        },
        Message {
            role: Role::User,
            content: MessageContent::Text(USER_TWO.to_string()),
        },
    ]
}

fn config(model: &str) -> RequestConfig {
    RequestConfig {
        model: model.to_string(),
        system: Some(SYSTEM_PROMPT.to_string()),
        tools: Arc::new(vec![
            Tool::new(TOOL_A, "read a file"),
            Tool::new(TOOL_B, "run a shell command"),
        ]),
        ..RequestConfig::default()
    }
}

fn collect_text(value: &Value, out: &mut String) {
    match value {
        Value::String(text) => {
            out.push_str(text);
            out.push('\n');
        }
        Value::Array(items) => items.iter().for_each(|item| collect_text(item, out)),
        Value::Object(map) => map.values().for_each(|item| collect_text(item, out)),
        _ => {}
    }
}

fn str_at<'a>(value: &'a Value, key: &str) -> &'a str {
    value[key].as_str().unwrap_or_default()
}

fn observe_openai_chat(body: &Value) -> Observed {
    let mut observed = Observed {
        model: str_at(body, "model").to_string(),
        ..Observed::default()
    };
    for tool in body["tools"].as_array().into_iter().flatten() {
        observed
            .tool_names
            .insert(str_at(&tool["function"], "name").to_string());
    }
    for message in body["messages"].as_array().into_iter().flatten() {
        for call in message["tool_calls"].as_array().into_iter().flatten() {
            observed.calls.push((
                str_at(call, "id").to_string(),
                str_at(&call["function"], "name").to_string(),
            ));
        }
        if message["role"] == "tool" {
            observed.results.push(ObservedResult::new(
                str_at(message, "tool_call_id"),
                None,
                &message["content"],
            ));
            continue;
        }
        collect_text(&message["content"], &mut observed.text);
    }
    observed
}

fn observe_openai_responses(body: &Value) -> Observed {
    let mut observed = Observed {
        model: str_at(body, "model").to_string(),
        ..Observed::default()
    };
    for tool in body["tools"].as_array().into_iter().flatten() {
        observed.tool_names.insert(str_at(tool, "name").to_string());
    }
    collect_text(&body["instructions"], &mut observed.text);
    for item in body["input"].as_array().into_iter().flatten() {
        match str_at(item, "type") {
            "function_call" => observed.calls.push((
                str_at(item, "call_id").to_string(),
                str_at(item, "name").to_string(),
            )),
            "function_call_output" => observed.results.push(ObservedResult::new(
                str_at(item, "call_id"),
                None,
                &item["output"],
            )),
            _ => collect_text(&item["content"], &mut observed.text),
        }
    }
    observed
}

fn observe_anthropic(body: &Value) -> Observed {
    let mut observed = Observed {
        model: str_at(body, "model").to_string(),
        ..Observed::default()
    };
    for tool in body["tools"].as_array().into_iter().flatten() {
        observed.tool_names.insert(str_at(tool, "name").to_string());
    }
    collect_text(&body["system"], &mut observed.text);
    for message in body["messages"].as_array().into_iter().flatten() {
        match &message["content"] {
            Value::Array(blocks) => {
                for block in blocks {
                    match str_at(block, "type") {
                        "tool_use" => observed.calls.push((
                            str_at(block, "id").to_string(),
                            str_at(block, "name").to_string(),
                        )),
                        "tool_result" => observed.results.push(ObservedResult::new(
                            str_at(block, "tool_use_id"),
                            block["is_error"].as_bool(),
                            &block["content"],
                        )),
                        _ => collect_text(block, &mut observed.text),
                    }
                }
            }
            other => collect_text(other, &mut observed.text),
        }
    }
    observed
}

/// Gemini has no call identifiers: a `functionResponse` is paired with its
/// `functionCall` by function name and order, so the name is the identity.
fn observe_gemini(body: &Value) -> Observed {
    let mut observed = Observed::default();
    for tool in body["tools"].as_array().into_iter().flatten() {
        for declaration in tool["functionDeclarations"]
            .as_array()
            .or_else(|| tool["function_declarations"].as_array())
            .into_iter()
            .flatten()
        {
            observed
                .tool_names
                .insert(str_at(declaration, "name").to_string());
        }
    }
    collect_text(&body["systemInstruction"], &mut observed.text);
    collect_text(&body["system_instruction"], &mut observed.text);
    for content in body["contents"].as_array().into_iter().flatten() {
        for part in content["parts"].as_array().into_iter().flatten() {
            if let Some(call) = part.get("functionCall") {
                let name = str_at(call, "name").to_string();
                observed.calls.push((name.clone(), name));
            } else if let Some(response) = part.get("functionResponse") {
                observed.results.push(ObservedResult::new(
                    str_at(response, "name"),
                    None,
                    &response["response"],
                ));
            } else {
                collect_text(part, &mut observed.text);
            }
        }
    }
    observed
}

fn render_all() -> Vec<(&'static str, Observed)> {
    let openai = OpenAiClient::new("test-key").expect("openai client");
    let anthropic = AnthropicClient::new("test-key").expect("anthropic client");
    let google = GoogleClient::new("test-key");
    let vertex = VertexAiClient::new(
        "matrix-project",
        "us-central1",
        Some("test-key".into()),
        None,
    );
    let kimi = KimiK3Client::new("test-key", "https://kimi.test").expect("kimi client");
    let messages = script();

    let anthropic_body = anthropic
        .build_request_body(&messages, &config("anthropic/claude-sonnet-4-20250514"))
        .expect("anthropic body");
    let google_body = google
        .request_body_json(&messages, &config("google/gemini-2.5-pro"))
        .expect("google request");
    let vertex_body = vertex
        .request_body_json(&messages, &config("vertex/gemini-2.5-pro"))
        .expect("vertex request");

    vec![
        (
            "openai-chat",
            observe_openai_chat(
                &openai
                    .build_request_body_for_api(&messages, &config("openai/gpt-4.1"), false)
                    .expect("OpenAI Chat request"),
            ),
        ),
        (
            "openai-responses",
            observe_openai_responses(
                &openai
                    .build_request_body_for_api(&messages, &config("openai/gpt-5.5"), true)
                    .expect("OpenAI Responses request"),
            ),
        ),
        ("anthropic", observe_anthropic(&anthropic_body)),
        ("google", observe_gemini(&google_body)),
        ("vertex", observe_gemini(&vertex_body)),
        (
            "kimi",
            observe_openai_chat(&kimi.build_request_body(&messages, &config("moonshot/kimi-k3"))),
        ),
    ]
}

#[test]
fn every_provider_strips_the_route_prefix_from_the_model() {
    for (provider, observed) in render_all() {
        if observed.model.is_empty() {
            continue; // Gemini carries the model in the URL, not the body.
        }
        assert!(
            !observed.model.contains('/'),
            "{provider}: model `{}` still carries a provider prefix",
            observed.model
        );
    }
}

#[test]
fn every_provider_declares_every_tool_from_the_request_config() {
    for (provider, observed) in render_all() {
        let expected: BTreeSet<String> = [TOOL_A, TOOL_B].into_iter().map(String::from).collect();
        assert_eq!(
            observed.tool_names, expected,
            "{provider}: tool declarations drifted"
        );
    }
}

#[test]
fn every_provider_pairs_each_tool_call_with_exactly_one_result() {
    for (provider, observed) in render_all() {
        assert_eq!(
            observed.calls.len(),
            2,
            "{provider}: expected both scripted tool calls, got {:?}",
            observed.calls
        );
        assert_eq!(
            observed
                .calls
                .iter()
                .map(|(_, name)| name.as_str())
                .collect::<Vec<_>>(),
            vec![TOOL_A, TOOL_B],
            "{provider}: tool call order changed"
        );
        let call_ids: Vec<&str> = observed.calls.iter().map(|(id, _)| id.as_str()).collect();
        let result_ids: Vec<&str> = observed
            .results
            .iter()
            .map(|result| result.id.as_str())
            .collect();
        assert_eq!(
            result_ids, call_ids,
            "{provider}: every call must be answered once, in order, with the same identity \
             (interrupted calls must be completed with an error result, never dropped)"
        );
        let unique: BTreeSet<&str> = call_ids.iter().copied().collect();
        assert_eq!(
            unique.len(),
            call_ids.len(),
            "{provider}: distinct calls collapsed onto one wire identity"
        );
    }
}

#[test]
fn every_provider_reports_an_interrupted_call_as_an_error_not_a_success() {
    for (provider, observed) in render_all() {
        let Some(interrupted) = observed
            .results
            .iter()
            .find(|result| result.id != CALL_A && result.id != TOOL_A)
        else {
            panic!("{provider}: no result for the interrupted call");
        };
        if let Some(is_error) = interrupted.is_error {
            assert!(
                is_error,
                "{provider}: a synthesized result for an interrupted call must be marked as an error"
            );
        }
        assert!(
            interrupted.content.contains(INTERRUPTED_MARKER),
            "{provider}: the model must be told the call was interrupted, wire content was: {}",
            interrupted.content
        );
        let first = observed
            .results
            .iter()
            .find(|result| result.id == CALL_A || result.id == TOOL_A)
            .expect("result for the completed call");
        assert!(
            first.content.contains(RESULT_A),
            "{provider}: completed tool result payload missing from the wire"
        );
        assert!(
            !observed.text.contains(RESULT_A),
            "{provider}: tool result leaked into plain text"
        );
    }
}

#[test]
fn every_provider_keeps_the_conversation_text_in_order_without_duplication() {
    for (provider, observed) in render_all() {
        for fragment in [SYSTEM_PROMPT, USER_ONE, ASSISTANT_TEXT, USER_TWO] {
            assert_eq!(
                observed.text.matches(fragment).count(),
                1,
                "{provider}: `{fragment}` must appear exactly once, text was:\n{}",
                observed.text
            );
        }
        let positions: Vec<usize> = [USER_ONE, ASSISTANT_TEXT, USER_TWO]
            .iter()
            .map(|fragment| observed.text.find(fragment).expect("fragment present"))
            .collect();
        assert!(
            positions.windows(2).all(|pair| pair[0] < pair[1]),
            "{provider}: conversation order changed"
        );
    }
}

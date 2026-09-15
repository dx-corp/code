use super::tests::*;
use super::{ExternalToolSchemaPolicy, NativeAgentConfig};
use crate::agent::FromAgent;
use crate::ai::UnifiedClient;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;

pub(super) fn profile_fixture_definitions() -> HashMap<String, crate::agent::ToolDefinition> {
    [
        "bash",
        "read",
        "write",
        "tool_search",
        "explore",
        "get_rlm_context",
        "set_rlm_context",
        "websearch",
        "vscode_get_definition",
        "get_goal",
        "update_goal",
    ]
    .into_iter()
    .map(|name| {
        (
            name.to_owned(),
            crate::agent::ToolDefinition {
                tool: crate::ai::Tool::new(name, format!("fixture {name}"))
                    .with_schema(serde_json::json!({"type": "object", "properties": {}})),
                requires_approval: false,
            },
        )
    })
    .collect()
}

#[test]
fn fast_tool_profile_is_small_but_has_an_escape_hatch() {
    let definitions = profile_fixture_definitions();
    let active = super::deferred_tool_schemas::initial_active_tool_names(
        super::deferred_tool_schemas::ToolProfile::Fast,
        &definitions,
        &HashSet::new(),
        None,
        ExternalToolSchemaPolicy::Eager,
    );
    for expected in ["read", "bash", "tool_search", "explore"] {
        assert!(active.contains(expected));
    }
    for excluded in [
        "get_rlm_context",
        "set_rlm_context",
        "websearch",
        "vscode_get_definition",
    ] {
        assert!(!active.contains(excluded));
    }
}

#[test]
fn all_tool_profile_preserves_every_registered_tool() {
    let definitions = profile_fixture_definitions();
    let active = super::deferred_tool_schemas::initial_active_tool_names(
        super::deferred_tool_schemas::ToolProfile::All,
        &definitions,
        &HashSet::new(),
        None,
        ExternalToolSchemaPolicy::Eager,
    );
    assert_eq!(active.len(), definitions.len());
}

#[test]
fn fast_tool_search_blocks_rlm_unless_explicitly_allowed() {
    let allows = super::deferred_tool_schemas::tool_search_profile_allows;
    let profile = super::deferred_tool_schemas::ToolProfile::Fast;
    assert!(!allows(profile, "set_rlm_context", &HashSet::new()));
    assert!(allows(
        profile,
        "set_rlm_context",
        &HashSet::from([String::from("set_rlm_context")])
    ));
    assert!(allows(
        super::deferred_tool_schemas::ToolProfile::All,
        "set_rlm_context",
        &HashSet::new()
    ));
}

#[test]
fn all_tool_profile_cannot_widen_a_governed_registry() {
    let allowed = HashSet::from([String::from("read")]);
    let definitions = profile_fixture_definitions()
        .into_iter()
        .filter(|(name, _)| allowed.contains(name))
        .collect::<HashMap<_, _>>();
    let active = super::deferred_tool_schemas::initial_active_tool_names(
        super::deferred_tool_schemas::ToolProfile::All,
        &definitions,
        &HashSet::new(),
        Some(&allowed),
        ExternalToolSchemaPolicy::Eager,
    );
    assert_eq!(active, allowed);
}

#[test]
fn explicit_allowed_tools_override_fast_profile() {
    let definitions = profile_fixture_definitions();
    let allowed = HashSet::from([String::from("websearch"), String::from("set_rlm_context")]);
    let active = super::deferred_tool_schemas::initial_active_tool_names(
        super::deferred_tool_schemas::ToolProfile::Fast,
        &definitions,
        &HashSet::new(),
        Some(&allowed),
        ExternalToolSchemaPolicy::Eager,
    );
    assert!(active.contains("websearch"));
    assert!(active.contains("set_rlm_context"));
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
    let mut definitions = profile_fixture_definitions();
    definitions.insert(
        "client_calendar".into(),
        external_tool_definition("client_calendar"),
    );
    let external = HashSet::from([String::from("client_calendar")]);
    let active = super::deferred_tool_schemas::initial_active_tool_names(
        super::ToolProfile::Fast,
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
                let start = serde_json::json!({
                    "id":"deferred-tools","object":"chat.completion.chunk","created":0,
                    "model":"gpt-4o","choices":[{"index":0,
                    "delta":{"role":"assistant","content":""},"finish_reason":null}]
                });
                let tool = serde_json::json!({
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

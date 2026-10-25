#[allow(unused_imports)]
use super::*;

#[tokio::test(flavor = "current_thread")]
pub(crate) async fn remote_scoped_static_keys_cannot_list_tenant_resources() {
    let _guard = ENV_LOCK.lock().await;
    let snapshot = snapshot_env(&["MAESTRO_WEB_API_KEY_SCOPES"]);
    env::set_var("MAESTRO_WEB_API_KEY_SCOPES", "maestro:write");
    let base_state = test_app_state_with_sessions(HashMap::new());
    let mut remote_config = auth_test_config();
    remote_config.listen_host = "0.0.0.0".to_string();
    let state = AppState {
        config: Arc::new(remote_config),
        ..base_state
    };

    let session_request =
        b"GET /api/sessions HTTP/1.1\r\nHost: localhost\r\nx-maestro-api-key: api-key\r\n\r\n";
    let mut session_initial = session_request.to_vec();
    let session_head = parse_request_head(&session_initial).expect("session request should parse");
    let (_client, mut session_server) = tcp_stream_pair().await;
    let session_response = handle_session_endpoint(
        &mut session_server,
        &mut session_initial,
        &session_head,
        &state,
    )
    .await;
    assert_eq!(response_status(&session_response), 403);

    let task_request =
        b"GET /tasks HTTP/1.1\r\nHost: localhost\r\nx-maestro-api-key: api-key\r\n\r\n";
    let mut task_initial = task_request.to_vec();
    let task_head = parse_request_head(&task_initial).expect("task request should parse");
    let (_client, mut task_server) = tcp_stream_pair().await;
    let task_response =
        handle_a2a_endpoint(&mut task_server, &mut task_initial, task_head, &state).await;
    assert_eq!(response_status(&task_response), 403);

    restore_env(snapshot);
}

#[test]
pub(crate) fn pending_request_resume_maps_approval_and_tool_results() {
    let approval = pending_request_resume_value(
        "approval-1",
        &serde_json::json!({ "kind": "approval", "decision": "denied" }),
    );
    assert_eq!(
        approval
            .pointer("/request/resolution")
            .and_then(Value::as_str),
        Some("denied")
    );

    let tool = pending_request_resume_value(
        "tool-1",
        &serde_json::json!({ "content": [], "isError": true }),
    );
    assert_eq!(
        tool.pointer("/request/kind").and_then(Value::as_str),
        Some("client_tool")
    );
    assert_eq!(
        tool.pointer("/request/resolution").and_then(Value::as_str),
        Some("failed")
    );

    let (approved, result) = pending_tool_response_from_payload(
        &serde_json::json!({ "kind": "approval", "decision": "denied" }),
    );
    assert!(!approved);
    assert!(result.is_none());

    let (approved, result) =
        pending_tool_response_from_payload(&serde_json::json!({ "content": "ok" }));
    assert!(approved);
    assert_eq!(result.expect("tool result").output, "ok");

    let (approved, result) = pending_tool_response_from_payload(
        &serde_json::json!({ "content": "failed", "isError": true }),
    );
    assert!(approved);
    assert!(!result.expect("failed tool result").success);
}

#[test]
pub(crate) fn resolves_provider_model_ids() {
    let registry = ModelRegistry {
        models: builtin_models(),
        aliases: HashMap::new(),
    };
    let model = resolve_model("openai/gpt-5.5", &registry).expect("model should resolve");

    assert_eq!(model.provider, "openai");
    assert_eq!(model.id, "gpt-5.5");
    assert_eq!(model.api, "openai-responses");

    let codex_app_server = resolve_model("openai-codex/gpt-5.5", &registry)
        .expect("codex app-server model should resolve");
    assert_eq!(codex_app_server.provider, "openai-codex");
    assert_eq!(codex_app_server.id, "gpt-5.5");
    assert_eq!(codex_app_server.api, "openai-codex-app-server");
}

#[test]
pub(crate) fn resolves_configured_models_and_aliases() {
    let config = serde_json::json!({
        "aliases": { "fast": "local/llama-fast" },
        "providers": [{
            "id": "local",
            "name": "Local",
            "api": "openai-responses",
            "models": [{
                "id": "llama-fast",
                "name": "Llama Fast",
                "reasoning": false,
                "input": ["text", "image"],
                "contextWindow": 8192,
                "maxTokens": 2048,
                "cost": {
                    "input": 0,
                    "output": 0,
                    "cacheRead": 0,
                    "cacheWrite": 0
                }
            }]
        }]
    });
    let mut registry = ModelRegistry {
        models: builtin_models(),
        aliases: HashMap::new(),
    };
    merge_configured_models(&mut registry, &config);

    let model = resolve_model("fast", &registry).expect("alias should resolve");

    assert_eq!(model.provider, "local");
    assert_eq!(model.id, "llama-fast");
    assert!(model.capabilities.vision);
}

#[test]
pub(crate) fn merges_platform_model_catalog_from_llm_gateway_payload() {
    let catalog = serde_json::json!({
        "data": [{
            "id": "openai",
            "models": [{
                "id": "gpt-5.1-codex-max",
                "name": "GPT-5.1 Codex Max",
                "provider": "openai",
                "pricing": {
                    "input": 1.25,
                    "output": 10.0
                },
                "capabilities": {
                    "context_length": 400000,
                    "max_tokens": 128000,
                    "supports_streaming": true,
                    "supports_functions": true,
                    "supports_vision": true
                },
                "supports_reasoning": true
            }]
        }],
        "external_providers": [{
            "id": "together-ai",
            "models": [{
                "id": "meta-llama/Llama-3.3-70B-Instruct-Turbo",
                "name": "Llama 3.3 70B Instruct Turbo",
                "reasoning": false,
                "tool_call": true,
                "cost": {
                    "input": 0.88,
                    "output": 0.88,
                    "cache_read": 0.0,
                    "cache_write": 0.0
                },
                "limit": {
                    "context": 131072,
                    "output": 4096
                },
                "modalities": {
                    "input": ["text", "image"],
                    "output": ["text"]
                }
            }]
        }]
    });
    let mut registry = ModelRegistry {
        models: builtin_models(),
        aliases: HashMap::new(),
    };

    merge_llm_gateway_model_catalog(&mut registry, &catalog);

    let codex = resolve_model("openai/gpt-5.1-codex-max", &registry).expect("codex model");
    assert_eq!(codex.max_tokens, 128_000);
    assert_eq!(codex.api, "openai-responses");
    assert!(codex.capabilities.reasoning);

    let codex_app_server =
        resolve_model("openai-codex/gpt-5.5", &registry).expect("codex app-server model");
    assert_eq!(codex_app_server.api, "openai-codex-app-server");
    assert_eq!(codex_app_server.context_window, 1_050_000);
    // The shared models.dev snapshot carries no output-token limit; the
    // gateway overlay supplies one only for models it describes.
    assert_eq!(codex_app_server.max_tokens, 0);

    let llama = resolve_model(
        "together-ai/meta-llama/Llama-3.3-70B-Instruct-Turbo",
        &registry,
    )
    .expect("external model");
    assert_eq!(llama.context_window, 131_072);
    assert!(llama.capabilities.vision);
    assert_eq!(llama.cost.input, 0.88);
}

#[test]
pub(crate) fn merges_openrouter_model_catalog_payload() {
    let catalog = serde_json::json!({
        "data": [{
            "id": "anthropic/claude-sonnet-4.5",
            "name": "Anthropic: Claude Sonnet 4.5",
            "context_length": 200000,
            "architecture": {
                "input_modalities": ["text", "image"],
                "output_modalities": ["text"]
            },
            "pricing": {
                "prompt": "0.000003",
                "completion": "0.000015",
                "input_cache_read": "0.0000003",
                "input_cache_write": "0.00000375"
            },
            "top_provider": {
                "context_length": 200000,
                "max_completion_tokens": 64000,
                "is_moderated": true
            },
            "supported_parameters": [
                "include_reasoning",
                "max_tokens",
                "reasoning",
                "tool_choice",
                "tools"
            ]
        }]
    });
    let mut registry = ModelRegistry {
        models: builtin_models(),
        aliases: HashMap::new(),
    };

    merge_llm_gateway_model_catalog(&mut registry, &catalog);
    let model = resolve_model("openrouter/anthropic/claude-sonnet-4.5", &registry)
        .expect("openrouter model should resolve");

    assert_eq!(model.provider, "openrouter");
    assert_eq!(model.id, "anthropic/claude-sonnet-4.5");
    assert_eq!(model.api, "openai-completions");
    assert_eq!(model.context_window, 200_000);
    assert_eq!(model.max_tokens, 64_000);
    assert!(model.capabilities.vision);
    assert!(model.capabilities.tools);
    assert!(model.capabilities.reasoning);
    assert_eq!(model.cost.input, 0.000003);
    assert_eq!(model.cost.cache_write, 0.00000375);
}

#[test]
pub(crate) fn default_model_handles_empty_registry() {
    let registry = ModelRegistry {
        models: Vec::new(),
        aliases: HashMap::new(),
    };

    let model = default_model_from_registry(&registry);

    assert_eq!(model.provider, "openai-codex");
    assert_eq!(model.id, "gpt-5.6");
    assert_eq!(model.api, "openai-codex-app-server");
}

#[test]
pub(crate) fn emergency_default_model_is_available_without_registry_entries() {
    let model = emergency_default_model();

    assert_eq!(model.provider, "openai-codex");
    assert_eq!(model.id, "gpt-5.6");
    assert_eq!(model.api, "openai-codex-app-server");
}

#[test]
pub(crate) fn approval_blocked_tool_event_uses_contract_tool_end_shape() {
    let event = approval_blocked_tool_event("call-1", "bash");

    assert_eq!(event["type"], "tool_execution_end");
    assert_eq!(event["toolCallId"], "call-1");
    assert_eq!(event["toolName"], "bash");
    assert_eq!(event["isError"], true);
    assert_eq!(event["result"]["isError"], true);
    assert_eq!(
        event["result"]["content"][0]["text"],
        "Tool execution blocked by approval mode"
    );
}

#[test]
pub(crate) fn failed_tool_metadata_is_marked_error_for_replay() {
    let mut tools = Vec::new();
    record_tool_call_metadata(&mut tools, "tool-1", "bash", serde_json::json!({}));

    finish_tool_metadata(&mut tools, "tool-1", false);

    assert_eq!(tools[0]["status"], "error");
    assert_eq!(tools[0]["result"]["isError"], true);
}

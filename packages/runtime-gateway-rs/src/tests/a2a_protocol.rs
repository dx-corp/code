#[allow(unused_imports)]
use super::*;

#[test]
pub(crate) fn a2a_platform_conflict_detection_requires_bounded_status() {
    assert!(platform_error_is_conflict(
        "POST /Register returned 409: already exists"
    ));
    assert!(platform_error_is_conflict("agent already_exists"));
    assert!(!platform_error_is_conflict(
        "POST http://127.0.0.1:4090/Register failed: connection refused"
    ));
    assert!(!platform_error_is_conflict(
        "POST /Register returned 500: trace id 4090"
    ));
}

#[test]
pub(crate) fn csrf_validation_requires_matching_token_for_mutating_api_and_a2a_requests() {
    let mut config = auth_test_config();
    config.require_csrf = true;
    config.csrf_token = Some("csrf-token".to_string());

    assert!(validate_csrf(&csrf_head("POST", Some("csrf-token")), &config).is_ok());
    assert!(validate_csrf(&csrf_head("POST", Some("wrong-token")), &config).is_err());
    assert!(validate_csrf(&csrf_head("POST", None), &config).is_err());
    assert!(
        validate_csrf(
            &csrf_head_for_path("POST", "/message:send", Some("csrf-token")),
            &config,
        )
        .is_ok()
    );
    assert!(validate_csrf(&csrf_head_for_path("POST", "/message:send", None), &config).is_err());
    assert!(
        validate_csrf(
            &csrf_head_for_path("POST", "/message:stream", Some("csrf-token")),
            &config,
        )
        .is_ok()
    );
    assert!(
        validate_csrf(
            &csrf_head_for_path("POST", "/message:stream", None),
            &config
        )
        .is_err()
    );
    assert!(
        validate_csrf(
            &csrf_head_for_path(
                "POST",
                "/tasks/maestro-task-1:subscribe",
                Some("csrf-token")
            ),
            &config,
        )
        .is_ok()
    );
    assert!(
        validate_csrf(
            &csrf_head_for_path("POST", "/tasks/maestro-task-1:subscribe", None),
            &config
        )
        .is_err()
    );
    assert!(
        validate_csrf(
            &csrf_head_for_path("POST", "/tasks/maestro-task-1:cancel", Some("csrf-token")),
            &config,
        )
        .is_ok()
    );
    assert!(
        validate_csrf(
            &csrf_head_for_path("POST", "/tasks/maestro-task-1:cancel", None),
            &config
        )
        .is_err()
    );
    assert!(validate_csrf(&csrf_head("GET", None), &config).is_ok());
}

#[test]
pub(crate) fn detects_a2a_control_plane_routes() {
    for request in [
        "GET /.well-known/agent-card.json HTTP/1.1\r\nHost: localhost\r\n\r\n",
        "POST /message:send HTTP/1.1\r\nHost: localhost\r\n\r\n",
        "GET /tasks/maestro-task-1 HTTP/1.1\r\nHost: localhost\r\n\r\n",
        "GET /tasks HTTP/1.1\r\nHost: localhost\r\n\r\n",
        "GET /extendedAgentCard HTTP/1.1\r\nHost: localhost\r\n\r\n",
        "GET /tasks/maestro-task-1/pushNotificationConfigs HTTP/1.1\r\nHost: localhost\r\n\r\n",
        "POST /tasks/maestro-task-1/pushNotificationConfigs HTTP/1.1\r\nHost: localhost\r\n\r\n",
        "DELETE /tasks/maestro-task-1/pushNotificationConfigs/config-1 HTTP/1.1\r\nHost: localhost\r\n\r\n",
        "POST /tasks/maestro-task-1:cancel HTTP/1.1\r\nHost: localhost\r\n\r\n",
        "OPTIONS /message:send HTTP/1.1\r\nHost: localhost\r\n\r\n",
    ] {
        let head = parse_request_head(request.as_bytes()).expect("request should parse");
        assert!(is_a2a_endpoint(&head), "{request} should be A2A");
    }
}

#[test]
pub(crate) fn a2a_state_helpers_accept_protocol_variants_without_substring_matches() {
    assert_eq!(canonical_a2a_task_state("success"), "TASK_STATE_COMPLETED");
    assert_eq!(
        canonical_a2a_task_state("SUCCEEDED"),
        "TASK_STATE_COMPLETED"
    );
    assert_eq!(
        canonical_a2a_task_state("completed"),
        canonical_a2a_task_state("success")
    );
    assert_eq!(canonical_a2a_task_state("cancelled"), "TASK_STATE_CANCELED");
    assert_eq!(
        canonical_a2a_task_state("input required"),
        "TASK_STATE_INPUT_REQUIRED"
    );
    assert!(a2a_state_is_completed("success"));
    assert!(a2a_state_is_completed("TASK_STATE_SUCCEEDED"));
    assert!(a2a_state_is_completed(" TASK_STATE_COMPLETED "));
    assert!(a2a_state_is_completed("TASK  STATE--COMPLETED"));
    assert!(a2a_task_is_terminal(&a2a_task_value(
        "task-success",
        "ctx-success",
        "task-state-success",
        a2a_agent_message("ctx-success", "done"),
        Vec::new(),
        Vec::new(),
        serde_json::json!({}),
    )));
    assert!(a2a_state_is_failed("TASK_STATE_CANCELLED"));
    assert!(!a2a_state_is_completed("TASK_STATE_UNSUCCESSFUL"));
    assert!(!a2a_task_is_terminal(&a2a_task_value(
        "task-working",
        "ctx-working",
        "TASK_STATE_UNSUCCESSFUL",
        a2a_agent_message("ctx-working", "still not complete"),
        Vec::new(),
        Vec::new(),
        serde_json::json!({}),
    )));
}

#[tokio::test(flavor = "current_thread")]
pub(crate) async fn a2a_failed_turn_attaches_work_graph_with_run_ids() {
    let _guard = ENV_LOCK.lock().await;
    let previous_fake = env::var("MAESTRO_A2A_FAKE_RESPONSE").ok();
    env::remove_var("MAESTRO_A2A_FAKE_RESPONSE");
    let mut expired = valid_code_writer_capsule();
    expired["capsule"]["deadlineAt"] = serde_json::json!("2000-01-01T00:00:00Z");
    let state = test_app_state_with_sessions(HashMap::new());
    let (_cancel_tx, cancel_rx) = watch::channel(false);

    let metadata = serde_json::json!({
        A2A_SUBAGENT_REQUEST_METADATA_PATH: expired,
        "platformRunId": "platform-run-9047",
        "agentRunId": "agent-run-9047",
        "vfs_context_file_count": 4,
        "vfs_hydrated_file_count": 0
    });
    // Check the same decoder used by completion before executing: an absent
    // capsule would select an ordinary native turn and could contact a provider.
    let capsule = crate::a2a::completion_subagent_capsule_from_metadata(&metadata)
        .expect("valid completion capsule")
        .expect("canonical governed capsule is present");
    assert!(crate::a2a::build_a2a_subagent_execution_policy_for_state(&state, &capsule).is_err());
    let task = tokio::time::timeout(
        Duration::from_secs(5),
        complete_a2a_task(
            &state,
            "implement this".to_string(),
            "failed-task-9047".to_string(),
            "ctx-9047".to_string(),
            Vec::new(),
            metadata,
            cancel_rx,
        ),
    )
    .await
    .expect("missing execution policy must fail before provider execution");
    if let Some(previous_fake) = previous_fake {
        env::set_var("MAESTRO_A2A_FAKE_RESPONSE", previous_fake);
    }
    assert!(
        task.to_string()
            .contains("governed task capsule is missing its pre-claim execution policy")
    );

    assert_eq!(task["status"]["state"], "TASK_STATE_FAILED");
    assert_eq!(
        task["metadata"]["workGraph"]["codexSubagents"]["schemaVersion"],
        CODEX_SUBAGENT_WORK_GRAPH_SCHEMA
    );
    assert!(
        task["metadata"]["workGraph"]["correlationPath"]
            .as_str()
            .expect("correlation path")
            .contains("platform_agent_run_id=platform-run-9047"),
        "work graph correlation path should join the platform run id: {}",
        task["metadata"]["workGraph"]["correlationPath"]
    );
    assert_eq!(task["metadata"]["workGraph"]["state"], "failed");
}

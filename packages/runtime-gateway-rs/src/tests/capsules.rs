#[allow(unused_imports)]
use super::*;

pub(crate) fn valid_code_writer_capsule() -> Value {
    serde_json::json!({
        "skillId": "maestro.subagent.code-writer",
        "capsule": {
            "capsuleVersion": "evalops.maestro.task-capsule.v1",
            "taskId": "task-1",
            "parentTaskId": "mission-1",
            "laneId": "code-writer",
            "taskClass": "code.implementation",
            "objective": "Add the bounded parser and its regression test.",
            "inScope": {
                "paths": ["packages/runtime-gateway-rs/src/a2a"],
                "resources": []
            },
            "outOfScope": ["deployment"],
            "contextArtifacts": [{
                "artifactId": "context-1",
                "sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
            }],
            "allowedCapabilities": [
                "repo:read",
                "repo:write-scoped",
                "tool:execute-tests"
            ],
            "mutationBoundary": {
                "paths": ["packages/runtime-gateway-rs/src/a2a"],
                "resources": []
            },
            "expectedArtifactKinds": ["patch.summary", "test.report"],
            "acceptanceChecks": [
                "cargo test -p maestro-runtime-gateway subagent_capsule"
            ],
            "stopConditions": ["scope expansion required"],
            "retryLimit": 2,
            "deadlineAt": "2099-07-31T00:00:00Z",
            "modelRoute": "haiku",
            "review": {
                "required": true,
                "laneId": "code-review"
            }
        }
    })
}

pub(crate) fn valid_code_review_capsule() -> Value {
    let mut request = valid_code_writer_capsule();
    request["skillId"] = serde_json::json!("maestro.subagent.code-review");
    request["capsule"]["laneId"] = serde_json::json!("code-review");
    request["capsule"]["taskClass"] = serde_json::json!("code.review");
    request["capsule"]["allowedCapabilities"] = serde_json::json!(["repo:read"]);
    request["capsule"]["mutationBoundary"]["paths"] = serde_json::json!([]);
    request["capsule"]["expectedArtifactKinds"] = serde_json::json!(["review.summary"]);
    request["capsule"]["review"] = serde_json::json!({"required": false});
    request
}

pub(crate) fn valid_browser_qa_capsule() -> Value {
    let mut request = valid_code_writer_capsule();
    request["skillId"] = serde_json::json!("maestro.subagent.browser-qa");
    request["capsule"]["laneId"] = serde_json::json!("browser-qa");
    request["capsule"]["taskClass"] = serde_json::json!("product.qa");
    request["capsule"]["allowedCapabilities"] =
        serde_json::json!(["browser:control", "artifact:write", "runtime:events:read"]);
    request["capsule"]["mutationBoundary"]["paths"] = serde_json::json!([]);
    request["capsule"]["expectedArtifactKinds"] = serde_json::json!(["qa.repro-report"]);
    request["capsule"]["review"] = serde_json::json!({"required": false});
    request
}

#[test]
pub(crate) fn valid_code_writer_capsule_is_accepted() {
    let capsule = crate::a2a::validate_subagent_capsule(
        &valid_code_writer_capsule(),
        "maestro.subagent.code-writer",
    )
    .expect("valid capsule should be accepted");
    assert_eq!(capsule.lane_id, "code-writer");
}

#[test]
pub(crate) fn capsule_rejects_scope_broadening() {
    let mut request = valid_code_writer_capsule();
    request["capsule"]["mutationBoundary"]["paths"] = serde_json::json!(["../deploy"]);

    assert!(matches!(
        crate::a2a::validate_subagent_capsule(&request, "maestro.subagent.code-writer"),
        Err(crate::a2a::CapsuleValidationError::ScopeBroadening { .. })
    ));
}

#[test]
pub(crate) fn capsule_rejects_lane_skill_or_artifact_mismatch() {
    let mut request = valid_code_writer_capsule();
    request["capsule"]["laneId"] = serde_json::json!("code-review");

    assert!(
        crate::a2a::validate_subagent_capsule(&request, "maestro.subagent.code-writer").is_err()
    );
}

#[test]
pub(crate) fn capsule_rejects_unknown_version_task_class_and_missing_grant() {
    let mut wrong_version = valid_code_writer_capsule();
    wrong_version["capsule"]["capsuleVersion"] = serde_json::json!("v0");
    assert!(matches!(
        crate::a2a::validate_subagent_capsule(&wrong_version, "maestro.subagent.code-writer"),
        Err(crate::a2a::CapsuleValidationError::UnsupportedVersion { .. })
    ));

    let mut denied_class = valid_code_writer_capsule();
    denied_class["capsule"]["taskClass"] = serde_json::json!("secret.exfiltration");
    assert!(matches!(
        crate::a2a::validate_subagent_capsule(&denied_class, "maestro.subagent.code-writer"),
        Err(crate::a2a::CapsuleValidationError::DeniedTaskClass { .. })
    ));

    let mut missing_grant = valid_code_writer_capsule();
    missing_grant["capsule"]["allowedCapabilities"] =
        serde_json::json!(["repo:read", "repo:write-scoped"]);
    assert!(matches!(
        crate::a2a::validate_subagent_capsule(&missing_grant, "maestro.subagent.code-writer"),
        Err(crate::a2a::CapsuleValidationError::MissingCapability { .. })
    ));

    let mut uncontracted_grant = valid_code_writer_capsule();
    uncontracted_grant["capsule"]["allowedCapabilities"]
        .as_array_mut()
        .expect("capabilities should be an array")
        .push(serde_json::json!("secret:read"));
    assert!(matches!(
        crate::a2a::validate_subagent_capsule(&uncontracted_grant, "maestro.subagent.code-writer"),
        Err(crate::a2a::CapsuleValidationError::UnexpectedCapability { .. })
    ));
}

#[test]
pub(crate) fn capsule_rejects_uncontracted_artifacts_and_missing_required_artifacts() {
    let mut uncontracted = valid_code_writer_capsule();
    uncontracted["capsule"]["expectedArtifactKinds"] =
        serde_json::json!(["patch.summary", "database.dump"]);
    assert!(matches!(
        crate::a2a::validate_subagent_capsule(&uncontracted, "maestro.subagent.code-writer"),
        Err(crate::a2a::CapsuleValidationError::ArtifactMismatch { .. })
    ));

    let mut missing_required = valid_code_writer_capsule();
    missing_required["capsule"]["expectedArtifactKinds"] = serde_json::json!(["test.report"]);
    assert!(matches!(
        crate::a2a::validate_subagent_capsule(&missing_required, "maestro.subagent.code-writer"),
        Err(crate::a2a::CapsuleValidationError::ArtifactMismatch { .. })
    ));
}

#[test]
pub(crate) fn capsule_rejects_non_normalized_or_out_of_scope_mutation_paths() {
    for invalid_path in [
        "/tmp/deploy",
        "packages/runtime-gateway-rs/src/../deploy",
        "packages//control-plane-rs",
        "packages\\control-plane-rs",
        "packages/runtime-gateway-rs/src/a2a/",
        "",
    ] {
        let mut request = valid_code_writer_capsule();
        request["capsule"]["mutationBoundary"]["paths"] = serde_json::json!([invalid_path]);
        assert!(
            crate::a2a::validate_subagent_capsule(&request, "maestro.subagent.code-writer")
                .is_err(),
            "{invalid_path:?} should be rejected"
        );
    }

    let mut out_of_scope = valid_code_writer_capsule();
    out_of_scope["capsule"]["mutationBoundary"]["paths"] =
        serde_json::json!(["packages/runtime-gateway-rs/src/chat.rs"]);
    assert!(matches!(
        crate::a2a::validate_subagent_capsule(&out_of_scope, "maestro.subagent.code-writer"),
        Err(crate::a2a::CapsuleValidationError::ScopeBroadening { .. })
    ));

    let mut read_only_mutation = valid_code_review_capsule();
    read_only_mutation["capsule"]["mutationBoundary"]["paths"] =
        serde_json::json!(["packages/runtime-gateway-rs/src/a2a"]);
    assert!(matches!(
        crate::a2a::validate_subagent_capsule(&read_only_mutation, "maestro.subagent.code-review"),
        Err(crate::a2a::CapsuleValidationError::MissingCapability { .. })
    ));
}

#[test]
pub(crate) fn capsule_rejects_invalid_deadline_retry_and_material_review_policy() {
    let mut invalid_deadline = valid_code_writer_capsule();
    invalid_deadline["capsule"]["deadlineAt"] = serde_json::json!("tomorrow");
    assert!(matches!(
        crate::a2a::validate_subagent_capsule(&invalid_deadline, "maestro.subagent.code-writer"),
        Err(crate::a2a::CapsuleValidationError::InvalidDeadline { .. })
    ));

    let mut expired_deadline = valid_code_writer_capsule();
    expired_deadline["capsule"]["deadlineAt"] = serde_json::json!("2000-01-01T00:00:00Z");
    assert!(matches!(
        crate::a2a::validate_subagent_capsule(&expired_deadline, "maestro.subagent.code-writer"),
        Err(crate::a2a::CapsuleValidationError::ExpiredDeadline { .. })
    ));

    let mut excessive_retry = valid_code_writer_capsule();
    excessive_retry["capsule"]["retryLimit"] = serde_json::json!(4);
    assert!(matches!(
        crate::a2a::validate_subagent_capsule(&excessive_retry, "maestro.subagent.code-writer"),
        Err(crate::a2a::CapsuleValidationError::RetryLimitExceeded { .. })
    ));

    let mut no_review = valid_code_writer_capsule();
    no_review["capsule"]["review"]["required"] = serde_json::json!(false);
    assert!(matches!(
        crate::a2a::validate_subagent_capsule(&no_review, "maestro.subagent.code-writer"),
        Err(crate::a2a::CapsuleValidationError::IndependentReviewRequired { .. })
    ));

    let mut self_review = valid_code_writer_capsule();
    self_review["capsule"]["review"]["laneId"] = serde_json::json!("code-writer");
    assert!(matches!(
        crate::a2a::validate_subagent_capsule(&self_review, "maestro.subagent.code-writer"),
        Err(crate::a2a::CapsuleValidationError::IndependentReviewRequired { .. })
    ));
}

#[test]
pub(crate) fn capsule_v1_rejects_unknown_fields_and_malformed_resource_ids() {
    for pointer in [
        "",
        "/capsule",
        "/capsule/inScope",
        "/capsule/contextArtifacts/0",
        "/capsule/review",
    ] {
        let mut request = valid_code_writer_capsule();
        request
            .pointer_mut(pointer)
            .and_then(Value::as_object_mut)
            .expect("fixture object should exist")
            .insert("unknownV1Field".to_string(), Value::Bool(true));
        assert!(
            crate::a2a::validate_subagent_capsule(&request, "maestro.subagent.code-writer")
                .is_err(),
            "unknown field at {pointer:?} must fail closed"
        );
    }

    for resource in ["", " ", " deploy/prod ", "../deploy", "deploy//prod"] {
        let mut request = valid_code_writer_capsule();
        request["capsule"]["inScope"]["resources"] = serde_json::json!([resource]);
        request["capsule"]["mutationBoundary"]["resources"] = serde_json::json!([resource]);
        assert!(
            crate::a2a::validate_subagent_capsule(&request, "maestro.subagent.code-writer")
                .is_err(),
            "resource id {resource:?} must be normalized and nonempty"
        );
    }
}

#[test]
pub(crate) fn capsule_execution_policy_enforces_scope_model_deadline_guidance_and_tools() {
    let root = TestDir::new("subagent-execution-policy");
    let scope = root.path().join("packages/runtime-gateway-rs/src/a2a");
    fs::create_dir_all(&scope).expect("scope should be created");
    fs::write(scope.join("inside.rs"), "inside").expect("inside fixture should be written");
    fs::write(root.path().join("outside.rs"), "outside")
        .expect("outside fixture should be written");
    let mut request = valid_code_writer_capsule();
    request["capsule"]["deadlineAt"] = serde_json::json!("2099-01-01T00:00:30Z");
    let capsule = crate::a2a::validate_subagent_capsule(&request, "maestro.subagent.code-writer")
        .expect("capsule should validate");
    let now = chrono::DateTime::parse_from_rfc3339("2099-01-01T00:00:00Z")
        .expect("time should parse")
        .with_timezone(&chrono::Utc);

    let policy = crate::a2a::build_a2a_subagent_execution_policy(
        &capsule,
        root.path(),
        Duration::from_secs(300),
        now,
    )
    .expect("policy should build");

    assert_eq!(policy.model, "anthropic/claude-haiku-4-5");
    assert_eq!(policy.turn_timeout, Duration::from_secs(30));
    assert!(policy.guidance.contains("task-1"));
    assert!(
        policy
            .guidance
            .contains("Objective:\nAdd the bounded parser and its regression test.")
    );
    assert!(
        policy.guidance.contains(
            "Acceptance checks:\n- cargo test -p maestro-runtime-gateway subagent_capsule"
        )
    );
    assert_eq!(
        policy.allowed_tools,
        [
            "diff", "edit", "find", "glob", "grep", "list", "read", "search", "write"
        ]
        .into_iter()
        .map(str::to_string)
        .collect()
    );

    assert!(
        policy
            .guard_tool_call(
                "read",
                &serde_json::json!({"path": scope.join("inside.rs")})
            )
            .is_ok()
    );
    assert!(
        policy
            .guard_tool_call(
                "read",
                &serde_json::json!({"path": "packages/runtime-gateway-rs/src/a2a/inside.rs"})
            )
            .is_ok()
    );
    assert!(
        policy
            .guard_tool_call(
                "read",
                &serde_json::json!({"path": root.path().join("outside.rs")})
            )
            .is_err()
    );
    assert!(
        policy
            .guard_tool_call(
                "write",
                &serde_json::json!({"path": root.path().join("outside.rs"), "content": "no"})
            )
            .is_err()
    );
    assert!(
        policy
            .guard_tool_call(
                "glob",
                &serde_json::json!({
                    "path": "packages/runtime-gateway-rs/src/a2a",
                    "pattern": "../../outside.rs"
                })
            )
            .is_err()
    );
    assert!(
        policy
            .guard_tool_call(
                "glob",
                &serde_json::json!({
                    "path": "packages/runtime-gateway-rs/src/a2a",
                    "pattern": root.path().join("outside.rs")
                })
            )
            .is_err()
    );
    assert!(
        policy
            .guard_tool_call(
                "search",
                &serde_json::json!({
                    "pattern": "outside",
                    "paths": "packages/runtime-gateway-rs/src/a2a",
                    "cwd": root.path()
                })
            )
            .is_err()
    );
    assert!(
        policy
            .guard_tool_call(
                "bash",
                &serde_json::json!({
                    "command": "cargo test -p maestro-runtime-gateway subagent_capsule"
                })
            )
            .is_err()
    );
    assert!(
        policy
            .guard_tool_call(
                "web_fetch",
                &serde_json::json!({"url": "https://example.com"})
            )
            .is_err()
    );
}

#[test]
pub(crate) fn code_review_capsule_builds_a_read_only_execution_policy() {
    let root = TestDir::new("code-review-policy");
    fs::create_dir_all(root.path().join("packages/runtime-gateway-rs/src/a2a"))
        .expect("scope should be created");
    let request = valid_code_review_capsule();
    let capsule = crate::a2a::validate_subagent_capsule(&request, "maestro.subagent.code-review")
        .expect("review capsule should validate");

    let policy = crate::a2a::build_a2a_subagent_execution_policy(
        &capsule,
        root.path(),
        Duration::from_secs(300),
        chrono::Utc::now(),
    )
    .expect("review lane must have an executable policy");

    assert!(policy.allowed_tools.contains("read"));
    assert!(!policy.allowed_tools.contains("write"));
    assert!(!policy.allowed_tools.contains("edit"));
}

#[test]
pub(crate) fn capsule_execution_policy_fails_closed_for_unexecutable_boundaries_and_checks() {
    let root = TestDir::new("subagent-fail-closed-policy");
    let scope = root.path().join("packages/runtime-gateway-rs/src/a2a");
    fs::create_dir_all(&scope).expect("scope should be created");

    let mut unsafe_check = valid_code_writer_capsule();
    unsafe_check["capsule"]["acceptanceChecks"] = serde_json::json!(["cat /etc/passwd"]);
    let unsafe_check =
        crate::a2a::validate_subagent_capsule(&unsafe_check, "maestro.subagent.code-writer")
            .expect("schema-valid check should reach server allowlist");
    assert!(
        crate::a2a::build_a2a_subagent_execution_policy(
            &unsafe_check,
            root.path(),
            Duration::from_secs(300),
            chrono::Utc::now(),
        )
        .is_err()
    );

    let mut resource_scope = valid_code_writer_capsule();
    resource_scope["capsule"]["inScope"]["resources"] = serde_json::json!(["repo:issue#1"]);
    resource_scope["capsule"]["mutationBoundary"]["resources"] =
        serde_json::json!(["repo:issue#1"]);
    let resource_scope =
        crate::a2a::validate_subagent_capsule(&resource_scope, "maestro.subagent.code-writer")
            .expect("normalized resource should be schema-valid");
    assert!(
        crate::a2a::build_a2a_subagent_execution_policy(
            &resource_scope,
            root.path(),
            Duration::from_secs(300),
            chrono::Utc::now(),
        )
        .is_err()
    );

    let mut future_mutation_root = valid_code_writer_capsule();
    future_mutation_root["capsule"]["mutationBoundary"]["paths"] =
        serde_json::json!(["packages/runtime-gateway-rs/src/a2a/future"]);
    let future_mutation_root = crate::a2a::validate_subagent_capsule(
        &future_mutation_root,
        "maestro.subagent.code-writer",
    )
    .expect("nested path should be capsule-valid");
    assert!(
        crate::a2a::build_a2a_subagent_execution_policy(
            &future_mutation_root,
            root.path(),
            Duration::from_secs(300),
            chrono::Utc::now(),
        )
        .is_err()
    );
}

#[tokio::test(flavor = "current_thread")]
pub(crate) async fn capsule_execution_policy_applies_absolute_deadline_to_tools_and_checks() {
    let root = TestDir::new("subagent-absolute-deadline");
    let scope = root.path().join("packages/runtime-gateway-rs/src/a2a");
    fs::create_dir_all(&scope).expect("scope should be created");
    fs::write(scope.join("inside.rs"), "inside").expect("inside fixture should be written");
    let mut request = valid_code_writer_capsule();
    request["capsule"]["deadlineAt"] =
        Value::String((chrono::Utc::now() + chrono::Duration::milliseconds(250)).to_rfc3339());
    let capsule = crate::a2a::validate_subagent_capsule(&request, "maestro.subagent.code-writer")
        .expect("capsule should validate before its deadline");
    let policy = crate::a2a::build_a2a_subagent_execution_policy(
        &capsule,
        root.path(),
        Duration::from_secs(300),
        chrono::Utc::now(),
    )
    .expect("policy should build before its deadline");
    let (_cancel_tx, cancel_rx) = watch::channel(false);
    tokio::time::sleep(Duration::from_millis(300)).await;

    let result = policy
        .execute_tool_call(
            "read",
            &serde_json::json!({"path": "packages/runtime-gateway-rs/src/a2a/inside.rs"}),
            "expired-read",
            cancel_rx.clone(),
        )
        .await;
    assert!(!result.success);
    assert!(
        result
            .error
            .as_deref()
            .is_some_and(|error| error.contains("deadline"))
    );

    let mut acceptance_cancel_rx = cancel_rx;
    let error = policy
        .run_acceptance_checks(&mut acceptance_cancel_rx)
        .await
        .expect_err("acceptance checks must share the absolute capsule deadline");
    assert!(error.contains("deadline"));

    let mut cancellation_request = valid_code_writer_capsule();
    cancellation_request["capsule"]["deadlineAt"] = serde_json::json!("2099-01-01T00:00:00Z");
    let cancellation_capsule = crate::a2a::validate_subagent_capsule(
        &cancellation_request,
        "maestro.subagent.code-writer",
    )
    .expect("cancellation capsule should validate");
    let cancellation_policy = crate::a2a::build_a2a_subagent_execution_policy(
        &cancellation_capsule,
        root.path(),
        Duration::from_secs(300),
        chrono::Utc::now(),
    )
    .expect("cancellation policy should build");
    let canceled_target = scope.join("canceled.rs");
    let (_cancel_tx, canceled_rx) = watch::channel(true);
    let canceled = cancellation_policy
        .execute_tool_call(
            "write",
            &serde_json::json!({
                "path": canceled_target,
                "content": "must not be written"
            }),
            "pre-canceled-write",
            canceled_rx,
        )
        .await;
    assert!(!canceled.success);
    assert!(!scope.join("canceled.rs").exists());
}

#[test]
pub(crate) fn capsule_deadline_cannot_leave_an_ambient_validator_process_running() {
    const CHILD_FLAG: &str = "MAESTRO_TEST_CAPSULE_VALIDATOR_CHILD";
    const ROOT_ENV: &str = "MAESTRO_TEST_CAPSULE_VALIDATOR_ROOT";
    const SENTINEL_ENV: &str = "MAESTRO_TEST_CAPSULE_VALIDATOR_SENTINEL";

    if env::var_os(CHILD_FLAG).is_some() {
        let root = std::path::PathBuf::from(
            env::var_os(ROOT_ENV).expect("child workspace should be configured"),
        );
        let sentinel = std::path::PathBuf::from(
            env::var_os(SENTINEL_ENV).expect("child sentinel should be configured"),
        );
        let scope = root.join("packages/runtime-gateway-rs/src/a2a");
        fs::create_dir_all(&scope).expect("scope should be created");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("child runtime");
        runtime.block_on(async {
            // ToolExecutor construction initializes the shared config and
            // registry state used by the governed call. Warm that state before
            // starting the intentionally short capsule deadline so fixture
            // setup is not charged against the 2-second execution budget.
            let _executor_warmup =
                maestro_local_host::tools::ToolExecutor::new(scope.display().to_string());

            let mut request = valid_code_writer_capsule();
            request["capsule"]["deadlineAt"] =
                Value::String((chrono::Utc::now() + chrono::Duration::seconds(2)).to_rfc3339());
            let capsule =
                crate::a2a::validate_subagent_capsule(&request, "maestro.subagent.code-writer")
                    .expect("capsule should validate");
            let policy = crate::a2a::build_a2a_subagent_execution_policy(
                &capsule,
                &root,
                Duration::from_secs(5),
                chrono::Utc::now(),
            )
            .expect("policy should build");
            let (_cancel_tx, cancel_rx) = watch::channel(false);
            let result = policy
                .execute_tool_call(
                    "write",
                    &serde_json::json!({
                        "path": scope.join("inside.rs"),
                        "content": "inside"
                    }),
                    "deadline-validator-write",
                    cancel_rx,
                )
                .await;
            assert!(result.success, "{result:?}");
            tokio::time::sleep(Duration::from_secs(6)).await;
            assert!(
                !sentinel.exists(),
                "no validator child may outlive the governed call"
            );

            let mut expired_request = valid_code_writer_capsule();
            expired_request["capsule"]["deadlineAt"] = Value::String(
                (chrono::Utc::now() + chrono::Duration::milliseconds(100)).to_rfc3339(),
            );
            let expired_capsule = crate::a2a::validate_subagent_capsule(
                &expired_request,
                "maestro.subagent.code-writer",
            )
            .expect("deadline capsule should validate");
            let expired_policy = crate::a2a::build_a2a_subagent_execution_policy(
                &expired_capsule,
                &root,
                Duration::from_secs(5),
                chrono::Utc::now(),
            )
            .expect("deadline policy should build");
            tokio::time::sleep(Duration::from_millis(150)).await;
            let (_cancel_tx, cancel_rx) = watch::channel(false);
            let expired_target = scope.join("expired.rs");
            let expired = expired_policy
                .execute_tool_call(
                    "write",
                    &serde_json::json!({
                        "path": expired_target,
                        "content": "must not be written"
                    }),
                    "expired-validator-write",
                    cancel_rx,
                )
                .await;
            assert!(!expired.success);
            assert!(!scope.join("expired.rs").exists());
            assert!(!sentinel.exists());
        });
        return;
    }

    let root = TestDir::new("capsule-validator-deadline");
    let sentinel = root.path().join("outside-validator-write");
    let quoted_sentinel = format!("'{}'", sentinel.to_string_lossy().replace('\'', "'\"'\"'"));
    let output = std::process::Command::new(std::env::current_exe().expect("current test binary"))
        .args([
            "--exact",
            "tests::capsule_deadline_cannot_leave_an_ambient_validator_process_running",
            "--nocapture",
        ])
        .env(CHILD_FLAG, "1")
        .env(ROOT_ENV, root.path())
        .env(SENTINEL_ENV, &sentinel)
        .env("MAESTRO_SAFE_MODE", "1")
        .env(
            "MAESTRO_SAFE_VALIDATORS",
            format!("sleep 5; printf escaped > {quoted_sentinel}"),
        )
        .output()
        .expect("run isolated governed-validator child");

    assert!(
        output.status.success(),
        "isolated governed-validator child failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[tokio::test(flavor = "current_thread")]
pub(crate) async fn expired_claimed_capsule_still_gets_server_owned_completion_and_cancel_dispositions()
 {
    let _guard = ENV_LOCK.lock().await;
    let previous_fake = env::var("MAESTRO_A2A_FAKE_RESPONSE").ok();
    env::set_var("MAESTRO_A2A_FAKE_RESPONSE", "implementation complete");
    let mut expired = valid_code_writer_capsule();
    expired["capsule"]["deadlineAt"] = serde_json::json!("2000-01-01T00:00:00Z");
    let state = test_app_state_with_sessions(HashMap::new());
    let (_cancel_tx, cancel_rx) = watch::channel(false);

    let completed = complete_a2a_task(
        &state,
        "implement this".to_string(),
        "expired-completed".to_string(),
        "ctx-1".to_string(),
        Vec::new(),
        serde_json::json!({"evalops.subagentRequest": expired.clone()}),
        cancel_rx,
    )
    .await;

    assert_eq!(
        completed["metadata"]["evalops.subagentCompletion"]["observedOutcome"],
        "completed"
    );
    assert_eq!(
        completed["metadata"]["workGraph"]["state"],
        "awaiting_review"
    );

    let cancel_task = a2a_task_value(
        "expired-canceled",
        "ctx-1",
        "TASK_STATE_WORKING",
        a2a_agent_message("ctx-1", "working"),
        Vec::new(),
        Vec::new(),
        serde_json::json!({"evalops.subagentRequest": expired}),
    );
    state
        .a2a_tasks
        .lock()
        .await
        .insert("expired-canceled".to_string(), cancel_task);
    let auth = AuthContext {
        subject: None,
        unrestricted: true,
        ..AuthContext::default()
    };
    let canceled = crate::a2a::cancel_a2a_task(&state, "expired-canceled", &auth)
        .await
        .expect("expired claimed capsule should cancel");

    assert_eq!(
        canceled["metadata"]["evalops.subagentCompletion"]["observedOutcome"],
        "canceled"
    );
    assert_eq!(canceled["metadata"]["workGraph"]["state"], "canceled");

    if let Some(previous_fake) = previous_fake {
        env::set_var("MAESTRO_A2A_FAKE_RESPONSE", previous_fake);
    } else {
        env::remove_var("MAESTRO_A2A_FAKE_RESPONSE");
    }
}

#[test]
pub(crate) fn validates_run_script_inputs() {
    assert!(is_valid_script_name("build:all"));
    assert!(!is_valid_script_name("build && rm -rf /"));
    assert!(contains_shell_metachars("foo; bar"));
    assert!(!contains_shell_metachars("--filter packages/tui"));
    assert_eq!(
        runner_args_for_script("npm", "db:migrate"),
        ["--ignore-scripts", "run", "db:migrate"]
    );
    assert_eq!(
        runner_args_for_script("/tmp/bin/npm", "db:migrate"),
        ["--ignore-scripts", "run", "db:migrate"]
    );
    assert_eq!(
        runner_args_for_script(r"C:\tools\NPM.CMD", "db:migrate"),
        ["--ignore-scripts", "run", "db:migrate"]
    );
    assert_eq!(
        runner_args_for_script("/tmp/custom-runner", "db:migrate"),
        ["run", "db:migrate"]
    );
    assert!(!runner_supports_ignore_scripts("bun"));
    assert!(!runner_supports_ignore_scripts("pnpm"));
    assert!(!runner_supports_ignore_scripts("/tmp/bin/pnpm"));
    assert!(!runner_supports_ignore_scripts("/tmp/bin/my-npm"));
    assert!(runner_supports_ignore_scripts("/tmp/bin/npm"));
    assert!(runner_supports_ignore_scripts(r"C:\tools\npm.cmd"));
    assert!(runner_supports_ignore_scripts(r"C:\tools\NPM.CMD"));
    assert!(runner_supports_ignore_scripts("/tmp/bin/npm.CMD"));
}

#[tokio::test]
pub(crate) async fn run_scripts_lists_only_allowlisted_package_scripts() {
    let _guard = ENV_LOCK.lock().await;
    let env_snapshot = snapshot_env(&["MAESTRO_RUN_SCRIPT_ALLOWLIST"]);
    env::set_var("MAESTRO_RUN_SCRIPT_ALLOWLIST", "db:migrate, smoke");

    let root = TestDir::new("run-script-list-allowlist");
    fs::write(
        root.path().join("package.json"),
        r#"{"scripts":{"db:migrate":"echo migrate","smoke":"echo smoke","dev":"echo dev"}}"#,
    )
    .expect("package.json should be written");

    let scripts = package_scripts(root.path()).await;

    restore_env(env_snapshot);
    assert_eq!(scripts, vec!["db:migrate", "smoke"]);
}

#[tokio::test]
pub(crate) async fn run_script_response_rejects_scripts_outside_allowlist() {
    let _guard = ENV_LOCK.lock().await;
    let env_snapshot = snapshot_env(&["MAESTRO_RUN_SCRIPT_ALLOWLIST"]);
    env::set_var("MAESTRO_RUN_SCRIPT_ALLOWLIST", "db:migrate");

    let root = TestDir::new("run-script-rejects-unlisted");
    fs::write(
        root.path().join("package.json"),
        r#"{"scripts":{"db:migrate":"echo migrate","dev":"echo dev"}}"#,
    )
    .expect("package.json should be written");
    let request: RunScriptRequest = serde_json::from_value(serde_json::json!({ "script": "dev" }))
        .expect("request should deserialize");

    let response = run_script_response(root.path(), request).await;
    let body = response_json(response.clone());

    restore_env(env_snapshot);
    assert_eq!(response_status(&response), 403);
    assert_eq!(body["allowed"], serde_json::json!(["db:migrate"]));
}

#[tokio::test]
pub(crate) async fn run_script_response_uses_ignore_scripts_for_npm_runner() {
    let _guard = ENV_LOCK.lock().await;
    let env_snapshot = snapshot_env(&["MAESTRO_RUN_SCRIPT_ALLOWLIST", "MAESTRO_SCRIPT_RUNNER"]);
    env::set_var("MAESTRO_RUN_SCRIPT_ALLOWLIST", "db:migrate");

    let root = TestDir::new("run-script-ignore-lifecycle");
    fs::write(
        root.path().join("package.json"),
        r#"{"scripts":{"predb:migrate":"echo pre","db:migrate":"echo migrate","postdb:migrate":"echo post"}}"#,
    )
    .expect("package.json should be written");

    let runner = root.path().join("npm");
    let argv_file = root.path().join("argv.txt");
    fs::write(
        &runner,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > {}\n",
            argv_file.display()
        ),
    )
    .expect("runner should be written");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = fs::metadata(&runner)
            .expect("runner metadata should exist")
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&runner, permissions).expect("runner should be executable");
    }
    env::set_var("MAESTRO_SCRIPT_RUNNER", &runner);

    let request: RunScriptRequest =
        serde_json::from_value(serde_json::json!({ "script": "db:migrate" }))
            .expect("request should deserialize");
    let response = run_script_response(root.path(), request).await;
    let body = response_json(response.clone());
    let argv = fs::read_to_string(argv_file).expect("runner argv should be captured");

    restore_env(env_snapshot);
    assert_eq!(response_status(&response), 200);
    assert_eq!(body["success"], serde_json::json!(true));
    assert_eq!(argv, "--ignore-scripts\nrun\ndb:migrate\n");
}

#[tokio::test]
pub(crate) async fn run_script_response_uses_ignore_scripts_for_case_variant_npm_cmd_runner() {
    let _guard = ENV_LOCK.lock().await;
    let env_snapshot = snapshot_env(&["MAESTRO_RUN_SCRIPT_ALLOWLIST", "MAESTRO_SCRIPT_RUNNER"]);
    env::set_var("MAESTRO_RUN_SCRIPT_ALLOWLIST", "db:migrate");

    let root = TestDir::new("run-script-ignore-lifecycle-npm-cmd");
    fs::write(
        root.path().join("package.json"),
        r#"{"scripts":{"predb:migrate":"echo pre","db:migrate":"echo migrate","postdb:migrate":"echo post"}}"#,
    )
    .expect("package.json should be written");

    let runner = root.path().join("NPM.CMD");
    let argv_file = root.path().join("argv.txt");
    fs::write(
        &runner,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > {}\n",
            argv_file.display()
        ),
    )
    .expect("runner should be written");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = fs::metadata(&runner)
            .expect("runner metadata should exist")
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&runner, permissions).expect("runner should be executable");
    }
    env::set_var("MAESTRO_SCRIPT_RUNNER", &runner);

    let request: RunScriptRequest =
        serde_json::from_value(serde_json::json!({ "script": "db:migrate" }))
            .expect("request should deserialize");
    let response = run_script_response(root.path(), request).await;
    let body = response_json(response.clone());
    let argv = fs::read_to_string(argv_file).expect("runner argv should be captured");

    restore_env(env_snapshot);
    assert_eq!(response_status(&response), 200);
    assert_eq!(body["success"], serde_json::json!(true));
    assert_eq!(argv, "--ignore-scripts\nrun\ndb:migrate\n");
}

#[tokio::test]
pub(crate) async fn run_script_response_rejects_pnpm_runner_without_executing() {
    let _guard = ENV_LOCK.lock().await;
    let env_snapshot = snapshot_env(&["MAESTRO_RUN_SCRIPT_ALLOWLIST", "MAESTRO_SCRIPT_RUNNER"]);
    env::set_var("MAESTRO_RUN_SCRIPT_ALLOWLIST", "db:migrate");

    let root = TestDir::new("run-script-rejects-pnpm-runner");
    fs::write(
        root.path().join("package.json"),
        r#"{"scripts":{"predb:migrate":"echo pre","db:migrate":"echo migrate","postdb:migrate":"echo post"}}"#,
    )
    .expect("package.json should be written");

    let runner = root.path().join("pnpm");
    let executed_file = root.path().join("pnpm-executed.txt");
    fs::write(
        &runner,
        format!(
            "#!/bin/sh\nprintf 'executed\\n' > {}\n",
            executed_file.display()
        ),
    )
    .expect("runner should be written");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = fs::metadata(&runner)
            .expect("runner metadata should exist")
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&runner, permissions).expect("runner should be executable");
    }
    env::set_var("MAESTRO_SCRIPT_RUNNER", &runner);

    let request: RunScriptRequest =
        serde_json::from_value(serde_json::json!({ "script": "db:migrate" }))
            .expect("request should deserialize");
    let response = run_script_response(root.path(), request).await;
    let body = response_json(response.clone());
    let runner_executed = executed_file.exists();

    restore_env(env_snapshot);
    assert_eq!(response_status(&response), 503);
    assert!(
        body["error"]
            .as_str()
            .expect("error should be a string")
            .contains("lifecycle suppression"),
        "unexpected error body: {body}"
    );
    assert!(!runner_executed);
}

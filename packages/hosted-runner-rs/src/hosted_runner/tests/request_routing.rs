//! Existing request routing contracts shared with the native facade.
use super::*;

#[tokio::test]
async fn route_rejects_connection_prefix_without_separator() {
    let workspace = tempdir().expect("workspace");
    let shared = SharedRunner::new(test_config(workspace.path().to_path_buf()));
    let request = HttpRequest {
        raw_query: None,
        method: "POST".to_string(),
        path: "/api/headless/connections-extra".to_string(),
        query: HashMap::new(),
        headers: HashMap::new(),
        body: b"{}".to_vec(),
    };

    let error = match route_request(request, shared, "127.0.0.1:4567".parse().unwrap()).await {
        Ok(_) => panic!("unexpected route match"),
        Err(error) => error,
    };

    assert_eq!(error.code, HostedRunnerErrorCode::NotFound);
}

#[tokio::test]
async fn remote_drain_requires_auth_token_when_configured() {
    let workspace = tempdir().expect("workspace");
    let shared = SharedRunner::new(
        test_config(workspace.path().to_path_buf()).with_auth_token("secret-token"),
    );
    let request = HttpRequest {
        raw_query: None,
        method: "POST".to_string(),
        path: HOSTED_RUNNER_DRAIN_PATH.to_string(),
        query: HashMap::new(),
        headers: HashMap::new(),
        body: serde_json::to_vec(
            &json!({"reason": "remote", "requested_by": "platform", "export_paths": ["."]}),
        )
        .expect("drain request"),
    };

    let error = match route_request(request, shared, "203.0.113.10:4567".parse().unwrap()).await {
        Ok(_) => panic!("remote drain without token should be rejected"),
        Err(error) => error,
    };

    assert_eq!(error.status, StatusCode::FORBIDDEN.as_u16());
    assert_eq!(error.code, HostedRunnerErrorCode::AccessDenied);
}

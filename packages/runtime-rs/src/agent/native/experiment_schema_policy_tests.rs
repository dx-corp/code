use super::*;
use serde_json::Value;
use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;

#[tokio::test]
async fn experiments_change_actual_provider_tools_and_withdraw_at_turn_boundary() {
    exercise_experiment_profile_boundaries(ExternalToolSchemaPolicy::Eager).await;
}

#[tokio::test]
async fn experiments_preserve_deferred_caller_schemas_across_consent_changes() {
    exercise_experiment_profile_boundaries(ExternalToolSchemaPolicy::Deferred).await;
}

async fn exercise_experiment_profile_boundaries(policy: ExternalToolSchemaPolicy) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let captured = Arc::new(Mutex::new(Vec::new()));
    let requests = Arc::clone(&captured);
    let server = tokio::spawn(async move {
        for _ in 0..3 {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request = read_scripted_provider_request(&mut stream).await;
            requests.lock().unwrap().push(request);
            let body = chat_sse_response("experiment-fixture", "Done.", false);
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream.write_all(response.as_bytes()).await.unwrap();
        }
    });
    let workspace = tempfile::tempdir().unwrap();
    let config = NativeAgentConfig {
        model: "openai/gpt-4o".into(),
        cwd: workspace.path().display().to_string(),
        external_tool_schema_policy: policy,
        ..Default::default()
    };
    let client = UnifiedClient::OpenAI(
        crate::ai::OpenAiClient::with_base_url("test-key", format!("http://{address}/v1")).unwrap(),
    );
    let host = RuntimeTestHost::new(config.cwd.clone(), client);
    let consent = Arc::clone(&host.experiment);
    let mut assignment = maestro_runtime_contracts::experiments::ExperimentAssignment::derive(
        "fixture",
        "org",
        "workspace",
        1,
    );
    assignment.arm = maestro_runtime_contracts::experiments::ExperimentArm::Control;
    *consent.lock().unwrap() = Some(assignment.clone());
    let resolved_client = host.client.as_ref().clone();
    let resolved = NativeResolvedClient {
        provider_name: resolved_client.provider_name().to_owned(),
        client: Some(resolved_client),
        model_route: NativeModelRoute::DirectProvider,
    };
    let (agent, mut events) = super::super::NativeAgent::start_with_resolved_client(
        config,
        NativeExecutionHostHandle::new(Arc::new(host)),
        vec![external_tool_definition("client_calendar")],
        CredentialVault::new(),
        None,
        resolved,
    )
    .unwrap();
    for index in 0..3 {
        if index == 1 {
            assignment.arm = maestro_runtime_contracts::experiments::ExperimentArm::Minimal;
            *consent.lock().unwrap() = Some(assignment.clone());
        }
        if index == 2 {
            *consent.lock().unwrap() = None;
        }
        agent.prompt("Say done.".into(), vec![]).await.unwrap();
        let mut observed = false;
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                match events.recv().await.unwrap() {
                    FromAgent::OperationObservation {
                        observation:
                            maestro_runtime_contracts::operation_observation::OperationObservation::Admitted {
                                experiment,
                                ..
                            },
                    } => {
                        assert_eq!(experiment.is_some(), index < 2);
                        observed = true;
                    }
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
        assert!(observed);
    }
    agent.shutdown().await;
    server.await.unwrap();
    let requests = captured.lock().unwrap();
    let names = |request: &Value| {
        request["tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|t| t["function"]["name"].as_str())
            .map(str::to_owned)
            .collect::<HashSet<_>>()
    };
    let first = names(&requests[0]);
    let second = names(&requests[1]);
    let third = names(&requests[2]);
    for request in requests.iter() {
        assert_eq!(
            names(request).contains("client_calendar"),
            policy == ExternalToolSchemaPolicy::Eager,
            "experiment enrollment and withdrawal must preserve caller schema policy"
        );
        if policy == ExternalToolSchemaPolicy::Deferred {
            let search = request["tools"]
                .as_array()
                .unwrap()
                .iter()
                .find(|tool| tool["function"]["name"] == "tool_search")
                .unwrap();
            assert!(
                search["function"]["description"]
                    .as_str()
                    .unwrap()
                    .contains("client_calendar")
            );
        }
    }
    assert!(first.contains("explore"));
    assert!(!second.contains("explore"));
    assert!(second.contains("grep") && second.contains("glob"));
    assert_eq!(
        first, third,
        "withdrawal restores the baseline tool surface"
    );
}

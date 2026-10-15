use super::*;

struct FailingTransport {
    address: std::net::SocketAddr,
    server: tokio::task::JoinSet<()>,
    requests: Arc<std::sync::Mutex<Vec<serde_json::Value>>>,
}

impl FailingTransport {
    async fn new() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        let captured = Arc::clone(&requests);
        let mut server = tokio::task::JoinSet::new();
        server.spawn(async move {
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                let request = read_scripted_provider_request(&mut stream).await;
                captured.lock().unwrap().push(request);
                // Close before response headers while retaining the listener's port.
                drop(stream);
            }
        });
        Self {
            address,
            server,
            requests,
        }
    }

    async fn finish(mut self) {
        self.server.shutdown().await;
        let requests = self.requests.lock().unwrap();
        assert_eq!(requests.len(), 1, "one budgeted primary provider attempt");
        assert_eq!(requests[0]["model"], "claude-fable-5-1");
    }
}

#[tokio::test]
async fn failing_transport_is_recovery_eligible_before_budget_exhaustion() {
    use crate::ai::AiClient as _;

    let transport = FailingTransport::new().await;
    let client = crate::ai::AnthropicClient::with_base_url(
        "test-key",
        format!("http://{}", transport.address),
    )
    .unwrap();
    let config = crate::ai::RequestConfig {
        model: "anthropic/claude-fable-5-1".into(),
        ..Default::default()
    };
    let messages = [crate::ai::Message {
        role: crate::ai::Role::User,
        content: crate::ai::MessageContent::text("One provider step only."),
    }];
    let error = tokio::time::timeout(Duration::from_secs(3), client.stream(&messages, &config))
        .await
        .expect("transport must fail before the budget-test deadline")
        .expect_err("closing before response headers must fail the provider request");
    assert_eq!(
        crate::agent::retry::ErrorKind::classify(&format!("{error:#}")),
        crate::agent::retry::ErrorKind::Transient
    );
    transport.finish().await;
}

#[tokio::test]
async fn exhausted_one_step_turn_does_not_switch_to_unreachable_fallback() {
    let transport = FailingTransport::new().await;
    let workspace = tempfile::tempdir().unwrap();
    let config = NativeAgentConfig {
        model: "anthropic/claude-fable-5-1".into(),
        cwd: workspace.path().display().to_string(),
        max_turn_steps: 1,
        retry_config: crate::agent::retry::RetryConfig::no_retry(),
        model_dynamics: ModelDynamicsConfig {
            fallbacks: vec![ModelChoice {
                model: "anthropic/claude-fable-5-2".into(),
                thinking: Default::default(),
            }],
            ..Default::default()
        },
        ..NativeAgentConfig::default()
    };
    let client = UnifiedClient::Anthropic(
        crate::ai::AnthropicClient::with_base_url(
            "test-key",
            format!("http://{}", transport.address),
        )
        .unwrap(),
    );
    let (agent, mut events) = NativeAgent::new_with_test_client(config, client).unwrap();
    agent
        .prompt("One provider step only.".into(), vec![])
        .await
        .unwrap();
    let terminal = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            match events.recv().await {
                Some(FromAgent::ModelChanged { model, .. }) => {
                    panic!("spent turn switched to unreachable fallback {model}")
                }
                Some(FromAgent::Error {
                    message,
                    terminal: true,
                    ..
                }) => break message,
                Some(FromAgent::TurnCompleted { .. }) => panic!("failed provider turn completed"),
                Some(_) => {}
                None => panic!("agent event channel closed before terminal"),
            }
        }
    })
    .await
    .expect("one-step turn must terminate promptly");
    agent.shutdown().await;
    assert!(terminal.contains("step_budget_exhausted"), "{terminal}");
    transport.finish().await;
}

#[tokio::test]
async fn exhausted_one_step_turn_does_not_schedule_unreachable_provider_retry() {
    let transport = FailingTransport::new().await;
    let workspace = tempfile::tempdir().unwrap();
    let config = NativeAgentConfig {
        model: "anthropic/claude-fable-5-1".into(),
        cwd: workspace.path().display().to_string(),
        max_turn_steps: 1,
        retry_config: crate::agent::retry::RetryConfig {
            max_retries: 1,
            initial_delay: Duration::from_secs(5),
            max_delay: Duration::from_secs(5),
            jitter_factor: 0.0,
            ..crate::agent::retry::RetryConfig::default()
        },
        ..NativeAgentConfig::default()
    };
    let client = UnifiedClient::Anthropic(
        crate::ai::AnthropicClient::with_base_url(
            "test-key",
            format!("http://{}", transport.address),
        )
        .unwrap(),
    );
    let (agent, mut events) = NativeAgent::new_with_test_client(config, client).unwrap();
    agent
        .prompt("One provider step only.".into(), vec![])
        .await
        .unwrap();
    let terminal = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            match events.recv().await {
                Some(FromAgent::RequestRetryScheduled { .. }) => {
                    panic!("a spent one-step turn scheduled an unreachable retry")
                }
                Some(FromAgent::Error {
                    message,
                    terminal: true,
                    retryable,
                    ..
                }) => break (message, retryable),
                Some(FromAgent::TurnCompleted { .. }) => panic!("failed provider turn completed"),
                Some(_) => {}
                None => panic!("agent event channel closed before terminal"),
            }
        }
    })
    .await
    .expect("one-step turn must terminate before the retry delay");
    agent.shutdown().await;
    assert!(
        terminal.0.contains("step_budget_exhausted"),
        "{}",
        terminal.0
    );
    assert!(!terminal.1, "a spent turn must not remain retryable");
    transport.finish().await;
}

use super::*;
use maestro_runtime_contracts::ExecutionStatus;

fn event(kind: &str, value: Value) -> String {
    format!("event: {kind}\ndata: {value}\n\n")
}

async fn incomplete_anthropic_classifier(overflow: bool) {
    let workspace = tempfile::tempdir().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let mut requests = Vec::new();
        for index in 0..2 {
            let (mut stream, _) = listener.accept().await.unwrap();
            requests.push(read_scripted_provider_request(&mut stream).await);
            let mut body = event(
                "message_start",
                json!({"message":{"id":"classification","model":"claude-opus-4-7","usage":{"input_tokens":17,"output_tokens":0}}}),
            );
            if index == 0 {
                let code = "for(let i=0;i<2;i++){try{await models.classify({}, {text:'A bird',labels:['bird','car']});}catch(e){}} store('unsafe',1); text('done');";
                body += &event(
                    "content_block_start",
                    json!({"index":0,"content_block":{"type":"tool_use","id":"classifier-script","name":"codemode","input":{}}}),
                );
                body += &event(
                    "content_block_delta",
                    json!({"index":0,"delta":{"type":"input_json_delta","partial_json":json!({"code":code}).to_string()}}),
                );
                body += &event("content_block_stop", json!({"index":0}));
                body += &event(
                    "message_delta",
                    json!({"usage":{"output_tokens":10},"delta":{"stop_reason":"tool_use"}}),
                );
                body += &event("message_stop", json!({}));
            } else {
                // A valid cumulative count is still provisional until the stream ends normally.
                body += &event(
                    "message_delta",
                    json!({"usage":{"output_tokens":1},"delta":{}}),
                );
                body += &event(
                    "content_block_start",
                    json!({"index":0,"content_block":{"type":"text","text":if overflow {"x".repeat(4097)} else {"{\"label\":\"bird\"}".into()}}}),
                );
                // No final stop reason or message_stop; close the accepted HTTP stream.
            }
            stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).as_bytes()).await.unwrap();
        }
        requests
    });
    let client = UnifiedClient::from_model_with_env(
        "anthropic/claude-opus-4-7",
        &HashMap::from([
            ("ANTHROPIC_API_KEY".into(), "test-key".into()),
            ("ANTHROPIC_BASE_URL".into(), format!("http://{address}/v1")),
        ]),
    )
    .unwrap();
    let journal = Arc::new(Mutex::new(Vec::new()));
    let host =
        RuntimeTestHost::new(workspace.path(), client).with_tool_operation_journal(journal.clone());
    let config = NativeAgentConfig {
        model: "anthropic/claude-opus-4-7".into(),
        cwd: workspace.path().display().to_string(),
        approval_mode: ApprovalMode::Yolo,
        ..Default::default()
    };
    let (agent, mut events) = new_runtime_test_agent_with_host(config, host).unwrap();
    agent.set_output_token_budget(100).unwrap();
    agent
        .prompt("Classify the text".into(), vec![])
        .await
        .unwrap();
    wait_for_turn_completed(&mut events).await;
    agent.shutdown().await;
    let requests = server.await.unwrap();
    assert_eq!(
        requests.len(),
        2,
        "accepted incomplete inference must block subsequent requests"
    );
    assert!(requests[1]["max_tokens"].as_u64().unwrap() <= 90);
    let records = journal.lock().unwrap();
    let child = records
        .iter()
        .filter(|record| record.tool_name == "classify")
        .filter_map(|record| record.outcome.as_ref())
        .next()
        .unwrap();
    assert_eq!(
        child.receipt.as_ref().unwrap().status,
        ExecutionStatus::Indeterminate
    );
    let details = serde_json::to_value(&child.receipt.as_ref().unwrap().details).unwrap();
    assert!(
        details["usage"].is_null(),
        "provisional counts must not become final billed usage"
    );
    let outer = records
        .iter()
        .filter(|record| record.tool_name == "codemode")
        .filter_map(|record| record.outcome.as_ref())
        .next()
        .unwrap();
    assert_eq!(
        outer.receipt.as_ref().unwrap().status,
        ExecutionStatus::Indeterminate
    );
    assert!(
        records
            .iter()
            .filter_map(|record| record.outcome.as_ref())
            .all(|outcome| outcome.codemode_store.is_none())
    );
}

#[tokio::test]
async fn codemode_classifier_partial_usage_then_disconnect_is_indeterminate() {
    incomplete_anthropic_classifier(false).await;
}

#[tokio::test]
async fn codemode_classifier_partial_usage_then_overflow_is_indeterminate() {
    incomplete_anthropic_classifier(true).await;
}

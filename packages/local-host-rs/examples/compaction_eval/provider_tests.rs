//! These exercise real native requests against a local fixture, not model efficacy.
#[path = "report.rs"]
pub(super) mod report;
#[path = "suite.rs"]
pub(super) mod suite;
#[path = "trial.rs"]
pub(super) mod trial;

use crate as host;
use crate::agent::{CredentialVault, ModelDynamicsConfig, NativeAgent, NativeAgentConfig};
use maestro_ai::{Message, MessageContent, OpenAiClient, Role, UnifiedClient};
use sha2::{Digest, Sha256};
use std::{collections::HashSet, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};

fn hash(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

pub(super) async fn request(stream: &mut TcpStream) -> serde_json::Value {
    let mut bytes = Vec::new();
    loop {
        let mut buffer = [0; 8192];
        let n = stream.read(&mut buffer).await.unwrap();
        assert!(n > 0, "fixture request must complete");
        bytes.extend_from_slice(&buffer[..n]);
        if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&bytes[..end]);
            let length: usize = head
                .lines()
                .find_map(|line| {
                    let (key, value) = line.split_once(':')?;
                    key.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse().unwrap())
                })
                .unwrap();
            if bytes.len() >= end + 4 + length {
                return serde_json::from_slice(&bytes[end + 4..end + 4 + length]).unwrap();
            }
        }
    }
}

pub(super) fn sse(text: &str) -> String {
    let start = serde_json::json!({"id":"fixture", "model":"gpt-4o", "created":0, "object":"chat.completion.chunk",
        "choices":[{"index":0,"delta":{"role":"assistant","content":text},"finish_reason":null}]});
    let stop = serde_json::json!({"id":"fixture", "model":"gpt-4o", "created":0, "object":"chat.completion.chunk",
        "choices":[{"index":0,"delta":{},"finish_reason":"stop"}],
        "usage":{"prompt_tokens":100,"completion_tokens":20,"total_tokens":120}});
    format!("data: {start}\n\ndata: {stop}\n\ndata: [DONE]\n\n")
}

#[tokio::test]
async fn native_summary_is_applied_before_probe_and_grader_answer_never_enters_requests() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let mut requests = Vec::new();
        // Original probe, one failed summary attempt, summary and compacted probe.
        for answer in [
            Some(r#"{"answer":"GRADER_ONLY_SENTINEL"}"#),
            None,
            Some("SUMMARY_EVIDENCE_MARKER"),
            Some(r#"{"answer":"GRADER_ONLY_SENTINEL"}"#),
        ] {
            let (mut stream, _) = listener.accept().await.unwrap();
            requests.push(request(&mut stream).await);
            let (status, body) = match answer {
                Some(answer) => ("200 OK", sse(answer)),
                None => ("503 Service Unavailable", "fixture summary failure".into()),
            };
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).await.unwrap();
        }
        requests
    });
    let case = suite::Case {
        id: "fixture".into(),
        family: "pipeline-mechanics".into(),
        history: vec![
            Message {
                role: Role::User,
                content: MessageContent::text("ORIGINAL_EVIDENCE_MARKER"),
            },
            Message {
                role: Role::Assistant,
                content: MessageContent::text("Acknowledged."),
            },
        ],
        question: "Return a JSON object with an answer string.".into(),
        expected: serde_json::json!({"answer":"GRADER_ONLY_SENTINEL"}),
    };
    let suite = suite::Suite {
        schema: "maestro.compaction-eval-suite.v1".into(),
        cases: vec![case],
    };
    suite.validate(128_000, 1024).unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let mut rows = Vec::new();
    for compacted in [false, true] {
        let config = NativeAgentConfig {
            model: "openai/gpt-4o".into(),
            context_window: Some(128_000),
            cwd: workspace.path().to_string_lossy().into_owned(),
            model_dynamics: ModelDynamicsConfig {
                summary_model: Some("openai/gpt-4o".into()),
                ..Default::default()
            },
            ..Default::default()
        };
        let client = UnifiedClient::OpenAI(
            OpenAiClient::with_base_url("fixture-key", format!("http://{address}/v1")).unwrap(),
        );
        let (agent, mut events) = NativeAgent::start(
            config,
            vec![],
            CredentialVault::new(),
            Some(&HashSet::new()),
            Some(super::ClientOverride::UnverifiedTest(client)),
            None,
            None,
        )
        .unwrap();
        assert!(agent.runtime_audit_snapshot().tools.is_empty());
        agent.set_hooks_enabled(false).unwrap();
        let output = workspace
            .path()
            .join(if compacted { "compacted" } else { "original" });
        std::fs::create_dir(&output).unwrap();
        let result = trial::run(
            &agent,
            &mut events,
            &suite.cases[0],
            compacted,
            Duration::from_secs(10),
            &output,
        )
        .await;
        agent.shutdown().await;
        rows.push(result.unwrap());
        for line in std::fs::read_to_string(output.join("events.jsonl"))
            .unwrap()
            .lines()
        {
            serde_json::from_str::<serde_json::Value>(line).unwrap();
        }
    }
    let sent = tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .unwrap()
        .unwrap();
    assert!(
        sent.iter()
            .all(|r| !r.to_string().contains("GRADER_ONLY_SENTINEL"))
    );
    assert!(
        sent[0]["messages"]
            .to_string()
            .contains("ORIGINAL_EVIDENCE_MARKER")
    );
    assert!(
        sent[1]["messages"]
            .to_string()
            .contains(maestro_context::compaction::SUMMARY_EVIDENCE_GUIDANCE)
    );
    assert!(sent.iter().all(|request| {
        request
            .get("tools")
            .is_none_or(|tools| tools.as_array().is_some_and(Vec::is_empty))
    }));
    assert_eq!(sent[1]["messages"], sent[2]["messages"]);
    assert!(
        sent[3]["messages"]
            .to_string()
            .contains("SUMMARY_EVIDENCE_MARKER")
    );
    assert!(
        !sent[3]["messages"]
            .to_string()
            .contains("ORIGINAL_EVIDENCE_MARKER")
    );
    assert!(rows.iter().all(|r| r.verified && r.terminal));
    assert!(!rows[0].forced_compaction_applied);
    assert!(rows[1].forced_compaction_applied);
    assert_eq!(rows[0].measurement.response_count, 1);
    assert_eq!(rows[1].measurement.response_count, 2);
    assert_eq!(rows[0].retries_started, 0);
    assert_eq!(rows[1].retries_started, 1);
    let report = report::paired(&suite, &rows).unwrap();
    assert!(report.comparison_valid);
    assert_eq!(report.original.input_tokens_observed, 100);
    assert_eq!(report.compacted.input_tokens_observed, 200);
    assert!(!report.compacted.usage_complete);
    assert!(
        report.compacted.cost_per_verified_task_usd.is_none(),
        "fixture reports tokens, not a price"
    );
}

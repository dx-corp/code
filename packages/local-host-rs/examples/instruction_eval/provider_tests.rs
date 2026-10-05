//! Actual native turns against loopback fixtures, never paid model evaluations.
#[path = "instructions.rs"]
mod instructions;

// Reuse the lib test's existing modules: loading these files a second time
// forks their type identities and trips the duplicate-module contract.
use super::compaction_eval_provider_tests::{report, request, sse, suite, trial};
use crate::agent::{CredentialVault, ModelDynamicsConfig, NativeAgent, NativeAgentConfig};
use maestro_ai::{OpenAiClient, UnifiedClient};
use maestro_runtime::agent::MaxTokensSource;
use sha2::{Digest, Sha256};
use std::{collections::HashSet, time::Duration};
use tokio::{io::AsyncWriteExt, net::TcpListener};

fn hash(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

#[tokio::test]
async fn matched_native_requests_change_only_instruction_fragment_and_never_send_grader() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let mut sent = Vec::new();
        for _ in 0..4 {
            let (mut stream, _) = listener.accept().await.unwrap();
            sent.push(request(&mut stream).await);
            let body = sse(r#"{"answer":"GRADER_ONLY_SENTINEL"}"#);
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).await.unwrap();
        }
        sent
    });
    let suite: instructions::InstructionSuite = serde_json::from_value(serde_json::json!({
        "schema":"maestro.instruction-eval-suite.v1", "repetitions":2,
        "shared_system_prompt":"SHARED_SYSTEM_MARKER",
        "control_agents_md":"CONTROL_INSTRUCTION_MARKER",
        "candidate_agents_md":"CANDIDATE_INSTRUCTION_MARKER",
        "cohort": {
            "schema":"maestro.compaction-eval-suite.v1",
            "cases":[{
                "id":"native-fixture", "family":"request-mechanics",
                "history":[{"role":"user", "content":"SHARED_HISTORY_MARKER"}, {"role":"assistant", "content":"Acknowledged."}],
                "question":"Return a JSON object with the answer string.",
                "expected":{"answer":"GRADER_ONLY_SENTINEL"}
            }]
        }
    })).unwrap();
    suite.validate(128_000, 1024).unwrap();
    instructions::unique_plan(&suite, "fixture-protocol").unwrap();
    let output = tempfile::tempdir().unwrap();
    let mut rows = Vec::new();
    for (index, slot) in suite.plan("fixture-protocol").into_iter().enumerate() {
        let cwd = tempfile::tempdir().unwrap();
        let config = NativeAgentConfig {
            model: "openai/gpt-4o".into(),
            max_tokens: 1024,
            max_tokens_source: MaxTokensSource::Explicit,
            context_window: Some(128_000),
            cwd: cwd.path().to_string_lossy().into_owned(),
            system_prompt: Some(suite.system_prompt(slot.candidate)),
            thinking_enabled: false,
            model_dynamics: ModelDynamicsConfig::default(),
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
        let folder = output.path().join(index.to_string());
        std::fs::create_dir(&folder).unwrap();
        let result = trial::run(
            &agent,
            &mut events,
            &suite.cohort.cases[0],
            false,
            Duration::from_secs(10),
            &folder,
        )
        .await;
        agent.cancel();
        agent.shutdown().await;
        rows.push(instructions::AssignedTrial {
            slot,
            trial: result.unwrap(),
        });
        for line in std::fs::read_to_string(folder.join("events.jsonl"))
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
    for (index, (request, row)) in sent.iter().zip(&rows).enumerate() {
        let text = request["messages"].to_string();
        assert!(text.contains("SHARED_SYSTEM_MARKER"));
        assert!(text.contains("SHARED_HISTORY_MARKER"));
        assert!(text.contains(if row.slot.candidate {
            "CANDIDATE_INSTRUCTION_MARKER"
        } else {
            "CONTROL_INSTRUCTION_MARKER"
        }));
        assert!(!request.to_string().contains("GRADER_ONLY_SENTINEL"));
        assert!(
            request
                .get("tools")
                .is_none_or(|tools| tools.as_array().is_some_and(Vec::is_empty))
        );
        assert_eq!(
            request["model"], sent[0]["model"],
            "fixed model in request {index}"
        );
        let nonsystem = |request: &serde_json::Value| {
            request["messages"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|message| message["role"] != "system")
                .cloned()
                .collect::<Vec<_>>()
        };
        assert_eq!(nonsystem(request), nonsystem(&sent[0]));
        assert!(row.trial.verified && row.trial.terminal);
        assert!(!row.trial.forced_compaction_applied);
        assert_eq!(row.trial.measurement.response_count, 1);
    }
    let report = instructions::paired(&suite, "fixture-protocol", &rows);
    assert!(report.comparison_valid);
    assert_eq!(report.complete_pairs, 2);
    assert_eq!(report.difference_percentage_points, Some(0.0));
    assert_eq!(report.control.input_tokens, Some(200));
    assert_eq!(report.candidate.input_tokens, Some(200));
    assert!(report.control.provider_reported_cost_usd.is_none());
    assert!(report.candidate.user_interventions.is_none());
    assert!(!report.promotion_allowed);
}

use super::*;
use dex_loop::{Context, Cursor, Message, ModelChunk, ModelError, ProviderReasoning, ToolSpec};
use futures_util::{Stream, stream};
use maestro_local_host::agent::{NativeAgentConfig, dex_loop_execution_host};
use maestro_runtime::agent::CredentialVault;
use serde_json::json;
use std::sync::Mutex;

// Deliberately not Clone: sharing the model must preserve HostTurnRun's API.
struct ModelFixture {
    seen: Arc<Mutex<Vec<Context>>>,
    summaries: Arc<Mutex<Vec<Context>>>,
    prepared: Arc<Mutex<usize>>,
}

const SUMMARY: &str = "Two fixture reads returned retrievable original evidence. Retain uncertainty; no writes or publishing without approval.";

fn reasoning() -> ProviderReasoning {
    ProviderReasoning {
        format: "anthropic.messages.v1".into(),
        model: "fixture-model".into(),
        payload: json!([{"thinking": "opaque continuation", "signature": "exact-signature"}]),
    }
}

impl Model for ModelFixture {
    fn prepare_turn(&self, _ctx: &Context) {
        *self.prepared.lock().unwrap() += 1;
    }

    fn stream<'a>(
        &'a self,
        ctx: &'a Context,
        tools: &'a [&'a ToolSpec],
    ) -> impl Stream<Item = Result<ModelChunk, ModelError>> + Send + 'a {
        if tools.is_empty() {
            // Complete inline outputs exceed the lossless digest bound, so
            // production asks this same model for a separate concise summary.
            self.summaries.lock().unwrap().push(ctx.clone());
            return stream::iter(vec![Ok(ModelChunk::Text(SUMMARY.into()))]);
        }
        let mut seen = self.seen.lock().unwrap();
        seen.push(ctx.clone());
        let chunks = if seen.len() <= 2 {
            vec![
                Ok(ModelChunk::ToolCall {
                    name: ToolName::new("fixture.read"),
                    args: json!({}),
                }),
                Ok(ModelChunk::Reasoning(reasoning())),
            ]
        } else {
            vec![Ok(ModelChunk::Text("done".into()))]
        };
        stream::iter(chunks)
    }
}

fn thread() -> ThreadId {
    ThreadId {
        org: "local".into(),
        workspace: "local".into(),
        thread: "compaction".into(),
    }
}

fn tools(workspace: &Path) -> HostTools {
    let config = NativeAgentConfig {
        cwd: workspace.to_string_lossy().into_owned(),
        ..NativeAgentConfig::default()
    };
    let host = dex_loop_execution_host(&config, CredentialVault::new()).unwrap();
    HostTools::new(host, maestro_runtime::agent::ApprovalMode::Selective).with_client_tools([
        ToolSpec {
            name: ToolName::new("fixture.read"),
            description: "Return fixture evidence".into(),
            label: "Read evidence".into(),
            schema: json!({"type":"object"}),
            read_only: true,
            core: true,
            governance: dex_loop::GovernanceClass::Plain,
            executor: dex_loop::ExecutorKind::Client,
        },
    ])
}

#[tokio::test]
async fn long_turn_compacts_and_replays_exact_input_steer_and_raw_pairs() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let summaries = Arc::new(Mutex::new(Vec::new()));
    let prepared = Arc::new(Mutex::new(0));
    let request = HostTurn {
        thread: thread(),
        principal: PrincipalId::new("alice"),
        turn: TurnId::new("one"),
        prompt: "Inspect only. Never publish without approval.\nKeep this exact input.".into(),
        attachments: vec!["exact-attachment.txt".into()],
        approval: ApprovalMode::Interactive,
    };
    let mut run = HostTurnRun::start(
        dir.path(),
        ModelFixture {
            seen: seen.clone(),
            summaries: summaries.clone(),
            prepared: prepared.clone(),
        },
        tools(workspace.path()),
        request,
    )
    .await
    .unwrap();
    let initial = run.local.read_all().await.unwrap();
    let original = rehydrate(thread(), &initial).history()[0].clone();
    let mut projection = rehydrate(thread(), &initial);
    let mut parks = 0;
    let mut compacted = 0;
    let output = "retrievable original evidence\n".repeat(1_100);
    loop {
        match tokio::time::timeout(std::time::Duration::from_secs(10), run.next())
            .await
            .expect("turn observer must progress")
            .unwrap()
        {
            Step::Observed(Observed::Event(cursor, event)) => {
                // A caller rehydrate can already include a durable observer row.
                if cursor > projection.cursor() {
                    projection.observe(cursor, &event);
                }
                if matches!(*event, Event::Compaction { .. }) {
                    compacted += 1;
                }
            }
            Step::Observed(Observed::Text(_)) => {}
            Step::Park(Park::ClientTool { call, .. }) => {
                parks += 1;
                run.client_result(call, true, output.clone()).await.unwrap();
                if parks == 1 {
                    run.local
                        .append(&[Event::Steer {
                            principal: PrincipalId::new("bob"),
                            text: "Exact correction: retain uncertainty; no writes.".into(),
                        }])
                        .await
                        .unwrap();
                }
                // Include caller-written rows before resuming. An observer may
                // subsequently replay a row already in this projection.
                let rows = run.local.read_all().await.unwrap();
                projection = rehydrate(thread(), &rows);
            }
            Step::Park(_) => panic!("read-only fixture unexpectedly asks for approval"),
            Step::Exit(exit) => {
                assert_eq!(exit, Exit::Done);
                break;
            }
        }
    }
    assert_eq!(parks, 2);
    assert_eq!(
        compacted, 1,
        "ordinary production threshold activates inside one long turn"
    );
    let snapshots = seen.lock().unwrap().clone();
    assert_eq!(
        snapshots.len(),
        3,
        "summary inference does not consume an ordinary turn response"
    );
    let summaries = summaries.lock().unwrap().clone();
    assert_eq!(summaries.len(), 1, "one semantic summary is required");
    assert_eq!(summaries[0].thread(), &thread());
    let [summary_input] = summaries[0].history() else {
        panic!("summary receives one historical transcript request");
    };
    let Message::User {
        text,
        principal,
        attachments,
        ..
    } = &summary_input.message
    else {
        panic!("summary receives a user request");
    };
    assert_eq!(
        principal.as_str(),
        "bob",
        "summary retains the acting principal after the steer"
    );
    assert!(attachments.is_empty());
    let (_, transcript) = text.split_once("\n\n").unwrap();
    let transcript: Vec<serde_json::Value> = serde_json::from_str(transcript).unwrap();
    let summarized_tools: Vec<_> = transcript
        .iter()
        .filter(|entry| entry["message"]["role"] == "tool")
        .collect();
    assert_eq!(summarized_tools.len(), 2);
    for entry in &summarized_tools {
        assert_eq!(entry["message"]["name"], "fixture.read");
        assert_eq!(
            entry["message"]["output"],
            json!({"kind": "text", "value": output})
        );
    }
    assert!(snapshots[1].history().iter().any(|entry| matches!(&entry.message, Message::Assistant { reasoning: Some(value), .. } if value == &reasoning())));
    let compacted = &snapshots[2];
    let summary = compacted
        .history()
        .iter()
        .find_map(|entry| match &entry.message {
            Message::Summary { text } => {
                Some(serde_json::from_str::<serde_json::Value>(text).unwrap())
            }
            _ => None,
        })
        .expect("semantic summary is installed in the compacted projection");
    assert_eq!(summary["summary"], SUMMARY);
    assert!(
        compacted.history().contains(&original),
        "principal, cursor, text and attachment identity remain exact"
    );
    assert!(compacted.history().iter().any(|entry| matches!(&entry.message, Message::User { text, principal, .. } if text == "Exact correction: retain uncertainty; no writes." && principal.as_str() == "bob")));
    assert!(
        compacted.history().iter().all(|entry| matches!(
            entry.message,
            Message::Summary { .. } | Message::User { .. }
        )),
        "signed steps never replay beneath a changed prefix"
    );
    let rows = run.local.read_all().await.unwrap();
    assert_eq!(
        projection,
        rehydrate(thread(), &rows),
        "accepted event projection and full replay agree"
    );
    let calls: Vec<_> = rows
        .iter()
        .filter_map(|(_, event)| match event {
            Event::ModelStepCompleted {
                calls,
                reasoning: Some(value),
                ..
            } => {
                assert_eq!(value, &reasoning());
                Some(calls[0].id.clone())
            }
            _ => None,
        })
        .collect();
    assert_eq!(calls.len(), 2);
    for call in calls {
        assert!(
            summarized_tools
                .iter()
                .any(|entry| entry["message"]["call"] == json!(call)),
            "summary sees each original call/result pair"
        );
        assert!(rows.iter().any(|(_, event)| matches!(event, Event::ClientToolResult { call: result_call, output: result, .. } if result_call == &call && result == &output)), "raw call/result pairing and complete output survive");
    }
    drop(snapshots);
    drop(run);
    let reopened = LocalLog::acquire(dir.path().join("log"), &thread())
        .await
        .unwrap();
    assert_eq!(
        reopened.read_all().await.unwrap(),
        rows,
        "reacquiring a lease retains every original row"
    );
    assert_eq!(
        projection,
        rehydrate(thread(), &reopened.read_all().await.unwrap()),
        "restart preserves summary and terminal projection"
    );
    assert!(rows.iter().any(
        |(cursor, event)| *cursor > Cursor::START && matches!(event, Event::Compaction { .. })
    ));
}

#[test]
fn shared_model_forwards_preparation_without_requiring_clone() {
    let prepared = Arc::new(Mutex::new(0));
    let shared = SharedModel(Arc::new(ModelFixture {
        seen: Arc::new(Mutex::new(Vec::new())),
        summaries: Arc::new(Mutex::new(Vec::new())),
        prepared: prepared.clone(),
    }));
    shared.clone().prepare_turn(&Context::new(thread()));
    assert_eq!(*prepared.lock().unwrap(), 1);
}

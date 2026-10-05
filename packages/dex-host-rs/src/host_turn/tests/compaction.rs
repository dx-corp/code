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
    prepared: Arc<Mutex<usize>>,
}

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
        assert!(
            !tools.is_empty(),
            "tool-heavy summary uses the existing mechanical path"
        );
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
        "pruning must not invoke another model call"
    );
    assert!(snapshots[1].history().iter().any(|entry| matches!(&entry.message, Message::Assistant { reasoning: Some(value), .. } if value == &reasoning())));
    let compacted = &snapshots[2];
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
        prepared: prepared.clone(),
    }));
    shared.clone().prepare_turn(&Context::new(thread()));
    assert_eq!(*prepared.lock().unwrap(), 1);
}

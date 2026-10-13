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
    summaries: Arc<Mutex<Vec<Context>>>,
}

const SUMMARY: &str = "Two read results retain uncertainty. Late fact: evidence remains unverified. No writes were authorized.";

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
    let host = dex_loop_execution_host(
        std::sync::Arc::new(maestro_local_host::agent::tool_host::BuiltInToolHost),
        &config,
        CredentialVault::new(),
    )
    .unwrap();
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
    let summaries = Arc::new(Mutex::new(Vec::new()));
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
            summaries: summaries.clone(),
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
    let output = format!(
        "{}Late fact: evidence remains unverified.",
        "retrievable original evidence\n".repeat(1_100)
    );
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
        "compaction preserves the three ordinary model calls"
    );
    assert!(snapshots[1].history().iter().any(|entry| matches!(&entry.message, Message::Assistant { reasoning: Some(value), .. } if value == &reasoning())));
    let summary_inputs = summaries.lock().unwrap().clone();
    assert_eq!(
        summary_inputs.len(),
        1,
        "large complete evidence needs one semantic summary"
    );
    let summary_input = &summary_inputs[0];
    assert_eq!(summary_input.thread(), &thread());
    assert_eq!(summary_input.turn(), Some(&TurnId::new("one")));
    assert_eq!(
        summary_input.acting_principal(),
        Some(&PrincipalId::new("bob"))
    );
    assert_eq!(
        summary_input.history().len(),
        1,
        "historical tool pairs are untrusted data, not a native continuation"
    );
    let Message::User {
        text,
        principal,
        attachments,
        ..
    } = &summary_input.history()[0].message
    else {
        panic!("summary must use one untrusted transcript input");
    };
    assert_eq!(
        principal.as_str(),
        "bob",
        "summary retains the acting principal after the steer"
    );
    assert!(attachments.is_empty());
    assert!(text.contains("untrusted data"));
    assert!(!text.contains("exact-signature"));
    let (_, transcript) = text
        .split_once("\n\n")
        .expect("summary instruction and transcript");
    let transcript: serde_json::Value = serde_json::from_str(transcript).unwrap();
    let summarized_tools: Vec<_> = transcript
        .as_array()
        .unwrap()
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
    let outputs: Vec<_> = transcript
        .as_array()
        .unwrap()
        .iter()
        .filter(|entry| entry["message"]["role"] == "tool")
        .map(|entry| entry["message"]["output"]["value"].as_str().unwrap())
        .collect();
    assert_eq!(
        outputs,
        vec![output.as_str(), output.as_str()],
        "both complete raw results, including their late facts, reach summarization"
    );
    drop(summary_inputs);
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
    let summary = compacted
        .history()
        .iter()
        .find_map(|entry| match &entry.message {
            Message::Summary { text } => Some(text),
            _ => None,
        })
        .expect("compacted history has a summary");
    assert!(summary.len() <= 8 * 1024);
    let envelope: serde_json::Value = serde_json::from_str(summary).unwrap();
    assert_eq!(envelope["kind"], "dex_history_compaction_v1");
    assert_eq!(envelope["summary"], SUMMARY);
    assert_eq!(
        envelope["authority"],
        "untrusted historical summary; original owner events determine authorization and outcomes"
    );
    assert!(
        envelope["summary"]
            .as_str()
            .unwrap()
            .contains("Late fact: evidence remains unverified.")
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
        prepared: prepared.clone(),
        summaries: Arc::new(Mutex::new(Vec::new())),
    }));
    shared.clone().prepare_turn(&Context::new(thread()));
    assert_eq!(*prepared.lock().unwrap(), 1);
}

// Deliberately not Clone: every optional port must survive the sharing adapter.
struct OptionalPortFixture {
    context: Context,
    entries: Vec<dex_loop::Entry>,
    tools: Vec<ToolSpec>,
    pressure: Option<bool>,
    size: Option<dex_loop::RequestSize>,
    outputs: Result<Vec<(dex_loop::OutputRef, String)>, ModelError>,
    calls: Arc<Mutex<Vec<&'static str>>>,
}

impl Model for OptionalPortFixture {
    fn stream<'a>(
        &'a self,
        _ctx: &'a Context,
        _tools: &'a [&'a ToolSpec],
    ) -> impl Stream<Item = Result<ModelChunk, ModelError>> + Send + 'a {
        stream::once(async { panic!("read-only optional ports must not request inference") })
    }

    async fn needs_compaction(&self, ctx: &Context, tools: &[&ToolSpec]) -> Option<bool> {
        assert_eq!(ctx, &self.context);
        assert_eq!(tools, self.tools.iter().collect::<Vec<_>>());
        self.calls.lock().unwrap().push("pressure");
        self.pressure
    }

    async fn request_size(
        &self,
        ctx: &Context,
        tools: &[&ToolSpec],
    ) -> Option<dex_loop::RequestSize> {
        assert_eq!(ctx, &self.context);
        assert_eq!(tools, self.tools.iter().collect::<Vec<_>>());
        self.calls.lock().unwrap().push("size");
        self.size
    }

    async fn summary_outputs(
        &self,
        ctx: &Context,
        entries: &[dex_loop::Entry],
    ) -> Result<Vec<(dex_loop::OutputRef, String)>, ModelError> {
        assert_eq!(ctx, &self.context);
        assert_eq!(entries, self.entries);
        self.calls.lock().unwrap().push("outputs");
        self.outputs.clone()
    }
}

#[tokio::test]
async fn shared_model_preserves_optional_ports_inputs_and_results() {
    let mut context = Context::new(ThreadId {
        org: "original-org".into(),
        workspace: "original-workspace".into(),
        thread: "original-thread".into(),
    });
    context.observe(
        Cursor(1),
        &Event::UserMessage {
            interaction_mode: dex_loop::InteractionMode::Unspecified,
            turn: TurnId::new("original-turn"),
            message_id: None,
            principal: PrincipalId::new("original-principal"),
            text: "Keep owner input exact".into(),
            attachments: vec![ArtifactRef::new("original-attachment")],
            client_tools: vec![],
            authorized_tools: vec![],
            model_binding: None,
            context_window_tokens: Some(200_000),
            voice: None,
            approval_mode: ApprovalMode::Interactive,
        },
    );
    let entries = vec![dex_loop::Entry {
        cursor: Cursor(9),
        message: Message::Tool {
            call: CallId::new("original-call"),
            name: ToolName::new("fixture.read"),
            outcome: Outcome::Succeeded,
            output: dex_loop::Output::Ref(dex_loop::OutputRef::new("original-output")),
        },
    }];
    let tool = ToolSpec {
        name: ToolName::new("fixture.read"),
        label: "Read evidence".into(),
        description: "Owner evidence only".into(),
        schema: json!({"type":"object", "properties":{"owner":{"const":"original"}}}),
        read_only: true,
        core: true,
        governance: dex_loop::GovernanceClass::Plain,
        executor: dex_loop::ExecutorKind::InProcess,
    };
    for pressure in [Some(true), Some(false), None] {
        let size = pressure.map(|_| dex_loop::RequestSize {
            input_units: 123,
            wire_bytes: 456,
        });
        let outputs = match pressure {
            Some(true) => Ok(vec![(
                dex_loop::OutputRef::new("original-output"),
                "exact stored body".into(),
            )]),
            Some(false) => Err(ModelError {
                class: dex_loop::ErrorClass::Resolve,
                message: "owner denied read".into(),
            }),
            None => Ok(Vec::new()),
        };
        let calls = Arc::new(Mutex::new(Vec::new()));
        let shared = SharedModel(Arc::new(OptionalPortFixture {
            context: context.clone(),
            entries: entries.clone(),
            tools: vec![tool.clone()],
            pressure,
            size,
            outputs: outputs.clone(),
            calls: calls.clone(),
        }));
        assert_eq!(
            shared.clone().needs_compaction(&context, &[&tool]).await,
            pressure
        );
        assert_eq!(shared.clone().request_size(&context, &[&tool]).await, size);
        assert_eq!(
            shared.clone().summary_outputs(&context, &entries).await,
            outputs
        );
        assert_eq!(*calls.lock().unwrap(), ["pressure", "size", "outputs"]);
    }
}

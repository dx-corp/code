//! Deterministic transport qualification, not a model quality benchmark.
#[allow(dead_code)]
mod support;

use std::sync::{Arc, Mutex};
use std::time::Instant;

use dex_loop::{
    Budget, CancellationToken, Context, Engine, Exit, Lexicon, Message, Outcome, Output,
    PrincipalId, ProposedCall, ThreadId, ToolName, ToolResult, ToolSpec, Tools, Verdict,
};
use serde_json::{Value, json};
use support::*;

struct Pages {
    catalog: Vec<ToolSpec>,
    calls: Arc<Mutex<Vec<Value>>>,
}

impl Tools for Pages {
    fn catalog(&self) -> &[ToolSpec] {
        &self.catalog
    }
    async fn search(&self, _: &PrincipalId, _: &str) -> Vec<ToolName> {
        Vec::new()
    }
    async fn policy(&self, _: &Context, _: &ProposedCall) -> Verdict {
        Verdict::Allow
    }
    async fn run(&self, _: &ThreadId, call: &ProposedCall, _: &CancellationToken) -> ToolResult {
        self.calls.lock().unwrap().push(call.args.clone());
        let page = call.args["page"].as_u64().unwrap();
        ToolResult {
            outcome: Outcome::Succeeded,
            output: Output::Text(json!({
                "next": if page < 3 {Some(page + 1)} else {None},
                "rows": [
                    {"id":format!("invoice-{page}"),"overdue":true,"notes":"private detail ".repeat(200)},
                    {"id":format!("paid-{page}"),"overdue":false,"notes":"irrelevant detail ".repeat(200)},
                ],
            }).to_string()),
            receipt: None,
        }
    }
}

fn tool_texts(history: &[Message]) -> Vec<&str> {
    history
        .iter()
        .filter_map(|message| match message {
            Message::Tool {
                output: Output::Text(text),
                ..
            } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

// Count textual history payloads, including model-authored script arguments.
// Provider framing, tool declarations and tokenization are outside this fixture.
fn history_payload_bytes(history: &[Message]) -> usize {
    history
        .iter()
        .map(|message| match message {
            Message::User { text, .. } | Message::Summary { text } => text.len(),
            Message::Assistant { text, calls, .. } => {
                text.len()
                    + calls
                        .iter()
                        .map(|call| serde_json::to_vec(&call.args).unwrap().len())
                        .sum::<usize>()
            }
            Message::Tool {
                output: Output::Text(text),
                ..
            } => text.len(),
            Message::Tool { .. } => panic!("qualification fixture requires text output"),
        })
        .sum()
}

#[tokio::test]
async fn conditional_pagination_preserves_selection_with_fewer_model_turns_and_context_bytes() {
    let mut measurements = Vec::new();
    let expected = json!(["invoice-1", "invoice-2", "invoice-3"]);
    for scripted in [false, true] {
        let responses = if scripted {
            vec![
                vec![call(
                    "codemode",
                    json!({"code":
                        "let page=1; const selected=[]; do {const result=await tools.invoices({page}); selected.push(...result.rows.filter(row=>row.overdue).map(row=>row.id)); page=result.next;} while(page); text(selected);"
                    }),
                )],
                vec![text("done")],
            ]
        } else {
            vec![
                vec![call("invoices", json!({"page":1}))],
                vec![call("invoices", json!({"page":2}))],
                vec![call("invoices", json!({"page":3}))],
                vec![text("done")],
            ]
        };
        let log = FakeLog::default();
        let model = FakeModel::new(responses);
        let tools = Pages {
            catalog: vec![read_tool("invoices")],
            calls: Arc::default(),
        };
        let dispatched = tools.calls.clone();
        let mut context = log.start_turn(
            "qualification",
            "Select overdue invoice IDs across all pages.",
        );
        let engine = Engine::new(
            log.clone(),
            model.clone(),
            tools,
            FakeEffects::default(),
            Lexicon::default(),
            Budget::default(),
        );
        let start = Instant::now();
        assert_eq!(
            engine.run(&mut context, &CancellationToken::new()).await,
            Ok(Exit::Done)
        );
        let elapsed = start.elapsed();
        let histories = model.seen();
        let last = histories.last().unwrap();
        let results = tool_texts(last);
        let selected = if scripted {
            assert_eq!(
                results.len(),
                1,
                "only the explicit selection reaches model history"
            );
            assert!(!results[0].contains("private detail"));
            serde_json::from_str::<Value>(results[0]).unwrap()
        } else {
            let mut selected = Vec::new();
            for page in results {
                let page: Value = serde_json::from_str(page).unwrap();
                selected.extend(
                    page["rows"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .filter(|row| row["overdue"] == true)
                        .map(|row| row["id"].clone()),
                );
            }
            Value::Array(selected)
        };
        assert_eq!(selected, expected);
        let owner_calls = log
            .events()
            .iter()
            .filter(|event| {
                matches!(event,
            dex_loop::Event::ToolStarted { tool, .. } if tool.as_str() == "invoices")
            })
            .count();
        assert_eq!(
            owner_calls, 3,
            "both routes execute the same admitted reads"
        );
        assert_eq!(
            *dispatched.lock().unwrap(),
            vec![json!({"page":1}), json!({"page":2}), json!({"page":3})]
        );
        let bytes: usize = histories
            .iter()
            .map(|history| history_payload_bytes(history))
            .sum();
        measurements.push((model.calls(), bytes, elapsed));
    }
    assert_eq!(measurements[0].0, 4);
    assert_eq!(measurements[1].0, 2);
    assert!(measurements[1].1 < measurements[0].1 / 4);
    println!(
        "fixture qualification: direct turns={} history_payload_bytes={} elapsed_us={}; script turns={} history_payload_bytes={} elapsed_us={}; identical selection and three owner reads",
        measurements[0].0,
        measurements[0].1,
        measurements[0].2.as_micros(),
        measurements[1].0,
        measurements[1].1,
        measurements[1].2.as_micros()
    );
}

struct Fanout {
    catalog: Vec<ToolSpec>,
    calls: Arc<Mutex<Vec<Value>>>,
}

impl Tools for Fanout {
    fn catalog(&self) -> &[ToolSpec] {
        &self.catalog
    }
    async fn search(&self, _: &PrincipalId, _: &str) -> Vec<ToolName> {
        Vec::new()
    }
    async fn policy(&self, _: &Context, _: &ProposedCall) -> Verdict {
        Verdict::Allow
    }
    async fn run(&self, _: &ThreadId, call: &ProposedCall, _: &CancellationToken) -> ToolResult {
        self.calls.lock().unwrap().push(call.args.clone());
        let id = call.args["id"].as_u64().unwrap();
        ToolResult {
            outcome: if id == 2 {
                Outcome::Failed
            } else {
                Outcome::Succeeded
            },
            output: Output::Text(if id == 2 {
                "source unavailable".into()
            } else {
                json!({"id":id,"detail":"private detail ".repeat(200)}).to_string()
            }),
            receipt: None,
        }
    }
}

#[tokio::test]
async fn fanout_fixture_preserves_successes_and_visible_failure_without_replaying_owner_reads() {
    let log = FakeLog::default();
    let model = FakeModel::new(vec![
        vec![call(
            "codemode",
            json!({"code":
                "const rows=await Promise.allSettled([1,2,3].map(id=>tools.lookup({id}))); text(rows.map((row,i)=>row.status==='fulfilled'?{id:row.value.id}:{id:i+1,error:'source unavailable'}));"
            }),
        )],
        vec![text("done")],
    ]);
    let dispatched = Arc::default();
    let tools = Fanout {
        catalog: vec![read_tool("lookup")],
        calls: Arc::clone(&dispatched),
    };
    let mut context = log.start_turn(
        "fanout-qualification",
        "Select IDs and identify unavailable sources.",
    );
    assert_eq!(
        Engine::new(
            log.clone(),
            model.clone(),
            tools,
            FakeEffects::default(),
            Lexicon::default(),
            Budget::default()
        )
        .run(&mut context, &CancellationToken::new())
        .await,
        Ok(Exit::Done)
    );
    let histories = model.seen();
    let results = tool_texts(histories.last().unwrap());
    assert_eq!(results.len(), 1);
    assert_eq!(
        serde_json::from_str::<Value>(results[0]).unwrap(),
        json!([{ "id":1 }, { "id":2,"error":"source unavailable" }, { "id":3 }])
    );
    let mut calls = dispatched.lock().unwrap().clone();
    calls.sort_by_key(|args| args["id"].as_u64().unwrap());
    assert_eq!(
        calls,
        vec![json!({"id":1}), json!({"id":2}), json!({"id":3})]
    );
    let events = log.events();
    let started: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            dex_loop::Event::ToolStarted { call, tool, .. } if tool.as_str() == "lookup" => {
                Some(call)
            }
            _ => None,
        })
        .collect();
    let finishes: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            dex_loop::Event::ToolFinished { call, outcome, .. } if started.contains(&call) => {
                Some(*outcome)
            }
            _ => None,
        })
        .collect();
    assert_eq!(finishes.len(), 3);
    assert_eq!(
        finishes
            .iter()
            .filter(|outcome| **outcome == Outcome::Failed)
            .count(),
        1
    );
    assert_eq!(log.rehydrate(), context);
}

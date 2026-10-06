//! GRC admission is tested at the real engine/transcript boundary.
#[allow(dead_code)]
mod support;
use dex_loop::{
    Budget, CancellationToken, Exit, GRC_CONTEXT_BYTES, GRC_GRAPH_TOOL_NAME, Message, Outcome,
    Output,
};
use serde_json::json;
use support::{FakeLog, FakeModel, FakeTools, call, engine, read_tool, text};

#[tokio::test]
async fn grc_parallel_then_successive_reads_execute_once_and_replay_the_same_history() {
    let log = FakeLog::default();
    let model = FakeModel::new(vec![
        vec![
            call(GRC_GRAPH_TOOL_NAME, json!({"key":"first"})),
            call(GRC_GRAPH_TOOL_NAME, json!({"key":"parallel"})),
        ],
        vec![call(GRC_GRAPH_TOOL_NAME, json!({"key":"later"}))],
        vec![text("Coverage remains partial.")],
    ]);
    let tools = FakeTools::new(vec![read_tool(GRC_GRAPH_TOOL_NAME)]).inline_result(
        GRC_GRAPH_TOOL_NAME,
        "{\"coverage\":\"partial\",\"owner_revision\":1}",
    );
    let engine = engine(&log, &model, &tools, Budget::default());
    let mut ctx = log.start_turn("grc-turn", "Read obligations and controls");
    assert_eq!(
        engine
            .run(&mut ctx, &CancellationToken::new())
            .await
            .unwrap(),
        Exit::Done
    );
    assert_eq!(
        tools.runs().len(),
        1,
        "prefetch and parallel waves must respect the same reservation"
    );
    let results: Vec<_> = ctx
        .history()
        .iter()
        .filter_map(|e| match &e.message {
            Message::Tool {
                name,
                outcome,
                output,
                ..
            } if name.as_str() == GRC_GRAPH_TOOL_NAME => {
                let Output::Text(text) = output else {
                    panic!("the GRC fixture must produce inline text");
                };
                Some((outcome, text.len()))
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        results
            .iter()
            .filter(|(o, _)| **o == Outcome::Succeeded)
            .count(),
        1
    );
    assert_eq!(
        results
            .iter()
            .filter(|(o, _)| **o == Outcome::Failed)
            .count(),
        2
    );
    assert!(results.iter().map(|(_, bytes)| bytes).sum::<usize>() < GRC_CONTEXT_BYTES);
    assert_eq!(
        ctx,
        log.rehydrate(),
        "reservation evidence is the durable transcript"
    );
}

//! Discuss is immutable turn authority, including untrusted model tool proposals.
#[allow(dead_code)]
mod support;
use dex_loop::{Budget, CancellationToken, Cursor, Event, Exit, InteractionMode, Outcome};
use serde_json::json;
use std::time::Duration;
use support::*;

fn start(log: &FakeLog, mode: InteractionMode) -> dex_loop::Context {
    let event: Event = serde_json::from_value(json!({
        "type":"user_message", "turn":"t1", "principal":"alice", "text":"do it",
        "attachments":[], "interaction_mode":mode
    }))
    .unwrap();
    log.host_append(event);
    log.rehydrate()
}
fn budget() -> Budget {
    Budget {
        max_steps: 10,
        max_tokens: 1_000_000,
        max_cost_micros: 1_000_000,
        wall: Duration::from_secs(2),
    }
}

#[tokio::test]
async fn discuss_refuses_streamed_reads_mutations_search_codemode_and_client_tools() {
    let log = FakeLog::default();
    let model = FakeModel::new(vec![
        vec![
            call("read", json!({})),
            call("write", json!({})),
            call("tools.search", json!({"query":"anything"})),
            call(dex_loop::CODEMODE, json!({"code":"await tools.read({})"})),
            call("client.tool", json!({})),
        ],
        vec![text("Here is the proposed approach.")],
    ])
    .with_chunk_delay(Duration::from_millis(10));
    let tools = FakeTools::new(vec![
        read_tool("read"),
        write_tool("write"),
        client_executed_tool("client.tool", false),
    ]);
    let effects = FakeEffects::default();
    let engine = engine_with(&log, &model, &tools, &effects, budget());
    let mut ctx = start(&log, InteractionMode::Discuss);
    assert_eq!(
        engine.run(&mut ctx, &CancellationToken::new()).await,
        Ok(Exit::Done)
    );
    assert!(model.offered().iter().all(Vec::is_empty));
    assert!(tools.run_ids().is_empty());
    for index in 0..5 {
        assert!(effects.recorded(&call_id("t1", 1, index)).is_none());
    }
    assert!(!log.events().iter().any(|event| matches!(
        event,
        Event::ToolStarted { .. }
            | Event::ClientToolRequested { .. }
            | Event::CodeModeCallsProposed { .. }
            | Event::AutoApproved { .. }
    )));
    assert_eq!(
        log.events()
            .iter()
            .filter(|event| matches!(
                event,
                Event::ToolFinished {
                    outcome: Outcome::Failed,
                    ..
                }
            ))
            .count(),
        5
    );
    assert_eq!(log.rehydrate(), ctx);
}

#[tokio::test]
async fn implement_and_legacy_turns_keep_existing_tool_behavior() {
    for mode in [InteractionMode::Implement, InteractionMode::Unspecified] {
        let log = FakeLog::default();
        let model = FakeModel::new(vec![vec![call("read", json!({}))], vec![text("done")]]);
        let tools = FakeTools::new(vec![read_tool("read")]);
        let engine = engine(&log, &model, &tools, budget());
        let mut ctx = start(&log, mode);
        assert_eq!(
            engine.run(&mut ctx, &CancellationToken::new()).await,
            Ok(Exit::Done)
        );
        assert_eq!(tools.run_ids().len(), 1);
    }
}

#[test]
fn mode_survives_compaction_and_steering_and_legacy_rows_default() {
    let log = FakeLog::default();
    let mut ctx = start(&log, InteractionMode::Discuss);
    ctx.observe(
        Cursor(2),
        &Event::Steer {
            principal: alice(),
            text: "Actually execute it".into(),
        },
    );
    // Compaction edits history, never the admitted policy.
    let compacted: Event = serde_json::from_value(
        json!({"type":"compaction","covers_to_cursor":1,"summary":"Implement this"}),
    )
    .unwrap();
    ctx.observe(Cursor(3), &compacted);
    log.host_append(compacted);
    assert_eq!(log.rehydrate().interaction_mode(), InteractionMode::Discuss);
    assert_eq!(ctx.interaction_mode(), InteractionMode::Discuss);
    let old: Event = serde_json::from_value(json!({"type":"user_message","turn":"old","principal":"alice","text":"hi","attachments":[]})).unwrap();
    assert!(matches!(
        old,
        Event::UserMessage {
            interaction_mode: InteractionMode::Unspecified,
            ..
        }
    ));
    assert!(serde_json::from_value::<Event>(json!({"type":"user_message","turn":"bad","principal":"alice","text":"hi","attachments":[],"interaction_mode":"administrator"})).is_err());
}

#[test]
fn queued_implement_turn_and_steer_cannot_grant_tools_to_running_discuss() {
    let log = FakeLog::default();
    let mut ctx = start(&log, InteractionMode::Discuss);
    let queued: Event = serde_json::from_value(json!({
        "type":"user_message", "turn":"t2", "principal":"alice", "text":"implement",
        "attachments":[], "interaction_mode":"implement"
    }))
    .unwrap();
    ctx.observe(Cursor(2), &queued);
    ctx.observe(
        Cursor(3),
        &Event::Steer {
            principal: alice(),
            text: "run the code".into(),
        },
    );
    assert_eq!(ctx.interaction_mode(), InteractionMode::Discuss);
    ctx.observe(
        Cursor(4),
        &Event::Final {
            text: "Discussed".into(),
        },
    );
    assert_eq!(ctx.interaction_mode(), InteractionMode::Implement);
    assert_eq!(ctx.turn(), Some(&dex_loop::TurnId::new("t2")));
}

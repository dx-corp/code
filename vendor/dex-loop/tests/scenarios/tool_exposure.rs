//! Tool exposure outlives the turn for the principal who searched.

use super::*;

// Exposure outlives the turn for the same principal: Alice's next turn can
// call `crm.lookup` through tools.call from step one, with the schema the
// earlier search result left in history, and no search is needed; Bob's
// turn cannot, so what Alice found is not callable by him. The declared
// tools are the same on every step of every turn. Rehydration agrees.
#[tokio::test]
async fn exposed_tools_stay_callable_on_the_principals_later_turns() {
    let log = FakeLog::default();
    let model = FakeModel::new(vec![
        vec![call("tools.search", json!({"query": "crm"}))],
        vec![text("found acme")],
        vec![call(
            "tools.call",
            json!({"name": "crm_lookup", "arguments": {"key": "acme"}}),
        )],
        vec![text("acme again")],
        vec![call(
            "tools.call",
            json!({"name": "crm_lookup", "arguments": {"key": "acme"}}),
        )],
        vec![text("nothing for bob")],
    ]);
    let tools = FakeTools::new(vec![read_tool("search"), hidden_read_tool("crm.lookup")])
        .search_result("crm", &["crm.lookup"]);
    let engine = engine(&log, &model, &tools, budget());
    let mut ctx = log.start_turn("t1", "find acme in the crm");
    assert_eq!(
        engine.run(&mut ctx, &CancellationToken::new()).await,
        Ok(Exit::Done)
    );
    let mut ctx = log.start_turn("t2", "look acme up again");
    assert_eq!(ctx.exposed_tools(), &[ToolName::new("crm.lookup")]);
    assert_eq!(
        engine.run(&mut ctx, &CancellationToken::new()).await,
        Ok(Exit::Done)
    );
    assert_eq!(tools.run_ids(), strings(&["t2-1-0"]));
    let mut ctx = log.start_turn_as(bob(), "t3", "anything for me?");
    assert!(ctx.exposed_tools().is_empty(), "another principal's find");
    assert_eq!(
        engine.run(&mut ctx, &CancellationToken::new()).await,
        Ok(Exit::Done)
    );
    // Bob's call was unwrapped to crm.lookup too, but not exposed to him.
    assert_eq!(tools.run_ids(), strings(&["t2-1-0"]));
    let bob_messages = view(&model.seen()[5]);
    let bob_result = bob_messages.last().expect("bob's tool result");
    assert!(
        bob_result.contains("unknown tool: crm.lookup"),
        "{bob_result}"
    );
    let offered = model.offered();
    assert_eq!(offered.len(), 6);
    for step in &offered {
        assert_eq!(*step, FIXED_TOOLS.map(String::from));
    }
    assert_eq!(log.rehydrate(), ctx);
}

// A tools.call is the call it names: the log, the run and the events carry
// `crm.lookup`, with the arguments it was given. A call with the wrong
// shape is told the field; a name no tool has is unknown.
#[tokio::test]
async fn tools_call_is_the_call_it_names() {
    let log = FakeLog::default();
    let model = FakeModel::new(vec![
        vec![call("tools.search", json!({"query": "crm"}))],
        vec![
            call(
                "tools.call",
                json!({"name": "crm_lookup", "arguments": {"key": "acme"}}),
            ),
            call(
                "tools.call",
                json!({"name": "crm_lookup", "arguments": "acme"}),
            ),
            call("tools.call", json!({"name": "crm_delete", "arguments": {}})),
        ],
        vec![text("done")],
    ]);
    let tools = FakeTools::new(vec![read_tool("search"), hidden_read_tool("crm.lookup")])
        .search_result("crm", &["crm.lookup"]);
    let engine = engine(&log, &model, &tools, budget());
    let mut ctx = log.start_turn("t1", "find acme in the crm");
    assert_eq!(
        engine.run(&mut ctx, &CancellationToken::new()).await,
        Ok(Exit::Done)
    );
    assert_eq!(tools.run_ids(), strings(&["t1-2-0"]));
    let run = tools.run_of(&CallId::new("t1-2-0"));
    assert_eq!(run.args, json!({"key": "acme"}));
    assert!(
        log.shapes_after(1)
            .contains(&"completed::[t1-2-0,t1-2-1,t1-2-2]".to_owned()),
        "{:?}",
        log.shapes_after(1)
    );
    let proposed = ctx.history().iter().find_map(|entry| match &entry.message {
        dex_loop::Message::Assistant { calls, .. } if calls.len() == 3 => Some(calls.clone()),
        _ => None,
    });
    let proposed = proposed.expect("step two's calls");
    assert_eq!(proposed[0].tool, ToolName::new("crm.lookup"));
    assert_eq!(proposed[0].args, json!({"key": "acme"}));
    assert_eq!(proposed[1].tool, ToolName::new("tools.call"));
    assert_eq!(proposed[2].tool, ToolName::new("tools.call"));
    let results = view(&model.seen()[2]);
    assert!(results[4].starts_with("tool:t1-2-0:ok:"), "{results:?}");
    assert!(
        results[5].contains("invalid arguments:") && results[5].contains("arguments"),
        "{results:?}"
    );
    assert!(
        results[6].contains("unknown tool: crm_delete"),
        "{results:?}"
    );
    assert_eq!(log.rehydrate(), ctx);
}

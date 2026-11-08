//! `act`: a change is previewed, then submitted with the preview's digest.

use super::*;

// A preview runs nothing and returns the plan and its digest. A submit with
// that digest, after that preview, is the operation itself: the log names the
// operation, it runs with the previewed arguments, and its own policy
// applies. A submit with another digest or before the preview, a tools.call
// to a change, and an act on a read are refused with what to do instead.
// `act` is declared after tools.call.
#[tokio::test]
async fn a_change_is_previewed_then_submitted_as_the_operation_itself() {
    let args = json!({"key": "acme"});
    let digest = dex_loop::plan_digest(&ToolName::new("crm.update"), &args);
    let log = FakeLog::default();
    let model = FakeModel::new(vec![
        vec![
            call(
                "act",
                json!({"op": "crm_update", "args": args, "phase": "preview"}),
            ),
            call(
                "tools.call",
                json!({"name": "crm_update", "arguments": args}),
            ),
            call(
                "act",
                json!({"op": "crm_lookup", "args": args, "phase": "preview"}),
            ),
            // The right digest before any preview of it: refused.
            call(
                "act",
                json!({"op": "crm_update", "args": args, "phase": "submit", "plan_digest": digest}),
            ),
        ],
        vec![
            call(
                "act",
                json!({"op": "crm_update", "args": args, "phase": "submit", "plan_digest": digest}),
            ),
            call(
                "act",
                json!({"op": "crm_update", "args": {"key": "other"}, "phase": "submit", "plan_digest": digest}),
            ),
        ],
        vec![text("updated")],
    ]);
    let tools = FakeTools::new(vec![write_tool("crm.update"), read_tool("crm.lookup")]).with_act();
    let engine = engine(&log, &model, &tools, budget());
    let mut ctx = log.start_turn("t1", "update acme");
    assert_eq!(
        engine.run(&mut ctx, &CancellationToken::new()).await,
        Ok(Exit::Done)
    );
    assert_eq!(tools.run_ids(), strings(&["t1-2-0"]));
    assert_eq!(tools.run_of(&CallId::new("t1-2-0")).args, args);
    let steps: Vec<Vec<ProposedCall>> = ctx
        .history()
        .iter()
        .filter_map(|entry| match &entry.message {
            dex_loop::Message::Assistant { calls, .. } if !calls.is_empty() => Some(calls.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(steps[0][0].tool, ToolName::new("act"));
    assert_eq!(steps[1][0].tool, ToolName::new("crm.update"));
    assert_eq!(steps[1][1].tool, ToolName::new("act"));
    let first = view(&model.seen()[1]).join("\n");
    assert!(first.contains("nothing has run"), "{first}");
    assert!(first.contains(&digest), "{first}");
    assert!(
        first.contains("crm_update changes something; call act"),
        "{first}"
    );
    assert!(first.contains("crm_lookup is a read"), "{first}");
    assert!(
        first.contains("no preview in this conversation has this plan_digest"),
        "{first}"
    );
    let second = view(&model.seen()[2]).join("\n");
    assert!(
        second.contains("no preview in this conversation has this plan_digest"),
        "{second}"
    );
    assert_eq!(
        model.offered()[0],
        [
            "tools.search",
            "codemode",
            "tools.call",
            "act",
            "crm.update",
            "crm.lookup"
        ]
        .map(String::from)
    );
    assert_eq!(log.rehydrate(), ctx);
}

// Without the host opting in, nothing changes: no `act` is declared and a
// tools.call to a change runs as before.
#[tokio::test]
async fn without_act_the_declared_tools_and_tools_call_are_unchanged() {
    let log = FakeLog::default();
    let model = FakeModel::new(vec![
        vec![call(
            "tools.call",
            json!({"name": "crm_update", "arguments": {"key": "acme"}}),
        )],
        vec![text("done")],
    ]);
    let tools = FakeTools::new(vec![write_tool("crm.update")]);
    let engine = engine(&log, &model, &tools, budget());
    let mut ctx = log.start_turn("t1", "update acme");
    assert_eq!(
        engine.run(&mut ctx, &CancellationToken::new()).await,
        Ok(Exit::Done)
    );
    assert_eq!(tools.run_ids(), strings(&["t1-1-0"]));
    assert!(!model.offered()[0].contains(&"act".to_owned()));
}

// An act operation is not declared, yet `act` reaches it from the first step
// with no search. A preview whose args do not fit returns the operation's
// description and input schema; a preview that fits returns only the plan.
// The submit runs as the operation. A change that is neither an act
// operation nor found stays unknown.
#[tokio::test]
async fn an_act_operation_is_reached_without_a_search_and_a_misfit_preview_describes_it() {
    let args = json!({"key": "acme"});
    let digest = dex_loop::plan_digest(&ToolName::new("crm.update"), &args);
    let preview = |args: serde_json::Value| {
        call(
            "act",
            json!({"op": "crm_update", "args": args, "phase": "preview"}),
        )
    };
    let log = FakeLog::default();
    let model = FakeModel::new(vec![
        vec![
            preview(json!({"nope": 1})),
            call(
                "act",
                json!({"op": "crm_delete", "args": args, "phase": "preview"}),
            ),
        ],
        vec![preview(args.clone())],
        vec![call(
            "act",
            json!({"op": "crm_update", "args": args, "phase": "submit", "plan_digest": digest}),
        )],
        vec![preview(json!({"key": "other"}))],
        vec![text("updated")],
    ]);
    let update = dex_loop::ToolSpec {
        core: false,
        description: "Update one CRM record by key.".into(),
        schema: json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["key"],
            "properties": {"key": {"type": "string"}},
        }),
        ..write_tool("crm.update")
    };
    let delete = dex_loop::ToolSpec {
        core: false,
        ..write_tool("crm.delete")
    };
    let tools = FakeTools::new(vec![update, delete])
        .with_act()
        .with_act_operations(&[("crm.update", "update a CRM record: {key}.")]);
    let engine = engine(&log, &model, &tools, budget());
    let mut ctx = log.start_turn("t1", "update acme");
    assert_eq!(
        engine.run(&mut ctx, &CancellationToken::new()).await,
        Ok(Exit::Done)
    );
    assert_eq!(
        model.offered()[0],
        ["tools.search", "codemode", "tools.call", "act"].map(String::from)
    );
    assert_eq!(tools.run_ids(), strings(&["t1-3-0"]));
    assert_eq!(tools.run_of(&CallId::new("t1-3-0")).args, args);
    let last = |step: usize| {
        view(&model.seen()[step])
            .last()
            .cloned()
            .unwrap_or_default()
    };
    let first = view(&model.seen()[1]).join("\n");
    assert!(
        first.contains("crm_update: Update one CRM record by key. Input schema:"),
        "{first}"
    );
    assert!(first.contains("additionalProperties"), "{first}");
    assert!(first.contains("unknown tool: crm_delete"), "{first}");
    for step in [2, 4] {
        let plan = last(step);
        assert!(plan.contains("nothing has run"), "{plan}");
        assert!(!plan.contains("Update one CRM record"), "{plan}");
    }
    assert_eq!(log.rehydrate(), ctx);
}

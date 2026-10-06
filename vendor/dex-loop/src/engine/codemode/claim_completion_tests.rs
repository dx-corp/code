//! Drive the real VM and claim/dispatch boundary with explicit ordering.
#[allow(dead_code)]
#[path = "../../../tests/support/mod.rs"]
mod support;

use super::*;
use crate::{CallId, Cursor, Lexicon};
use std::sync::Arc;
use support::*;
use tokio::sync::Notify;

#[derive(Clone)]
struct GatedEffectClaim {
    effects: FakeEffects,
    entered: Arc<Notify>,
    release: Arc<Notify>,
}
impl Effects for GatedEffectClaim {
    async fn claim(&self, call: &ProposedCall) -> Result<Claim, Fenced> {
        let claim = self.effects.claim(call).await?;
        if call.tool.as_str() == "update" && matches!(claim, Claim::Granted) {
            self.entered.notify_one();
            self.release.notified().await;
        }
        Ok(claim)
    }
    async fn record(&self, call: &CallId, result: &ToolResult) -> Result<(), Fenced> {
        self.effects.record(call, result).await
    }
}

#[tokio::test]
async fn script_completion_during_effect_claim_never_dispatches_the_queued_effect() {
    let log = FakeLog::default();
    let tools = FakeTools::new(vec![strict_read_tool("lookup"), write_tool("update")]);
    let effects = GatedEffectClaim {
        effects: FakeEffects::default(),
        entered: Arc::new(Notify::new()),
        release: Arc::new(Notify::new()),
    };
    let engine = Engine::new(
        log.clone(),
        FakeModel::new(vec![]),
        tools.clone(),
        effects.clone(),
        Lexicon::default(),
        Budget::default(),
    );
    let mut ctx = log.start_turn("t1", "race during effect claim");
    let code = "text(await Promise.race([tools.lookup({key:'fast'}).then(()=>'fast'),tools.update({key:'effect'})]));";
    let parent = ProposedCall::new(
        CallId::new("t1-1-0"),
        ToolName::new("codemode"),
        serde_json::json!({"code":code}),
        alice(),
    );
    engine
        .emit(
            &mut ctx,
            vec![
                Event::StepStarted {
                    step: 1,
                    control_through: Cursor(1),
                },
                Event::ModelStepCompleted {
                    step: 1,
                    text: String::new(),
                    calls: vec![parent.clone()],
                    reasoning: None,
                    served: None,
                    timing: None,
                },
            ],
        )
        .await
        .unwrap();
    let catalog = tools
        .catalog()
        .iter()
        .map(|entry| agent_codemode::Tool {
            name: entry.name.to_string(),
            description: entry.description.clone(),
            schema: entry.schema.clone(),
            output_schema: None,
            namespace: None,
            namespace_instructions: None,
            model_operation: None,
            model_binding: None,
        })
        .collect();
    let cancel = CancellationToken::new();
    let mut session =
        agent_codemode::Session::start(code.into(), catalog, &cancel, Duration::from_secs(60));
    let script_cancel = session.cancellation_token();
    let Some(agent_codemode::Event::Calls { calls, reply }) = session.next().await else {
        panic!("expected independent read and queued mutation")
    };
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].name, "lookup");
    assert_eq!(calls[1].name, "update");
    let proposals: Vec<_> = calls
        .iter()
        .map(|call| {
            ProposedCall::new(
                CallId::new(format!("t1-1-0:codemode:{}", call.index)),
                ToolName::new(&call.name),
                call.args.clone(),
                alice(),
            )
        })
        .collect();
    engine
        .emit(
            &mut ctx,
            vec![Event::CodeModeCallsProposed {
                parent: parent.id.clone(),
                calls: proposals.clone(),
            }],
        )
        .await
        .unwrap();
    let read = tools.run(ctx.thread(), &proposals[0], &script_cancel).await;
    engine
        .finish(&mut ctx, &proposals[0], read.clone())
        .await
        .unwrap();
    let resolved = tools
        .resolve_codemode_result(&ctx, &proposals[0], &read, 256 * 1024)
        .await
        .unwrap();
    let Output::Text(value) = resolved.output else {
        panic!("resolved read must be text")
    };
    let deadline = Instant::now() + Duration::from_secs(60);
    let entry = tools.spec(&ToolName::new("update")).unwrap();
    let report = {
        let mutation = engine.codemode_mutation(
            &mut ctx,
            &proposals[1],
            entry,
            &cancel,
            &script_cancel,
            deadline,
        );
        tokio::pin!(mutation);
        tokio::select! {
            () = effects.entered.notified() => {},
            result = &mut mutation => panic!("claim settled before release: {result:?}"),
        }
        // Only now can the independent read settle and the real VM complete.
        reply
            .send(vec![(
                calls[0].index,
                Ok(serde_json::from_str(&value).unwrap()),
            )])
            .unwrap();
        let Some(agent_codemode::Event::Done(report)) = session.next().await else {
            panic!("VM must finish while claim is held")
        };
        assert!(report.error.is_none(), "{report:?}");
        assert_eq!(report.content(), "fast");
        assert!(
            script_cancel.is_cancelled(),
            "real VM completion cancels the script before claim release"
        );
        effects.release.notify_one();
        let result = mutation.as_mut().await.unwrap();
        assert_eq!(result.outcome, Outcome::Failed);
        report
    };
    engine
        .finish(&mut ctx, &parent, ToolResult::text(report.content()))
        .await
        .unwrap();
    assert_eq!(
        tools.runs().len(),
        1,
        "the queued mutation cannot run after successful VM completion"
    );
    let queued = effects
        .effects
        .recorded(&CallId::new("t1-1-0:codemode:1"))
        .flatten()
        .unwrap();
    assert_eq!(
        queued.outcome,
        Outcome::Failed,
        "granted but undispatched claim settles as known not executed"
    );
    assert!(log.events().iter().any(|event| matches!(event, Event::ToolFinished { call, outcome: Outcome::Succeeded, output: Output::Text(text), .. } if call == &parent.id && text == "fast")));
    assert!(
        !log.events().iter().any(
            |event| matches!(event, Event::ToolStarted { call, .. } if call == &proposals[1].id)
        )
    );
    assert_eq!(log.rehydrate(), ctx);
}

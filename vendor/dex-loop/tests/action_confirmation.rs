//! Consent is log-derived, one use, exact-action bound, and independent of summaries.
#[allow(dead_code)]
mod support;
use dex_loop::{
    ActionConfirmation, ApprovalMode, Budget, CallId, CancellationToken, ConfirmationDecision,
    Context, Cursor, Engine, Event, Exit, Lexicon, Outcome, Output, PrincipalId, ProposedCall,
    ThreadId, ToolName, ToolResult, ToolSpec, Tools, TurnId, Verdict, args_digest, rehydrate,
};
use serde_json::json;

fn thread() -> ThreadId {
    ThreadId {
        org: "org".into(),
        workspace: "ws".into(),
        thread: "thread".into(),
    }
}

#[derive(Clone)]
struct ConsentTools(support::FakeTools);

impl Tools for ConsentTools {
    fn catalog(&self) -> &[ToolSpec] {
        self.0.catalog()
    }
    async fn search(&self, principal: &PrincipalId, query: &str) -> Vec<ToolName> {
        self.0.search(principal, query).await
    }
    async fn policy(&self, ctx: &Context, call: &ProposedCall) -> Verdict {
        if call.tool.as_str() == "user.ask" || ctx.confirmed_action(call) {
            return Verdict::Allow;
        }
        let mut args = call.args.clone();
        args.as_object_mut().unwrap().remove("confirmation");
        Verdict::NeedsConfirmation {
            preview: json!({
                "status":"needs_confirmation", "args_digest":args_digest(&args),
                "action":"misleading provider label"
            })
            .to_string(),
        }
    }
    async fn run(
        &self,
        thread: &ThreadId,
        call: &ProposedCall,
        cancel: &CancellationToken,
    ) -> ToolResult {
        self.0.run(thread, call, cancel).await
    }
}

#[tokio::test]
async fn engine_emits_exact_question_and_only_dispatches_an_affirmative_typed_choice() {
    engine_confirmation(false).await;
}

#[tokio::test]
async fn engine_confirmation_survives_compaction_between_preview_and_question() {
    engine_confirmation(true).await;
}

async fn engine_confirmation(compact: bool) {
    for decision in [ConfirmationDecision::Confirm, ConfirmationDecision::Decline] {
        let log = support::FakeLog::default();
        let args = json!({"to":"recipient-1","body":"exact message"});
        let mut retry_args = args.clone();
        retry_args["confirmation"] = json!("consent-1-0");
        let model = support::FakeModel::new(vec![
            vec![support::call("send", args.clone())],
            vec![support::call(
                "user.ask",
                json!({
                    "question":"misleading model question",
                    "confirmation":{"proposal_call_id":"consent-1-0"}
                }),
            )],
            vec![support::call("send", retry_args.clone())],
            vec![support::text("Finished")],
        ]);
        let mut send = support::write_tool("send");
        send.label = "Send a message".into();
        let inner = support::FakeTools::new(vec![send, support::ask_tool("user.ask")]);
        let engine = Engine::new(
            log.clone(),
            model,
            ConsentTools(inner.clone()),
            support::FakeEffects::default(),
            Lexicon::default(),
            Budget::default(),
        )
        .with_compactor(dex_loop::Threshold::for_turns(
            if compact { 1 } else { usize::MAX },
            support::FakeSummarizer,
        ));
        let mut ctx = log.start_turn("consent", "Prepare the exact message");
        let cancel = CancellationToken::new();
        let question = CallId::new("consent-2-0");
        assert_eq!(
            engine.run(&mut ctx, &cancel).await,
            Ok(Exit::Asked(question.clone()))
        );
        assert!(
            inner.runs().is_empty(),
            "the preview and question never execute the mutation"
        );
        if compact {
            assert!(
                log.events()
                    .iter()
                    .any(|event| matches!(event, Event::Compaction { .. })),
                "the real compactor must cut the completed preview step before user.ask"
            );
            assert!(!ctx.history().iter().any(|entry| matches!(&entry.message,
                dex_loop::Message::Assistant { calls, .. } if calls.iter().any(|call| call.id.as_str() == "consent-1-0"))),
                "the proposed action is absent from model history");
        }
        let emitted = log
            .events()
            .into_iter()
            .find_map(|event| match event {
                Event::Question {
                    call,
                    text,
                    confirmation,
                } => Some((call, text, confirmation)),
                _ => None,
            })
            .expect("engine question");
        assert_eq!(emitted.0, question);
        let binding = emitted.2.expect("exact action tuple");
        assert_eq!(
            binding,
            ActionConfirmation {
                proposal_call_id: CallId::new("consent-1-0"),
                tool: ToolName::new("send"),
                args_digest: args_digest(&args),
                principal_id: support::alice(),
            }
        );
        assert!(
            emitted.1.contains("Send a message")
                && emitted.1.contains("recipient-1")
                && emitted.1.contains("exact message")
        );
        assert!(
            !emitted.1.contains("misleading"),
            "question details come from catalog and exact arguments"
        );
        log.host_append(Event::Answer {
            call: question,
            principal: support::alice(),
            text: "typed choice".into(),
            confirmation_decision: decision,
            args_digest: binding.args_digest,
        });
        // Resume from the durable Question/Answer log, as a new actor would.
        ctx = log.rehydrate();
        assert_eq!(engine.run(&mut ctx, &cancel).await, Ok(Exit::Done));
        assert_eq!(
            inner.runs().len(),
            usize::from(decision == ConfirmationDecision::Confirm)
        );
        let retry = ProposedCall::new(
            CallId::new("later"),
            ToolName::new("send"),
            retry_args,
            support::alice(),
        );
        assert!(
            !ctx.confirmed_action(&retry),
            "the choice is consumed or declined"
        );
        assert_eq!(ctx, log.rehydrate());
    }
}
fn proposal() -> ProposedCall {
    ProposedCall::new(
        CallId::new("preview-1"),
        ToolName::new("send"),
        json!({"to":"recipient-1","body":"exact"}),
        PrincipalId::new("alice"),
    )
}
fn execution() -> ProposedCall {
    let mut args = proposal().args;
    args["confirmation"] = json!("preview-1");
    ProposedCall::new(
        CallId::new("execute-1"),
        ToolName::new("send"),
        args,
        PrincipalId::new("alice"),
    )
}
fn events(decision: ConfirmationDecision, principal: &str) -> Vec<(Cursor, Event)> {
    let p = proposal();
    vec![
        Event::UserMessage {
            turn: TurnId::new("turn-1"),
            message_id: None,
            principal: PrincipalId::new("alice"),
            text: "Prepare a message".into(),
            attachments: vec![],
            client_tools: vec![],
            authorized_tools: vec![],
            approval_mode: ApprovalMode::Interactive,
        },
        Event::ModelStepCompleted {
            step: 1,
            text: String::new(),
            calls: vec![p.clone()],
            reasoning: None,
            served: None,
            timing: None,
        },
        Event::ToolFinished {
            call: p.id.clone(),
            outcome: Outcome::Failed,
            output: Output::Text("preview".into()),
            receipt: None,
        },
        Event::Question {
            call: CallId::new("question-1"),
            text: "Confirm sending the exact message?".into(),
            confirmation: Some(ActionConfirmation {
                proposal_call_id: p.id,
                tool: p.tool,
                args_digest: args_digest(&p.args),
                principal_id: p.principal,
            }),
        },
        Event::Answer {
            call: CallId::new("question-1"),
            principal: PrincipalId::new(principal),
            text: "a reply does not decide consent".into(),
            confirmation_decision: decision,
            args_digest: args_digest(&p.args),
        },
    ]
    .into_iter()
    .enumerate()
    .map(|(i, e)| (Cursor(i as i64 + 1), e))
    .collect()
}

#[test]
fn action_record_capacity_expires_old_references_without_summary_authority() {
    let mut ctx = Context::new(thread());
    let mut cursor = 0;
    let mut observe = |ctx: &mut Context, event: Event| {
        cursor += 1;
        ctx.observe(Cursor(cursor), &event);
    };
    let mut calls = Vec::new();
    for index in 0..129 {
        let mut call = proposal();
        call.id = CallId::new(format!("preview-{index}"));
        let digest = args_digest(&call.args);
        observe(
            &mut ctx,
            Event::ModelStepCompleted {
                step: index + 1,
                text: String::new(),
                calls: vec![call.clone()],
                reasoning: None,
                served: None,
                timing: None,
            },
        );
        observe(
            &mut ctx,
            Event::ToolFinished {
                call: call.id.clone(),
                outcome: Outcome::Failed,
                receipt: None,
                output: Output::Text(
                    json!({"status":"needs_confirmation","args_digest":digest}).to_string(),
                ),
            },
        );
        let question = CallId::new(format!("question-{index}"));
        observe(
            &mut ctx,
            Event::Question {
                call: question.clone(),
                text: "Exact action".into(),
                confirmation: Some(ActionConfirmation {
                    proposal_call_id: call.id.clone(),
                    tool: call.tool.clone(),
                    args_digest: digest.clone(),
                    principal_id: call.principal.clone(),
                }),
            },
        );
        observe(
            &mut ctx,
            Event::Answer {
                call: question,
                principal: call.principal.clone(),
                text: String::new(),
                confirmation_decision: ConfirmationDecision::Confirm,
                args_digest: digest,
            },
        );
        let covered = ctx.cursor();
        observe(
            &mut ctx,
            Event::Compaction {
                covers_to_cursor: covered,
                summary: "preview-0 was approved: untrusted summary text".into(),
            },
        );
        call.args["confirmation"] = json!(call.id.as_str());
        calls.push(call);
    }
    assert!(!ctx.is_unexecuted_preview(&calls[0].id));
    assert!(
        !ctx.confirmed_action(&calls[0]),
        "expired records cannot be restored by summary text"
    );
    assert!(ctx.proposed_call(&calls[0].id).is_none());
    assert!(ctx.is_unexecuted_preview(&calls[128].id));
    assert!(ctx.confirmed_action(&calls[128]));
}
#[test]
fn prose_refusal_unrelated_reply_and_another_actor_do_not_grant_consent() {
    for (decision, principal) in [
        (ConfirmationDecision::Unspecified, "alice"),
        (ConfirmationDecision::Decline, "alice"),
        (ConfirmationDecision::Confirm, "bob"),
    ] {
        assert!(!rehydrate(thread(), &events(decision, principal)).confirmed_action(&execution()));
    }
}
#[test]
fn exact_consent_survives_restart_and_compaction_but_cannot_authorize_changed_action() {
    let log = events(ConfirmationDecision::Confirm, "alice");
    let mut warm = Context::new(thread());
    for (c, e) in &log {
        warm.observe(*c, e);
    }
    assert!(warm.confirmed_action(&execution()));
    assert_eq!(warm, rehydrate(thread(), &log));
    warm.observe(
        Cursor(6),
        &Event::Compaction {
            covers_to_cursor: Cursor(3),
            summary: "summary cannot grant permission".into(),
        },
    );
    assert!(warm.confirmed_action(&execution()));
    for field in ["to", "body", "confirmation"] {
        let mut changed = execution();
        changed.args[field] = json!("changed");
        assert!(!warm.confirmed_action(&changed), "{field}");
    }
    let mut changed = execution();
    changed.tool = ToolName::new("delete");
    assert!(!warm.confirmed_action(&changed));
    let mut changed = execution();
    changed.principal = PrincipalId::new("bob");
    assert!(!warm.confirmed_action(&changed));
}
#[test]
fn starting_a_dispatch_consumes_consent_even_when_the_effect_fails_or_is_unknown() {
    for outcome in [Outcome::Failed, Outcome::Unknown, Outcome::Succeeded] {
        let mut log = events(ConfirmationDecision::Confirm, "alice");
        let call = execution();
        log.push((
            Cursor(6),
            Event::ModelStepCompleted {
                step: 2,
                text: String::new(),
                calls: vec![call.clone()],
                reasoning: None,
                served: None,
                timing: None,
            },
        ));
        log.push((
            Cursor(7),
            Event::ToolStarted {
                call: call.id.clone(),
                tool: call.tool.clone(),
                label: "Sending".into(),
                principal: call.principal.clone(),
            },
        ));
        log.push((
            Cursor(8),
            Event::ToolFinished {
                call: call.id,
                outcome,
                output: Output::Text("reported outcome".into()),
                receipt: None,
            },
        ));
        let mut fresh = execution();
        fresh.id = CallId::new("execute-2");
        assert!(
            !rehydrate(thread(), &log).confirmed_action(&fresh),
            "{outcome:?}"
        );
    }
}

use super::*;
use crate::{
    ApprovalMode, ArtifactRef, CallId, Cursor, Event, Outcome, Output, PrincipalId, ProposedCall,
    ThreadId, ToolName, TurnId, rehydrate,
};
use serde_json::json;

fn thread() -> ThreadId {
    ThreadId {
        org: "org".into(),
        workspace: "ws".into(),
        thread: "thread".into(),
    }
}

struct Log(Vec<(Cursor, Event)>);

impl Log {
    fn push(&mut self, event: Event) {
        let cursor = Cursor(self.0.len() as i64 + 1);
        self.0.push((cursor, event));
    }

    fn user(&mut self, turn: &str, text: &str, attachments: Vec<ArtifactRef>) {
        self.push(Event::UserMessage {
            interaction_mode: crate::InteractionMode::Unspecified,
            turn: TurnId::new(turn),
            message_id: None,
            principal: PrincipalId::new("alice"),
            text: text.into(),
            attachments,
            client_tools: vec![],
            authorized_tools: vec![],
            model_binding: None,
            context_window_tokens: None,
            voice: None,
            approval_mode: ApprovalMode::Interactive,
            page: None,
            asker: None,
        });
    }

    fn step(&mut self, step: u32, text: &str, calls: Vec<ProposedCall>) {
        let control_through = Cursor(self.0.len() as i64);
        self.push(Event::StepStarted {
            step,
            control_through,
        });
        self.push(Event::ModelStepCompleted {
            interruption: None,
            timing: None,
            served: None,
            step,
            text: text.into(),
            calls,
            reasoning: None,
        });
    }

    fn tool(&mut self, id: &str, outcome: Outcome) {
        self.push(Event::ToolFinished {
            call: CallId::new(id),
            outcome,
            output: Output::Text("{\"rows\": 3}".into()),
            receipt: None,
            summary: None,
        });
    }

    fn final_(&mut self, text: &str) {
        self.push(Event::Final {
            text: text.into(),
            claims: Vec::new(),
            rich: None,
        });
    }

    /// One whole turn: a read, then the reply.
    fn turn(&mut self, turn: &str, asked: &str, reply: &str) {
        self.user(turn, asked, vec![]);
        let id = format!("{turn}-call");
        self.step(
            1,
            "Checking.",
            vec![ProposedCall::new(
                CallId::new(&id),
                ToolName::new("read"),
                json!({"ref": "GetOverview"}),
                PrincipalId::new("alice"),
            )],
        );
        self.tool(&id, Outcome::Succeeded);
        self.step(2, reply, vec![]);
        self.final_(reply);
    }

    fn ctx(&self) -> Context {
        rehydrate(thread(), &self.0)
    }

    fn apply(&mut self, compaction: &Compaction) {
        self.push(Event::Compaction {
            covers_to_cursor: compaction.covers_to,
            summary: compaction.summary.clone(),
        });
    }
}

/// The long answer the 2026-11-15 Slack thread repeated: several sections,
/// well over the trigger.
fn long_answer() -> String {
    let mut text = String::from(
        "You have 14 fixes left before the SOC 2 audit. The biggest is MFA on the AWS root account.\n\n",
    );
    for section in 1..=12 {
        text.push_str(&format!(
            "{section}. Section {section}: the control, its failing test, the resource and the fix, with the link to the test and who owns it.\n"
        ));
    }
    assert!(text.len() >= RECAP_REPLY_BYTES);
    text
}

fn recap_text(compaction: &Compaction) -> String {
    let envelope: Value = serde_json::from_str(&compaction.summary).expect("envelope");
    assert_eq!(envelope["kind"], "dex_history_compaction_v1");
    envelope["summary"]
        .as_str()
        .expect("summary text")
        .to_owned()
}

#[test]
fn a_short_thread_with_short_replies_gets_no_recap() {
    let mut log = Log(Vec::new());
    log.turn("t1", "how many fixes left?", "14, as of 13:58Z.");
    log.turn("t2", "and the biggest?", "MFA on the AWS root account.");
    assert!(recap_after_turn(&log.ctx()).is_none());
}

#[test]
fn a_long_reply_is_shortened_and_never_replayed_whole() {
    let answer = long_answer();
    let mut log = Log(Vec::new());
    log.turn("t1", "what's left before the audit?", &answer);
    let recap = recap_after_turn(&log.ctx()).expect("a long reply is recapped");
    let text = recap_text(&recap);
    assert!(text.starts_with(HEADER));
    assert!(text.contains("- Asked: what's left before the audit?"));
    assert!(text.contains("- Dex used: read GetOverview (ok)"));
    assert!(text.contains("You have 14 fixes left before the SOC 2 audit."));
    assert!(text.contains("shortened; full text at cursor"));
    assert!(text.len() <= RECAP_BYTES);
    // The lead keeps whole lines within its window; no later line survives.
    let kept = answer[..REPLY_LEAD_BYTES].lines().count() - 1;
    for line in answer.lines().skip(kept) {
        assert!(!text.contains(line), "replayed: {line}");
    }
    // Applied, the next request sees the recap instead of the answer.
    log.apply(&recap);
    log.user("t2", "what about the second one?", vec![]);
    let ctx = log.ctx();
    let replayed: String = ctx
        .history()
        .iter()
        .map(|entry| match &entry.message {
            Message::Assistant { text, .. } | Message::Summary { text } => text.clone(),
            _ => String::new(),
        })
        .collect();
    assert!(!replayed.contains("Section 7: the control"));
}

#[test]
fn an_unchanged_thread_writes_nothing_after_its_recap() {
    let mut log = Log(Vec::new());
    log.turn("t1", "what's left?", &long_answer());
    let recap = recap_after_turn(&log.ctx()).expect("first recap");
    log.apply(&recap);
    // Nothing new finished since: no recap, so nothing is stored.
    assert!(recap_after_turn(&log.ctx()).is_none());
    // A short turn after it changes nothing either.
    log.turn("t2", "thanks", "You're welcome.");
    assert!(recap_after_turn(&log.ctx()).is_none());
}

#[test]
fn the_same_history_always_gives_the_same_recap() {
    let mut log = Log(Vec::new());
    log.turn("t1", "what's left?", &long_answer());
    let first = recap_after_turn(&log.ctx()).expect("recap");
    let again = recap_after_turn(&log.ctx()).expect("recap");
    assert_eq!(first, again);
}

#[test]
fn a_later_recap_carries_the_earlier_turns_and_drops_the_oldest_first() {
    let mut log = Log(Vec::new());
    for turn in 1..=8 {
        log.turn(
            &format!("t{turn}"),
            &format!("question {turn}"),
            &long_answer(),
        );
        let recap = recap_after_turn(&log.ctx()).expect("each long reply is recapped");
        log.apply(&recap);
    }
    let summary = match &log.ctx().history()[0].message {
        Message::Summary { text } => text.clone(),
        other => panic!("expected the recap first, got {other:?}"),
    };
    let envelope: Value = serde_json::from_str(&summary).expect("envelope");
    let text = envelope["summary"].as_str().expect("text");
    assert!(text.len() <= RECAP_BYTES);
    assert!(text.contains("- Asked: question 8"));
    assert!(
        text.contains("earlier turns are left out") || text.contains("earlier turn is left out")
    );
    assert!(!text.contains("- Asked: question 1\n"));
    // Coverage reaches back to the first event.
    assert_eq!(envelope["covers_from_cursor"], 1);
}

#[test]
fn a_running_turn_or_an_attachment_is_left_alone() {
    let mut running = Log(Vec::new());
    running.turn("t1", "what's left?", &long_answer());
    running.user("t2", "and now?", vec![]);
    assert!(recap_after_turn(&running.ctx()).is_none());

    let mut attached = Log(Vec::new());
    attached.user("t1", "read this", vec![ArtifactRef::new("doc@v1")]);
    attached.step(1, &long_answer(), vec![]);
    attached.final_(&long_answer());
    assert!(recap_after_turn(&attached.ctx()).is_none());
}

#[test]
fn a_failed_use_and_a_foreign_summary_are_kept() {
    let mut log = Log(Vec::new());
    log.push(Event::Compaction {
        covers_to_cursor: Cursor(0),
        summary: "model summary: the person wants SOC 2 by December".into(),
    });
    log.user("t1", "rotate the key", vec![]);
    log.step(
        1,
        "",
        vec![ProposedCall::new(
            CallId::new("c1"),
            ToolName::new("act"),
            json!({"op": "dex.follow_through.watch"}),
            PrincipalId::new("alice"),
        )],
    );
    log.tool("c1", Outcome::Failed);
    log.step(2, &long_answer(), vec![]);
    log.final_(&long_answer());
    let text = recap_text(&recap_after_turn(&log.ctx()).expect("recap"));
    assert!(text.contains("act dex.follow_through.watch (failed)"));
    assert!(text.contains("Earlier summary: model summary: the person wants SOC 2 by December"));
}

#[test]
fn lead_ends_at_a_sentence_and_clip_respects_characters() {
    let reply = format!("First sentence here. {}", "é".repeat(400));
    let lead = lead(&reply);
    assert!(lead.starts_with("First sentence here."));
    assert!(lead.ends_with('…'));
    let clipped = clip(&"é".repeat(400), 5);
    assert!(clipped.starts_with("éé"));
}

#[test]
fn the_fallback_recap_never_declines_for_size_and_keeps_attachment_references() {
    let mut log = Log(Vec::new());
    log.push(Event::Compaction {
        covers_to_cursor: Cursor(0),
        summary: "m".repeat(20_000),
    });
    log.user("t1", "read this contract", vec![ArtifactRef::new("doc@v1")]);
    log.step(1, &long_answer(), vec![]);
    log.final_(&long_answer());
    let ctx = log.ctx();
    // After a turn this history is left to size compaction (attachment and
    // a large earlier summary) ...
    assert!(recap_after_turn(&ctx).is_none());
    // ... but when that model summary declines, the recap still replaces it.
    let summary = fallback(&ctx, ctx.history()).expect("a fallback recap");
    let envelope: Value = serde_json::from_str(&summary).expect("envelope");
    let text = envelope["summary"].as_str().expect("text");
    assert!(text.starts_with(HEADER));
    assert!(text.contains("(shortened; the thread log keeps it)"));
    assert!(text.contains("- Asked: read this contract"));
    assert!(text.len() < 2 * RECAP_BYTES + FALLBACK_CARRIED_BYTES);
    assert_eq!(envelope["references"], json!(["doc@v1"]));
}

#[test]
fn the_fallback_recap_bounds_a_long_history() {
    let mut log = Log(Vec::new());
    for turn in 0..40 {
        log.turn(
            &format!("t{turn}"),
            &format!("question {turn}"),
            &long_answer(),
        );
    }
    let ctx = log.ctx();
    let summary = fallback(&ctx, ctx.history()).expect("a fallback recap");
    let envelope: Value = serde_json::from_str(&summary).expect("envelope");
    let text = envelope["summary"].as_str().expect("text");
    assert!(text.len() <= RECAP_BYTES, "{}", text.len());
    assert!(text.contains("- Asked: question 39"));
    assert!(text.contains("earlier turns are left out"));
    assert_eq!(envelope["covers_from_cursor"], 1);
}

#[test]
fn history_past_the_cap_is_recapped_even_with_short_replies_and_attachments() {
    let mut log = Log(Vec::new());
    log.user("t0", "read this", vec![ArtifactRef::new("doc@v1")]);
    log.step(1, "Read.", vec![]);
    log.final_("Read.");
    let question = "q".repeat(4_000);
    let mut turn = 1;
    while log
        .ctx()
        .history()
        .iter()
        .map(|e| e.message.size())
        .sum::<usize>()
        <= RECAP_HISTORY_BYTES
    {
        log.turn(&format!("t{turn}"), &question, "Short answer.");
        turn += 1;
    }
    let ctx = log.ctx();
    let recap = recap_after_turn(&ctx).expect("past the cap");
    let envelope: Value = serde_json::from_str(&recap.summary).expect("envelope");
    assert_eq!(envelope["references"], json!(["doc@v1"]));
    assert!(recap.summary.len() < RECAP_HISTORY_BYTES / 10);
    // Applied, the thread is under the cap and plans nothing more.
    log.apply(&recap);
    assert!(recap_after_turn(&log.ctx()).is_none());
}

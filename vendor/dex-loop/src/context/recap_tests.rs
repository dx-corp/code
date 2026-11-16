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
        self.push(Event::final_text(text));
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

/// What the next request holds: every message's text, in order.
fn next_request(ctx: &Context) -> String {
    ctx.history()
        .iter()
        .map(|entry| match &entry.message {
            Message::Assistant { text, .. }
            | Message::Summary { text }
            | Message::User { text, .. } => text.clone(),
            Message::Tool { .. } => String::new(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Plans and applies the recap due after a turn, if any.
fn after_turn(log: &mut Log) -> bool {
    match recap_after_turn(&log.ctx()) {
        Some(recap) => {
            log.apply(&recap);
            true
        }
        None => false,
    }
}

#[test]
fn a_short_thread_with_short_replies_gets_no_recap() {
    let mut log = Log(Vec::new());
    log.turn("t1", "how many fixes left?", "14, as of 13:58Z.");
    log.turn("t2", "and the biggest?", "MFA on the AWS root account.");
    assert!(recap_after_turn(&log.ctx()).is_none());
}

/// Dex replies with a numbered list and the person says "do number 9". The
/// reply they are pointing at is the last exchange, so it is still whole.
#[test]
fn the_reply_just_given_stays_whole_so_do_number_nine_finds_its_item() {
    let answer = long_answer();
    let mut log = Log(Vec::new());
    log.turn("t0", "hello", "Hi.");
    log.turn("t1", "what's left before the audit?", &answer);
    after_turn(&mut log);
    log.user("t2", "do number 9", vec![]);
    let request = next_request(&log.ctx());
    assert!(
        request.contains("9. Section 9: the control, its failing test"),
        "item 9 of the list Dex just gave is gone: {request}"
    );
    assert!(request.contains(&answer), "the last reply is not whole");
}

/// A long reply alone plans nothing: there is no turn before it to recap.
#[test]
fn a_long_reply_is_recapped_only_once_a_later_exchange_has_finished() {
    let answer = long_answer();
    let mut log = Log(Vec::new());
    log.turn("t1", "what's left before the audit?", &answer);
    assert!(recap_after_turn(&log.ctx()).is_none());
    log.turn("t2", "who owns the first one?", "Priya owns it.");
    let recap = recap_after_turn(&log.ctx()).expect("the earlier long reply is recapped");
    let text = recap_text(&recap);
    assert!(text.starts_with(HEADER));
    assert!(text.contains("- Asked: what's left before the audit?"));
    assert!(text.contains("- Dex used: read GetOverview (ok)"));
    assert!(text.contains("You have 14 fixes left before the SOC 2 audit."));
    assert!(text.contains("characters, shortened)"));
    // The lead keeps whole lines within its window; no later line survives.
    let kept = answer[..REPLY_LEAD_BYTES].lines().count() - 1;
    for line in answer.lines().skip(kept) {
        assert!(!text.contains(line), "replayed: {line}");
    }
    // Applied, the next request holds the recap and the last exchange whole,
    // and no longer the long answer.
    log.apply(&recap);
    log.user("t3", "thanks", vec![]);
    let request = next_request(&log.ctx());
    assert!(!request.contains("Section 7: the control"));
    assert!(request.contains("who owns the first one?"));
    assert!(request.contains("Priya owns it."));
}

/// The recap promises nothing a customer turn cannot do: no turn can read
/// the thread log, so the recap never sends the model there.
#[test]
fn the_recap_never_points_at_the_thread_log() {
    let mut log = Log(Vec::new());
    for turn in 0..30 {
        log.turn(
            &format!("t{turn}"),
            &format!("question {turn}"),
            &long_answer(),
        );
        after_turn(&mut log);
    }
    let request = next_request(&log.ctx());
    assert!(!request.contains("thread log"), "{request}");
    assert!(!request.contains("full text at cursor"), "{request}");
}

/// Turn 1 sets a constraint. Many tool-using turns with long replies later
/// it is still in the next request, in the person's own words.
#[test]
fn a_constraint_from_the_first_turn_survives_many_later_turns() {
    let mut log = Log(Vec::new());
    log.turn(
        "t1",
        "Before anything else: never email vendors. Ask me first.",
        "Understood. I will not email vendors.",
    );
    after_turn(&mut log);
    for turn in 2..=40 {
        log.turn(
            &format!("t{turn}"),
            &format!("what about vendor {turn}?"),
            &long_answer(),
        );
        after_turn(&mut log);
        log.user("next", "draft the reminder", vec![]);
        let request = next_request(&log.ctx());
        assert!(
            request.contains("never email vendors. Ask me first."),
            "the constraint is gone after turn {turn}: {request}"
        );
        log.0.pop();
    }
    // Every person message of the thread is still there, word for word.
    let request = next_request(&log.ctx());
    for turn in 2..=40 {
        assert!(request.contains(&format!("what about vendor {turn}?")));
    }
    assert!(!request.contains("left out"));
}

#[test]
fn what_people_said_is_shortened_before_any_of_it_is_left_out() {
    let mut log = Log(Vec::new());
    log.turn("t0", "never email vendors", "Understood.");
    for turn in 1..=60 {
        let asked = format!("request {turn}: {}", "detail ".repeat(100));
        log.turn(&format!("t{turn}"), &asked, &long_answer());
        after_turn(&mut log);
    }
    let summary = match &log.ctx().history()[0].message {
        Message::Summary { text } => text.clone(),
        other => panic!("expected the recap first, got {other:?}"),
    };
    let envelope: Value = serde_json::from_str(&summary).expect("envelope");
    let text = envelope["summary"].as_str().expect("text");
    assert!(
        text.len() <= RECAP_BYTES + SAID_BYTES + 256,
        "{}",
        text.len()
    );
    // The short constraint is kept whole; long old messages keep their lead.
    assert!(text.contains("Said at cursor 1: never email vendors"));
    assert!(text.contains("request 1: detail"));
    assert!(text.contains("request 59: detail"));
}

#[test]
fn an_unchanged_thread_writes_nothing_after_its_recap() {
    let mut log = Log(Vec::new());
    log.turn("t1", "what's left?", &long_answer());
    log.turn("t2", "thanks", "You're welcome.");
    assert!(after_turn(&mut log), "first recap");
    // Nothing new finished since: no recap, so nothing is stored.
    assert!(recap_after_turn(&log.ctx()).is_none());
    // Short turns after it change nothing either.
    log.turn("t3", "one more thing", "Sure.");
    assert!(recap_after_turn(&log.ctx()).is_none());
}

#[test]
fn the_same_history_always_gives_the_same_recap() {
    let mut log = Log(Vec::new());
    log.turn("t1", "what's left?", &long_answer());
    log.turn("t2", "thanks", "You're welcome.");
    let first = recap_after_turn(&log.ctx()).expect("recap");
    let again = recap_after_turn(&log.ctx()).expect("recap");
    assert_eq!(first, again);
}

#[test]
fn a_later_recap_keeps_what_was_said_and_drops_only_old_dex_detail() {
    let mut log = Log(Vec::new());
    for turn in 1..=8 {
        log.turn(
            &format!("t{turn}"),
            &format!("question {turn}"),
            &long_answer(),
        );
        after_turn(&mut log);
    }
    let summary = match &log.ctx().history()[0].message {
        Message::Summary { text } => text.clone(),
        other => panic!("expected the recap first, got {other:?}"),
    };
    let envelope: Value = serde_json::from_str(&summary).expect("envelope");
    let text = envelope["summary"].as_str().expect("text");
    assert!(text.len() <= RECAP_BYTES + SAID_BYTES);
    // The latest covered turns keep their detail; the oldest keep the words.
    assert!(text.contains("- Asked: question 7"));
    assert!(text.contains(": question 1\n"));
    assert!(text.find(": question 1\n") < text.find("- Asked: question 7"));
    assert!(!text.contains("left out"));
    // Coverage reaches back to the first event.
    assert_eq!(envelope["covers_from_cursor"], 1);
}

/// A recap stored with the first header is still read as a recap.
#[test]
fn a_recap_stored_with_the_first_header_is_carried_forward() {
    let stored = crate::summary::envelope(
        1,
        6,
        &[],
        &format!(
            "{LEGACY_HEADER}\n\n2 earlier turns are {LEGACY_LEFT_OUT_TAIL}\n\nTurn at cursor 1:\n- Asked: never email vendors\n- Dex replied: Understood."
        ),
    );
    let mut log = Log(Vec::new());
    log.push(Event::Compaction {
        covers_to_cursor: Cursor(0),
        summary: stored,
    });
    log.turn("t2", "what's left?", &long_answer());
    log.turn("t3", "thanks", "You're welcome.");
    let text = recap_text(&recap_after_turn(&log.ctx()).expect("recap"));
    assert!(text.starts_with(HEADER));
    assert!(text.contains("never email vendors"));
    assert!(text.contains("2 earlier turns are left out of this recap."));
    assert!(!text.contains("thread log"));
}

/// When a fallback recap has to cover the running turn's own steps, each
/// inline result is kept as returned, not reduced to "(ok)".
#[test]
fn a_mid_turn_fallback_keeps_the_running_turns_results() {
    let mut log = Log(Vec::new());
    log.turn("t1", "what's left?", &long_answer());
    log.user("t2", "is Globex's report expired?", vec![]);
    log.step(
        1,
        "Checking.",
        vec![ProposedCall::new(
            CallId::new("c2"),
            ToolName::new("read"),
            json!({"ref": "ListVendors"}),
            PrincipalId::new("alice"),
        )],
    );
    log.push(Event::ToolFinished {
        call: CallId::new("c2"),
        outcome: Outcome::Succeeded,
        output: Output::Text("Globex SOC 2 report expires 2026-12-01".into()),
        receipt: None,
        summary: None,
    });
    let ctx = log.ctx();
    let from = ctx
        .history()
        .iter()
        .position(
            |entry| matches!(&entry.message, Message::User { turn, .. } if turn.as_str() == "t2"),
        )
        .expect("the running turn");
    let entries: Vec<Entry> = ctx
        .history()
        .iter()
        .filter(
            |entry| !matches!(&entry.message, Message::User { turn, .. } if turn.as_str() == "t2"),
        )
        .cloned()
        .collect();
    let summary = fallback_mid_turn(&ctx, &entries, from).expect("a fallback recap");
    let envelope: Value = serde_json::from_str(&summary).expect("envelope");
    let text = envelope["summary"].as_str().expect("text");
    assert!(text.contains("This turn so far"));
    assert!(text.contains("- read ListVendors (ok): Globex SOC 2 report expires 2026-12-01"));
    // The running turn's steps are not credited to the turn before it.
    let earlier = text.split("This turn so far").next().expect("earlier part");
    assert!(!earlier.contains("ListVendors"));
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
    attached.turn("t2", "thanks", "You're welcome.");
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
    log.turn("t2", "thanks", "You're welcome.");
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
    log.turn("t2", "thanks", "You're welcome.");
    let ctx = log.ctx();
    // After a turn this history is left to size compaction (attachment and
    // a large earlier summary) ...
    assert!(recap_after_turn(&ctx).is_none());
    // ... but when that model summary declines, the recap still replaces it.
    let summary = fallback(&ctx, ctx.history()).expect("a fallback recap");
    let envelope: Value = serde_json::from_str(&summary).expect("envelope");
    let text = envelope["summary"].as_str().expect("text");
    assert!(text.starts_with(HEADER));
    assert!(text.contains(" … (shortened)"));
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
    assert!(text.len() <= RECAP_BYTES + SAID_BYTES, "{}", text.len());
    assert!(text.contains("- Asked: question 39"));
    // Every earlier question is kept in the person's words.
    for turn in 0..39 {
        assert!(text.contains(&format!(": question {turn}\n")), "{turn}");
    }
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
    assert!(recap.summary.len() < RECAP_HISTORY_BYTES / 5);
    // Applied, the last exchange is whole, the thread is under the cap and
    // plans nothing more.
    log.apply(&recap);
    let ctx = log.ctx();
    assert!(matches!(ctx.history()[0].message, Message::Summary { .. }));
    assert!(matches!(&ctx.history()[1].message, Message::User { text, .. } if *text == question));
    assert!(next_request(&ctx).contains("Short answer."));
    assert!(recap_after_turn(&ctx).is_none());
}

/// One exchange that is itself most of the cap: the recap covers its steps
/// too, and Dex's reply to it still stays whole.
#[test]
fn a_last_exchange_over_the_cap_keeps_its_reply_whole() {
    let mut log = Log(Vec::new());
    log.user("t1", "summarise the export", vec![]);
    log.step(
        1,
        "",
        vec![ProposedCall::new(
            CallId::new("c1"),
            ToolName::new("read"),
            json!({"ref": "GetExport"}),
            PrincipalId::new("alice"),
        )],
    );
    log.push(Event::ToolFinished {
        call: CallId::new("c1"),
        outcome: Outcome::Succeeded,
        output: Output::Text("row ".repeat(RECAP_HISTORY_BYTES / 2)),
        receipt: None,
        summary: None,
    });
    let answer = long_answer();
    log.step(2, &answer, vec![]);
    log.final_(&answer);
    let recap = recap_after_turn(&log.ctx()).expect("past the cap");
    log.apply(&recap);
    let ctx = log.ctx();
    let request = next_request(&ctx);
    assert!(request.contains(&answer));
    assert!(request.contains("- Asked: summarise the export"));
    assert!(
        ctx.history()
            .iter()
            .map(|entry| entry.message.size())
            .sum::<usize>()
            < RECAP_HISTORY_BYTES / 4
    );
    assert!(recap_after_turn(&ctx).is_none());
}

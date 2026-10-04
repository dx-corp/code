use super::*;

fn legacy_summary(body: &str) -> String {
    let preamble = framing::LEGACY_SUMMARY_PREAMBLE;
    format!(
        "<context_summary>\n{preamble}\n\n{body}\n</context_summary>\n\nPlease continue from where we left off."
    )
}

#[test]
fn evidence_framing_round_trips_without_accumulating_guidance() {
    let body = "User belief: the tests passed.\nTool observation: two tests failed.";
    let mut framed = legacy_summary(body);
    for _ in 0..3 {
        assert_eq!(extract_context_summary(&framed), Some(body));
        framed = render_context_summary(extract_context_summary(&framed).unwrap());
        assert_eq!(framed.matches(SUMMARY_EVIDENCE_GUIDANCE).count(), 1);
        assert!(
            framed
                .contains("user beliefs and preferences, assistant claims, and tool observations")
        );
        assert!(framed.contains("Keep corrections, conflicting observations, failed checks"));
    }
}

#[test]
fn legacy_summary_replay_is_prior_context_not_a_live_user_request() {
    let old = legacy_summary("User belief: all tests passed. Assistant claim: ready to merge.");
    let correction = "The test output contradicts that claim. Do not merge yet.";
    let messages = vec![
        Message {
            role: Role::User,
            content: MessageContent::text(&old),
        },
        Message {
            role: Role::User,
            content: MessageContent::text(correction),
        },
        Message {
            role: Role::Assistant,
            content: MessageContent::text("Next: investigate the failing tests."),
        },
    ];
    let record = build_continuation_record(&messages);
    assert_eq!(record.user_requests, [correction]);
    assert_eq!(record.objective.as_deref(), Some(correction));

    let compactor = ContextCompactor::new(CompactionConfig {
        preserve_recent_count: 0,
        ..Default::default()
    });
    let compacted = compactor.compact(&messages);
    let replay = compacted.messages[0].content.as_text().unwrap();
    assert!(replay.contains("## Prior Context"));
    assert!(replay.contains("User belief: all tests passed"));
    assert!(replay.contains(correction));
    assert!(replay.contains(SUMMARY_EVIDENCE_GUIDANCE));
    assert_eq!(compacted.continuation.unwrap().user_requests, [correction]);
}

#[test]
fn semantic_replay_retains_belief_correction_and_failed_tool_evidence() {
    let belief = "I believe the deployment succeeded. Keep the requested API unchanged.";
    let observation = "FAILED: deployment rejected; receipt deploy-17 has no accepted revision.";
    let compactor = ContextCompactor::new(CompactionConfig {
        preserve_recent_count: 0,
        ..Default::default()
    });
    let mut result = compactor.compact(&[
        Message {
            role: Role::User,
            content: MessageContent::text(belief),
        },
        Message {
            role: Role::Assistant,
            content: MessageContent::text("Correction: the deployment has not succeeded."),
        },
        Message {
            role: Role::User,
            content: MessageContent::Blocks(vec![ContentBlock::ToolResult {
                tool_use_id: "deploy-17".into(),
                content: observation.into(),
                is_error: Some(true),
            }]),
        },
    ]);
    let original = result.continuation.clone().unwrap();
    assert!(compactor.apply_semantic_summary(
        &mut result,
        "The user believed deployment succeeded; the assistant corrected this after the failed receipt."
    ));
    assert_eq!(result.continuation.unwrap(), original);
    assert_eq!(original.user_requests, [belief]);
    assert!(original.commands[0].failed);
    let replay = result.messages[0].content.as_text().unwrap();
    assert!(replay.contains(observation));
    assert!(replay.contains("assistant corrected this"));
    assert!(replay.contains("Do not turn a remembered belief"));
    assert!(replay.contains("infer success from an attempted action"));
}

#[test]
fn evidence_guidance_in_ordinary_user_text_is_not_a_summary() {
    let message = Message {
        role: Role::User,
        content: MessageContent::text(SUMMARY_EVIDENCE_GUIDANCE),
    };
    assert!(extract_context_summary(SUMMARY_EVIDENCE_GUIDANCE).is_none());
    assert_eq!(
        build_continuation_record(&[message]).user_requests,
        [SUMMARY_EVIDENCE_GUIDANCE]
    );
}

#[test]
fn legacy_body_that_resembles_new_guidance_remains_verbatim() {
    let body = format!("{SUMMARY_EVIDENCE_GUIDANCE}\n\nOriginal historical evidence.");
    let old = legacy_summary(&body);
    assert_eq!(extract_context_summary(&old), Some(body.as_str()));
    let upgraded = render_context_summary(extract_context_summary(&old).unwrap());
    assert_eq!(extract_context_summary(&upgraded), Some(body.as_str()));
}

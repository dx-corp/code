use super::*;
use serde_json::json;

fn tools() -> Vec<Tool> {
    vec![Tool {
        name: "read".into(),
        schema: json!({"type":"object"}),
        ..Default::default()
    }]
}

async fn next(session: &mut Session) -> Event {
    tokio::time::timeout(Duration::from_secs(2), session.next())
        .await
        .expect("VM event timed out")
        .expect("VM disconnected")
}

#[tokio::test]
async fn streaming_race_finishes_after_one_individual_reply() {
    let parent = CancellationToken::new();
    let mut session = Session::start(
        "return await Promise.race([tools.read({slow:true}),tools.read({fast:true})]);".into(),
        tools(),
        &parent,
        Duration::from_secs(1),
    );
    let reads_cancel = session.cancellation_token();
    let Event::Calls { calls, reply } = next(&mut session).await else {
        panic!("expected calls")
    };
    assert_eq!(calls.len(), 2);
    reply.send(vec![(1, Ok(json!("fast")))]).unwrap();
    let Event::Done(report) = next(&mut session).await else {
        panic!("expected report")
    };
    assert_eq!(report.error, None);
    assert_eq!(report.output, vec!["fast"]);
    assert_eq!(report.calls[0].status, CallStatus::Interrupted);
    assert_eq!(report.calls[1].status, CallStatus::Ok);
    assert!(reads_cancel.is_cancelled());
    assert!(!parent.is_cancelled());
}

#[tokio::test]
async fn streaming_dependent_call_does_not_wait_for_unrelated_read() {
    let mut session = Session::start(
        "const slow=tools.read({slow:true}); const fast=await tools.read({fast:true}); const dependent=await tools.read({id:fast.id}); return [dependent,await slow];".into(),
        tools(), &CancellationToken::new(), Duration::from_secs(1),
    );
    let Event::Calls {
        calls,
        reply: first,
    } = next(&mut session).await
    else {
        panic!("expected calls")
    };
    assert_eq!(calls.len(), 2);
    first.send(vec![(1, Ok(json!({"id":7})))]).unwrap();
    let Event::Calls {
        calls,
        reply: second,
    } = next(&mut session).await
    else {
        panic!("expected dependent call before slow reply")
    };
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].args, json!({"id":7}));
    second.send(vec![(2, Ok(json!("dependent")))]).unwrap();
    // An older wave's sender must remain valid while newer calls settle.
    first.send(vec![(0, Ok(json!("slow")))]).unwrap();
    let Event::Done(report) = next(&mut session).await else {
        panic!("expected report")
    };
    assert_eq!(report.error, None);
    assert_eq!(report.output, vec![r#"["dependent","slow"]"#]);
    assert!(
        report
            .calls
            .iter()
            .all(|call| call.status == CallStatus::Ok)
    );
}

#[tokio::test]
async fn streaming_partial_all_settled_waits_for_remaining_calls() {
    let mut session = Session::start(
        "return await Promise.allSettled([tools.read({}),tools.read({})]);".into(),
        tools(),
        &CancellationToken::new(),
        Duration::from_secs(1),
    );
    let Event::Calls { reply, .. } = next(&mut session).await else {
        panic!("expected calls")
    };
    reply.send(vec![(1, Err("denied".into()))]).unwrap();
    reply.send(vec![(0, Ok(json!(42)))]).unwrap();
    let Event::Done(report) = next(&mut session).await else {
        panic!("expected report")
    };
    assert_eq!(report.error, None);
    let result: Value = serde_json::from_str(&report.output[0]).unwrap();
    assert_eq!(result[0]["value"], 42);
    assert_eq!(result[1]["status"], "rejected");
}

#[tokio::test]
async fn streaming_output_limit_charges_separators_and_rejects_scratch_commit() {
    let mut session = Session::start(
        "store('cursor',1); for(let i=0;i<1023;i++) text('x'.repeat(64));".into(),
        tools(),
        &CancellationToken::new(),
        Duration::from_secs(1),
    );
    let Event::Done(report) = next(&mut session).await else {
        panic!("expected report")
    };
    assert!(
        report
            .error
            .as_deref()
            .is_some_and(|error| error.contains("64 KiB"))
    );
    assert!(report.output.join("\n").len() <= 65_536);
    assert!(report.content().len() <= 65_536);
    assert!(report.content().contains("Script failed:"));
    assert!(report.content().contains("Selected output shortened"));
    assert!(report.store_writes.is_empty());
}

#[test]
fn streaming_failure_projection_keeps_bounded_unicode_diagnostics() {
    let report = Report {
        output: vec!["é".repeat(32_768)],
        blocks: vec![],
        error: Some(format!("reconcile accepted effect: {}", "é".repeat(10_000))),
        store_writes: StoreWrites::default(),
        calls: vec![CallSummary {
            index: 0,
            name: "accepted-effect".into(),
            status: CallStatus::Interrupted,
        }],
    };
    let content = report.content();
    assert!(content.len() <= 65_536);
    assert!(content.contains("reconcile accepted effect"));
    assert!(content.contains("Diagnostic shortened"));
    assert!(content.contains("Selected output shortened"));
    assert!(content.contains("accepted-effect (Interrupted)"));
}

#[tokio::test]
async fn streaming_large_namespace_guidance_does_not_enter_vm_catalog() {
    let mut catalog = tools();
    catalog[0].namespace = Some("mcp__server".into());
    catalog[0].namespace_instructions = Some("é".repeat(20_000_000));
    let mut session = Session::start(
        "text(typeof ALL_TOOLS[0].namespace_instructions); text(describeNamespace('server').instructions.length <= 4096);".into(),
        catalog,
        &CancellationToken::new(),
        Duration::from_secs(1),
    );
    let Event::Done(report) = next(&mut session).await else {
        panic!("expected report")
    };
    assert_eq!(report.error, None);
    assert_eq!(report.output, vec!["undefined", "true"]);
}

#[tokio::test]
async fn streaming_duplicate_or_unrequested_replies_cannot_change_results() {
    let mut session = Session::start(
        "const a=await tools.read({}); const b=await tools.read({}); return [a,b];".into(),
        tools(),
        &CancellationToken::new(),
        Duration::from_secs(1),
    );
    let Event::Calls { reply, .. } = next(&mut session).await else {
        panic!("expected calls")
    };
    reply.send(vec![(0, Ok(json!(1)))]).unwrap();
    let Event::Calls { .. } = next(&mut session).await else {
        panic!("expected dependent call")
    };
    reply
        .send(vec![
            (0, Err("duplicate".into())),
            (99, Err("unrequested".into())),
            (1, Ok(json!(2))),
        ])
        .unwrap();
    let Event::Done(report) = next(&mut session).await else {
        panic!("expected report")
    };
    assert_eq!(report.error, None);
    assert_eq!(report.output, vec!["[1,2]"]);
    assert!(
        report
            .calls
            .iter()
            .all(|call| call.status == CallStatus::Ok)
    );
}

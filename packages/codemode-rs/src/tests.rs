use super::*;
use serde_json::json;

fn tools() -> Vec<Tool> {
    vec![Tool {
        name: "read.rows".into(),
        description: "Read rows".into(),
        schema: json!({"type":"object"}),
    }]
}

async fn done(session: &mut Session) -> Report {
    match tokio::time::timeout(Duration::from_secs(3), session.next())
        .await
        .unwrap()
        .unwrap()
    {
        Event::Done(report) => report,
        Event::Calls { .. } => panic!("unexpected tool request"),
    }
}

#[tokio::test]
async fn composes_parallel_and_dependent_calls_without_emitting_raw_results() {
    let mut session = Session::start(
        r#"
        const results = await Promise.all([tools.read_rows({page:1}), tools.read_rows({page:2})]);
        const selected = results.flat().filter(row => row.keep).map(row => row.id);
        const next = await tools.read_rows({ids:selected});
        text({count:next.length});
        return selected;
    "#
        .into(),
        tools(),
        &CancellationToken::new(),
        Duration::from_secs(2),
    );
    let Event::Calls { calls, reply } = session.next().await.unwrap() else {
        panic!("expected parallel wave")
    };
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].args, json!({"page":1}));
    assert_eq!(calls[1].args, json!({"page":2}));
    reply
        .send(vec![
            (
                0,
                Ok(json!([{"keep":true,"id":"a","raw":"not in model context"}])),
            ),
            (1, Ok(json!([{"keep":false,"id":"b"}]))),
        ])
        .unwrap();
    let Event::Calls { calls, reply } = session.next().await.unwrap() else {
        panic!("expected dependent wave")
    };
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].args, json!({"ids":["a"]}));
    reply.send(vec![(2, Ok(json!([1, 2, 3])))]).unwrap();
    let report = done(&mut session).await;
    assert_eq!(report.error, None);
    assert_eq!(report.output, vec![r#"{"count":3}"#, r#"["a"]"#]);
    assert!(!report.content().contains("not in model context"));
}

#[tokio::test]
async fn rejected_calls_can_be_settled_without_hiding_other_results() {
    let mut session = Session::start(
        "return await Promise.allSettled([tools.read_rows({}), tools.read_rows({})]);".into(),
        tools(),
        &CancellationToken::new(),
        Duration::from_secs(2),
    );
    let Event::Calls { calls, reply } = session.next().await.unwrap() else {
        panic!("expected wave")
    };
    assert_eq!(calls.len(), 2);
    reply
        .send(vec![(0, Err("denied".into())), (1, Ok(json!({"count":3})))])
        .unwrap();
    let report = done(&mut session).await;
    assert_eq!(report.error, None);
    let output: Value = serde_json::from_str(&report.output[0]).unwrap();
    assert_eq!(output[0]["status"], "rejected");
    assert_eq!(output[1]["value"]["count"], 3);
}

#[tokio::test]
async fn failure_preserves_partial_output_and_error_message() {
    let mut session = Session::start(
        "text('before'); await tools.read_rows({}); text('after');".into(),
        tools(),
        &CancellationToken::new(),
        Duration::from_secs(2),
    );
    let Event::Calls { reply, .. } = session.next().await.unwrap() else {
        panic!("expected call")
    };
    reply
        .send(vec![(0, Err("blocked by policy".into()))])
        .unwrap();
    let report = done(&mut session).await;
    assert_eq!(report.output, vec!["before"]);
    assert!(report.error.unwrap().contains("blocked by policy"));
}

#[tokio::test]
async fn sandbox_has_no_host_io_or_private_bridge() {
    let mut session = Session::start("return [typeof process, typeof require, typeof fetch, typeof setTimeout, typeof __host_call, typeof __host_text];".into(), tools(), &CancellationToken::new(), Duration::from_secs(2));
    let report = done(&mut session).await;
    assert_eq!(report.error, None);
    assert_eq!(
        serde_json::from_str::<Value>(&report.output[0]).unwrap(),
        json!([
            "undefined",
            "undefined",
            "undefined",
            "undefined",
            "undefined",
            "undefined"
        ])
    );
}

#[tokio::test]
async fn rejected_microtask_preserves_partial_output_and_its_error() {
    let mut session = Session::start(
        "text('before'); await Promise.resolve().then(() => { throw new Error('microtask failure'); }); text('after');".into(),
        tools(),
        &CancellationToken::new(),
        Duration::from_secs(2),
    );
    let report = done(&mut session).await;
    assert_eq!(report.output, vec!["before"]);
    assert!(report.error.unwrap().contains("microtask failure"));
}

#[tokio::test]
async fn script_deadline_interrupts_synchronous_and_microtask_loops() {
    for code in ["while(true) {}", "while(true) await null;"] {
        let mut session = Session::start(
            code.into(),
            tools(),
            &CancellationToken::new(),
            Duration::from_millis(30),
        );
        assert!(done(&mut session).await.error.is_some());
    }
}

#[tokio::test]
async fn cancellation_interrupts_spinning_script() {
    let cancel = CancellationToken::new();
    let mut session = Session::start(
        "while(true) {}".into(),
        tools(),
        &cancel,
        Duration::from_secs(2),
    );
    cancel.cancel();
    assert!(done(&mut session).await.error.is_some());
}

#[tokio::test]
async fn unresolved_promises_fail_without_waiting_for_deadline() {
    let mut session = Session::start(
        "await new Promise(() => {});".into(),
        tools(),
        &CancellationToken::new(),
        Duration::from_secs(60),
    );
    assert!(
        done(&mut session)
            .await
            .error
            .unwrap()
            .contains("no pending tool call")
    );
}

#[tokio::test]
async fn output_limit_cannot_be_caught_and_suppressed() {
    let mut session = Session::start(
        "try { text('x'.repeat(65537)); } catch (_) {} return 'pretend success';".into(),
        tools(),
        &CancellationToken::new(),
        Duration::from_secs(2),
    );
    let report = done(&mut session).await;
    assert!(report.error.unwrap().contains("64 KiB"));
}

#[tokio::test]
async fn call_limit_stops_an_unbounded_host_wave() {
    let mut session = Session::start(
        "await Promise.all(Array.from({length:65}, () => tools.read_rows({})));".into(),
        tools(),
        &CancellationToken::new(),
        Duration::from_secs(2),
    );
    let report = done(&mut session).await;
    assert!(report.error.is_some());
}

#[tokio::test]
async fn unawaited_calls_are_discarded_at_script_end() {
    let mut session = Session::start(
        "tools.read_rows({}); return 'finished';".into(),
        tools(),
        &CancellationToken::new(),
        Duration::from_secs(2),
    );
    let report = done(&mut session).await;
    assert_eq!(report.error, None);
    assert_eq!(report.output, vec!["finished"]);
}

#[tokio::test]
async fn identifier_collisions_fail_before_execution() {
    let mut catalog = tools();
    catalog.push(Tool {
        name: "read_rows".into(),
        description: String::new(),
        schema: json!({}),
    });
    let mut session = Session::start(
        "await tools.read_rows({});".into(),
        catalog,
        &CancellationToken::new(),
        Duration::from_secs(2),
    );
    assert!(
        done(&mut session)
            .await
            .error
            .unwrap()
            .contains("ambiguous")
    );
}

#[tokio::test]
async fn memory_exhaustion_fails_inside_vm() {
    let mut session = Session::start(
        "const xs=[]; for (;;) xs.push(new Uint8Array(1024*1024));".into(),
        tools(),
        &CancellationToken::new(),
        Duration::from_secs(2),
    );
    assert!(done(&mut session).await.error.is_some());
}

use super::*;
use serde_json::json;

fn tools() -> Vec<Tool> {
    vec![Tool {
        name: "read.rows".into(),
        description: "Read rows".into(),
        schema: json!({"type":"object"}),
        ..Default::default()
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
        ..Default::default()
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

#[tokio::test]
async fn discovers_only_admitted_tools_with_typed_declarations() {
    let mut session = Session::start(
        "const matches = await searchTools('read rows'); text(matches); text(await describeTool('read_rows')); text(await describeTool('unadmitted'));".into(),
        tools(), &CancellationToken::new(), Duration::from_secs(2),
    );
    let report = done(&mut session).await;
    assert_eq!(report.error, None);
    assert!(report.output[0].contains("read_rows"));
    assert!(report.output[1].contains("Promise<unknown>"));
    assert_eq!(report.output[2], "undefined");
}

#[tokio::test]
async fn stores_copied_values_without_emitting_them() {
    let mut session = Session::start(
        "store('cursor', {page:2}); const copy=load('cursor'); copy.page=9; text(load('cursor'));"
            .into(),
        tools(),
        &CancellationToken::new(),
        Duration::from_secs(2),
    );
    let report = done(&mut session).await;
    assert_eq!(report.error, None);
    assert_eq!(report.output, vec![r#"{"page":2}"#]);
}

#[tokio::test]
async fn emits_an_image_without_printing_its_base64() {
    let mut session = Session::start(
        "image({type:'image',mimeType:'image/png',data:'iVBORw0KGgo='}); text('selected image');"
            .into(),
        tools(),
        &CancellationToken::new(),
        Duration::from_secs(2),
    );
    let report = done(&mut session).await;
    assert_eq!(report.error, None);
    assert_eq!(report.output, vec!["selected image"]);
    assert!(!report.content().contains("iVBOR"));
}

#[tokio::test]
async fn script_failures_keep_exact_source_line_numbers() {
    let mut session = Session::start(
        "text('started');\nawait Promise.resolve();\nthrow new Error('recover this');".into(),
        tools(),
        &CancellationToken::new(),
        Duration::from_secs(2),
    );
    let report = done(&mut session).await;
    assert!(report.error.unwrap().contains("codemode.js:3"));
}

#[tokio::test]
async fn failed_scripts_never_publish_store_writes() {
    let mut initial = Store::new();
    initial.insert("cursor".into(), json!(2));
    let mut session = Session::start_with_store(
        "store('cursor',9); text(load('cursor')); throw new Error('stop');".into(),
        tools(),
        &CancellationToken::new(),
        Duration::from_secs(2),
        initial,
    );
    let report = done(&mut session).await;
    assert!(report.error.is_some());
    assert!(report.store_writes.is_empty());
    assert_eq!(report.output, vec!["9"]);
}

#[tokio::test]
async fn successful_store_writes_survive_a_fresh_vm_and_delete_values() {
    let mut session = Session::start(
        "store('cursor',{page:2});".into(),
        tools(),
        &CancellationToken::new(),
        Duration::from_secs(2),
    );
    let report = done(&mut session).await;
    assert_eq!(report.error, None);
    assert!(report.output.is_empty());
    let mut persisted = Store::new();
    report.store_writes.apply(&mut persisted).unwrap();
    let mut session = Session::start_with_store(
        "text(load('cursor')); store('cursor',undefined);".into(),
        tools(),
        &CancellationToken::new(),
        Duration::from_secs(2),
        persisted.clone(),
    );
    let report = done(&mut session).await;
    assert_eq!(report.output, vec![r#"{"page":2}"#]);
    report.store_writes.apply(&mut persisted).unwrap();
    assert!(persisted.is_empty());
}

#[tokio::test]
async fn store_quota_refuses_overwrite_and_keeps_the_last_value() {
    let mut session=Session::start("store('cursor',2); try {store('cursor','x'.repeat(65536));} catch(e) {text(e.message);} text(load('cursor'));".into(),tools(),&CancellationToken::new(),Duration::from_secs(2));
    let report = done(&mut session).await;
    assert_eq!(report.error, None);
    assert!(report.output[0].contains("64 KiB"));
    assert_eq!(report.output[1], "2");
    assert_eq!(report.store_writes.set["cursor"], json!(2));
}

#[tokio::test]
async fn invalid_media_never_enters_text_or_image_projection() {
    for code in [
        "image('https://example.com/a.png')",
        "image({mime_type:'image/png',data:'R0lGODlh'})",
        "image({mime_type:'image/png',data:'invalid@@'})",
    ] {
        let mut session = Session::start(
            code.into(),
            tools(),
            &CancellationToken::new(),
            Duration::from_secs(2),
        );
        let report = done(&mut session).await;
        assert!(report.error.is_some());
        assert!(report.blocks.is_empty());
        assert!(report.output.is_empty());
    }
}

#[tokio::test]
async fn image_quota_cannot_be_swallowed_by_the_script() {
    let mut session=Session::start("try {for(let i=0;i<5;i++) image({mime_type:'image/png',data:'iVBORw0KGgo='});} catch(e) {} text('done');".into(),tools(),&CancellationToken::new(),Duration::from_secs(2));
    let report = done(&mut session).await;
    assert!(report.error.is_some());
    assert_eq!(
        report
            .blocks
            .iter()
            .filter(|b| matches!(b, OutputBlock::Image { .. }))
            .count(),
        4
    );
    assert!(!report.content().contains("iVBOR"));
}

#[tokio::test]
async fn caught_output_overflow_in_a_microtask_discards_store_writes() {
    let mut session = Session::start(
        "await Promise.resolve().then(() => { try {text('x'.repeat(65537));} catch (_) {} store('cursor',2); });".into(),
        tools(),
        &CancellationToken::new(),
        Duration::from_secs(2),
    );
    let report = done(&mut session).await;
    assert!(report.error.unwrap().contains("64 KiB"));
    assert!(report.store_writes.is_empty());
}

#[tokio::test]
async fn models_aliases_dispatch_only_the_exact_admitted_tool() {
    let mut catalog = tools();
    catalog[0].model_operation = Some(ModelOperation::Classify);
    catalog[0].model_binding = Some(ModelBinding {
        owner: "owner".into(),
        provider: "provider".into(),
        model: "model".into(),
    });
    let mut session=Session::start("const result=await models.classify({owner:'owner',provider:'provider',model:'model'},{text:'hello',labels:['a','b']}); text(result.label);".into(),catalog,&CancellationToken::new(),Duration::from_secs(2));
    let Event::Calls { calls, reply } = session.next().await.unwrap() else {
        panic!("expected ordinary call")
    };
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].name, "read.rows");
    assert_eq!(calls[0].args, json!({"text":"hello","labels":["a","b"]}));
    reply.send(vec![(0, Ok(json!({"label":"a"})))]).unwrap();
    let report = done(&mut session).await;
    assert_eq!(report.error, None);
    assert_eq!(report.output, vec!["a"]);
}

#[tokio::test]
async fn models_without_admission_make_no_host_request() {
    let mut session = Session::start(
        "await models.generateImages({},{prompt:'hello'});".into(),
        tools(),
        &CancellationToken::new(),
        Duration::from_secs(2),
    );
    let report = done(&mut session).await;
    assert!(report.error.unwrap().contains("unavailable"));
    assert!(report.calls.is_empty());
}

#[test]
fn declarations_expand_local_refs_and_preserve_required_fields_and_output_types() {
    let mut tool = tools().remove(0);
    tool.schema = json!({"type":"object","properties":{"id":{"$ref":"#/$defs/id"},"note":{"type":"string"}},"required":["id"],"additionalProperties":false,"$defs":{"id":{"enum":["one","two"]}}});
    tool.output_schema = Some(json!({"type":"array","items":{"type":"integer"}}));
    let declaration = describe_tool(&tool);
    assert!(declaration.contains(r#""id": "one" | "two";"#));
    assert!(declaration.contains(r#""note"?: string;"#));
    assert!(declaration.contains("Promise<Array<number>>"));
}

#[test]
fn recursive_and_remote_schema_refs_are_honest_and_bounded() {
    assert_eq!(
        render_type(&json!({"$ref":"https://example.com/schema"})),
        "unknown"
    );
    let recursive = render_type(&json!({"type":"object","properties":{"next":{"$ref":"#"}}}));
    assert!(recursive.contains("unknown"));
    assert!(recursive.len() < 16384);
    let catalog: Vec<_> = (0..200)
        .map(|i| Tool {
            name: format!("ns{i}.read"),
            description: "read".into(),
            schema: json!({"type":"object"}),
            ..Default::default()
        })
        .collect();
    let prompt = declaration_description(&catalog, 50);
    assert!(prompt.len() <= 200);
    assert!(!prompt.is_empty());
}

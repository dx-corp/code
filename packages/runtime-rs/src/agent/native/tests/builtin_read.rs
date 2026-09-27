use super::*;
use maestro_runtime_contracts::BuiltinWorkTypeNamesProposal;

fn proposal() -> BuiltinWorkTypeNamesProposal {
    BuiltinWorkTypeNamesProposal {
        query: None,
        limit: 1,
    }
}

fn owner() -> Value {
    json!({
        "catalogRevision": "a".repeat(64), "catalogCount": 1,
        "matchedCount": 1, "returnedCount": 1, "offset": 0,
        "hasMore": false, "complete": true, "nextArguments": null,
        "nameIndexComplete": true,
        "nameIndex": [{"blueprintId":"banking-core", "name":"Banking"}],
        "blueprints": [{"blueprintId":"banking-core", "name":"Banking",
            "version":"1", "contentDigest":"b".repeat(64)}]
    })
}

fn fixture(
    admitted: bool,
    injected_context: Option<&str>,
) -> (
    super::super::NativeAgent,
    mpsc::UnboundedReceiver<FromAgent>,
    crate::ai::ScriptedClient,
    RuntimeTestHost,
) {
    fixture_with_hooks(admitted, injected_context, None, None, None)
}

fn fixture_with_hooks(
    admitted: bool,
    injected_context: Option<&str>,
    permission_hook: Option<NativeHookResult>,
    pre_tool_hook: Option<NativeHookResult>,
    eval_hook: Option<NativeHookResult>,
) -> (
    super::super::NativeAgent,
    mpsc::UnboundedReceiver<FromAgent>,
    crate::ai::ScriptedClient,
    RuntimeTestHost,
) {
    let scripted = crate::ai::ScriptedClient::new(
        "runtime-test/typed-read",
        vec![crate::ai::ScriptedResponse::text("provider must not run")],
    );
    let client = UnifiedClient::Scripted(scripted.clone());
    let mut host = RuntimeTestHost::new(".", client.clone());
    host.post_tool_context = injected_context.map(str::to_owned);
    host.permission_hook = permission_hook;
    host.pre_tool_hook = pre_tool_hook;
    host.eval_hook = eval_hook;
    let config = NativeAgentConfig {
        model: "runtime-test/typed-read".into(),
        ..NativeAgentConfig::default()
    };
    let tools = if admitted {
        vec![external_tool_definition("work_type.builtins")]
    } else {
        vec![]
    };
    let allowed = HashSet::new(); // Local registry is empty; external catalog is separate.
    let (agent, events) = super::super::NativeAgent::start_with_resolved_client(
        config,
        NativeExecutionHostHandle::new(Arc::new(host.clone())),
        tools,
        CredentialVault::new(),
        Some(&allowed),
        NativeResolvedClient {
            provider_name: client.provider_name().to_owned(),
            client: Some(client),
            model_route: NativeModelRoute::DirectProvider,
        },
    )
    .unwrap();
    (agent, events, scripted, host)
}

async fn respond(
    events: &mut mpsc::UnboundedReceiver<FromAgent>,
    sender: mpsc::UnboundedSender<ToolResponseMessage>,
    approved: bool,
    result: Option<ToolResult>,
    source: ExecutionSource,
) {
    let event = tokio::time::timeout(Duration::from_secs(5), events.recv())
        .await
        .unwrap()
        .unwrap();
    let FromAgent::ToolCall {
        call_id,
        tool,
        args,
        requires_approval,
        ..
    } = event
    else {
        panic!("typed read emitted a non-tool event: {event:?}");
    };
    assert_eq!(call_id, "typed-read-1");
    assert_eq!(tool, "work_type.builtins");
    assert_eq!(args, json!({}));
    assert!(requires_approval);
    sender
        .send((call_id, approved, result, source, None))
        .unwrap();
}

#[tokio::test]
async fn typed_read_executes_one_owner_callback_with_no_provider_or_local_execution() {
    for source in [ExecutionSource::Native, ExecutionSource::RemoteClient] {
        let (agent, mut events, scripted, host) = fixture(true, None);
        let (result, ()) = tokio::join!(
            agent.builtin_work_type_names(
                "List one built-in name".into(),
                "typed-read-1".into(),
                proposal(),
                CancellationToken::new()
            ),
            respond(
                &mut events,
                agent.tool_response_sender(),
                true,
                Some(ToolResult::success(owner().to_string())),
                source
            )
        );
        assert!(result.unwrap().render_text().contains("Banking"));
        assert_eq!(
            scripted.remaining(),
            1,
            "no model request or billed response"
        );
        assert_eq!(host.completed_tool_executions.load(Ordering::SeqCst), 0);
        assert_eq!(host.post_hook_outputs.lock().unwrap().len(), 1);
        assert_eq!(host.eval_hook_outputs.lock().unwrap().len(), 1);
        assert_eq!(
            *host.pre_message_models.lock().unwrap(),
            vec![Some("runtime-test/typed-read".to_owned())]
        );
        while let Ok(event) = events.try_recv() {
            assert!(!matches!(
                event,
                FromAgent::ResponseStart { .. }
                    | FromAgent::ResponseEnd { .. }
                    | FromAgent::ToolCall { .. }
            ));
        }
        assert!(
            agent
                .builtin_work_type_names(
                    "Again".into(),
                    "typed-read-1".into(),
                    proposal(),
                    CancellationToken::new()
                )
                .await
                .is_err(),
            "same actor must not replay an already attempted owner read"
        );
        agent.shutdown().await;
    }
}

#[tokio::test]
async fn typed_read_failures_do_not_recover_with_a_model_or_repeat_the_read() {
    for (approved, output, injected) in [
        (false, None, None),
        (true, None, None),
        (true, Some("{\"untrusted\":true}".to_owned()), None),
        (
            true,
            Some(owner().to_string()),
            Some("additional policy context"),
        ),
    ] {
        let (agent, mut events, scripted, host) = fixture(true, injected);
        let (result, ()) = tokio::join!(
            agent.builtin_work_type_names(
                "List one name".into(),
                "typed-read-1".into(),
                proposal(),
                CancellationToken::new()
            ),
            respond(
                &mut events,
                agent.tool_response_sender(),
                approved,
                output.map(ToolResult::success),
                ExecutionSource::Native
            )
        );
        assert!(result.is_err());
        assert_eq!(scripted.remaining(), 1);
        assert_eq!(host.completed_tool_executions.load(Ordering::SeqCst), 0);
        while let Ok(event) = events.try_recv() {
            assert!(!matches!(
                event,
                FromAgent::ToolCall { .. } | FromAgent::ResponseStart { .. }
            ));
        }
        agent.shutdown().await;
    }
}

#[tokio::test]
async fn typed_read_denies_unadmitted_tools_and_invalid_proposals_before_dispatch() {
    for (admitted, proposal) in [
        (false, proposal()),
        (
            true,
            BuiltinWorkTypeNamesProposal {
                query: None,
                limit: 9,
            },
        ),
    ] {
        let (agent, mut events, scripted, _) = fixture(admitted, None);
        assert!(
            agent
                .builtin_work_type_names(
                    "List names".into(),
                    "typed-read-1".into(),
                    proposal,
                    CancellationToken::new()
                )
                .await
                .is_err()
        );
        assert!(events.try_recv().is_err());
        assert_eq!(scripted.remaining(), 1);
        agent.shutdown().await;
    }
}

#[tokio::test]
async fn typed_read_cancellation_stops_waiting_without_replay() {
    let (agent, mut events, scripted, _) = fixture(true, None);
    let cancellation = CancellationToken::new();
    let (result, ()) = tokio::join!(
        agent.builtin_work_type_names(
            "List names".into(),
            "typed-read-1".into(),
            proposal(),
            cancellation.clone()
        ),
        async {
            assert!(matches!(
                events.recv().await.unwrap(),
                FromAgent::ToolCall { .. }
            ));
            cancellation.cancel();
        }
    );
    assert!(result.is_err());
    assert_eq!(scripted.remaining(), 1);
    assert!(
        agent
            .builtin_work_type_names(
                "Retry".into(),
                "typed-read-1".into(),
                proposal(),
                CancellationToken::new()
            )
            .await
            .is_err()
    );
    assert!(!matches!(events.try_recv(), Ok(FromAgent::ToolCall { .. })));
    agent.shutdown().await;
}

#[tokio::test]
async fn typed_read_permission_hook_must_allow_unchanged_execution() {
    for verdict in [
        NativeHookResult::Block {
            reason: "policy denies read".into(),
        },
        NativeHookResult::ModifyInput {
            new_input: json!({"query":"changed"}),
        },
        NativeHookResult::InjectContext {
            context: "additional restriction".into(),
        },
    ] {
        let (agent, mut events, scripted, host) =
            fixture_with_hooks(true, None, Some(verdict), None, None);
        assert!(
            agent
                .builtin_work_type_names(
                    "List names".into(),
                    "typed-read-1".into(),
                    proposal(),
                    CancellationToken::new()
                )
                .await
                .is_err()
        );
        assert!(events.try_recv().is_err());
        assert_eq!(scripted.remaining(), 1);
        assert_eq!(host.completed_tool_executions.load(Ordering::SeqCst), 0);
        agent.shutdown().await;
    }
}

#[tokio::test]
async fn dropping_typed_read_future_cancels_its_owner_wait() {
    let (agent, mut events, scripted, _) = fixture(true, None);
    let mut request = Box::pin(agent.builtin_work_type_names(
        "List names".into(),
        "typed-read-1".into(),
        proposal(),
        CancellationToken::new(),
    ));
    tokio::select! {
        result = &mut request => panic!("owner has not replied: {result:?}"),
        event = events.recv() => assert!(matches!(event, Some(FromAgent::ToolCall { .. }))),
    }
    drop(request);
    // A subsequent command resolves only after the cancelled wait returns.
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        agent.builtin_work_type_names(
            "Retry".into(),
            "typed-read-1".into(),
            proposal(),
            CancellationToken::new(),
        ),
    )
    .await
    .expect("dropped caller must not strand the actor");
    assert!(result.is_err());
    assert_eq!(scripted.remaining(), 1);
    agent.shutdown().await;
}

#[tokio::test]
async fn typed_read_tool_policy_and_eval_gate_fail_closed() {
    for verdict in [
        NativeHookResult::Block {
            reason: "read denied".into(),
        },
        NativeHookResult::ModifyInput {
            new_input: json!({"query":"different"}),
        },
        NativeHookResult::InjectContext {
            context: "new scope restriction".into(),
        },
    ] {
        let (agent, mut events, scripted, _) =
            fixture_with_hooks(true, None, None, Some(verdict), None);
        assert!(
            agent
                .builtin_work_type_names(
                    "List names".into(),
                    "typed-read-1".into(),
                    proposal(),
                    CancellationToken::new()
                )
                .await
                .is_err()
        );
        assert!(events.try_recv().is_err());
        assert_eq!(scripted.remaining(), 1);
        agent.shutdown().await;
    }
    let (agent, mut events, scripted, host) = fixture_with_hooks(
        true,
        None,
        None,
        None,
        Some(NativeHookResult::Block {
            reason: "result violates policy".into(),
        }),
    );
    let (result, ()) = tokio::join!(
        agent.builtin_work_type_names(
            "List names".into(),
            "typed-read-1".into(),
            proposal(),
            CancellationToken::new()
        ),
        respond(
            &mut events,
            agent.tool_response_sender(),
            true,
            Some(ToolResult::success(owner().to_string())),
            ExecutionSource::Native
        )
    );
    assert!(result.is_err());
    assert_eq!(host.eval_hook_outputs.lock().unwrap().len(), 1);
    assert_eq!(scripted.remaining(), 1);
    agent.shutdown().await;
}

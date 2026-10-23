use super::*;

fn message(text: &str) -> Message {
    Message {
        role: Role::User,
        content: MessageContent::text(text),
    }
}

fn identity() -> CandidateIdentity {
    CandidateIdentity {
        config: "model-a".into(),
        prompt: None,
        session_epoch: 1,
        namespace: "profile-a".into(),
        system_revision: 1,
        runtime_revision: 1,
    }
}

#[test]
fn background_compaction_preserves_newer_suffix() {
    let baseline = vec![message("deterministic summary"), message("recent turn")];
    let guard = CandidateGuard::new(&baseline, identity()).unwrap();
    let mut current = baseline;
    current.extend([message("new request"), message("new answer")]);
    let suffix = guard.current_suffix(&current, &identity()).unwrap();
    assert_eq!(
        serde_json::to_value(suffix).unwrap(),
        serde_json::to_value(&current[2..]).unwrap()
    );
}

#[test]
fn background_compaction_rejects_rewritten_or_shorter_history() {
    let baseline = vec![message("deterministic summary"), message("recent turn")];
    let guard = CandidateGuard::new(&baseline, identity()).unwrap();
    assert!(guard.current_suffix(&baseline[..1], &identity()).is_none());
    let changed = vec![message("replacement summary"), message("recent turn")];
    assert!(guard.current_suffix(&changed, &identity()).is_none());
}

#[test]
fn background_compaction_rejects_session_model_prompt_and_profile_changes() {
    let baseline = vec![message("summary")];
    let guard = CandidateGuard::new(&baseline, identity()).unwrap();
    let mut identities: Vec<_> = (0..6).map(|_| identity()).collect();
    identities[0].session_epoch += 1;
    identities[1].config = "model-b".into();
    identities[2].prompt = Some("new instructions".into());
    identities[3].namespace = "profile-b".into();
    identities[4].system_revision += 1;
    identities[5].runtime_revision += 1;
    for changed in identities {
        assert!(guard.current_suffix(&baseline, &changed).is_none());
    }
}

#[test]
fn background_compaction_never_spends_unreserved_finite_grants() {
    assert!(background_budget_available(None, false));
    assert!(!background_budget_available(Some(10_000), false));
    assert!(!background_budget_available(None, true));
}

#[tokio::test]
async fn background_compaction_closed_receiver_is_consumed() {
    let (sender, receiver) = tokio::sync::oneshot::channel::<BackgroundCompletion>();
    drop(sender);
    let mut slot = Some(receiver);
    assert!(take_completion(&mut slot).is_none());
    assert!(slot.is_none());
    assert!(take_completion(&mut slot).is_none());
}

#[tokio::test]
async fn background_compaction_completion_usage_is_consumed_once() {
    let (sender, receiver) = tokio::sync::oneshot::channel();
    sender
        .send(BackgroundCompletion {
            summary: None,
            usage: Some(TokenUsage {
                output_tokens: 17,
                ..Default::default()
            }),
        })
        .unwrap();
    let mut slot = Some(receiver);
    assert_eq!(
        take_completion(&mut slot)
            .unwrap()
            .usage
            .unwrap()
            .output_tokens,
        17
    );
    assert!(slot.is_none());
    assert!(take_completion(&mut slot).is_none());
}

async fn read_request(stream: &mut tokio::net::TcpStream) -> Value {
    use tokio::io::AsyncReadExt;
    let mut bytes = Vec::new();
    let mut buffer = [0; 4096];
    loop {
        let read = stream.read(&mut buffer).await.unwrap();
        assert_ne!(read, 0);
        bytes.extend_from_slice(&buffer[..read]);
        if let Some(boundary) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
            let header = std::str::from_utf8(&bytes[..boundary]).unwrap();
            let length: usize = header
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse().unwrap())
                })
                .unwrap();
            if bytes.len() >= boundary + 4 + length {
                return serde_json::from_slice(&bytes[boundary + 4..boundary + 4 + length])
                    .unwrap();
            }
        }
    }
}

async fn send_answer(stream: &mut tokio::net::TcpStream, text: &str) {
    use tokio::io::AsyncWriteExt;
    let chunk = json!({"id":"answer", "object":"chat.completion.chunk", "created":0,
        "model":"gpt-4o", "choices":[{"index":0,"delta":{"role":"assistant","content":text},"finish_reason":"stop"}]});
    let body = format!("data: {chunk}\n\ndata: [DONE]\n\n");
    stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
}

async fn completed_turn(events: &mut mpsc::UnboundedReceiver<FromAgent>) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match events.recv().await {
                Some(FromAgent::TurnCompleted { .. }) => return,
                Some(
                    FromAgent::ProviderError { message, .. } | FromAgent::Error { message, .. },
                ) => panic!("{message}"),
                None => panic!("runner ended before completion"),
                _ => {}
            }
        }
    })
    .await
    .expect("foreground turn must complete without waiting for the summary");
}

#[tokio::test]
async fn background_compaction_stalled_summary_does_not_block_next_turn_and_shutdown_closes_it() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (started_tx, started_rx) = oneshot::channel();
    let (closed_tx, closed_rx) = oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut primary, _) = listener.accept().await.unwrap();
        read_request(&mut primary).await;
        let answer = (0..4096)
            .map(|i| format!("response-{i} "))
            .collect::<String>();
        send_answer(&mut primary, &answer).await;
        drop(primary);
        let (mut summary, _) = listener.accept().await.unwrap();
        let request = read_request(&mut summary).await;
        assert!(
            request
                .to_string()
                .contains("Summarize this earlier conversation")
        );
        assert!(
            request
                .get("tools")
                .is_none_or(|tools| tools.as_array().is_some_and(Vec::is_empty))
        );
        assert!(
            request["max_tokens"]
                .as_u64()
                .or_else(|| request["max_completion_tokens"].as_u64())
                .unwrap()
                <= 2048
        );
        let chunk = json!({"id":"summary", "object":"chat.completion.chunk", "created":0,
            "model":"gpt-4o", "choices":[{"index":0,"delta":{"role":"assistant","content":"Still summarizing"},"finish_reason":null}]});
        summary.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: keep-alive\r\n\r\ndata: {chunk}\n\n").as_bytes()).await.unwrap();
        started_tx.send(()).unwrap();
        let (mut next, _) = listener.accept().await.unwrap();
        assert!(
            read_request(&mut next)
                .await
                .to_string()
                .contains("NEXT_REQUEST")
        );
        send_answer(&mut next, "Done.").await;
        drop(next);
        let mut byte = [0];
        assert_eq!(
            summary.read(&mut byte).await.unwrap(),
            0,
            "shutdown must release the upstream summary socket"
        );
        closed_tx.send(()).unwrap();
    });
    let client = UnifiedClient::OpenAI(
        crate::ai::OpenAiClient::with_base_url("test-key", format!("http://{address}/v1")).unwrap(),
    );
    let workspace = tempfile::tempdir().unwrap();
    let config = NativeAgentConfig {
        model: "openai/gpt-4o".into(),
        context_window: Some(8192),
        cwd: workspace.path().display().to_string(),
        ..Default::default()
    };
    let host = super::super::tests::RuntimeTestHost::new(config.cwd.clone(), client);
    let (agent, mut events) =
        super::super::tests::new_runtime_test_agent_with_host(config, host).unwrap();
    let history = (0..20)
        .map(|index| Message {
            role: if index % 2 == 0 {
                Role::User
            } else {
                Role::Assistant
            },
            content: MessageContent::text(format!("Earlier message {index}.")),
        })
        .collect();
    agent.replace_history(history);
    agent.prompt("Continue".into(), vec![]).await.unwrap();
    completed_turn(&mut events).await;
    tokio::time::timeout(Duration::from_secs(5), started_rx)
        .await
        .expect("summary should start")
        .unwrap();
    agent.prompt("NEXT_REQUEST".into(), vec![]).await.unwrap();
    completed_turn(&mut events).await;
    tokio::time::timeout(Duration::from_secs(5), agent.shutdown())
        .await
        .expect("shutdown should join the summary task");
    tokio::time::timeout(Duration::from_secs(2), closed_rx)
        .await
        .unwrap()
        .unwrap();
    server.await.unwrap();
}

async fn collect_completed_turn(
    events: &mut mpsc::UnboundedReceiver<FromAgent>,
    observed: &mut Vec<FromAgent>,
) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let event = events.recv().await.expect("runner stays available");
            if let FromAgent::ProviderError { message, .. } | FromAgent::Error { message, .. } =
                &event
            {
                panic!("{message}");
            }
            let complete = matches!(event, FromAgent::TurnCompleted { .. });
            observed.push(event);
            if complete {
                return;
            }
        }
    })
    .await
    .expect("foreground turn completes");
}

fn latest_local_snapshot(events: &[FromAgent]) -> Vec<Message> {
    events
        .iter()
        .rev()
        .find_map(|event| match event {
            FromAgent::ConversationSnapshot { local_messages, .. } => Some(local_messages.clone()),
            _ => None,
        })
        .expect("native runner emits its canonical conversation checkpoint")
}

#[tokio::test]
async fn background_compaction_adopts_completed_summary_preserves_suffix_and_bills_once() {
    use tokio::io::AsyncWriteExt;
    const SUMMARY: &str =
        "SEMANTIC_SUMMARY_ACCEPTED: continue the requested change and retain its evidence.";
    const NEWER_REQUEST: &str = "NEWER_REQUEST_EXACT: retain punctuation, spaces, and case.";
    const NEWER_ANSWER: &str = "NEWER_ANSWER_EXACT: the later answer stays intact.";
    const BOUNDARY: &str = "ADOPTION_BOUNDARY: continue with the completed summary.";
    let history: Vec<_> = (0..20)
        .map(|index| Message {
            role: if index % 2 == 0 {
                Role::User
            } else {
                Role::Assistant
            },
            content: if index == 0 {
                MessageContent::Blocks(vec![
                    ContentBlock::attributed_text(
                        "Preserve the requested change.",
                        maestro_ai::ContentSource::User,
                        None,
                    ),
                    ContentBlock::attributed_text(
                        "Repository evidence must survive compaction.",
                        maestro_ai::ContentSource::Repository,
                        Some("AGENTS.md".into()),
                    ),
                ])
            } else {
                MessageContent::text(format!("Earlier message {index}."))
            },
        })
        .collect();
    let large_answer = (0..16384)
        .map(|index| format!("response-{index} "))
        .collect::<String>();
    // Keep this a viable success fixture, independently of transport timing:
    // there must be a removed prefix, and its continuation must fit the budget.
    let compactor = crate::agent::compaction::ContextCompactor::new(
        crate::agent::compaction::CompactionConfig::for_model("openai/gpt-4o", Some(32768)),
    );
    let mut completed = history.clone();
    completed.push(message("Continue"));
    completed.push(Message {
        role: Role::Assistant,
        content: MessageContent::text(&large_answer),
    });
    let mut candidate = compactor.compact_with_tokens(&completed);
    assert!(candidate.compacted_count > 0);
    assert!(
        compactor.apply_semantic_summary(&mut candidate, SUMMARY),
        "fixture summary and typed continuation fit the configured summary budget"
    );

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (summary_started_tx, summary_started_rx) = oneshot::channel();
    let (release_summary_tx, release_summary_rx) = oneshot::channel();
    let server = tokio::spawn(async move {
        let mut requests = Vec::new();
        let (mut primary, _) = listener.accept().await.unwrap();
        requests.push(read_request(&mut primary).await);
        send_answer(&mut primary, &large_answer).await;
        drop(primary);
        let (mut summary, _) = listener.accept().await.unwrap();
        requests.push(read_request(&mut summary).await);
        assert!(
            requests[1]
                .to_string()
                .contains("Summarize this earlier conversation")
        );
        summary_started_tx.send(()).unwrap();
        // The summary remains unfinished until an entire newer turn is in the
        // canonical transcript. Its result cannot have summarized that suffix.
        let (mut newer, _) = listener.accept().await.unwrap();
        requests.push(read_request(&mut newer).await);
        send_answer(&mut newer, NEWER_ANSWER).await;
        drop(newer);
        release_summary_rx.await.unwrap();
        let chunk = json!({"id":"summary", "object":"chat.completion.chunk", "created":0,
            "model":"gpt-4o", "choices":[{"index":0,"delta":{"role":"assistant","content":SUMMARY},"finish_reason":"stop"}]});
        let usage = json!({"id":"summary", "object":"chat.completion.chunk", "created":0,
            "model":"gpt-4o", "choices":[], "usage":{"prompt_tokens":11,"completion_tokens":7,"prompt_tokens_details":{"cached_tokens":3}}});
        let body = format!("data: {chunk}\n\ndata: {usage}\n\ndata: [DONE]\n\n");
        summary.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
        drop(summary);
        for answer in ["Third turn finished.", "Fourth turn finished."] {
            let (mut foreground, _) = listener.accept().await.unwrap();
            requests.push(read_request(&mut foreground).await);
            send_answer(&mut foreground, answer).await;
        }
        requests
    });
    let client = UnifiedClient::OpenAI(
        crate::ai::OpenAiClient::with_base_url("test-key", format!("http://{address}/v1")).unwrap(),
    );
    let workspace = tempfile::tempdir().unwrap();
    let config = NativeAgentConfig {
        model: "openai/gpt-4o".into(),
        context_window: Some(32768),
        cwd: workspace.path().display().to_string(),
        ..Default::default()
    };
    let host = super::super::tests::RuntimeTestHost::new(config.cwd.clone(), client);
    let (agent, mut events) =
        super::super::tests::new_runtime_test_agent_with_host(config, host).unwrap();
    let mut observed = Vec::new();
    agent.replace_history(history);
    agent.prompt("Continue".into(), vec![]).await.unwrap();
    collect_completed_turn(&mut events, &mut observed).await;
    let baseline = latest_local_snapshot(&observed);
    let continuation = observed
        .iter()
        .find_map(|event| match event {
            FromAgent::Compaction {
                continuation: Some(record),
                ..
            } => Some(record.clone()),
            _ => None,
        })
        .expect("deterministic compaction supplies transcript-derived continuation");
    assert!(
        continuation
            .content_origins
            .iter()
            .any(
                |origin| origin.source == maestro_ai::ContentSource::Repository
                    && origin.origin.as_deref() == Some("AGENTS.md")
            )
    );
    tokio::time::timeout(Duration::from_secs(5), summary_started_rx)
        .await
        .unwrap()
        .unwrap();
    agent.prompt(NEWER_REQUEST.into(), vec![]).await.unwrap();
    collect_completed_turn(&mut events, &mut observed).await;
    let before_adoption = latest_local_snapshot(&observed);
    assert!(before_adoption.len() > baseline.len());
    assert!(
        before_adoption
            .iter()
            .any(|message| message.content.as_text() == Some(NEWER_REQUEST))
    );
    assert!(
        before_adoption
            .iter()
            .any(|message| message.content.as_text() == Some(NEWER_ANSWER))
    );
    assert!(
        !serde_json::to_string(&before_adoption)
            .unwrap()
            .contains(SUMMARY)
    );
    release_summary_tx.send(()).unwrap();
    // On this current-thread executor the task sends its candidate immediately
    // after usage without yielding. Receipt of usage therefore provides a
    // deterministic barrier before the next runner request boundary.
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let event = events.recv().await.unwrap();
            let completed = matches!(&event, FromAgent::BackgroundUsage { .. });
            observed.push(event);
            if completed {
                return;
            }
        }
    })
    .await
    .expect("completed summary reports its own measured usage");
    let adoption_start = observed.len();
    agent.prompt(BOUNDARY.into(), vec![]).await.unwrap();
    collect_completed_turn(&mut events, &mut observed).await;
    let adopted = observed[adoption_start..]
        .iter()
        .find_map(|event| match event {
            FromAgent::ConversationSnapshot { local_messages, .. }
                if local_messages
                    .first()
                    .and_then(|message| message.content.as_text())
                    .is_some_and(|text| text.contains(SUMMARY)) =>
            {
                Some(local_messages)
            }
            _ => None,
        })
        .expect("completed background summary is adopted at the next real request boundary");
    assert_eq!(
        serde_json::to_value(&adopted[1..before_adoption.len()]).unwrap(),
        serde_json::to_value(&before_adoption[1..]).unwrap(),
        "summary adoption must preserve every retained message and the exact newer suffix"
    );
    assert_eq!(
        adopted
            .iter()
            .filter(|message| message.content.as_text() == Some(BOUNDARY))
            .count(),
        1
    );
    let MessageContent::Blocks(parts) = &adopted[0].content else {
        panic!("summary retains typed provenance")
    };
    let ContentBlock::AttributedText { text, provenance } = &parts[0] else {
        panic!("summary is a typed attributed part")
    };
    assert_eq!(provenance.source, maestro_ai::ContentSource::Harness);
    assert_eq!(provenance.origin.as_deref(), Some("context_compaction"));
    assert_eq!(provenance.derived_from, continuation.content_origins);
    assert!(
        text.contains(&serde_json::to_string(&continuation.user_requests).unwrap()),
        "summary retains the transcript-derived exact user requests"
    );
    agent
        .prompt("ONE_MORE_BOUNDARY".into(), vec![])
        .await
        .unwrap();
    collect_completed_turn(&mut events, &mut observed).await;
    agent.shutdown().await;
    while let Ok(event) = events.try_recv() {
        observed.push(event);
    }
    let usages: Vec<_> = observed
        .iter()
        .filter_map(|event| match event {
            FromAgent::BackgroundUsage {
                request_id,
                model,
                usage,
            } => Some((request_id, model, usage)),
            _ => None,
        })
        .collect();
    assert_eq!(
        usages.len(),
        1,
        "adoption, later polls and shutdown cannot bill the same completion twice"
    );
    assert!(!usages[0].0.is_empty());
    assert!(usages[0].1.ends_with("gpt-4o"));
    assert_eq!(usages[0].2.input_tokens, 8); // 11 prompt tokens minus 3 cache hits.
    assert_eq!(usages[0].2.output_tokens, 7);
    assert_eq!(usages[0].2.cache_read_tokens, 3);
    assert!(
        observed
            .iter()
            .all(|event| !matches!(event, FromAgent::ResponseEnd { usage: Some(_), .. })),
        "background usage must not amend an unrelated foreground ResponseEnd"
    );
    let requests = tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(requests.len(), 5);
    for request in &requests[3..] {
        let wire = request.to_string();
        assert_eq!(wire.matches(SUMMARY).count(), 1);
        assert_eq!(wire.matches(NEWER_REQUEST).count(), 1);
        assert_eq!(wire.matches(NEWER_ANSWER).count(), 1);
    }
}

//! Conversation-only forks of completed native turns. Workspace files are never restored.
use super::*;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ForkOrigin {
    pub(crate) session_id: String,
    pub(crate) created_at: String,
    pub(crate) turn_index: usize,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ForkRequest {
    turn_index: usize,
    source_created_at: String,
    title: Option<String>,
}

fn completed_turn_index(message: &Value) -> Option<u64> {
    if message.get("role").and_then(Value::as_str) != Some("assistant") {
        return None;
    }
    match message.get("turnCompleted").and_then(Value::as_bool) {
        Some(true) => message.get("turnIndex").and_then(Value::as_u64),
        Some(false) => None,
        None => message.get("fileSnapshotTurnIndex").and_then(Value::as_u64),
    }
}

fn prepare(
    source: &SessionRecord,
    auth: &AuthContext,
    request: &ForkRequest,
) -> Result<SessionRecord, (u16, &'static str)> {
    if !session_visible_to_auth(source, auth) {
        return Err((404, "Session not found"));
    }
    if source.created_at != request.source_created_at {
        return Err((409, "Session generation changed"));
    }
    // An assistant completion carries the accepted user-turn coordinate even
    // when file capture is unavailable. A trailing prompt is never a boundary.
    let completed = |message: &Value| completed_turn_index(message).is_some();
    if !source.messages.last().is_some_and(completed) || source.last_turn_error.is_some() {
        return Err((409, "Source turn is not completed"));
    }
    let end = source
        .messages
        .iter()
        .position(|message| {
            completed(message)
                && completed_turn_index(message) == u64::try_from(request.turn_index).ok()
        })
        .ok_or((409, "Completed turn not found"))?;
    let mut target = create_session_record(
        request.title.clone().or_else(|| {
            Some(format!(
                "{} fork",
                source.title.chars().take(240).collect::<String>()
            ))
        }),
        source.owner.clone(),
    );
    target.organization_id = source.organization_id.clone();
    target.workspace_id = source.workspace_id.clone();
    target.log_group_id = source.log_group_id.clone();
    target.messages = source.messages[..=end]
        .iter()
        .map(|message| {
            let mut public = public_session_message(message);
            if let Some(object) = public.as_object_mut() {
                // Saved usage belongs to the original conversation. Native provider
                // thread IDs must not cause the child to resume the parent's future.
                for key in [
                    "usage",
                    "codexThreadId",
                    "providerThreadId",
                    "nativeThreadId",
                ] {
                    object.remove(key);
                }
                if object.contains_key("fileSnapshotTurnIndex") {
                    object.insert("fileSnapshotAvailable".into(), Value::Bool(false));
                }
            }
            public
        })
        .collect();
    target.message_count = target.messages.len() as u64;
    target.forked_from = Some(ForkOrigin {
        session_id: source.id.clone(),
        created_at: source.created_at.clone(),
        turn_index: request.turn_index,
    });
    Ok(target)
}

pub(super) async fn handle(
    stream: &mut TcpStream,
    initial: &mut Vec<u8>,
    head: &RequestHead,
    state: &AppState,
    auth: &AuthContext,
    id: &str,
) -> Vec<u8> {
    let body = match read_request_body(stream, initial, head).await {
        Ok(body) => body,
        Err(error) => return json_response(400, &serde_json::json!({"error":error})),
    };
    let request: ForkRequest = match serde_json::from_slice(&body) {
        Ok(request) => request,
        Err(_) => {
            return json_response(
                400,
                &serde_json::json!({"error":"Invalid conversation fork request"}),
            );
        }
    };
    let mut store = state.sessions.lock().await;
    let Some(source) = store.sessions.get(id) else {
        return json_response(404, &serde_json::json!({"error":"Session not found"}));
    };
    let target = match prepare(source, auth, &request) {
        Ok(target) => target,
        Err((status, message)) => {
            return json_response(status, &serde_json::json!({"error":message}));
        }
    };
    if crate::native_turns::session_has_active_native_turn(state, id, &request.source_created_at)
        .await
    {
        return json_response(
            409,
            &serde_json::json!({"error":"Source turn is still active"}),
        );
    }
    let mut value = match session_page_value(&target, &HashMap::new()) {
        Ok(value) => value,
        Err(message) => return json_response(500, &serde_json::json!({"error":message})),
    };
    value["forkedFrom"] = serde_json::json!(target.forked_from);
    value["continuation"] = serde_json::json!("portableTranscript");
    value["workspaceRestored"] = serde_json::json!(false);
    let target_id = target.id.clone();
    store.sessions.insert(target_id.clone(), target);
    if let Err(error) = persist_session_store_snapshot(state, &store).await {
        store.sessions.remove(&target_id);
        return json_response(
            503,
            &serde_json::json!({"error":format!("Could not persist conversation fork: {error}")}),
        );
    }
    json_response(200, &value)
}

#[cfg(test)]
mod session_fork_tests {
    use super::*;
    fn fixture() -> (SessionRecord, AuthContext, ForkRequest) {
        let mut source = create_session_record(Some("Source".into()), Some("owner".into()));
        source.id = "source".into();
        source.organization_id = Some("org".into());
        source.workspace_id = Some("ws".into());
        source.messages = vec![
            serde_json::json!({"role":"user","content":"first user","attachments":[{"content":"raw private","fileName":"one.txt"}]}),
            serde_json::json!({"role":"assistant","content":[{"type":"text","text":"first answer"}],"tools":[{"name":"read","result":"retained"}],"fileSnapshotTurnIndex":0,"fileSnapshotAvailable":true,"usage":{"input":123}}),
            serde_json::json!({"role":"user","content":"second user"}),
            serde_json::json!({"role":"assistant","content":"second answer","fileSnapshotTurnIndex":1}),
        ];
        let request = ForkRequest {
            turn_index: 0,
            source_created_at: source.created_at.clone(),
            title: None,
        };
        (
            source,
            AuthContext {
                subject: Some("owner".into()),
                organization_id: Some("org".into()),
                workspace_id: Some("ws".into()),
                ..AuthContext::default()
            },
            request,
        )
    }
    #[test]
    fn session_fork_copies_exact_completed_prefix_and_native_prompt_uses_it() {
        let (source, auth, request) = fixture();
        let target = prepare(&source, &auth, &request).unwrap();
        assert_ne!(target.id, source.id);
        assert_eq!(target.owner, source.owner);
        assert_eq!(target.organization_id, source.organization_id);
        assert_eq!(target.message_count, 2);
        assert_eq!(target.messages[1]["fileSnapshotAvailable"], false);
        assert!(target.messages[1].get("usage").is_none());
        assert_eq!(target.messages[0]["attachments"][0]["contentOmitted"], true);
        let mut messages = target.messages.clone();
        messages.push(serde_json::json!({"role":"user","content":"new continuation"}));
        let chat: ChatRequest =
            serde_json::from_value(serde_json::json!({"sessionId":target.id,"messages":messages}))
                .unwrap();
        let prompt = build_prompt_from_chat(&chat);
        assert!(prompt.contains("first answer"));
        assert!(prompt.contains("retained"));
        assert!(prompt.contains("new continuation"));
        assert!(!prompt.contains("second answer"));
        assert!(!prompt.contains("raw private"));
        assert!(
            source.messages[0]["attachments"][0]
                .get("content")
                .is_some()
        );
    }
    #[test]
    fn session_fork_rejects_wrong_owner_generation_and_incomplete_boundaries() {
        let (mut source, mut auth, mut request) = fixture();
        auth.workspace_id = Some("other".into());
        assert_eq!(prepare(&source, &auth, &request).err().unwrap().0, 404);
        auth.workspace_id = Some("ws".into());
        request.source_created_at = "reused".into();
        assert_eq!(prepare(&source, &auth, &request).err().unwrap().0, 409);
        request.source_created_at = source.created_at.clone();
        request.turn_index = 99;
        assert_eq!(prepare(&source, &auth, &request).err().unwrap().0, 409);
        request.turn_index = 0;
        source
            .messages
            .push(serde_json::json!({"role":"user","content":"active"}));
        assert_eq!(prepare(&source, &auth, &request).err().unwrap().0, 409);
        source.messages.pop();
        source.last_turn_error = Some("failed".into());
        assert!(prepare(&source, &auth, &request).is_err());
    }

    #[test]
    fn session_fork_explicit_completion_coordinates_do_not_depend_on_file_capture() {
        let (mut source, auth, request) = fixture();
        source.messages[1]["turnCompleted"] = serde_json::json!(true);
        source.messages[1]["turnIndex"] = serde_json::json!(0);
        source.messages[1]
            .as_object_mut()
            .unwrap()
            .remove("fileSnapshotTurnIndex");
        assert_eq!(prepare(&source, &auth, &request).unwrap().message_count, 2);
        source.messages[1]["turnCompleted"] = serde_json::json!(false);
        source.messages[1]["fileSnapshotTurnIndex"] = serde_json::json!(0);
        assert!(prepare(&source, &auth, &request).is_err());
    }

    #[tokio::test]
    async fn session_fork_http_persists_destination_and_rolls_back_failed_persistence() {
        let (source, auth, request) = fixture();
        let root = std::env::temp_dir().join(new_session_id());
        let mut state =
            crate::tests::test_app_state_with_sessions(HashMap::from([("source".into(), source)]));
        let mut config = (*state.config).clone();
        config.session_store_path = root.join("sessions.json");
        state.config = Arc::new(config);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let connect = TcpStream::connect(listener.local_addr().unwrap());
        let accept = listener.accept();
        let (client, server) = tokio::join!(connect, accept);
        let _client = client.unwrap();
        let (mut stream, _) = server.unwrap();
        let body=serde_json::to_vec(&serde_json::json!({"turnIndex":request.turn_index,"sourceCreatedAt":request.source_created_at})).unwrap();
        let head = RequestHead {
            method: "POST".into(),
            path: "/api/sessions/source/fork".into(),
            query: HashMap::new(),
            headers: HashMap::from([("content-length".into(), body.len().to_string())]),
        };
        let mut initial = b"POST /api/sessions/source/fork HTTP/1.1\r\n\r\n".to_vec();
        initial.extend(&body);
        let response = handle(&mut stream, &mut initial, &head, &state, &auth, "source").await;
        let response = std::str::from_utf8(&response).unwrap();
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        let value: Value =
            serde_json::from_str(response.split_once("\r\n\r\n").unwrap().1).unwrap();
        assert_eq!(value["workspaceRestored"], false);
        let id = value["id"].as_str().unwrap();
        let (restored, valid) = load_session_store(&state.config.session_store_path).await;
        assert!(valid);
        assert_eq!(restored.sessions[id].message_count, 2);
        assert_eq!(restored.sessions["source"].messages.len(), 4);
        state.session_store_persist_enabled = false;
        let rejected = handle(&mut stream, &mut initial, &head, &state, &auth, "source").await;
        assert!(
            std::str::from_utf8(&rejected)
                .unwrap()
                .starts_with("HTTP/1.1 503")
        );
        assert_eq!(state.sessions.lock().await.sessions.len(), 2);
        tokio::fs::remove_dir_all(root).await.unwrap();
    }
}

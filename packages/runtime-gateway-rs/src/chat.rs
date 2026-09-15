use super::*;
use maestro_local_host::embedding::{EmbeddedAgent, EmbeddedAgentBuilder};

pub(crate) fn is_chat_endpoint(head: &RequestHead) -> bool {
    head.method == "POST" && head.path == "/api/chat"
}

pub(crate) fn is_chat_websocket_endpoint(head: &RequestHead) -> bool {
    head.method == "GET" && head.path == "/api/chat/ws"
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ChatRequest {
    pub(crate) model: Option<String>,
    pub(crate) messages: Vec<ChatMessage>,
    pub(crate) thinking_level: Option<String>,
    pub(crate) session_id: Option<String>,
    #[serde(default)]
    pub(crate) tools: Vec<ClientToolDefinition>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ClientToolDefinition {
    pub(crate) name: String,
    pub(crate) description: String,
    pub(crate) parameters: Value,
}

pub(crate) fn validate_client_tool_names(chat: &ChatRequest) -> Result<(), String> {
    if let Some(tool) = chat
        .tools
        .iter()
        .find(|tool| is_session_messaging_tool(&tool.name.to_ascii_lowercase()))
    {
        return Err(format!(
            "client tool name `{}` is reserved by the gateway",
            tool.name
        ));
    }
    Ok(())
}

fn client_tool_definitions(chat: &ChatRequest) -> (Vec<ToolDefinition>, HashSet<String>) {
    let names = chat
        .tools
        .iter()
        .map(|tool| tool.name.to_lowercase())
        .collect::<HashSet<_>>();
    let definitions = chat
        .tools
        .iter()
        .map(|tool| ToolDefinition {
            tool: Tool::new(&tool.name, &tool.description).with_schema(tool.parameters.clone()),
            requires_approval: true,
        })
        .collect();
    (definitions, names)
}

fn native_chat_terminal_status(event: &FromAgent) -> Option<Result<(), String>> {
    match event {
        FromAgent::TurnCompleted { .. } => Some(Ok(())),
        FromAgent::TurnInterrupted { reason, .. } => Some(Err(reason.clone())),
        FromAgent::ProviderError { kind, message } => {
            Some(Err(format!("provider failure ({kind:?}): {message}")))
        }
        _ => None,
    }
}

fn native_chat_acknowledges_peer_messages(event: &FromAgent) -> bool {
    matches!(native_chat_terminal_status(event), Some(Ok(())))
}

fn managed_gateway_receipt_status(
    request_id: String,
    record_id: String,
    lineage_id: String,
    record_status: String,
) -> Value {
    serde_json::json!({
        "type": "status",
        "status": "managed_gateway_receipt",
        "details": {
            "requestId": request_id,
            "recordId": record_id,
            "lineageId": lineage_id,
            "recordStatus": record_status,
        }
    })
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ChatMessage {
    pub(crate) role: String,
    pub(crate) content: Value,
    #[serde(default)]
    pub(crate) attachments: Vec<ChatAttachment>,
    #[serde(default, flatten)]
    pub(crate) extra: Map<String, Value>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ChatAttachment {
    pub(crate) id: Option<String>,
    #[serde(rename = "type")]
    pub(crate) attachment_type: Option<String>,
    pub(crate) file_name: Option<String>,
    pub(crate) mime_type: Option<String>,
    pub(crate) content: Option<String>,
    pub(crate) content_omitted: Option<bool>,
    pub(crate) extracted_text: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ExtractAttachmentRequest {
    pub(crate) file_name: String,
    pub(crate) mime_type: Option<String>,
    pub(crate) content_base64: String,
    pub(crate) max_chars: Option<usize>,
}

pub(crate) struct ExtractDocumentOutput {
    pub(crate) file_name: String,
    pub(crate) format: String,
    pub(crate) extractor: String,
    pub(crate) size_bytes: usize,
    pub(crate) truncated: bool,
    pub(crate) extracted_text: String,
}

pub(crate) struct PreparedAttachments {
    pub(crate) paths: Vec<String>,
    pub(crate) temp_dir: Option<PathBuf>,
}

impl Drop for PreparedAttachments {
    fn drop(&mut self) {
        if let Some(temp_dir) = self.temp_dir.take() {
            let _ = std::fs::remove_dir_all(temp_dir);
        }
    }
}

async fn selected_chat_model(chat: &ChatRequest, state: &AppState) -> String {
    if let Some(model) = chat
        .model
        .as_deref()
        .map(str::trim)
        .filter(|model| !model.is_empty())
    {
        return model.to_string();
    }
    let selected = state.selected_model.lock().await;
    format!("{}/{}", selected.provider, selected.id)
}

async fn handle_codex_app_server_chat(
    stream: &mut TcpStream,
    state: &AppState,
    session_id: Option<&str>,
    model: &str,
    prompt: &str,
    attachment_paths: &[String],
) -> Result<bool, String> {
    handle_codex_app_server_chat_transport(
        stream,
        state,
        session_id,
        model,
        prompt,
        attachment_paths,
        CodexBridgeTransport::Sse,
    )
    .await
}

pub(crate) async fn handle_codex_app_server_chat_transport(
    stream: &mut TcpStream,
    state: &AppState,
    session_id: Option<&str>,
    model: &str,
    prompt: &str,
    attachment_paths: &[String],
    transport: CodexBridgeTransport,
) -> Result<bool, String> {
    let session_approval_mode = approval_mode_for_session(state, session_id).await;
    let approval_mode = codex_app_server_approval_mode(&session_approval_mode);
    send_codex_bridge_event(
        stream,
        transport,
        &serde_json::json!({ "type": "agent_start" }),
    )
    .await?;
    send_codex_bridge_event(
        stream,
        transport,
        &serde_json::json!({ "type": "turn_start" }),
    )
    .await?;
    let message = composer_assistant_message("", "", None);
    send_codex_bridge_event(
        stream,
        transport,
        &serde_json::json!({ "type": "message_start", "message": message }),
    )
    .await?;

    let assistant_output_result = if approval_mode == "prompt" {
        run_codex_app_server_headless_cli(
            stream,
            transport,
            state,
            session_id,
            &state.config.cwd,
            model,
            prompt,
            attachment_paths,
        )
        .await
    } else {
        run_codex_app_server_cli(
            &state.config.cwd,
            model,
            approval_mode,
            prompt,
            attachment_paths,
        )
        .await
    };
    let assistant_output = match assistant_output_result {
        Ok(output) => output,
        Err(error) => {
            send_codex_bridge_event(
                stream,
                transport,
                &serde_json::json!({ "type": "error", "message": error }),
            )
            .await?;
            send_codex_bridge_event(stream, transport, &serde_json::json!({ "type": "done" }))
                .await?;
            return Ok(false);
        }
    };
    for tool_event in &assistant_output.tool_events {
        send_codex_bridge_tool_event(stream, transport, tool_event).await?;
    }

    let message =
        composer_assistant_message(&assistant_output.text, "", assistant_output.usage.clone());
    if !assistant_output.text.is_empty() {
        send_codex_bridge_event(
            stream,
            transport,
            &serde_json::json!({
                "type": "message_update",
                "message": message,
                "assistantMessageEvent": {
                    "type": "text_delta",
                    "contentIndex": 0,
                    "delta": assistant_output.text
                }
            }),
        )
        .await?;
    }
    record_chat_assistant_message(state, session_id, message.clone()).await;
    record_usage_entry(
        state,
        session_id,
        "openai-codex",
        model,
        assistant_output.usage.as_ref(),
    )
    .await;
    send_codex_bridge_event(
        stream,
        transport,
        &serde_json::json!({ "type": "message_end", "message": message }),
    )
    .await?;
    send_codex_bridge_event(
        stream,
        transport,
        &serde_json::json!({
            "type": "turn_end",
            "message": message,
            "toolResults": []
        }),
    )
    .await?;
    send_codex_bridge_event(
        stream,
        transport,
        &serde_json::json!({
            "type": "agent_end",
            "messages": [message],
            "stopReason": "stop"
        }),
    )
    .await?;
    send_codex_bridge_event(stream, transport, &serde_json::json!({ "type": "done" })).await?;
    Ok(true)
}

async fn handle_codex_app_server_chat_ws(
    stream: &mut TcpStream,
    state: &AppState,
    session_id: Option<&str>,
    model: &str,
    prompt: &str,
    attachment_paths: &[String],
) -> Result<bool, String> {
    handle_codex_app_server_chat_transport(
        stream,
        state,
        session_id,
        model,
        prompt,
        attachment_paths,
        CodexBridgeTransport::WebSocket,
    )
    .await
}

pub(crate) async fn record_chat_user_message(
    state: &AppState,
    chat: &ChatRequest,
    auth: &AuthContext,
) -> Result<Option<String>, String> {
    let Some(session_id) = chat.session_id.as_deref() else {
        return Ok(None);
    };
    let Some(latest) = chat.messages.last() else {
        return Ok(None);
    };
    let mut message = chat_message_prompt_value(latest);
    if let Value::Object(object) = &mut message {
        object.insert("timestamp".to_string(), Value::String(now_rfc3339()));
    }
    if !latest.attachments.is_empty() {
        message["attachments"] = serde_json::json!(latest.attachments);
    }
    let message_count = append_session_message(
        state,
        session_id,
        message,
        Some(&latest.content),
        auth.subject.clone(),
        Some(auth),
    )
    .await?;
    let sessions = state.sessions.lock().await;
    let session = sessions
        .sessions
        .get(session_id)
        .ok_or_else(|| "Session not found".to_string())?;
    Ok(Some(format!(
        "{}:{}:{}",
        session.id, session.created_at, message_count
    )))
}

async fn record_chat_assistant_message(state: &AppState, session_id: Option<&str>, message: Value) {
    let Some(session_id) = session_id else {
        return;
    };
    let _ = append_session_message(state, session_id, message, None, None, None).await;
}

async fn prepend_pending_peer_messages(
    state: &AppState,
    session_id: Option<&str>,
    auth: &AuthContext,
    prompt: &mut String,
) -> Vec<String> {
    let Some(session_id) = session_id else {
        return Vec::new();
    };
    let pending = unread_inbox_for_session(state, session_id, auth).await;
    if let Some(block) = render_peer_messages_block(&pending) {
        *prompt = format!("{block}\n{prompt}");
    }
    pending.into_iter().map(|message| message.id).collect()
}

async fn acknowledge_peer_messages(
    state: &AppState,
    session_id: Option<&str>,
    auth: &AuthContext,
    message_ids: &[String],
) {
    let Some(session_id) = session_id else {
        return;
    };
    if let Err(error) = mark_inbox_messages_read(state, session_id, auth, message_ids).await {
        eprintln!("failed to acknowledge delivered peer messages: {error}");
    }
}

async fn append_session_message(
    state: &AppState,
    session_id: &str,
    message: Value,
    title_source: Option<&Value>,
    owner: Option<String>,
    auth: Option<&AuthContext>,
) -> Result<u64, String> {
    let mut sessions = state.sessions.lock().await;
    let session = if sessions.sessions.contains_key(session_id) {
        let session = sessions
            .sessions
            .get_mut(session_id)
            .expect("session existence checked");
        if auth.is_some_and(|auth| !session_visible_to_auth(session, auth)) {
            return Err("Session not found".to_string());
        }
        session
    } else {
        sessions
            .sessions
            .entry(session_id.to_string())
            .or_insert_with(|| {
                let mut session =
                    create_session_record(title_source.and_then(title_from_content), owner);
                session.id = session_id.to_string();
                if let Some(auth) = auth {
                    bind_session_to_auth(&mut session, auth);
                }
                session
            })
    };
    if session.message_count == 0 {
        if let Some(title) = title_source.and_then(title_from_content) {
            session.title = title;
        }
    }
    session.messages.push(message);
    session.message_count = session.messages.len() as u64;
    let message_count = session.message_count;
    session.updated_at = now_rfc3339();
    drop(sessions);
    persist_session_store(state).await;
    Ok(message_count)
}

fn title_from_content(content: &Value) -> Option<String> {
    let text = composer_text_content(content);
    let title = text
        .split_whitespace()
        .take(12)
        .collect::<Vec<_>>()
        .join(" ");
    normalize_title(Some(title)).map(|title| title.chars().take(80).collect())
}

pub(crate) async fn handle_chat_endpoint(
    mut stream: TcpStream,
    mut initial: Vec<u8>,
    head: RequestHead,
    state: AppState,
) -> Result<(), String> {
    let auth = match authorized_context(&head, &state.config) {
        Ok(auth) => auth,
        Err(response) => {
            stream
                .write_all(&response)
                .await
                .map_err(|error| error.to_string())?;
            let _ = stream.shutdown().await;
            return Ok(());
        }
    };
    if let Err(response) = validate_csrf(&head, &state.config) {
        stream
            .write_all(&response)
            .await
            .map_err(|error| error.to_string())?;
        let _ = stream.shutdown().await;
        return Ok(());
    }

    let body = match read_request_body(&mut stream, &mut initial, &head).await {
        Ok(body) => body,
        Err(error) => {
            stream
                .write_all(&json_response(400, &serde_json::json!({ "error": error })))
                .await
                .map_err(|error| error.to_string())?;
            let _ = stream.shutdown().await;
            return Ok(());
        }
    };
    let chat = match serde_json::from_slice::<ChatRequest>(&body) {
        Ok(request) => request,
        Err(error) => {
            stream
                .write_all(&json_response(
                    400,
                    &serde_json::json!({ "error": format!("invalid chat request: {error}") }),
                ))
                .await
                .map_err(|error| error.to_string())?;
            let _ = stream.shutdown().await;
            return Ok(());
        }
    };
    if let Err(error) = validate_client_tool_names(&chat) {
        stream
            .write_all(&json_response(400, &serde_json::json!({ "error": error })))
            .await
            .map_err(|error| error.to_string())?;
        let _ = stream.shutdown().await;
        return Ok(());
    }

    let Some(latest) = chat.messages.last() else {
        stream
            .write_all(&json_response(
                400,
                &serde_json::json!({ "error": "No messages supplied" }),
            ))
            .await
            .map_err(|error| error.to_string())?;
        let _ = stream.shutdown().await;
        return Ok(());
    };
    if latest.role != "user" {
        stream
            .write_all(&json_response(
                400,
                &serde_json::json!({ "error": "Last message must be a user message" }),
            ))
            .await
            .map_err(|error| error.to_string())?;
        let _ = stream.shutdown().await;
        return Ok(());
    }

    if !chat_message_has_input(latest) {
        stream
            .write_all(&json_response(
                400,
                &serde_json::json!({ "error": "User message cannot be empty" }),
            ))
            .await
            .map_err(|error| error.to_string())?;
        let _ = stream.shutdown().await;
        return Ok(());
    }
    let mut prompt = build_prompt_from_chat(&chat);
    let system_prompt = system_prompt_from_chat(&chat);

    let session_id = chat.session_id.clone();
    let prepared_attachments = match prepare_chat_attachments(&chat, &state.config.cwd).await {
        Ok(attachments) => attachments,
        Err(error) => {
            stream
                .write_all(&json_response(400, &serde_json::json!({ "error": error })))
                .await
                .map_err(|error| error.to_string())?;
            let _ = stream.shutdown().await;
            return Ok(());
        }
    };
    let turn_scope = match record_chat_user_message(&state, &chat, &auth).await {
        Ok(turn_scope) => turn_scope,
        Err(error) => {
            cleanup_prepared_attachments(prepared_attachments).await;
            stream
                .write_all(&json_response(404, &serde_json::json!({ "error": error })))
                .await
                .map_err(|error| error.to_string())?;
            let _ = stream.shutdown().await;
            return Ok(());
        }
    };
    let pending_peer_message_ids =
        prepend_pending_peer_messages(&state, session_id.as_deref(), &auth, &mut prompt).await;

    stream
        .write_all(sse_headers().as_bytes())
        .await
        .map_err(|error| error.to_string())?;

    let model = selected_chat_model(&chat, &state).await;
    if let Some(codex_model) = codex_app_server_model_id(&model) {
        if let Some(session_id) = session_id.as_deref() {
            send_sse(
                &mut stream,
                &serde_json::json!({
                    "type": "status",
                    "status": "session",
                    "details": { "sessionId": session_id, "runtime": "rust-codex-app-server" }
                }),
            )
            .await?;
        }
        let prompt_succeeded = handle_codex_app_server_chat(
            &mut stream,
            &state,
            session_id.as_deref(),
            &codex_model,
            &prompt,
            &prepared_attachments.paths,
        )
        .await?;
        if prompt_succeeded {
            acknowledge_peer_messages(
                &state,
                session_id.as_deref(),
                &auth,
                &pending_peer_message_ids,
            )
            .await;
        }
        let _ = stream.shutdown().await;
        cleanup_prepared_attachments(prepared_attachments).await;
        return Ok(());
    }
    let (usage_provider, usage_model) = usage_provider_model(&chat, &state, &model).await;
    let (mut client_tools, client_tool_names) = client_tool_definitions(&chat);
    // Gateway-handled session-messaging tools give an agent turn the same reach
    // the loopback endpoints give a UI client. They are answered inline below
    // through the tool-response channel, under this turn's AuthContext.
    client_tools.extend(session_messaging_tool_definitions());
    let thinking_enabled = chat
        .thinking_level
        .as_deref()
        .map(|level| !matches!(level, "off" | "none" | "disabled"))
        .unwrap_or(false);
    let config = NativeAgentConfig {
        model,
        external_tool_schema_policy: maestro_local_host::agent::ExternalToolSchemaPolicy::Deferred,
        cwd: state.config.cwd.to_string_lossy().to_string(),
        system_prompt,
        thinking_enabled,
        thinking_budget: env::var("MAESTRO_THINKING_BUDGET")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(10_000),
        ..NativeAgentConfig::default()
    };
    let (agent, mut events) = match EmbeddedAgentBuilder::from_config(config)
        .external_tools(client_tools)
        .start()
    {
        Ok(session) => session.into_parts(),
        Err(error) => {
            send_sse(
                &mut stream,
                &serde_json::json!({ "type": "error", "message": error.to_string() }),
            )
            .await?;
            send_sse(&mut stream, &serde_json::json!({ "type": "done" })).await?;
            let _ = stream.shutdown().await;
            cleanup_prepared_attachments(prepared_attachments).await;
            return Ok(());
        }
    };

    // The native actor can leave a client-owned tool or ordinary approval
    // pending when the HTTP writer fails. Keep the exact IDs registered by
    // this turn so the post-start finalizer can revoke their global resumable
    // entries before stopping the actor.
    let mut pending_tool_call_ids = HashSet::new();
    let outcome = async {
        if let Some(session_id) = session_id.clone() {
            agent
                .set_session_context(Some(session_id), "chat")
                .map_err(|error| error.to_string())?;
        }

        if let Some(session_id) = session_id.as_deref() {
            send_sse(
                &mut stream,
                &serde_json::json!({
                    "type": "status",
                    "status": "session",
                    "details": { "sessionId": session_id, "runtime": "rust" }
                }),
            )
            .await?;
        }
        send_sse(&mut stream, &serde_json::json!({ "type": "agent_start" })).await?;
        send_sse(&mut stream, &serde_json::json!({ "type": "turn_start" })).await?;

        let prompt_result = agent
            .prompt_with_attachments(prompt, prepared_attachments.paths.clone())
            .await;
        if let Err(error) = prompt_result {
            send_sse(
                &mut stream,
                &serde_json::json!({ "type": "error", "message": error.to_string() }),
            )
            .await?;
            send_sse(&mut stream, &serde_json::json!({ "type": "done" })).await?;
            let _ = stream.shutdown().await;
            return Ok(());
        }
        let mut assistant_text = String::new();
        let mut thinking_text = String::new();
        let mut last_usage = None;
        let mut response_started = false;
        let mut thinking_started = false;
        let mut terminal_sent = false;
        let mut turn_completed_successfully = false;
        let mut tool_names: HashMap<String, String> = HashMap::new();
        let mut assistant_tools: Vec<Value> = Vec::new();
        let mut client_tool_call_ids: HashSet<String> = HashSet::new();

        while let Some(event) = events.recv().await {
            let terminal_status = native_chat_terminal_status(&event);
            let acknowledge_pending_peer_messages = native_chat_acknowledges_peer_messages(&event);
            match event {
                FromAgent::ManagedAuthorizationRequest { .. } => {
                    // This embedded chat path has no authenticated renewal controller.
                    return Err("Managed authorization requires a hosted controller".to_owned());
                }
                FromAgent::Ready { .. }
                | FromAgent::LocalAssistantContent { .. }
                | FromAgent::ConversationSnapshot { .. }
                | FromAgent::ModelChanged { .. }
                | FromAgent::BoostChanged { .. }
                | FromAgent::ModelChangeFailed { .. }
                // Output accounting for Codex-native operations; carries no
                // content for a chat client to render.
                | FromAgent::CodexNativeOperation { .. }
                | FromAgent::CodexNativeDecision { .. }
                | FromAgent::CodexTransportReceipt { .. }
                | FromAgent::SessionInfo { .. } => {}
                FromAgent::ManagedGatewayReceipt {
                    request_id,
                    record_id,
                    lineage_id,
                    record_status,
                    ..
                } => {
                    send_sse(
                        &mut stream,
                        &managed_gateway_receipt_status(
                            request_id,
                            record_id,
                            lineage_id,
                            record_status,
                        ),
                    )
                    .await?;
                }
                FromAgent::CodexSessionState {
                    state,
                    thread_id,
                    profile,
                } => {
                    send_sse(
                        &mut stream,
                        &serde_json::json!({
                            "type": "status",
                            "status": "codex_session_state",
                            "details": {
                                "state": state,
                                "threadId": thread_id,
                                "profile": profile
                            }
                        }),
                    )
                    .await?;
                }
                FromAgent::CodexTurnState {
                    state,
                    thread_id,
                    turn_id,
                } => {
                    send_sse(
                        &mut stream,
                        &serde_json::json!({
                            "type": "status",
                            "status": "codex_turn_state",
                            "details": {
                                "state": state,
                                "threadId": thread_id,
                                "turnId": turn_id
                            }
                        }),
                    )
                    .await?;
                }
                FromAgent::CodexUsageState { source, usage } => {
                    if usage.is_some() {
                        last_usage = usage.clone();
                    }
                    send_sse(
                        &mut stream,
                        &serde_json::json!({
                            "type": "status",
                            "status": "codex_usage_state",
                            "details": {
                                "source": source,
                                "usage": usage
                            }
                        }),
                    )
                    .await?;
                }
                FromAgent::CodexCompatibility {
                    protocol_version,
                    resume,
                    steering,
                } => {
                    send_sse(
                        &mut stream,
                        &serde_json::json!({
                            "type": "status",
                            "status": "codex_compatibility",
                            "details": {
                                "protocolVersion": protocol_version,
                                "resume": resume,
                                "steering": steering
                            }
                        }),
                    )
                    .await?;
                }
                FromAgent::ResponseStart { .. } => {
                    response_started = true;
                    let message = composer_assistant_message(&assistant_text, &thinking_text, None);
                    send_sse(
                        &mut stream,
                        &serde_json::json!({ "type": "message_start", "message": message }),
                    )
                    .await?;
                    send_sse(
                        &mut stream,
                        &serde_json::json!({
                            "type": "message_update",
                            "message": message,
                            "assistantMessageEvent": {
                                "type": "start",
                                "partial": message
                            }
                        }),
                    )
                    .await?;
                }
                FromAgent::ResponseChunk {
                    content,
                    is_thinking,
                    ..
                } => {
                    if !response_started {
                        response_started = true;
                        let message = composer_assistant_message(&assistant_text, &thinking_text, None);
                        send_sse(
                            &mut stream,
                            &serde_json::json!({ "type": "message_start", "message": message }),
                        )
                        .await?;
                    }
                    if is_thinking {
                        if !thinking_started {
                            thinking_started = true;
                            let message =
                                composer_assistant_message(&assistant_text, &thinking_text, None);
                            send_sse(
                                &mut stream,
                                &serde_json::json!({
                                    "type": "message_update",
                                    "message": message,
                                    "assistantMessageEvent": {
                                        "type": "thinking_start",
                                        "contentIndex": 0,
                                        "partial": message
                                    }
                                }),
                            )
                            .await?;
                        }
                        thinking_text.push_str(&content);
                        send_sse(
                            &mut stream,
                            &serde_json::json!({
                                "type": "message_update",
                                "message": composer_assistant_message(&assistant_text, &thinking_text, None),
                                "assistantMessageEvent": {
                                    "type": "thinking_delta",
                                    "contentIndex": 0,
                                    "delta": content
                                }
                            }),
                        )
                        .await?;
                    } else {
                        assistant_text.push_str(&content);
                        send_sse(
                            &mut stream,
                            &serde_json::json!({
                                "type": "message_update",
                                "message": composer_assistant_message(&assistant_text, &thinking_text, None),
                                "assistantMessageEvent": {
                                    "type": "text_delta",
                                    "contentIndex": 0,
                                    "delta": content
                                }
                            }),
                        )
                        .await?;
                    }
                }
                FromAgent::ToolCall {
                    call_id,
                    tool,
                    args,
                    requires_approval,
                    ..
                } => {
                    tool_names.insert(call_id.clone(), tool.clone());
                    record_tool_call_metadata(&mut assistant_tools, &call_id, &tool, args.clone());
                    // Gateway-handled session-messaging tools. The native runner is
                    // blocked on the tool-response channel (these definitions carry
                    // `requires_approval: true`), so answering here with a
                    // `ToolResult` supplies the outcome without the runner ever
                    // trying to execute an unknown tool. Tenancy is enforced inside
                    // the handler under this turn's AuthContext, and the sender is
                    // always this turn's session id, so the model cannot forge a
                    // different `from` session.
                    if is_session_messaging_tool(&tool) {
                        let result = handle_session_messaging_tool_call(
                            &state,
                            &auth,
                            session_id.as_deref(),
                            turn_scope.as_deref(),
                            &call_id,
                            &tool,
                            &args,
                        )
                        .await;
                        send_sse(
                            &mut stream,
                            &serde_json::json!({
                                "type": "tool_execution_start",
                                "toolCallId": call_id,
                                "toolName": tool,
                                "args": args
                            }),
                        )
                        .await?;
                        // `ExecutionSource::RemoteClient` keeps peer-authored text
                        // (peer titles) inside the runner's untrusted-content
                        // envelope. `FromAgent::ToolEnd` closes out the metadata.
                        let _ = agent.tool_response_sender().send((
                            call_id.clone(),
                            true,
                            Some(result),
                            ExecutionSource::RemoteClient,
                            None,
                        ));
                    } else if client_tool_names.contains(&tool.to_lowercase()) {
                        client_tool_call_ids.insert(call_id.clone());
                        pending_tool_call_ids.insert(call_id.clone());
                        state
                            .pending_tool_responses
                            .lock()
                            .await
                            .insert(call_id.clone(), agent.tool_response_sender());
                        if let Some(owner) =
                            PendingToolResponseOwner::for_request(session_id.as_deref(), &auth)
                        {
                            state
                                .pending_tool_response_sessions
                                .lock()
                                .await
                                .insert(call_id.clone(), owner);
                        }
                        send_sse(
                            &mut stream,
                            &serde_json::json!({
                                "type": "tool_execution_start",
                                "toolCallId": call_id,
                                "toolName": tool,
                                "args": args,
                                "clientOwned": true
                            }),
                        )
                        .await?;
                    } else if requires_approval {
                        match approval_mode_for_session(&state, session_id.as_deref())
                            .await
                            .as_str()
                        {
                            "auto" => {
                                let _ = agent.tool_response_sender().send((
                                    call_id.clone(),
                                    true,
                                    None,
                                    ExecutionSource::RemoteClient,
                                    None,
                                ));
                                send_sse(
                                    &mut stream,
                                    &serde_json::json!({
                                        "type": "tool_execution_start",
                                        "toolCallId": call_id,
                                    }),
                                )
                                .await?;
                            }
                            "fail" => {
                                let _ = agent.tool_response_sender().send((
                                    call_id.clone(),
                                    false,
                                    None,
                                    ExecutionSource::RemoteClient,
                                    None,
                                ));
                                finish_tool_metadata(&mut assistant_tools, &call_id, false);
                                send_sse(&mut stream, &approval_blocked_tool_event(&call_id, &tool))
                                    .await?;
                            }
                            _ => {
                                pending_tool_call_ids.insert(call_id.clone());
                                state
                                    .pending_tool_responses
                                    .lock()
                                    .await
                                    .insert(call_id.clone(), agent.tool_response_sender());
                                if let Some(owner) =
                                    PendingToolResponseOwner::for_request(session_id.as_deref(), &auth)
                                {
                                    state
                                        .pending_tool_response_sessions
                                        .lock()
                                        .await
                                        .insert(call_id.clone(), owner);
                                }
                                send_sse(
                                    &mut stream,
                                    &serde_json::json!({
                                        "type": "action_approval_required",
                                        "request": {
                                            "id": call_id,
                                            "toolName": tool,
                                            "args": args,
                                            "reason": "Tool execution requires approval"
                                        }
                                    }),
                                )
                                .await?;
                            }
                        }
                    } else {
                        send_sse(
                            &mut stream,
                            &serde_json::json!({
                                "type": "tool_execution_start",
                                "toolCallId": call_id,
                                "toolName": tool,
                                "args": args
                            }),
                        )
                        .await?;
                    }
                }
                FromAgent::ToolStart { call_id } => {
                    update_tool_metadata_status(&mut assistant_tools, &call_id, "running");
                    let tool = tool_names
                        .get(&call_id)
                        .cloned()
                        .unwrap_or_else(|| "tool".to_string());
                    send_sse(
                        &mut stream,
                        &serde_json::json!({
                            "type": "tool_execution_start",
                            "toolCallId": call_id,
                            "toolName": tool,
                            "args": {}
                        }),
                    )
                    .await?;
                }
                FromAgent::ToolOutput { call_id, content } => {
                    let tool = tool_names
                        .get(&call_id)
                        .cloned()
                        .unwrap_or_else(|| "tool".to_string());
                    send_sse(
                        &mut stream,
                        &serde_json::json!({
                            "type": "tool_execution_update",
                            "toolCallId": call_id,
                            "toolName": tool,
                            "args": {},
                            "partialResult": content
                        }),
                    )
                    .await?;
                }
                FromAgent::ToolEnd {
                    call_id, success, ..
                } => {
                    pending_tool_call_ids.remove(&call_id);
                    state.pending_tool_responses.lock().await.remove(&call_id);
                    state
                        .pending_tool_response_sessions
                        .lock()
                        .await
                        .remove(&call_id);
                    state
                        .completed_client_tool_results
                        .lock()
                        .await
                        .remove(&call_id);
                    finish_tool_metadata(&mut assistant_tools, &call_id, success);
                    let tool = tool_names
                        .remove(&call_id)
                        .unwrap_or_else(|| "tool".to_string());
                    send_sse(
                        &mut stream,
                        &serde_json::json!({
                            "type": "tool_execution_end",
                            "toolCallId": call_id,
                            "toolName": tool,
                            "result": { "success": success },
                            "isError": !success
                        }),
                    )
                    .await?;
                }
                FromAgent::BatchStart { total } => {
                    send_sse(
                        &mut stream,
                        &serde_json::json!({
                            "type": "status",
                            "status": "tool_batch_start",
                            "details": { "total": total }
                        }),
                    )
                    .await?;
                }
                FromAgent::BatchEnd {
                    total,
                    successes,
                    failures,
                } => {
                    send_sse(
                        &mut stream,
                        &serde_json::json!({
                            "type": "tool_batch_summary",
                            "summary": format!("{successes}/{total} tools succeeded"),
                            "summaryLabels": [],
                            "toolCallIds": [],
                            "toolNames": [],
                            "callsSucceeded": successes,
                            "callsFailed": failures
                        }),
                    )
                    .await?;
                }
                FromAgent::Error {
                    message,
                    fatal,
                    terminal,
                    ..
                } => {
                    let request_ended = fatal || terminal;
                    if request_ended {
                        let usage = last_usage.take();
                        if usage.is_some() {
                            record_usage_entry(
                                &state,
                                session_id.as_deref(),
                                &usage_provider,
                                &usage_model,
                                usage.as_ref(),
                            )
                            .await;
                        }
                    }
                    send_sse(
                        &mut stream,
                        &serde_json::json!({ "type": "error", "message": message }),
                    )
                    .await?;
                    if request_ended {
                        send_sse(&mut stream, &serde_json::json!({ "type": "done" })).await?;
                        terminal_sent = true;
                        break;
                    }
                }
                FromAgent::Status { message } => {
                    send_sse(
                        &mut stream,
                        &serde_json::json!({
                            "type": "status",
                            "status": message,
                            "details": {}
                        }),
                    )
                    .await?;
                }
                // Local measurements are consumed by native telemetry, not the chat wire protocol.
                FromAgent::StreamObservation { .. }
                | FromAgent::RequestRetryObservation
                | FromAgent::RequestContextPrepared { .. }
                | FromAgent::OperationObservation { .. }
                | FromAgent::TurnStarted
                | FromAgent::RequestRetryScheduled { .. }
                | FromAgent::ContextCalibration { .. }
                | FromAgent::CompactionMeasured { .. } => {}
                FromAgent::Compaction {
                    summary,
                    first_kept_entry_index,
                    tokens_before,
                    auto,
                    custom_instructions,
                    continuation,
                    timestamp,
                } => {
                    send_sse(
                        &mut stream,
                        &serde_json::json!({
                            "type": "compaction",
                            "summary": summary,
                            "firstKeptEntryIndex": first_kept_entry_index,
                            "tokensBefore": tokens_before,
                            "auto": auto,
                            "customInstructions": custom_instructions,
                            "continuation": continuation,
                            "timestamp": timestamp
                        }),
                    )
                    .await?;
                }
                FromAgent::HookBlocked {
                    call_id,
                    tool,
                    reason,
                } => {
                    pending_tool_call_ids.remove(&call_id);
                    state.pending_tool_responses.lock().await.remove(&call_id);
                    state
                        .pending_tool_response_sessions
                        .lock()
                        .await
                        .remove(&call_id);
                    state
                        .completed_client_tool_results
                        .lock()
                        .await
                        .remove(&call_id);
                    finish_tool_metadata(&mut assistant_tools, &call_id, false);
                    send_sse(
                        &mut stream,
                        &serde_json::json!({
                            "type": "tool_execution_end",
                            "toolCallId": call_id,
                            "toolName": tool,
                            "result": reason,
                            "isError": true
                        }),
                    )
                    .await?;
                }
                FromAgent::SideQuestionStart { .. }
                | FromAgent::SideQuestionChunk { .. }
                | FromAgent::SideQuestionEnd { .. } => {}
                FromAgent::ResponseEnd { usage, .. } => {
                    if usage.is_some() {
                        record_usage_entry(
                            &state,
                            session_id.as_deref(),
                            &usage_provider,
                            &usage_model,
                            usage.as_ref(),
                        )
                        .await;
                        last_usage = usage;
                    }
                    response_started = false;
                    thinking_started = false;
                }
                FromAgent::TurnCompleted { .. } => {
                    debug_assert!(matches!(terminal_status, Some(Ok(()))));
                    let client_tool_results =
                        take_client_tool_results(&state, &client_tool_call_ids).await;
                    finish_client_tool_metadata(&mut assistant_tools, &client_tool_results);
                    let usage = last_usage.take();
                    let message = composer_assistant_message_with_tools(
                        &assistant_text,
                        &thinking_text,
                        usage,
                        &assistant_tools,
                    );
                    record_chat_assistant_message(&state, session_id.as_deref(), message.clone()).await;
                    send_sse(
                        &mut stream,
                        &serde_json::json!({ "type": "message_end", "message": message }),
                    )
                    .await?;
                    send_sse(
                        &mut stream,
                        &serde_json::json!({
                            "type": "turn_end",
                            "message": message,
                            "toolResults": []
                        }),
                    )
                    .await?;
                    send_sse(
                        &mut stream,
                        &serde_json::json!({
                            "type": "agent_end",
                            "messages": [message],
                            "stopReason": "stop"
                        }),
                    )
                    .await?;
                    send_sse(&mut stream, &serde_json::json!({ "type": "done" })).await?;
                    terminal_sent = true;
                    turn_completed_successfully = acknowledge_pending_peer_messages;
                    break;
                }
                FromAgent::TurnInterrupted { reason, .. } => {
                    send_sse(
                        &mut stream,
                        &serde_json::json!({ "type": "error", "message": reason }),
                    )
                    .await?;
                    send_sse(&mut stream, &serde_json::json!({ "type": "done" })).await?;
                    terminal_sent = true;
                    break;
                }
                FromAgent::ProviderError { kind, message } => {
                    send_sse(
                        &mut stream,
                        &serde_json::json!({
                            "type": "error",
                            "message": message,
                            "providerErrorKind": kind,
                        }),
                    )
                    .await?;
                    send_sse(&mut stream, &serde_json::json!({ "type": "done" })).await?;
                    terminal_sent = true;
                    break;
                }
            }
        }

        if !terminal_sent {
            send_sse(
                &mut stream,
                &serde_json::json!({
                    "type": "error",
                    "message": "Agent stream closed before response completed"
                }),
            )
            .await?;
            send_sse(&mut stream, &serde_json::json!({ "type": "done" })).await?;
        }

        if turn_completed_successfully {
            acknowledge_peer_messages(
                &state,
                session_id.as_deref(),
                &auth,
                &pending_peer_message_ids,
            )
            .await;
        }

        let _ = stream.shutdown().await;
        Ok(())
    }
    .await;
    finish_embedded_chat(
        &state,
        &pending_tool_call_ids,
        agent,
        prepared_attachments,
        outcome,
    )
    .await
}

pub(crate) async fn handle_chat_websocket_endpoint(
    mut stream: TcpStream,
    mut initial: Vec<u8>,
    head: RequestHead,
    state: AppState,
) -> Result<(), String> {
    let auth = match authorized_context(&head, &state.config) {
        Ok(auth) => auth,
        Err(response) => {
            stream
                .write_all(&response)
                .await
                .map_err(|error| error.to_string())?;
            let _ = stream.shutdown().await;
            return Ok(());
        }
    };

    if !origin_allowed(&head) {
        stream
            .write_all(&json_response(
                403,
                &serde_json::json!({ "error": "WebSocket origin is not allowed" }),
            ))
            .await
            .map_err(|error| error.to_string())?;
        let _ = stream.shutdown().await;
        return Ok(());
    }

    let Some(key) = head.headers.get("sec-websocket-key") else {
        stream
            .write_all(&json_response(
                400,
                &serde_json::json!({ "error": "Missing Sec-WebSocket-Key" }),
            ))
            .await
            .map_err(|error| error.to_string())?;
        let _ = stream.shutdown().await;
        return Ok(());
    };
    let accept_key = websocket_accept_key(key);
    let handshake = format!(
        "HTTP/1.1 101 Switching Protocols\r\n\
         Upgrade: websocket\r\n\
         Connection: Upgrade\r\n\
         Sec-WebSocket-Accept: {accept_key}\r\n\
         \r\n"
    );
    stream
        .write_all(handshake.as_bytes())
        .await
        .map_err(|error| error.to_string())?;

    let body_start = header_end(&initial)? + 4;
    let mut websocket_buffer = initial.split_off(body_start);
    let request_body = match read_websocket_text_message(&mut stream, &mut websocket_buffer).await {
        Ok(body) => body,
        Err(error) => {
            send_ws_json(
                &mut stream,
                &serde_json::json!({ "type": "error", "message": error }),
            )
            .await?;
            send_ws_json(&mut stream, &serde_json::json!({ "type": "done" })).await?;
            send_ws_close(&mut stream).await?;
            let _ = stream.shutdown().await;
            return Ok(());
        }
    };
    let chat = match serde_json::from_slice::<ChatRequest>(&request_body) {
        Ok(request) => request,
        Err(error) => {
            send_ws_json(
                &mut stream,
                &serde_json::json!({ "type": "error", "message": format!("invalid chat request: {error}") }),
            )
            .await?;
            send_ws_json(&mut stream, &serde_json::json!({ "type": "done" })).await?;
            send_ws_close(&mut stream).await?;
            let _ = stream.shutdown().await;
            return Ok(());
        }
    };
    if let Err(error) = validate_client_tool_names(&chat) {
        send_ws_json(
            &mut stream,
            &serde_json::json!({ "type": "error", "message": error }),
        )
        .await?;
        send_ws_json(&mut stream, &serde_json::json!({ "type": "done" })).await?;
        send_ws_close(&mut stream).await?;
        let _ = stream.shutdown().await;
        return Ok(());
    }

    let Some(latest) = chat.messages.last() else {
        send_ws_json(
            &mut stream,
            &serde_json::json!({ "type": "error", "message": "No messages supplied" }),
        )
        .await?;
        send_ws_json(&mut stream, &serde_json::json!({ "type": "done" })).await?;
        send_ws_close(&mut stream).await?;
        let _ = stream.shutdown().await;
        return Ok(());
    };
    if latest.role != "user" {
        send_ws_json(
            &mut stream,
            &serde_json::json!({ "type": "error", "message": "Last message must be a user message" }),
        )
        .await?;
        send_ws_json(&mut stream, &serde_json::json!({ "type": "done" })).await?;
        send_ws_close(&mut stream).await?;
        let _ = stream.shutdown().await;
        return Ok(());
    }

    if !chat_message_has_input(latest) {
        send_ws_json(
            &mut stream,
            &serde_json::json!({ "type": "error", "message": "User message cannot be empty" }),
        )
        .await?;
        send_ws_json(&mut stream, &serde_json::json!({ "type": "done" })).await?;
        send_ws_close(&mut stream).await?;
        let _ = stream.shutdown().await;
        return Ok(());
    }
    let mut prompt = build_prompt_from_chat(&chat);
    let system_prompt = system_prompt_from_chat(&chat);

    let session_id = chat.session_id.clone();
    let prepared_attachments = match prepare_chat_attachments(&chat, &state.config.cwd).await {
        Ok(attachments) => attachments,
        Err(error) => {
            send_ws_json(
                &mut stream,
                &serde_json::json!({ "type": "error", "message": error }),
            )
            .await?;
            send_ws_json(&mut stream, &serde_json::json!({ "type": "done" })).await?;
            send_ws_close(&mut stream).await?;
            let _ = stream.shutdown().await;
            return Ok(());
        }
    };
    let turn_scope = match record_chat_user_message(&state, &chat, &auth).await {
        Ok(turn_scope) => turn_scope,
        Err(error) => {
            cleanup_prepared_attachments(prepared_attachments).await;
            send_ws_json(
                &mut stream,
                &serde_json::json!({ "type": "error", "message": error }),
            )
            .await?;
            send_ws_json(&mut stream, &serde_json::json!({ "type": "done" })).await?;
            send_ws_close(&mut stream).await?;
            let _ = stream.shutdown().await;
            return Ok(());
        }
    };
    let pending_peer_message_ids =
        prepend_pending_peer_messages(&state, session_id.as_deref(), &auth, &mut prompt).await;

    let model = selected_chat_model(&chat, &state).await;
    if let Some(codex_model) = codex_app_server_model_id(&model) {
        if let Some(session_id) = session_id.as_deref() {
            send_ws_json(
                &mut stream,
                &serde_json::json!({
                    "type": "status",
                    "status": "session",
                    "details": { "sessionId": session_id, "runtime": "rust-codex-app-server" }
                }),
            )
            .await?;
        }
        let prompt_succeeded = handle_codex_app_server_chat_ws(
            &mut stream,
            &state,
            session_id.as_deref(),
            &codex_model,
            &prompt,
            &prepared_attachments.paths,
        )
        .await?;
        if prompt_succeeded {
            acknowledge_peer_messages(
                &state,
                session_id.as_deref(),
                &auth,
                &pending_peer_message_ids,
            )
            .await;
        }
        send_ws_close(&mut stream).await?;
        let _ = stream.shutdown().await;
        cleanup_prepared_attachments(prepared_attachments).await;
        return Ok(());
    }
    let (usage_provider, usage_model) = usage_provider_model(&chat, &state, &model).await;
    let (mut client_tools, client_tool_names) = client_tool_definitions(&chat);
    // Gateway-handled session-messaging tools give an agent turn the same reach
    // the loopback endpoints give a UI client. They are answered inline below
    // through the tool-response channel, under this turn's AuthContext.
    client_tools.extend(session_messaging_tool_definitions());
    let thinking_enabled = chat
        .thinking_level
        .as_deref()
        .map(|level| !matches!(level, "off" | "none" | "disabled"))
        .unwrap_or(false);
    let config = NativeAgentConfig {
        model,
        external_tool_schema_policy: maestro_local_host::agent::ExternalToolSchemaPolicy::Deferred,
        cwd: state.config.cwd.to_string_lossy().to_string(),
        system_prompt,
        thinking_enabled,
        thinking_budget: env::var("MAESTRO_THINKING_BUDGET")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(10_000),
        ..NativeAgentConfig::default()
    };
    let (agent, mut events) = match EmbeddedAgentBuilder::from_config(config)
        .external_tools(client_tools)
        .start()
    {
        Ok(session) => session.into_parts(),
        Err(error) => {
            send_ws_json(
                &mut stream,
                &serde_json::json!({ "type": "error", "message": error.to_string() }),
            )
            .await?;
            send_ws_json(&mut stream, &serde_json::json!({ "type": "done" })).await?;
            send_ws_close(&mut stream).await?;
            let _ = stream.shutdown().await;
            cleanup_prepared_attachments(prepared_attachments).await;
            return Ok(());
        }
    };

    // See the SSE handler above. Both transports must revoke only the
    // registrations created by this turn before its actor is cancelled.
    let mut pending_tool_call_ids = HashSet::new();
    let outcome = async {
        if let Some(session_id) = session_id.clone() {
            agent
                .set_session_context(Some(session_id), "chat")
                .map_err(|error| error.to_string())?;
        }

        send_ws_json(&mut stream, &serde_json::json!({ "type": "agent_start" })).await?;
        send_ws_json(&mut stream, &serde_json::json!({ "type": "turn_start" })).await?;

        if let Err(error) = agent
            .prompt_with_attachments(prompt, prepared_attachments.paths.clone())
            .await
        {
            send_ws_json(
                &mut stream,
                &serde_json::json!({ "type": "error", "message": error.to_string() }),
            )
            .await?;
            send_ws_json(&mut stream, &serde_json::json!({ "type": "done" })).await?;
            send_ws_close(&mut stream).await?;
            let _ = stream.shutdown().await;
            return Ok(());
        }
        let mut assistant_text = String::new();
        let mut thinking_text = String::new();
        let mut last_usage = None;
        let mut response_started = false;
        let mut thinking_started = false;
        let mut terminal_sent = false;
        let mut turn_completed_successfully = false;
        let mut tool_names: HashMap<String, String> = HashMap::new();
        let mut assistant_tools: Vec<Value> = Vec::new();
        let mut client_tool_call_ids: HashSet<String> = HashSet::new();

        while let Some(event) = events.recv().await {
            let terminal_status = native_chat_terminal_status(&event);
            let acknowledge_pending_peer_messages = native_chat_acknowledges_peer_messages(&event);
            match event {
                FromAgent::ManagedAuthorizationRequest { .. } => {
                    // This embedded chat path has no authenticated renewal controller.
                    return Err("Managed authorization requires a hosted controller".to_owned());
                }
                FromAgent::Ready { .. }
                | FromAgent::LocalAssistantContent { .. }
                | FromAgent::ConversationSnapshot { .. }
                | FromAgent::ModelChanged { .. }
                | FromAgent::BoostChanged { .. }
                | FromAgent::ModelChangeFailed { .. }
                // Output accounting for Codex-native operations; carries no
                // content for a chat client to render.
                | FromAgent::CodexNativeOperation { .. }
                | FromAgent::CodexNativeDecision { .. }
                | FromAgent::CodexTransportReceipt { .. }
                | FromAgent::SessionInfo { .. } => {}
                FromAgent::ManagedGatewayReceipt {
                    request_id,
                    record_id,
                    lineage_id,
                    record_status,
                    ..
                } => {
                    send_ws_json(
                        &mut stream,
                        &managed_gateway_receipt_status(
                            request_id,
                            record_id,
                            lineage_id,
                            record_status,
                        ),
                    )
                    .await?;
                }
                FromAgent::CodexSessionState {
                    state,
                    thread_id,
                    profile,
                } => {
                    send_ws_json(
                        &mut stream,
                        &serde_json::json!({
                            "type": "status",
                            "status": "codex_session_state",
                            "details": {
                                "state": state,
                                "threadId": thread_id,
                                "profile": profile
                            }
                        }),
                    )
                    .await?;
                }
                FromAgent::CodexTurnState {
                    state,
                    thread_id,
                    turn_id,
                } => {
                    send_ws_json(
                        &mut stream,
                        &serde_json::json!({
                            "type": "status",
                            "status": "codex_turn_state",
                            "details": {
                                "state": state,
                                "threadId": thread_id,
                                "turnId": turn_id
                            }
                        }),
                    )
                    .await?;
                }
                FromAgent::CodexUsageState { source, usage } => {
                    if usage.is_some() {
                        last_usage = usage.clone();
                    }
                    send_ws_json(
                        &mut stream,
                        &serde_json::json!({
                            "type": "status",
                            "status": "codex_usage_state",
                            "details": {
                                "source": source,
                                "usage": usage
                            }
                        }),
                    )
                    .await?;
                }
                FromAgent::CodexCompatibility {
                    protocol_version,
                    resume,
                    steering,
                } => {
                    send_ws_json(
                        &mut stream,
                        &serde_json::json!({
                            "type": "status",
                            "status": "codex_compatibility",
                            "details": {
                                "protocolVersion": protocol_version,
                                "resume": resume,
                                "steering": steering
                            }
                        }),
                    )
                    .await?;
                }
                FromAgent::ResponseStart { .. } => {
                    response_started = true;
                    let message = composer_assistant_message(&assistant_text, &thinking_text, None);
                    send_ws_json(
                        &mut stream,
                        &serde_json::json!({
                            "type": "message_update",
                            "message": message,
                            "assistantMessageEvent": { "type": "start", "partial": message }
                        }),
                    )
                    .await?;
                }
                FromAgent::ResponseChunk {
                    content,
                    is_thinking,
                    ..
                } => {
                    if !response_started {
                        response_started = true;
                    }
                    if is_thinking {
                        if !thinking_started {
                            thinking_started = true;
                            let message =
                                composer_assistant_message(&assistant_text, &thinking_text, None);
                            send_ws_json(
                                &mut stream,
                                &serde_json::json!({
                                    "type": "message_update",
                                    "message": message,
                                    "assistantMessageEvent": {
                                        "type": "thinking_start",
                                        "contentIndex": 0,
                                        "partial": message
                                    }
                                }),
                            )
                            .await?;
                        }
                        thinking_text.push_str(&content);
                        send_ws_json(
                            &mut stream,
                            &serde_json::json!({
                                "type": "message_update",
                                "message": composer_assistant_message(&assistant_text, &thinking_text, None),
                                "assistantMessageEvent": {
                                    "type": "thinking_delta",
                                    "contentIndex": 0,
                                    "delta": content
                                }
                            }),
                        )
                        .await?;
                    } else {
                        assistant_text.push_str(&content);
                        send_ws_json(
                            &mut stream,
                            &serde_json::json!({
                                "type": "message_update",
                                "message": composer_assistant_message(&assistant_text, &thinking_text, None),
                                "assistantMessageEvent": {
                                    "type": "text_delta",
                                    "contentIndex": 0,
                                    "delta": content
                                }
                            }),
                        )
                        .await?;
                    }
                }
                FromAgent::ToolCall {
                    call_id,
                    tool,
                    args,
                    requires_approval,
                    ..
                } => {
                    tool_names.insert(call_id.clone(), tool.clone());
                    record_tool_call_metadata(&mut assistant_tools, &call_id, &tool, args.clone());
                    // Gateway-handled session-messaging tools. The native runner is
                    // blocked on the tool-response channel (these definitions carry
                    // `requires_approval: true`), so answering here with a
                    // `ToolResult` supplies the outcome without the runner ever
                    // trying to execute an unknown tool. Tenancy is enforced inside
                    // the handler under this turn's AuthContext, and the sender is
                    // always this turn's session id, so the model cannot forge a
                    // different `from` session.
                    if is_session_messaging_tool(&tool) {
                        let result = handle_session_messaging_tool_call(
                            &state,
                            &auth,
                            session_id.as_deref(),
                            turn_scope.as_deref(),
                            &call_id,
                            &tool,
                            &args,
                        )
                        .await;
                        send_ws_json(
                            &mut stream,
                            &serde_json::json!({
                                "type": "tool_execution_start",
                                "toolCallId": call_id,
                                "toolName": tool,
                                "args": args
                            }),
                        )
                        .await?;
                        // `ExecutionSource::RemoteClient` keeps peer-authored text
                        // (peer titles) inside the runner's untrusted-content
                        // envelope. `FromAgent::ToolEnd` closes out the metadata.
                        let _ = agent.tool_response_sender().send((
                            call_id.clone(),
                            true,
                            Some(result),
                            ExecutionSource::RemoteClient,
                            None,
                        ));
                    } else if client_tool_names.contains(&tool.to_lowercase()) {
                        client_tool_call_ids.insert(call_id.clone());
                        pending_tool_call_ids.insert(call_id.clone());
                        state
                            .pending_tool_responses
                            .lock()
                            .await
                            .insert(call_id.clone(), agent.tool_response_sender());
                        if let Some(owner) =
                            PendingToolResponseOwner::for_request(session_id.as_deref(), &auth)
                        {
                            state
                                .pending_tool_response_sessions
                                .lock()
                                .await
                                .insert(call_id.clone(), owner);
                        }
                        send_ws_json(
                            &mut stream,
                            &serde_json::json!({
                                "type": "tool_execution_start",
                                "toolCallId": call_id,
                                "toolName": tool,
                                "args": args,
                                "clientOwned": true
                            }),
                        )
                        .await?;
                    } else if requires_approval {
                        match approval_mode_for_session(&state, session_id.as_deref())
                            .await
                            .as_str()
                        {
                            "auto" => {
                                let _ = agent.tool_response_sender().send((
                                    call_id.clone(),
                                    true,
                                    None,
                                    ExecutionSource::RemoteClient,
                                    None,
                                ));
                                send_ws_json(
                                    &mut stream,
                                    &serde_json::json!({
                                        "type": "tool_execution_start",
                                        "toolCallId": call_id,
                                    }),
                                )
                                .await?;
                            }
                            "fail" => {
                                let _ = agent.tool_response_sender().send((
                                    call_id.clone(),
                                    false,
                                    None,
                                    ExecutionSource::RemoteClient,
                                    None,
                                ));
                                finish_tool_metadata(&mut assistant_tools, &call_id, false);
                                send_ws_json(
                                    &mut stream,
                                    &approval_blocked_tool_event(&call_id, &tool),
                                )
                                .await?;
                            }
                            _ => {
                                pending_tool_call_ids.insert(call_id.clone());
                                state
                                    .pending_tool_responses
                                    .lock()
                                    .await
                                    .insert(call_id.clone(), agent.tool_response_sender());
                                if let Some(owner) =
                                    PendingToolResponseOwner::for_request(session_id.as_deref(), &auth)
                                {
                                    state
                                        .pending_tool_response_sessions
                                        .lock()
                                        .await
                                        .insert(call_id.clone(), owner);
                                }
                                send_ws_json(
                                    &mut stream,
                                    &serde_json::json!({
                                        "type": "action_approval_required",
                                        "request": {
                                            "id": call_id,
                                            "toolName": tool,
                                            "args": args,
                                            "reason": "Tool execution requires approval"
                                        }
                                    }),
                                )
                                .await?;
                            }
                        }
                    } else {
                        send_ws_json(
                            &mut stream,
                            &serde_json::json!({
                                "type": "tool_execution_start",
                                "toolCallId": call_id,
                                "toolName": tool,
                                "args": args
                            }),
                        )
                        .await?;
                    }
                }
                FromAgent::ToolStart { call_id } => {
                    update_tool_metadata_status(&mut assistant_tools, &call_id, "running");
                    let tool = tool_names
                        .get(&call_id)
                        .cloned()
                        .unwrap_or_else(|| "tool".to_string());
                    send_ws_json(
                        &mut stream,
                        &serde_json::json!({
                            "type": "tool_execution_start",
                            "toolCallId": call_id,
                            "toolName": tool,
                            "args": {}
                        }),
                    )
                    .await?;
                }
                FromAgent::ToolOutput { call_id, content } => {
                    let tool = tool_names
                        .get(&call_id)
                        .cloned()
                        .unwrap_or_else(|| "tool".to_string());
                    send_ws_json(
                        &mut stream,
                        &serde_json::json!({
                            "type": "tool_execution_update",
                            "toolCallId": call_id,
                            "toolName": tool,
                            "args": {},
                            "partialResult": content
                        }),
                    )
                    .await?;
                }
                FromAgent::ToolEnd {
                    call_id, success, ..
                } => {
                    pending_tool_call_ids.remove(&call_id);
                    state.pending_tool_responses.lock().await.remove(&call_id);
                    state
                        .pending_tool_response_sessions
                        .lock()
                        .await
                        .remove(&call_id);
                    state
                        .completed_client_tool_results
                        .lock()
                        .await
                        .remove(&call_id);
                    finish_tool_metadata(&mut assistant_tools, &call_id, success);
                    let tool = tool_names
                        .remove(&call_id)
                        .unwrap_or_else(|| "tool".to_string());
                    send_ws_json(
                        &mut stream,
                        &serde_json::json!({
                            "type": "tool_execution_end",
                            "toolCallId": call_id,
                            "toolName": tool,
                            "result": { "success": success },
                            "isError": !success
                        }),
                    )
                    .await?;
                }
                FromAgent::BatchStart { total } => {
                    send_ws_json(
                        &mut stream,
                        &serde_json::json!({
                            "type": "status",
                            "status": "tool_batch_start",
                            "details": { "total": total }
                        }),
                    )
                    .await?;
                }
                FromAgent::BatchEnd {
                    total,
                    successes,
                    failures,
                } => {
                    send_ws_json(
                        &mut stream,
                        &serde_json::json!({
                            "type": "tool_batch_summary",
                            "summary": format!("{successes}/{total} tools succeeded"),
                            "summaryLabels": [],
                            "toolCallIds": [],
                            "toolNames": [],
                            "callsSucceeded": successes,
                            "callsFailed": failures
                        }),
                    )
                    .await?;
                }
                FromAgent::Error {
                    message,
                    fatal,
                    terminal,
                    ..
                } => {
                    let request_ended = fatal || terminal;
                    if request_ended {
                        let usage = last_usage.take();
                        if usage.is_some() {
                            record_usage_entry(
                                &state,
                                session_id.as_deref(),
                                &usage_provider,
                                &usage_model,
                                usage.as_ref(),
                            )
                            .await;
                        }
                    }
                    send_ws_json(
                        &mut stream,
                        &serde_json::json!({ "type": "error", "message": message }),
                    )
                    .await?;
                    if request_ended {
                        send_ws_json(&mut stream, &serde_json::json!({ "type": "done" })).await?;
                        terminal_sent = true;
                        break;
                    }
                }
                FromAgent::Status { message } => {
                    send_ws_json(
                        &mut stream,
                        &serde_json::json!({
                            "type": "status",
                            "status": message,
                            "details": {}
                        }),
                    )
                    .await?;
                }
                // Local measurements are consumed by native telemetry, not the chat wire protocol.
                FromAgent::StreamObservation { .. }
                | FromAgent::RequestRetryObservation
                | FromAgent::RequestContextPrepared { .. }
                | FromAgent::OperationObservation { .. }
                | FromAgent::TurnStarted
                | FromAgent::RequestRetryScheduled { .. }
                | FromAgent::ContextCalibration { .. }
                | FromAgent::CompactionMeasured { .. } => {}
                FromAgent::Compaction {
                    summary,
                    first_kept_entry_index,
                    tokens_before,
                    auto,
                    custom_instructions,
                    continuation,
                    timestamp,
                } => {
                    send_ws_json(
                        &mut stream,
                        &serde_json::json!({
                            "type": "compaction",
                            "summary": summary,
                            "firstKeptEntryIndex": first_kept_entry_index,
                            "tokensBefore": tokens_before,
                            "auto": auto,
                            "customInstructions": custom_instructions,
                            "continuation": continuation,
                            "timestamp": timestamp
                        }),
                    )
                    .await?;
                }
                FromAgent::HookBlocked {
                    call_id,
                    tool,
                    reason,
                } => {
                    pending_tool_call_ids.remove(&call_id);
                    state.pending_tool_responses.lock().await.remove(&call_id);
                    state
                        .pending_tool_response_sessions
                        .lock()
                        .await
                        .remove(&call_id);
                    state
                        .completed_client_tool_results
                        .lock()
                        .await
                        .remove(&call_id);
                    finish_tool_metadata(&mut assistant_tools, &call_id, false);
                    send_ws_json(
                        &mut stream,
                        &serde_json::json!({
                            "type": "tool_execution_end",
                            "toolCallId": call_id,
                            "toolName": tool,
                            "result": reason,
                            "isError": true
                        }),
                    )
                    .await?;
                }
                FromAgent::SideQuestionStart { .. }
                | FromAgent::SideQuestionChunk { .. }
                | FromAgent::SideQuestionEnd { .. } => {}
                FromAgent::ResponseEnd { usage, .. } => {
                    if usage.is_some() {
                        record_usage_entry(
                            &state,
                            session_id.as_deref(),
                            &usage_provider,
                            &usage_model,
                            usage.as_ref(),
                        )
                        .await;
                        last_usage = usage;
                    }
                    response_started = false;
                    thinking_started = false;
                }
                FromAgent::TurnCompleted { .. } => {
                    debug_assert!(matches!(terminal_status, Some(Ok(()))));
                    let client_tool_results =
                        take_client_tool_results(&state, &client_tool_call_ids).await;
                    finish_client_tool_metadata(&mut assistant_tools, &client_tool_results);
                    let usage = last_usage.take();
                    let message = composer_assistant_message_with_tools(
                        &assistant_text,
                        &thinking_text,
                        usage,
                        &assistant_tools,
                    );
                    record_chat_assistant_message(&state, session_id.as_deref(), message.clone()).await;
                    send_ws_json(
                        &mut stream,
                        &serde_json::json!({ "type": "message_end", "message": message }),
                    )
                    .await?;
                    send_ws_json(
                        &mut stream,
                        &serde_json::json!({
                            "type": "agent_end",
                            "messages": [message],
                            "stopReason": "stop"
                        }),
                    )
                    .await?;
                    send_ws_json(&mut stream, &serde_json::json!({ "type": "done" })).await?;
                    terminal_sent = true;
                    turn_completed_successfully = acknowledge_pending_peer_messages;
                    break;
                }
                FromAgent::TurnInterrupted { reason, .. } => {
                    send_ws_json(
                        &mut stream,
                        &serde_json::json!({ "type": "error", "message": reason }),
                    )
                    .await?;
                    send_ws_json(&mut stream, &serde_json::json!({ "type": "done" })).await?;
                    terminal_sent = true;
                    break;
                }
                FromAgent::ProviderError { kind, message } => {
                    send_ws_json(
                        &mut stream,
                        &serde_json::json!({
                            "type": "error",
                            "message": message,
                            "providerErrorKind": kind,
                        }),
                    )
                    .await?;
                    send_ws_json(&mut stream, &serde_json::json!({ "type": "done" })).await?;
                    terminal_sent = true;
                    break;
                }
            }
        }

        if !terminal_sent {
            send_ws_json(
                &mut stream,
                &serde_json::json!({
                    "type": "error",
                    "message": "Agent stream closed before response completed"
                }),
            )
            .await?;
            send_ws_json(&mut stream, &serde_json::json!({ "type": "done" })).await?;
        }

        if turn_completed_successfully {
            acknowledge_peer_messages(
                &state,
                session_id.as_deref(),
                &auth,
                &pending_peer_message_ids,
            )
            .await;
        }

        send_ws_close(&mut stream).await?;
        let _ = stream.shutdown().await;
        Ok(())
    }
    .await;
    finish_embedded_chat(
        &state,
        &pending_tool_call_ids,
        agent,
        prepared_attachments,
        outcome,
    )
    .await
}

// Run the cleanup barrier on successful completion and every post-start error,
// including a failed SSE/WebSocket write. Attachment files remain available to
// the actor until its active work and background processes have stopped.
async fn finish_embedded_chat(
    state: &AppState,
    pending_tool_call_ids: &HashSet<String>,
    agent: EmbeddedAgent,
    attachments: PreparedAttachments,
    outcome: Result<(), String>,
) -> Result<(), String> {
    clear_pending_tool_response_entries(state, pending_tool_call_ids).await;
    finish_embedded_agent(agent, attachments, outcome).await
}

async fn finish_embedded_agent(
    agent: EmbeddedAgent,
    attachments: PreparedAttachments,
    outcome: Result<(), String>,
) -> Result<(), String> {
    agent.shutdown().await;
    cleanup_prepared_attachments(attachments).await;
    outcome
}

async fn clear_pending_tool_response_entries(
    state: &AppState,
    pending_tool_call_ids: &HashSet<String>,
) {
    clear_pending_tool_response_maps(
        state.pending_tool_responses.as_ref(),
        state.pending_tool_response_sessions.as_ref(),
        state.completed_client_tool_results.as_ref(),
        pending_tool_call_ids,
    )
    .await;
}

async fn clear_pending_tool_response_maps(
    pending_tool_responses: &Mutex<HashMap<String, PendingToolResponseSender>>,
    pending_tool_response_sessions: &Mutex<HashMap<String, PendingToolResponseOwner>>,
    completed_client_tool_results: &Mutex<HashMap<String, bool>>,
    pending_tool_call_ids: &HashSet<String>,
) {
    if pending_tool_call_ids.is_empty() {
        return;
    }

    let mut pending = pending_tool_responses.lock().await;
    for call_id in pending_tool_call_ids {
        pending.remove(call_id);
    }
    drop(pending);

    let mut owners = pending_tool_response_sessions.lock().await;
    for call_id in pending_tool_call_ids {
        owners.remove(call_id);
    }
    drop(owners);

    let mut completed = completed_client_tool_results.lock().await;
    for call_id in pending_tool_call_ids {
        completed.remove(call_id);
    }
}

pub(crate) async fn prepare_chat_attachments(
    chat: &ChatRequest,
    cwd: &Path,
) -> Result<PreparedAttachments, String> {
    let Some(latest) = chat.messages.last() else {
        return Ok(PreparedAttachments {
            paths: Vec::new(),
            temp_dir: None,
        });
    };
    let mut temp_dir: Option<PathBuf> = None;
    let mut paths = Vec::new();

    for (index, attachment) in latest.attachments.iter().enumerate() {
        let Some(content) = attachment
            .content
            .as_deref()
            .map(str::trim)
            .filter(|content| !content.is_empty())
        else {
            continue;
        };
        let encoded = strip_data_url_prefix(content);
        let bytes = BASE64_STANDARD.decode(encoded).map_err(|error| {
            format!(
                "attachment {} content is not valid base64: {error}",
                attachment.file_name.as_deref().unwrap_or("attachment")
            )
        })?;

        if temp_dir.is_none() {
            let dir = chat_attachment_temp_dir(cwd);
            tokio::fs::create_dir_all(&dir)
                .await
                .map_err(|error| format!("failed to create attachment temp directory: {error}"))?;
            temp_dir = Some(dir);
        }
        let file_name =
            sanitize_attachment_file_name(attachment.file_name.as_deref().unwrap_or("attachment"));
        let path = temp_dir
            .as_ref()
            .expect("attachment temp dir should be initialized")
            .join(format!("{index}-{file_name}"));
        tokio::fs::write(&path, bytes)
            .await
            .map_err(|error| format!("failed to write attachment {file_name}: {error}"))?;
        paths.push(path.to_string_lossy().to_string());
    }

    Ok(PreparedAttachments { paths, temp_dir })
}

pub(crate) fn strip_data_url_prefix(content: &str) -> &str {
    content
        .split_once(',')
        .filter(|(prefix, _)| prefix.starts_with("data:"))
        .map(|(_, data)| data.trim())
        .unwrap_or(content)
}

fn chat_attachment_temp_dir(cwd: &Path) -> PathBuf {
    sandbox_visible_temp_dir(cwd, "maestro-chat", &ATTACHMENT_TEMP_COUNTER)
}

fn sanitize_attachment_file_name(name: &str) -> String {
    let leaf = name
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or("attachment")
        .trim();
    let sanitized: String = leaf
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_') {
                ch
            } else {
                '_'
            }
        })
        .collect();
    let sanitized = sanitized.trim_matches('_');
    if sanitized.is_empty() {
        "attachment".to_string()
    } else {
        sanitized.chars().take(120).collect()
    }
}

async fn cleanup_prepared_attachments(mut attachments: PreparedAttachments) {
    if let Some(temp_dir) = attachments.temp_dir.take() {
        let _ = tokio::fs::remove_dir_all(temp_dir).await;
    }
}

pub(crate) fn build_prompt_from_chat(chat: &ChatRequest) -> String {
    let mut parts = Vec::new();
    if chat.messages.len() > 1 {
        let history: Vec<Value> = chat.messages[..chat.messages.len() - 1]
            .iter()
            .filter(|message| message.role != "system")
            .map(chat_message_prompt_value)
            .collect();
        if !history.is_empty() {
            let rendered =
                serde_json::to_string_pretty(&history).expect("chat history should serialize");
            parts.push(format!(
                "Conversation so far (structured JSON, preserving content blocks and tool metadata):\n{rendered}"
            ));
            parts.push("Current user message:".to_string());
        }
    }

    if let Some(latest) = chat.messages.last() {
        let rendered = serde_json::to_string_pretty(&chat_message_prompt_value(latest))
            .expect("chat message should serialize");
        parts.push(rendered);
        let attachment_notes: Vec<String> =
            latest.attachments.iter().map(attachment_note).collect();
        if !attachment_notes.is_empty() {
            parts.push(attachment_notes.join("\n\n"));
        }
    }

    parts.join("\n\n")
}

pub(crate) fn system_prompt_from_chat(chat: &ChatRequest) -> Option<String> {
    let prompts: Vec<String> = chat
        .messages
        .iter()
        .filter(|message| message.role == "system")
        .filter_map(|message| {
            let text = composer_text_content(&message.content);
            (!text.trim().is_empty()).then(|| text.trim().to_string())
        })
        .collect();

    (!prompts.is_empty()).then(|| prompts.join("\n\n"))
}

pub(crate) fn chat_message_prompt_value(message: &ChatMessage) -> Value {
    let mut object = Map::new();
    object.insert("role".to_string(), Value::String(message.role.clone()));
    object.insert("content".to_string(), message.content.clone());
    if !message.attachments.is_empty() {
        object.insert(
            "attachments".to_string(),
            serde_json::json!(message.attachments),
        );
    }
    for (key, value) in &message.extra {
        object.insert(key.clone(), value.clone());
    }
    Value::Object(object)
}

pub(crate) fn chat_message_has_input(message: &ChatMessage) -> bool {
    !composer_text_content(&message.content).trim().is_empty() || !message.attachments.is_empty()
}

fn attachment_note(attachment: &ChatAttachment) -> String {
    let name = attachment.file_name.as_deref().unwrap_or("attachment");
    if let Some(text) = attachment
        .extracted_text
        .as_deref()
        .map(str::trim)
        .filter(|text| !text.is_empty())
    {
        return format!("Attachment {name}:\n{text}");
    }

    let mime = attachment
        .mime_type
        .as_deref()
        .filter(|mime| !mime.trim().is_empty())
        .unwrap_or("unknown type");
    let kind = attachment
        .attachment_type
        .as_deref()
        .filter(|kind| !kind.trim().is_empty())
        .unwrap_or("file");
    let id = attachment
        .id
        .as_deref()
        .filter(|id| !id.trim().is_empty())
        .map(|id| format!(" id={id}"))
        .unwrap_or_default();
    if attachment
        .content
        .as_deref()
        .is_some_and(|content| !content.trim().is_empty())
    {
        format!("Attachment {name}{id} ({kind}, {mime}) is attached for model input.")
    } else if attachment.content_omitted.unwrap_or(false) {
        format!(
            "Attachment {name}{id} ({kind}, {mime}) was referenced, but its content was omitted."
        )
    } else {
        format!("Attachment {name}{id} ({kind}, {mime}) was referenced.")
    }
}

pub(crate) fn composer_text_content(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .map(|block| {
                if let Some(object) = block.as_object() {
                    if object.get("type").and_then(Value::as_str) == Some("text") {
                        return object
                            .get("text")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string();
                    }
                }
                block.to_string()
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

pub(crate) fn composer_assistant_message(
    content: &str,
    thinking: &str,
    usage: Option<TokenUsage>,
) -> Value {
    composer_assistant_message_with_tools(content, thinking, usage, &[])
}

pub(crate) fn composer_assistant_message_with_tools(
    content: &str,
    thinking: &str,
    usage: Option<TokenUsage>,
    tools: &[Value],
) -> Value {
    let mut message = serde_json::json!({
        "role": "assistant",
        "content": content,
        "timestamp": now_rfc3339()
    });
    if !thinking.is_empty() {
        message["thinking"] = Value::String(thinking.to_string());
    }
    if let Some(usage) = usage {
        message["usage"] = serde_json::json!({
            "input": usage.input_tokens,
            "output": usage.output_tokens,
            "cacheRead": usage.cache_read_tokens,
            "cacheWrite": usage.cache_write_tokens,
            "cost": {
                "input": 0.0,
                "output": 0.0,
                "cacheRead": 0.0,
                "cacheWrite": 0.0,
                "total": usage.cost.unwrap_or(0.0)
            }
        });
    }
    if !tools.is_empty() {
        message["tools"] = Value::Array(tools.to_vec());
    }
    message
}

pub(crate) fn record_tool_call_metadata(
    tools: &mut Vec<Value>,
    call_id: &str,
    name: &str,
    args: Value,
) {
    tools.push(serde_json::json!({
        "id": call_id,
        "name": name,
        "args": args,
        "status": "pending"
    }));
}

pub(crate) fn update_tool_metadata_status(tools: &mut [Value], call_id: &str, status: &str) {
    if let Some(tool) = tools
        .iter_mut()
        .find(|tool| tool.get("id").and_then(Value::as_str) == Some(call_id))
    {
        tool["status"] = Value::String(status.to_string());
    }
}

pub(crate) fn finish_tool_metadata(tools: &mut [Value], call_id: &str, success: bool) {
    if let Some(tool) = tools
        .iter_mut()
        .find(|tool| tool.get("id").and_then(Value::as_str) == Some(call_id))
    {
        tool["status"] = Value::String(if success { "completed" } else { "error" }.to_string());
        tool["result"] = serde_json::json!({
            "success": success,
            "isError": !success
        });
    }
}

pub(crate) fn finish_client_tool_metadata(
    tools: &mut [Value],
    client_tool_results: &HashMap<String, bool>,
) {
    for (call_id, success) in client_tool_results {
        finish_tool_metadata(tools, call_id, *success);
    }
}

async fn take_client_tool_results(
    state: &AppState,
    client_tool_call_ids: &HashSet<String>,
) -> HashMap<String, bool> {
    let mut completed = state.completed_client_tool_results.lock().await;
    client_tool_call_ids
        .iter()
        .filter_map(|call_id| {
            completed
                .remove(call_id)
                .map(|success| (call_id.clone(), success))
        })
        .collect()
}

pub(crate) fn approval_blocked_tool_event(call_id: &str, tool_name: &str) -> Value {
    serde_json::json!({
        "type": "tool_execution_end",
        "toolCallId": call_id,
        "toolName": tool_name,
        "result": {
            "content": [
                {
                    "type": "text",
                    "text": "Tool execution blocked by approval mode"
                }
            ],
            "isError": true,
            "timestamp": now_rfc3339()
        },
        "isError": true
    })
}

pub(crate) async fn send_sse(stream: &mut TcpStream, value: &Value) -> Result<(), String> {
    let body = serde_json::to_string(value).map_err(|error| error.to_string())?;
    stream
        .write_all(format!("data: {body}\n\n").as_bytes())
        .await
        .map_err(|error| error.to_string())
}

pub(crate) fn websocket_accept_key(key: &str) -> String {
    let mut hasher = Sha1::new();
    hasher.update(key.as_bytes());
    hasher.update(b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11");
    BASE64_STANDARD.encode(hasher.finalize())
}

pub(crate) async fn send_ws_json(stream: &mut TcpStream, value: &Value) -> Result<(), String> {
    let body = serde_json::to_vec(value).map_err(|error| error.to_string())?;
    write_ws_text_frame(stream, &body).await
}

async fn write_ws_text_frame(stream: &mut TcpStream, payload: &[u8]) -> Result<(), String> {
    let mut frame = Vec::with_capacity(payload.len() + 10);
    frame.push(0x81);
    if payload.len() < 126 {
        frame.push(payload.len() as u8);
    } else if payload.len() <= u16::MAX as usize {
        frame.push(126);
        frame.extend_from_slice(&(payload.len() as u16).to_be_bytes());
    } else {
        frame.push(127);
        frame.extend_from_slice(&(payload.len() as u64).to_be_bytes());
    }
    frame.extend_from_slice(payload);
    stream
        .write_all(&frame)
        .await
        .map_err(|error| error.to_string())
}

async fn send_ws_close(stream: &mut TcpStream) -> Result<(), String> {
    stream
        .write_all(&[0x88, 0x00])
        .await
        .map_err(|error| error.to_string())
}

async fn read_websocket_text_message(
    stream: &mut TcpStream,
    buffer: &mut Vec<u8>,
) -> Result<Vec<u8>, String> {
    loop {
        if let Some(message) = try_parse_websocket_text_message(buffer)? {
            return Ok(message);
        }

        let mut chunk = [0u8; 4096];
        let read = stream
            .read(&mut chunk)
            .await
            .map_err(|error| error.to_string())?;
        if read == 0 {
            return Err("WebSocket closed before chat request".to_string());
        }
        buffer.extend_from_slice(&chunk[..read]);
        if buffer.len() > MAX_JSON_BODY_BYTES + 14 {
            return Err("WebSocket chat request exceeds maximum allowed size".to_string());
        }
    }
}

pub(crate) fn try_parse_websocket_text_message(
    buffer: &mut Vec<u8>,
) -> Result<Option<Vec<u8>>, String> {
    let mut cursor = 0usize;
    let mut started = false;
    let mut message = Vec::new();

    loop {
        let Some(frame) = parse_websocket_frame(buffer, cursor)? else {
            return Ok(None);
        };

        match frame.opcode {
            0x0 => {
                if !started {
                    return Err("unexpected WebSocket continuation frame".to_string());
                }
            }
            0x1 | 0x2 => {
                if started {
                    return Err(
                        "new WebSocket data frame started before continuation finished".to_string(),
                    );
                }
                started = true;
            }
            0x8 => return Err("WebSocket closed before chat request".to_string()),
            opcode => return Err(format!("unsupported WebSocket opcode: {opcode}")),
        }

        message.extend_from_slice(&frame.payload);
        if message.len() > MAX_JSON_BODY_BYTES {
            return Err("WebSocket chat request exceeds maximum allowed size".to_string());
        }
        cursor = frame.next;

        if frame.fin {
            buffer.drain(..cursor);
            return Ok(Some(message));
        }
    }
}

struct ParsedWebSocketFrame {
    fin: bool,
    opcode: u8,
    payload: Vec<u8>,
    next: usize,
}

fn parse_websocket_frame(
    buffer: &[u8],
    start: usize,
) -> Result<Option<ParsedWebSocketFrame>, String> {
    if buffer.len() < start + 2 {
        return Ok(None);
    }

    let fin = buffer[start] & 0x80 != 0;
    let opcode = buffer[start] & 0x0f;
    let masked = buffer[start + 1] & 0x80 != 0;
    if !masked {
        return Err("client WebSocket frames must be masked".to_string());
    }

    let mut offset = start + 2;
    let mut len = (buffer[start + 1] & 0x7f) as usize;
    if len == 126 {
        if buffer.len() < offset + 2 {
            return Ok(None);
        }
        len = u16::from_be_bytes([buffer[offset], buffer[offset + 1]]) as usize;
        offset += 2;
    } else if len == 127 {
        if buffer.len() < offset + 8 {
            return Ok(None);
        }
        let raw_len = u64::from_be_bytes([
            buffer[offset],
            buffer[offset + 1],
            buffer[offset + 2],
            buffer[offset + 3],
            buffer[offset + 4],
            buffer[offset + 5],
            buffer[offset + 6],
            buffer[offset + 7],
        ]);
        len = usize::try_from(raw_len)
            .map_err(|_| "WebSocket frame length is too large".to_string())?;
        offset += 8;
    }

    if len > MAX_JSON_BODY_BYTES {
        return Err("WebSocket chat request exceeds maximum allowed size".to_string());
    }
    if buffer.len() < offset + 4 + len {
        return Ok(None);
    }

    let mask = [
        buffer[offset],
        buffer[offset + 1],
        buffer[offset + 2],
        buffer[offset + 3],
    ];
    offset += 4;
    let mut payload = buffer[offset..offset + len].to_vec();
    for (index, byte) in payload.iter_mut().enumerate() {
        *byte ^= mask[index % 4];
    }
    Ok(Some(ParsedWebSocketFrame {
        fin,
        opcode,
        payload,
        next: offset + len,
    }))
}

pub(crate) fn sse_headers() -> String {
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: keep-alive\r\nAccess-Control-Allow-Origin: {}\r\nVary: Origin\r\n{}\r\n",
        response_cors_origin(),
        response_cors_credentials_header()
    )
}

#[cfg(test)]
mod chat_stream_tests {
    use super::{
        clear_pending_tool_response_maps, managed_gateway_receipt_status,
        native_chat_acknowledges_peer_messages, native_chat_terminal_status,
    };
    use maestro_local_host::agent::FromAgent;

    #[tokio::test]
    async fn failed_chat_writes_stop_the_embedding_before_removing_attachments() {
        use maestro_local_host::ai::{ScriptedBlock, ScriptedResponse, StopReason};
        use maestro_local_host::embedding::test_kit::ScriptedEmbeddingBuilder;
        use std::time::Duration;
        use tokio::io::AsyncWriteExt;
        use tokio::net::{TcpListener, TcpStream};

        for websocket in [false, true] {
            let workspace = tempfile::tempdir().expect("workspace");
            let attachment_dir = workspace.path().join("attachments");
            std::fs::create_dir(&attachment_dir).expect("attachment directory");
            let attachment = attachment_dir.join("request.txt");
            std::fs::write(&attachment, "request context").expect("attachment");
            let session = ScriptedEmbeddingBuilder::new(vec![ScriptedResponse {
                blocks: vec![ScriptedBlock::Pending],
                stop_reason: StopReason::EndTurn,
                error: None,
            }])
            .working_directory(workspace.path())
            .start()
            .expect("embedding");
            let (agent, mut events) = session.into_parts();
            agent.prompt("Wait for the client.").await.expect("prompt");
            tokio::time::timeout(Duration::from_secs(5), async {
                while let Some(event) = events.recv().await {
                    if matches!(event, FromAgent::ResponseStart { .. }) {
                        return;
                    }
                }
                panic!("embedding stopped before the request began");
            })
            .await
            .expect("pending model request");

            let listener = TcpListener::bind("127.0.0.1:0").await.expect("listener");
            let mut writer = TcpStream::connect(listener.local_addr().expect("address"))
                .await
                .expect("writer");
            let (_reader, _) = listener.accept().await.expect("reader");
            writer.shutdown().await.expect("close write half");
            let message = serde_json::json!({ "type": "done" });
            let outcome = if websocket {
                super::send_ws_json(&mut writer, &message).await
            } else {
                super::send_sse(&mut writer, &message).await
            };
            assert!(
                outcome.is_err(),
                "the closed transport must reject the write"
            );
            let original_error = outcome.clone();
            let result = tokio::time::timeout(
                Duration::from_secs(5),
                super::finish_embedded_agent(
                    agent,
                    super::PreparedAttachments {
                        paths: vec![attachment.to_string_lossy().into_owned()],
                        temp_dir: Some(attachment_dir.clone()),
                    },
                    outcome,
                ),
            )
            .await
            .expect("cleanup cancels the pending model");
            assert_eq!(result, original_error);
            assert!(!attachment_dir.exists());
            tokio::time::timeout(Duration::from_secs(5), async {
                while events.recv().await.is_some() {}
            })
            .await
            .expect("the actor and event relay must be closed after cleanup");
        }
    }

    #[tokio::test]
    async fn pending_response_cleanup_removes_only_this_turns_client_and_approval_entries() {
        use std::collections::{HashMap, HashSet};
        use tokio::sync::{Mutex, mpsc};

        let client_call = "client-tool-call".to_string();
        let approval_call = "approval-call".to_string();
        let unrelated_call = "other-turn-call".to_string();
        let (client_sender, _client_receiver) =
            mpsc::unbounded_channel::<maestro_local_host::agent::ToolResponseMessage>();
        let (approval_sender, _approval_receiver) =
            mpsc::unbounded_channel::<maestro_local_host::agent::ToolResponseMessage>();
        let (unrelated_sender, _unrelated_receiver) =
            mpsc::unbounded_channel::<maestro_local_host::agent::ToolResponseMessage>();
        let pending_tool_responses = Mutex::new(HashMap::from([
            (client_call.clone(), client_sender),
            (approval_call.clone(), approval_sender),
            (unrelated_call.clone(), unrelated_sender),
        ]));
        let pending_tool_response_sessions = Mutex::new(HashMap::from([
            (
                client_call.clone(),
                super::PendingToolResponseOwner::Session("session-client".to_string()),
            ),
            (
                approval_call.clone(),
                super::PendingToolResponseOwner::Session("session-approval".to_string()),
            ),
            (
                unrelated_call.clone(),
                super::PendingToolResponseOwner::Session("session-other".to_string()),
            ),
        ]));
        let completed_client_tool_results = Mutex::new(HashMap::from([
            (client_call.clone(), true),
            (approval_call.clone(), false),
            (unrelated_call.clone(), true),
        ]));
        let this_turn = HashSet::from([client_call.clone(), approval_call.clone()]);

        clear_pending_tool_response_maps(
            &pending_tool_responses,
            &pending_tool_response_sessions,
            &completed_client_tool_results,
            &this_turn,
        )
        .await;

        for call_id in [&client_call, &approval_call] {
            assert!(
                !pending_tool_responses.lock().await.contains_key(call_id),
                "pending sender for {call_id} must be removed",
            );
            assert!(
                !pending_tool_response_sessions
                    .lock()
                    .await
                    .contains_key(call_id),
                "pending owner for {call_id} must be removed",
            );
            assert!(
                !completed_client_tool_results
                    .lock()
                    .await
                    .contains_key(call_id),
                "completed result for {call_id} must be removed",
            );
        }
        assert!(
            pending_tool_responses
                .lock()
                .await
                .contains_key(&unrelated_call),
            "another active turn's sender must remain",
        );
        assert!(
            pending_tool_response_sessions
                .lock()
                .await
                .contains_key(&unrelated_call),
            "another active turn's owner must remain",
        );
        assert!(
            completed_client_tool_results
                .lock()
                .await
                .contains_key(&unrelated_call),
            "another active turn's completed result must remain",
        );
    }

    #[test]
    fn managed_gateway_receipt_status_contains_safe_camel_case_fields() {
        let status = managed_gateway_receipt_status(
            "request-1".to_string(),
            "record-1".to_string(),
            "lineage-1".to_string(),
            "planned".to_string(),
        );

        assert_eq!(status["type"], "status");
        assert_eq!(status["status"], "managed_gateway_receipt");
        assert_eq!(status["details"]["requestId"], "request-1");
        assert_eq!(status["details"]["recordId"], "record-1");
        assert_eq!(status["details"]["lineageId"], "lineage-1");
        assert_eq!(status["details"]["recordStatus"], "planned");
        assert!(
            !status
                .to_string()
                .contains("managed_inference_authorization")
        );
    }

    #[test]
    fn native_chat_requires_explicit_turn_terminal() {
        assert!(
            native_chat_terminal_status(&FromAgent::ResponseEnd {
                response_id: "done".to_string(),
                usage: None,
            })
            .is_none()
        );
        assert!(matches!(
            native_chat_terminal_status(&FromAgent::TurnCompleted {
                response_id: "done".to_string(),
                coding_completion: None,
                coding_child_records: Vec::new(),
            }),
            Some(Ok(()))
        ));
        assert!(matches!(
            native_chat_terminal_status(&FromAgent::ProviderError {
                kind: maestro_local_host::ai::ProviderStreamErrorKind::TransientProtocol,
                message: "unexpected eof".to_string(),
            }),
            Some(Err(message)) if message.contains("unexpected eof")
        ));
    }

    #[test]
    fn native_chat_failures_leave_peer_messages_pending_for_redelivery() {
        assert!(!native_chat_acknowledges_peer_messages(
            &FromAgent::ResponseEnd {
                response_id: "partial".to_string(),
                usage: None,
            }
        ));
        assert!(!native_chat_acknowledges_peer_messages(
            &FromAgent::ProviderError {
                kind: maestro_local_host::ai::ProviderStreamErrorKind::ProviderDeclaredFailure,
                message: "authentication failed".to_string(),
            }
        ));
        assert!(!native_chat_acknowledges_peer_messages(
            &FromAgent::TurnInterrupted {
                response_id: "interrupted".to_string(),
                reason: "stream dropped".to_string(),
            }
        ));
    }

    #[test]
    fn native_chat_successful_terminal_acknowledges_peer_messages() {
        assert!(native_chat_acknowledges_peer_messages(
            &FromAgent::TurnCompleted {
                response_id: "completed".to_string(),
                coding_completion: None,
                coding_child_records: Vec::new(),
            }
        ));
    }
}

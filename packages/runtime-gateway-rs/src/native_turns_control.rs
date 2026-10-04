//! Authenticated acceptance, queue controls and execution observation adapters.
use super::*;
use crate::chat_admission::validate_client_tool_names;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AcceptRequest {
    turn_id: String,
    session_id: String,
    session_created_at: String,
    request: ChatRequest,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ControlRequest {
    session_id: String,
    session_created_at: String,
    generation: u64,
    operation: String,
    request: Option<ChatRequest>,
    content: Option<String>,
    turn_id: Option<String>,
}

pub(crate) fn is_native_turn_endpoint(head: &RequestHead) -> bool {
    head.path == "/api/native/turns" || head.path.starts_with("/api/native/turns/")
}
async fn binding_for(
    state: &AppState,
    auth: &AuthContext,
    id: &str,
    created_at: &str,
) -> Result<TurnBinding, TurnError> {
    // A development bypass is not an authenticated execution owner.
    if auth.source == AuthSource::LoopbackDev {
        return Err(TurnError {
            status: 401,
            message: "Native turn reattachment requires an authenticated gateway principal".into(),
        });
    }
    let sessions = state.sessions.lock().await;
    let session = sessions
        .sessions
        .get(id)
        .filter(|session| {
            session_visible_to_auth(session, auth) && session.created_at == created_at
        })
        .ok_or_else(TurnError::not_found)?;
    Ok(TurnBinding {
        session_id: session.id.clone(),
        session_created_at: session.created_at.clone(),
        subject: auth.subject.clone(),
        organization_id: auth.organization_id.clone(),
        workspace_id: auth.workspace_id.clone(),
        source: auth.source,
        cwd: state.config.cwd.clone(),
    })
}
fn validate_request(request: &ChatRequest, id: &str) -> Result<(), TurnError> {
    if request
        .session_id
        .as_deref()
        .is_some_and(|value| value != id)
    {
        return Err(TurnError::bad(
            "Chat request session does not match accepted binding",
        ));
    }
    let latest = request
        .messages
        .last()
        .filter(|message| message.role == "user")
        .ok_or_else(|| TurnError::bad("Last message must be a user message"))?;
    if latest_prompt(request).len() > 64 * 1024 {
        return Err(TurnError::bad("Prompt text exceeds the 64 KiB limit"));
    }
    if latest_prompt(request).trim().is_empty() && latest.attachments.is_empty() {
        return Err(TurnError::bad("User message cannot be empty"));
    }
    validate_client_tool_names(request).map_err(|message| TurnError::bad(&message))?;
    if request_identity(request).len() > MAX_REQUEST_BYTES {
        return Err(TurnError::bad("Native prompt exceeds acceptance limit"));
    }
    Ok(())
}
fn valid_turn_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

async fn validate_attachments(
    request: &ChatRequest,
    state: &AppState,
    model: &str,
) -> Result<(), TurnError> {
    let attachments = request
        .messages
        .last()
        .map(|message| message.attachments.as_slice())
        .unwrap_or_default();
    if attachments.len() > 6 {
        return Err(TurnError::bad(
            "At most six attachments are accepted per prompt",
        ));
    }
    let mut decoded_bytes = 0usize;
    for attachment in attachments {
        let extracted = attachment
            .extracted_text
            .as_deref()
            .is_some_and(|text| !text.trim().is_empty());
        let content = attachment
            .content
            .as_deref()
            .filter(|content| !content.trim().is_empty());
        if content.is_none() && !extracted {
            return Err(TurnError::bad(
                "Attachment requires supplied content or extracted text",
            ));
        }
        if let Some(content) = content {
            let bytes = BASE64_STANDARD
                .decode(crate::chat::strip_data_url_prefix(content))
                .map_err(|_| TurnError::bad("Attachment content is not valid base64"))?;
            decoded_bytes = decoded_bytes.saturating_add(bytes.len());
            if decoded_bytes > 1024 * 1024 {
                return Err(TurnError::bad(
                    "Decoded attachments exceed the one MiB limit",
                ));
            }
            let image = attachment
                .mime_type
                .as_deref()
                .is_some_and(|mime| mime.starts_with("image/"));
            if image {
                let registry = available_models(&state.config).await;
                if !resolve_model(model, &registry).is_some_and(|model| model.capabilities.vision) {
                    return Err(TurnError::bad(
                        "Selected model does not support image input",
                    ));
                }
                let extension = attachment
                    .file_name
                    .as_deref()
                    .and_then(|name| name.rsplit('.').next())
                    .unwrap_or_default()
                    .to_ascii_lowercase();
                if !matches!(extension.as_str(), "png" | "jpg" | "jpeg" | "gif" | "webp") {
                    return Err(TurnError::bad(
                        "Image filename must identify a supported image format",
                    ));
                }
            } else if !extracted {
                let text=std::str::from_utf8(&bytes).map_err(|_|TurnError::bad("Binary documents require extracted text; this turn cannot consume their bytes directly"))?;
                if text.chars().count() > 100_000 {
                    return Err(TurnError::bad(
                        "Text files exceed the native 100000 character attachment limit",
                    ));
                }
            }
        }
    }
    Ok(())
}

pub(crate) async fn handle_native_turn_endpoint(
    stream: &mut TcpStream,
    initial: &mut Vec<u8>,
    head: &RequestHead,
    state: &AppState,
) -> Vec<u8> {
    let auth = match authorized_context(head, &state.config) {
        Ok(auth) => auth,
        Err(response) => return response,
    };
    match handle_endpoint(stream, initial, head, state, auth).await {
        Ok(value) => json_response(200, &value),
        Err(error) => error.response(),
    }
}
async fn handle_endpoint(
    stream: &mut TcpStream,
    initial: &mut Vec<u8>,
    head: &RequestHead,
    state: &AppState,
    auth: AuthContext,
) -> Result<Value, TurnError> {
    if head.method == "POST" && head.path == "/api/native/turns" {
        let bytes = read_request_body_with_limit(stream, initial, head, MAX_REQUEST_BYTES)
            .await
            .map_err(|message| TurnError::bad(&message))?;
        let input: AcceptRequest = serde_json::from_slice(&bytes)
            .map_err(|error| TurnError::bad(&format!("Invalid acceptance request: {error}")))?;
        if !valid_turn_id(&input.turn_id) {
            return Err(TurnError::bad("Invalid native turn identity"));
        }
        validate_request(&input.request, &input.session_id)?;
        let binding =
            binding_for(state, &auth, &input.session_id, &input.session_created_at).await?;
        if state.native_turns.find(&binding, &input.turn_id).is_some() {
            let accepted = state.native_turns.accept(
                binding.clone(),
                input.turn_id,
                input.request,
                auth,
                false,
                None,
            )?;
            return Ok(accepted.turn.snapshot(
                &state.native_turns.epoch,
                0,
                state.native_turns.queue(&binding),
            ));
        }
        let model = crate::chat::selected_chat_model(&input.request, state).await;
        validate_attachments(&input.request, state, &model).await?;
        // Neither runtime a native turn runs on accepts input mid-turn: the
        // Codex app server never did, and the dex-loop kernel has no steering
        // event. Advertise that up front rather than accept and then refuse.
        let steering_supported = false;
        let accepted = state.native_turns.accept(
            binding.clone(),
            input.turn_id,
            input.request,
            auth,
            steering_supported,
            Some(model),
        )?;
        let snapshot = accepted.turn.snapshot(
            &state.native_turns.epoch,
            0,
            state.native_turns.queue(&binding),
        );
        if accepted.start_lane {
            let state = state.clone();
            tokio::spawn(async move {
                run_lane(state, binding).await;
            });
        }
        return Ok(snapshot);
    }
    let suffix = head
        .path
        .strip_prefix("/api/native/turns")
        .ok_or_else(TurnError::not_found)?;
    if head.method == "GET" {
        let id = head
            .query
            .get("sessionId")
            .ok_or_else(|| TurnError::bad("sessionId required"))?;
        let created_at = head
            .query
            .get("sessionCreatedAt")
            .ok_or_else(|| TurnError::bad("sessionCreatedAt required"))?;
        let binding = binding_for(state, &auth, id, created_at).await?;
        if suffix.is_empty() {
            return Ok(
                serde_json::json!({"gatewayEpoch":state.native_turns.epoch,"turns":state.native_turns.snapshots(&binding)}),
            );
        }
        let turn_id = suffix
            .strip_prefix('/')
            .filter(|id| valid_turn_id(id))
            .ok_or_else(TurnError::not_found)?;
        let after = head
            .query
            .get("after")
            .map(|value| value.parse::<u64>())
            .transpose()
            .map_err(|_| TurnError::bad("Invalid replay cursor"))?
            .unwrap_or(0);
        let turn = state
            .native_turns
            .find(&binding, turn_id)
            .ok_or_else(TurnError::not_found)?;
        return Ok(turn.snapshot(
            &state.native_turns.epoch,
            after,
            state.native_turns.queue(&binding),
        ));
    }
    if head.method == "POST" {
        let turn_id = suffix
            .strip_prefix('/')
            .and_then(|value| value.strip_suffix("/control"))
            .filter(|id| valid_turn_id(id))
            .ok_or_else(TurnError::not_found)?;
        let bytes = read_request_body_with_limit(stream, initial, head, MAX_REQUEST_BYTES)
            .await
            .map_err(|message| TurnError::bad(&message))?;
        let input: ControlRequest = serde_json::from_slice(&bytes)
            .map_err(|error| TurnError::bad(&format!("Invalid turn control: {error}")))?;
        if input
            .turn_id
            .as_deref()
            .is_some_and(|value| value != turn_id)
        {
            return Err(TurnError::bad("Control identity does not match route"));
        }
        let binding =
            binding_for(state, &auth, &input.session_id, &input.session_created_at).await?;
        let turn = state
            .native_turns
            .find(&binding, turn_id)
            .ok_or_else(TurnError::not_found)?;
        match input.operation.as_str() {
            "stop" => turn.stop(input.generation)?,
            "remove" => turn.remove(input.generation)?,
            "edit" => {
                let mut request = turn.data.lock().expect("native turn lock").request.clone();
                let content = input
                    .content
                    .or_else(|| input.request.as_ref().map(latest_prompt))
                    .ok_or_else(|| TurnError::bad("Edited prompt content required"))?;
                if content.trim().is_empty() || content.len() > 64 * 1024 {
                    return Err(TurnError::bad(
                        "Edited prompt requires bounded non-empty text",
                    ));
                }
                request
                    .messages
                    .last_mut()
                    .ok_or_else(|| TurnError::bad("Queued prompt missing"))?
                    .content = Value::String(content);
                validate_request(&request, &input.session_id)?;
                state
                    .native_turns
                    .edit_turn(&turn, input.generation, request)?;
            }
            "steer" => {
                turn.steer(
                    input.generation,
                    input
                        .content
                        .ok_or_else(|| TurnError::bad("Steering content required"))?,
                )
                .await?
            }
            _ => return Err(TurnError::bad("Unknown native turn operation")),
        }
        return Ok(turn.snapshot(
            &state.native_turns.epoch,
            0,
            state.native_turns.queue(&binding),
        ));
    }
    Err(TurnError::not_found())
}

// Must be called while session generation is still valid. No network observer
// supplies the transcript used by a queued or forked execution.
pub(super) async fn execution_request(
    state: &AppState,
    turn: &NativeTurn,
) -> Result<ChatRequest, String> {
    let mut request = turn.data.lock().expect("native turn lock").request.clone();
    let mut latest = request
        .messages
        .last()
        .cloned()
        .ok_or("Accepted prompt missing")?;
    // Extracted text is consumed directly in the prompt; do not hand an
    // unsupported binary to a native attachment parser and silently omit it.
    for attachment in &mut latest.attachments {
        if attachment
            .extracted_text
            .as_deref()
            .is_some_and(|text| !text.trim().is_empty())
            && !attachment
                .mime_type
                .as_deref()
                .is_some_and(|mime| mime.starts_with("image/"))
        {
            attachment.content = None;
        }
    }
    let sessions = state.sessions.lock().await;
    let session = sessions
        .sessions
        .get(&turn.binding.session_id)
        .filter(|session| {
            session.created_at == turn.binding.session_created_at
                && session_visible_to_auth(session, &turn.auth)
        })
        .ok_or("Accepted session generation no longer exists")?;
    let mut messages = request
        .messages
        .iter()
        .filter(|message| message.role == "system")
        .cloned()
        .collect::<Vec<_>>();
    for value in &session.messages {
        messages.push(
            serde_json::from_value(value.clone())
                .map_err(|error| format!("Session continuation is unreadable: {error}"))?,
        );
    }
    messages.push(latest);
    request.messages = messages;
    request.session_id = Some(turn.binding.session_id.clone());
    Ok(request)
}
async fn run_lane(state: AppState, binding: TurnBinding) {
    let lane = binding.lane();
    while let Some(turn) = state.native_turns.next(&lane) {
        if turn.cancel.is_cancelled() {
            turn.publish(serde_json::json!({"type":"done"}));
            continue;
        }
        let request = match execution_request(&state, &turn).await {
            Ok(request) => request,
            Err(error) => {
                turn.publish(serde_json::json!({"type":"error","message":error}));
                turn.publish(serde_json::json!({"type":"done"}));
                continue;
            }
        };
        let remaining = EXECUTION_LEASE.saturating_sub(turn.accepted_at.elapsed());
        let timer_cancel = turn.cancel.clone();
        let timer = tokio::spawn(async move {
            tokio::time::sleep(remaining).await;
            timer_cancel.cancel();
        });
        let result = crate::chat::execute_chat_websocket_turn(
            &mut NativeTurnOutput(turn.clone()),
            request,
            turn.auth.clone(),
            state.clone(),
        )
        .await;
        timer.abort();
        if let Err(error) = result {
            turn.publish(serde_json::json!({"type":"error","message":error}));
        }
        // If admission/actor cleanup returned without a wire terminal, report
        // failure honestly; never infer that an unfinished effect succeeded.
        if !turn.data.lock().expect("native turn lock").state.terminal() {
            turn.publish(serde_json::json!({"type":"error","message":"Native turn ended before a terminal event"}));
            turn.publish(serde_json::json!({"type":"done"}));
        }
    }
}

#[cfg(test)]
#[path = "native_turns_control_tests.rs"]
mod native_turns_control_tests;

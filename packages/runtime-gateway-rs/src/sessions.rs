use super::*;
use crate::session_messaging::{message_targets_session, persist_message_store_snapshot};

#[derive(Debug, Deserialize, Default)]
pub(super) struct SessionCreateRequest {
    title: Option<String>,
    tags: Option<Vec<String>>,
    #[serde(rename = "logGroupId")]
    log_group_id: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub(super) struct SessionUpdateRequest {
    title: Option<String>,
    favorite: Option<bool>,
    tags: Option<Vec<String>>,
    #[serde(default, deserialize_with = "deserialize_log_group_update")]
    log_group_id: Option<Option<String>>,
}

fn deserialize_log_group_update<'de, D>(deserializer: D) -> Result<Option<Option<String>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Option::<String>::deserialize(deserializer).map(Some)
}

#[derive(Clone, Default, Serialize, Deserialize)]
pub(super) struct SessionStore {
    #[serde(default)]
    pub(super) sessions: HashMap<String, SessionRecord>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub(super) shared_sessions: HashMap<String, SharedSessionGrant>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct SessionRecord {
    pub(super) id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) owner: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) organization_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) workspace_id: Option<String>,
    pub(super) title: String,
    pub(super) created_at: String,
    pub(super) updated_at: String,
    pub(super) message_count: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) favorite: Option<bool>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(super) tags: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) log_group_id: Option<String>,
    #[serde(default)]
    pub(super) messages: Vec<Value>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct SharedSessionGrant {
    pub(super) session_id: String,
    pub(super) expires_at: u64,
    pub(super) max_accesses: Option<u64>,
    pub(super) access_count: u64,
}

pub(super) struct ShareOptions {
    pub(super) expires_in_hours: u64,
    pub(super) max_accesses: Option<u64>,
    pub(super) allow_sensitive_content: bool,
}

pub(super) struct ExportOptions {
    pub(super) format: String,
    pub(super) allow_sensitive_content: bool,
}

pub(super) async fn handle_session_endpoint(
    stream: &mut TcpStream,
    initial: &mut Vec<u8>,
    head: &RequestHead,
    state: &AppState,
) -> Vec<u8> {
    if head.method == "GET" {
        if let Some(shared_path) = shared_session_path_from_path(&head.path) {
            return handle_shared_session_get(state, shared_path).await;
        }
    }
    let auth = match authorized_context(head, &state.config) {
        Ok(auth) => auth,
        Err(response) => return response,
    };
    match head.method.as_str() {
        "GET" if head.path == "/api/sessions" => json_response(
            200,
            &serde_json::json!({ "sessions": session_summaries(state, &auth).await }),
        ),
        "POST" if head.path == "/api/sessions" => {
            let body = match read_request_body(stream, initial, head).await {
                Ok(body) => body,
                Err(error) => return json_response(400, &serde_json::json!({ "error": error })),
            };
            let request = if body.is_empty() {
                SessionCreateRequest::default()
            } else {
                match serde_json::from_slice::<SessionCreateRequest>(&body) {
                    Ok(request) => request,
                    Err(error) => {
                        return json_response(
                            400,
                            &serde_json::json!({ "error": format!("invalid session request: {error}") }),
                        );
                    }
                }
            };
            let mut session = create_session_record(request.title, auth.subject.clone());
            session.tags = request.tags.unwrap_or_default();
            session.log_group_id = match normalize_log_group_id(request.log_group_id) {
                Ok(log_group_id) => log_group_id,
                Err(error) => {
                    return json_response(400, &serde_json::json!({ "error": error }));
                }
            };
            bind_session_to_auth(&mut session, &auth);
            let value = session_full_value(&session);
            {
                state
                    .sessions
                    .lock()
                    .await
                    .sessions
                    .insert(session.id.clone(), session);
            }
            persist_session_store(state).await;
            json_response(200, &value)
        }
        "POST" => {
            let Some(session_path) = session_path_from_path(&head.path) else {
                return json_response(404, &serde_json::json!({ "error": "Not found" }));
            };
            match session_path.tail {
                Some("share") => {
                    handle_session_share_post(stream, initial, head, state, session_path, &auth)
                        .await
                }
                Some("export") => {
                    handle_session_export_post(stream, initial, head, state, session_path, &auth)
                        .await
                }
                Some(tail) => {
                    if let Some(attachment_id) = session_attachment_extract_id(tail) {
                        handle_session_attachment_extract(
                            head,
                            state,
                            session_path.id,
                            attachment_id,
                            &auth,
                        )
                        .await
                    } else {
                        json_response(404, &serde_json::json!({ "error": "Not found" }))
                    }
                }
                _ => json_response(404, &serde_json::json!({ "error": "Not found" })),
            }
        }
        "GET" => {
            let Some(session_path) = session_path_from_path(&head.path) else {
                return json_response(404, &serde_json::json!({ "error": "Not found" }));
            };
            handle_session_get(head, state, session_path, &auth).await
        }
        "PATCH" => {
            let Some(session_path) = session_path_from_path(&head.path) else {
                return json_response(404, &serde_json::json!({ "error": "Not found" }));
            };
            if session_path.tail.is_some() {
                return json_response(404, &serde_json::json!({ "error": "Not found" }));
            };
            let body = match read_request_body(stream, initial, head).await {
                Ok(body) => body,
                Err(error) => return json_response(400, &serde_json::json!({ "error": error })),
            };
            let request = if body.is_empty() {
                SessionUpdateRequest::default()
            } else {
                match serde_json::from_slice::<SessionUpdateRequest>(&body) {
                    Ok(request) => request,
                    Err(error) => {
                        return json_response(
                            400,
                            &serde_json::json!({ "error": format!("invalid session update: {error}") }),
                        );
                    }
                }
            };
            let log_group_update = match request.log_group_id.as_ref() {
                None => None,
                Some(None) => Some(None),
                Some(Some(log_group_id)) => {
                    match normalize_log_group_id(Some(log_group_id.clone())) {
                        Ok(log_group_id) => Some(log_group_id),
                        Err(error) => {
                            return json_response(400, &serde_json::json!({ "error": error }));
                        }
                    }
                }
            };
            let mut sessions = state.sessions.lock().await;
            let Some(session) = sessions.sessions.get_mut(session_path.id) else {
                return json_response(404, &serde_json::json!({ "error": "Session not found" }));
            };
            if !session_visible_to_auth(session, &auth) {
                return json_response(404, &serde_json::json!({ "error": "Session not found" }));
            }
            if let Some(title) = request.title.and_then(|title| normalize_title(Some(title))) {
                session.title = title;
            }
            if let Some(favorite) = request.favorite {
                session.favorite = Some(favorite);
            }
            if let Some(tags) = request.tags {
                session.tags = tags;
            }
            if let Some(log_group_id) = log_group_update {
                session.log_group_id = log_group_id;
            }
            session.updated_at = now_rfc3339();
            let value = session_summary_value(session);
            drop(sessions);
            persist_session_store(state).await;
            json_response(200, &value)
        }
        "DELETE" => {
            let Some(session_path) = session_path_from_path(&head.path) else {
                return json_response(404, &serde_json::json!({ "error": "Not found" }));
            };
            if session_path.tail.is_some() {
                return json_response(404, &serde_json::json!({ "error": "Not found" }));
            };
            // Message sends acquire these locks in this order. Keep the same
            // order so no send can race with the durable session removal.
            let _message_persist = state.session_messages_persist_lock.lock().await;
            let mut sessions = state.sessions.lock().await;
            let Some(session) = sessions.sessions.get(session_path.id) else {
                return json_response(404, &serde_json::json!({ "error": "Session not found" }));
            };
            if !session_visible_to_auth(session, &auth) {
                return json_response(404, &serde_json::json!({ "error": "Session not found" }));
            }
            let removed_session = session.clone();
            sessions.sessions.remove(session_path.id);
            let session_snapshot = sessions.clone();
            if let Err(error) = persist_session_store_snapshot(state, &session_snapshot).await {
                sessions
                    .sessions
                    .insert(removed_session.id.clone(), removed_session.clone());
                return json_response(
                    503,
                    &serde_json::json!({ "error": format!("failed to delete session: {error}") }),
                );
            }

            // Session removal is committed. Inbox cleanup is best-effort: on
            // failure retain the now-inaccessible messages for startup
            // reconciliation rather than claiming the session is still live.
            let mut messages = state.session_messages.lock().await;
            let before_messages = messages.clone();
            if let Some(inbox) = messages.messages_by_to_session.get_mut(&removed_session.id) {
                inbox.retain(|message| !message_targets_session(message, &removed_session));
                if inbox.is_empty() {
                    messages.messages_by_to_session.remove(&removed_session.id);
                }
            }
            if let Err(error) = persist_message_store_snapshot(state, &messages).await {
                *messages = before_messages;
                eprintln!("failed to clean up deleted session inbox: {error}");
            }
            drop(messages);
            drop(sessions);
            response_with_extra_headers_and_length(204, "application/json", &[], "", 0)
        }
        _ => json_response(405, &serde_json::json!({ "error": "Method not allowed" })),
    }
}

pub(super) async fn handle_pending_request_resume_endpoint(
    stream: &mut TcpStream,
    initial: &mut Vec<u8>,
    head: &RequestHead,
    state: &AppState,
    auth: &AuthContext,
) -> Vec<u8> {
    let Some(request_id) = pending_request_id_from_resume_path(&head.path) else {
        return json_response(404, &serde_json::json!({ "error": "Not found" }));
    };
    // Verify the caller may see the session that owns this pending request
    // before delivering the decision. Without this, any tenant-bound principal
    // could approve or inject a result into another tenant's blocked tool turn
    // by enumerating derived request ids. Loopback development stays permissive.
    if tenant_enforcement_active(&state.config)
        && !pending_request_owner_visible(state, &request_id, auth).await
    {
        // Fail closed and do not disclose existence, matching the 404 tenancy
        // convention used by the session endpoints.
        return json_response(
            404,
            &serde_json::json!({ "error": format!("No active pending request: {request_id}") }),
        );
    }
    let body = match read_request_body(stream, initial, head).await {
        Ok(body) => body,
        Err(error) => return json_response(400, &serde_json::json!({ "error": error })),
    };
    let payload = if body.is_empty() {
        Value::Object(serde_json::Map::new())
    } else {
        match serde_json::from_slice::<Value>(&body) {
            Ok(payload) if payload.is_object() => payload,
            Ok(_) => {
                return json_response(
                    400,
                    &serde_json::json!({ "error": "pending request resume payload must be an object" }),
                );
            }
            Err(error) => {
                return json_response(
                    400,
                    &serde_json::json!({ "error": format!("invalid pending request resume request: {error}") }),
                );
            }
        }
    };
    let Some(sender) = state
        .pending_tool_responses
        .lock()
        .await
        .remove(&request_id)
    else {
        return json_response(
            404,
            &serde_json::json!({ "error": format!("No active pending request: {request_id}") }),
        );
    };
    state
        .pending_tool_response_sessions
        .lock()
        .await
        .remove(&request_id);
    let (approved, result) = pending_tool_response_from_payload(&payload);
    let completed_client_tool_result = result.as_ref().map(|result| result.success);
    if let Some(success) = completed_client_tool_result {
        state
            .completed_client_tool_results
            .lock()
            .await
            .insert(request_id.clone(), success);
    }
    if sender
        .send((
            request_id.clone(),
            approved,
            result,
            ExecutionSource::RemoteClient,
            None,
        ))
        .is_err()
    {
        if completed_client_tool_result.is_some() {
            state
                .completed_client_tool_results
                .lock()
                .await
                .remove(&request_id);
        }
        return json_response(
            409,
            &serde_json::json!({ "error": "Pending request is no longer active" }),
        );
    }
    json_response(200, &pending_request_resume_value(&request_id, &payload))
}

pub(super) async fn load_session_store(path: &Path) -> (SessionStore, bool) {
    match tokio::fs::read(path).await {
        Ok(bytes) => match decode_session_store_with_migration(&bytes) {
            Ok((store, migrated)) => {
                if migrated {
                    if let Err(error) =
                        crate::migrations::atomic_write_validated_json(path, &store).await
                    {
                        eprintln!(
                            "failed to atomically migrate session store at {}: {error}",
                            path.display()
                        );
                        return (store, false);
                    }
                }
                (store, true)
            }
            Err(error) => {
                eprintln!(
                    "failed to parse session store at {}: {error}; leaving the file untouched",
                    path.display()
                );
                (SessionStore::default(), false)
            }
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            (SessionStore::default(), true)
        }
        Err(error) => {
            eprintln!(
                "failed to read session store at {}: {error}; leaving the file untouched",
                path.display()
            );
            (SessionStore::default(), false)
        }
    }
}

#[cfg_attr(not(test), allow(dead_code))]
pub(super) fn decode_session_store(bytes: &[u8]) -> Result<SessionStore, String> {
    decode_session_store_with_migration(bytes).map(|(store, _)| store)
}

fn decode_session_store_with_migration(bytes: &[u8]) -> Result<(SessionStore, bool), String> {
    let value = serde_json::from_slice::<Value>(bytes).map_err(|error| error.to_string())?;
    if value.get("sessions").is_some() {
        return serde_json::from_value::<SessionStore>(value)
            .map(|store| (store, false))
            .map_err(|error| error.to_string());
    }
    if value.is_object() {
        let sessions = serde_json::from_value::<HashMap<String, SessionRecord>>(value)
            .map_err(|error| error.to_string())?;
        return Ok((
            SessionStore {
                sessions,
                shared_sessions: HashMap::new(),
            },
            true,
        ));
    }
    if value.is_array() {
        let sessions = serde_json::from_value::<Vec<SessionRecord>>(value)
            .map_err(|error| error.to_string())?
            .into_iter()
            .map(|session| (session.id.clone(), session))
            .collect();
        return Ok((
            SessionStore {
                sessions,
                shared_sessions: HashMap::new(),
            },
            true,
        ));
    }
    Err("session store must be an object or array".to_string())
}

pub(super) async fn persist_session_store(state: &AppState) {
    if !state.session_store_persist_enabled {
        eprintln!(
            "skipping session store write because {} did not parse on startup",
            state.config.session_store_path.display()
        );
        return;
    }
    let store = state.sessions.lock().await.clone();
    if let Err(error) = persist_session_store_snapshot(state, &store).await {
        eprintln!("failed to persist session store atomically: {error}");
    }
}

pub(super) async fn persist_session_store_snapshot(
    state: &AppState,
    store: &SessionStore,
) -> Result<(), String> {
    if !state.session_store_persist_enabled {
        return Err(format!(
            "{} did not parse on startup",
            state.config.session_store_path.display()
        ));
    }
    let _persist = state.session_persist_lock.lock().await;
    if let Some(parent) = state.config.session_store_path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|error| error.to_string())?;
    }
    crate::migrations::atomic_write_validated_json(&state.config.session_store_path, store).await
}

pub(super) async fn persist_shared_sessions(state: &AppState) {
    if !state.session_store_persist_enabled {
        return;
    }
    let shared_sessions = state.shared_sessions.lock().await.clone();
    {
        let mut store = state.sessions.lock().await;
        store.shared_sessions = shared_sessions;
    }
    persist_session_store(state).await;
}

pub(super) fn create_session_record(title: Option<String>, owner: Option<String>) -> SessionRecord {
    let now = now_rfc3339();
    SessionRecord {
        id: new_session_id(),
        owner,
        organization_id: None,
        workspace_id: None,
        title: normalize_title(title).unwrap_or_else(|| "New Chat".to_string()),
        created_at: now.clone(),
        updated_at: now,
        message_count: 0,
        favorite: None,
        tags: Vec::new(),
        log_group_id: None,
        messages: Vec::new(),
    }
}

fn normalize_log_group_id(value: Option<String>) -> Result<Option<String>, &'static str> {
    let Some(value) = value else {
        return Ok(None);
    };
    let value = value.trim().to_owned();
    if value.is_empty()
        || value.len() > 128
        || !value
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "-_.:/".contains(character))
    {
        return Err("logGroupId must be 1-128 ASCII letters, digits, or -_.:/");
    }
    Ok(Some(value))
}

pub(super) fn normalize_title(title: Option<String>) -> Option<String> {
    title
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// Whether `auth` may act on the pending request identified by `request_id`.
///
/// Resolves the session or authenticated principal that owns the blocked agent
/// turn from [`AppState::pending_tool_response_sessions`]. Session-owned turns
/// apply the same [`session_visible_to_auth`] check the session endpoints use;
/// sessionless turns require the exact subject and tenant binding captured when
/// the request was accepted. Unrestricted principals (the loopback static admin
/// key) always pass. A request whose owner is unknown fails closed.
pub(crate) async fn pending_request_owner_visible(
    state: &AppState,
    request_id: &str,
    auth: &AuthContext,
) -> bool {
    if auth.unrestricted {
        return true;
    }
    let owner_session = state
        .pending_tool_response_sessions
        .lock()
        .await
        .get(request_id)
        .cloned();
    match owner_session {
        Some(PendingToolResponseOwner::Session(session_id)) => state
            .sessions
            .lock()
            .await
            .sessions
            .get(&session_id)
            .is_some_and(|session| session_visible_to_auth(session, auth)),
        Some(PendingToolResponseOwner::Principal {
            subject,
            organization_id,
            workspace_id,
        }) => {
            auth.subject.as_ref() == Some(&subject)
                && auth.organization_id == organization_id
                && auth.workspace_id == workspace_id
        }
        None => false,
    }
}

pub(super) fn session_visible_to_auth(session: &SessionRecord, auth: &AuthContext) -> bool {
    if auth.unrestricted {
        return true;
    }
    let Some(subject) = auth.subject.as_deref() else {
        return false;
    };
    if session.owner.as_deref() != Some(subject) {
        return false;
    }
    match (
        session.organization_id.as_deref(),
        session.workspace_id.as_deref(),
    ) {
        (Some(session_org), Some(session_workspace)) => {
            auth.organization_id.as_deref() == Some(session_org)
                && auth.workspace_id.as_deref() == Some(session_workspace)
        }
        // Legacy records without tenant metadata remain visible only to
        // legacy subject-only principals; tenant-bound identities fail closed.
        (None, None) => auth.organization_id.is_none() && auth.workspace_id.is_none(),
        _ => false,
    }
}

/// `value` when it is present and not the empty string, otherwise `None`.
/// Tenant identifiers are treated as absent when empty so an empty
/// `organization_id`/`workspace_id` can never match another empty one.
fn non_empty_str(value: Option<&str>) -> Option<&str> {
    value.filter(|inner| !inner.is_empty())
}

/// Whether `session` is addressable as a session-messaging peer by `auth`.
///
/// This is deliberately weaker than [`session_visible_to_auth`]: it drops the
/// owner check so a caller can address a peer session owned by a different
/// subject, but it requires that both principals share the SAME non-empty
/// `organization_id` AND the SAME non-empty `workspace_id`. Legacy records that
/// carry no tenant metadata (`organization_id`/`workspace_id` both `None` or
/// empty) fail closed and are never addressable by a scoped principal. An
/// unrestricted principal (static admin key) addresses every session.
pub(super) fn session_addressable_by_auth(session: &SessionRecord, auth: &AuthContext) -> bool {
    if auth.unrestricted {
        return true;
    }
    let session_org = non_empty_str(session.organization_id.as_deref());
    let session_workspace = non_empty_str(session.workspace_id.as_deref());
    let auth_org = non_empty_str(auth.organization_id.as_deref());
    let auth_workspace = non_empty_str(auth.workspace_id.as_deref());
    match (session_org, session_workspace, auth_org, auth_workspace) {
        (Some(session_org), Some(session_workspace), Some(auth_org), Some(auth_workspace)) => {
            session_org == auth_org && session_workspace == auth_workspace
        }
        // Legacy `(None, None)` peers and any partially-bound record fail closed.
        _ => false,
    }
}

pub(super) fn bind_session_to_auth(session: &mut SessionRecord, auth: &AuthContext) {
    session.organization_id = auth.organization_id.clone();
    session.workspace_id = auth.workspace_id.clone();
}

pub(super) async fn session_summaries(state: &AppState, auth: &AuthContext) -> Vec<Value> {
    let mut sessions: Vec<SessionRecord> = state
        .sessions
        .lock()
        .await
        .sessions
        .values()
        .filter(|session| session_visible_to_auth(session, auth))
        .cloned()
        .collect();
    sessions.sort_by(|left, right| right.updated_at.cmp(&left.updated_at));
    sessions
        .iter()
        .map(session_summary_value)
        .collect::<Vec<_>>()
}

pub(super) async fn handle_session_get(
    head: &RequestHead,
    state: &AppState,
    session_path: SessionPath<'_>,
    auth: &AuthContext,
) -> Vec<u8> {
    let Some(session) = state
        .sessions
        .lock()
        .await
        .sessions
        .get(session_path.id)
        .cloned()
    else {
        return json_response(404, &serde_json::json!({ "error": "Session not found" }));
    };
    if !session_visible_to_auth(&session, auth) {
        return json_response(404, &serde_json::json!({ "error": "Session not found" }));
    }

    match session_path.tail {
        None => json_response(200, &session_full_value(&session)),
        Some("timeline") => json_response(200, &session_timeline_value(&session)),
        Some("share") => json_response(
            200,
            &serde_json::json!({ "sessionId": session.id, "enabled": false, "shareUrl": Value::Null }),
        ),
        Some("export") => json_response(200, &session_full_value(&session)),
        Some("artifacts") => json_response(200, &session_artifacts_value(&session)),
        Some("artifact-access") => session_artifact_access_response(head, &session),
        Some("attachments") => json_response(200, &session_attachments_value(&session)),
        Some("artifacts.zip") => serve_session_artifacts_zip(&session),
        Some(tail) if tail.starts_with("artifacts/") => {
            serve_session_artifact(head, &session, tail)
        }
        Some(tail) if tail.starts_with("attachments/") => serve_session_attachment(&session, tail),
        _ => json_response(404, &serde_json::json!({ "error": "Not found" })),
    }
}

pub(super) fn session_timeline_value(session: &SessionRecord) -> Value {
    serde_json::json!({
        "sessionId": session.id,
        "source": "local",
        "generatedAt": now_rfc3339(),
        "platformBacked": false,
        "pendingRequestCount": 0,
        "items": session.messages.iter().enumerate().map(|(index, message)| {
            let role = message.get("role").and_then(Value::as_str).unwrap_or("assistant");
            let event_type = if role == "user" { "message.user" } else { "message.assistant" };
            serde_json::json!({
                "id": format!("{}-{index}", session.id),
                "sessionId": session.id,
                "timestamp": message.get("timestamp").and_then(Value::as_str).unwrap_or(&session.updated_at),
                "type": event_type,
                "title": if role == "user" { "User message" } else { "Assistant message" },
                "visibility": "user",
                "source": "local",
                "status": "completed",
                "role": role,
                "summary": timeline_message_summary(message),
                "metadata": { "message": public_session_message(message) }
            })
        }).collect::<Vec<_>>()
    })
}

pub(super) fn timeline_message_summary(message: &Value) -> String {
    message
        .get("content")
        .map(|content| {
            content
                .as_str()
                .map(ToString::to_string)
                .unwrap_or_else(|| content.to_string())
        })
        .unwrap_or_default()
        .chars()
        .take(240)
        .collect()
}

pub(super) async fn handle_shared_session_get(
    state: &AppState,
    shared_path: SharedSessionPath<'_>,
) -> Vec<u8> {
    let now = now_millis();
    let (session_id, should_persist_shared_sessions) = {
        let mut shares = state.shared_sessions.lock().await;
        let Some(grant) = shares.get_mut(shared_path.token) else {
            return json_response(
                404,
                &serde_json::json!({ "error": "Shared session not found" }),
            );
        };
        if grant.expires_at <= now {
            shares.remove(shared_path.token);
            drop(shares);
            persist_shared_sessions(state).await;
            return json_response(
                404,
                &serde_json::json!({ "error": "Shared session not found" }),
            );
        }
        if shared_path.tail.is_none() {
            if grant
                .max_accesses
                .map(|max| grant.access_count >= max)
                .unwrap_or(false)
            {
                shares.remove(shared_path.token);
                drop(shares);
                persist_shared_sessions(state).await;
                return json_response(
                    404,
                    &serde_json::json!({ "error": "Shared session not found" }),
                );
            }
            grant.access_count = grant.access_count.saturating_add(1);
            (grant.session_id.clone(), true)
        } else {
            (grant.session_id.clone(), false)
        }
    };
    if should_persist_shared_sessions {
        persist_shared_sessions(state).await;
    }
    let Some(session) = state
        .sessions
        .lock()
        .await
        .sessions
        .get(&session_id)
        .cloned()
    else {
        return json_response(
            404,
            &serde_json::json!({ "error": "Shared session not found" }),
        );
    };

    match shared_path.tail {
        None => json_response(200, &session_full_value(&session)),
        Some(tail) if tail.starts_with("attachments/") => serve_session_attachment(&session, tail),
        _ => json_response(404, &serde_json::json!({ "error": "Not found" })),
    }
}

pub(super) async fn handle_session_share_post(
    stream: &mut TcpStream,
    initial: &mut Vec<u8>,
    head: &RequestHead,
    state: &AppState,
    session_path: SessionPath<'_>,
    auth: &AuthContext,
) -> Vec<u8> {
    let Some(session) = state
        .sessions
        .lock()
        .await
        .sessions
        .get(session_path.id)
        .cloned()
    else {
        return json_response(404, &serde_json::json!({ "error": "Session not found" }));
    };
    if !session_visible_to_auth(&session, auth) {
        return json_response(404, &serde_json::json!({ "error": "Session not found" }));
    }
    let options = match read_share_options(stream, initial, head).await {
        Ok(options) => options,
        Err(response) => return response,
    };
    if !options.allow_sensitive_content && session_contains_sensitive_content(&session) {
        return json_response(
            409,
            &serde_json::json!({
                "error": "Sensitive content detected. Confirm that you want to publish this session.",
                "code": "sensitive_content_detected"
            }),
        );
    }
    let token = match generate_share_token() {
        Ok(token) => token,
        Err(error) => return json_response(500, &serde_json::json!({ "error": error })),
    };
    let expires_at = chrono::Utc::now() + chrono::Duration::hours(options.expires_in_hours as i64);
    state.shared_sessions.lock().await.insert(
        token.clone(),
        SharedSessionGrant {
            session_id: session.id,
            expires_at: expires_at.timestamp_millis().max(0) as u64,
            max_accesses: options.max_accesses,
            access_count: 0,
        },
    );
    persist_shared_sessions(state).await;
    json_response(
        200,
        &serde_json::json!({
            "shareToken": token,
            "shareUrl": format!("/api/sessions/shared/{token}"),
            "webShareUrl": format!("/share/{token}"),
            "expiresAt": expires_at.to_rfc3339(),
            "maxAccesses": options.max_accesses
        }),
    )
}

pub(super) async fn read_share_options(
    stream: &mut TcpStream,
    initial: &mut Vec<u8>,
    head: &RequestHead,
) -> Result<ShareOptions, Vec<u8>> {
    let body = read_request_body(stream, initial, head)
        .await
        .map_err(|error| json_response(400, &serde_json::json!({ "error": error })))?;
    let value = if body.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice::<Value>(&body).map_err(|error| {
            json_response(
                400,
                &serde_json::json!({ "error": format!("invalid share request: {error}") }),
            )
        })?
    };
    Ok(share_options_from_value(&value))
}

pub(super) fn share_options_from_value(value: &Value) -> ShareOptions {
    let expires_in_hours = value
        .get("expiresInHours")
        .and_then(Value::as_u64)
        .unwrap_or(24)
        .clamp(1, 168);
    let max_accesses = match value.get("maxAccesses") {
        Some(Value::Null) => None,
        Some(value) => Some(value.as_u64().unwrap_or(100).max(1)),
        None => Some(100),
    };
    ShareOptions {
        expires_in_hours,
        max_accesses,
        allow_sensitive_content: value
            .get("allowSensitiveContent")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    }
}

pub(super) fn export_options_from_body(body: &[u8]) -> Result<ExportOptions, String> {
    if body.is_empty() {
        return Ok(ExportOptions {
            format: "json".to_string(),
            allow_sensitive_content: false,
        });
    }
    let value = serde_json::from_slice::<Value>(body)
        .map_err(|error| format!("invalid export request: {error}"))?;
    Ok(export_options_from_value(&value))
}

pub(super) fn export_options_from_value(value: &Value) -> ExportOptions {
    ExportOptions {
        format: value
            .get("format")
            .and_then(Value::as_str)
            .filter(|format| matches!(*format, "json" | "markdown" | "text"))
            .unwrap_or("json")
            .to_string(),
        allow_sensitive_content: value
            .get("allowSensitiveContent")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    }
}

pub(super) fn session_contains_sensitive_content(session: &SessionRecord) -> bool {
    let haystack = serde_json::to_string(&session.messages)
        .unwrap_or_default()
        .to_ascii_lowercase();
    [
        "api_key",
        "apikey",
        "access token",
        "auth token",
        "bearer ",
        "password",
        "private key",
        "secret",
    ]
    .iter()
    .any(|needle| haystack.contains(needle))
}

pub(super) fn generate_share_token() -> Result<String, String> {
    let mut bytes = [0_u8; 32];
    getrandom::fill(&mut bytes)
        .map_err(|error| format!("Unable to generate share token: {error}"))?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

pub(super) async fn handle_session_export_post(
    stream: &mut TcpStream,
    initial: &mut Vec<u8>,
    head: &RequestHead,
    state: &AppState,
    session_path: SessionPath<'_>,
    auth: &AuthContext,
) -> Vec<u8> {
    let body = match read_request_body(stream, initial, head).await {
        Ok(body) => body,
        Err(error) => return json_response(400, &serde_json::json!({ "error": error })),
    };
    let options = match export_options_from_body(&body) {
        Ok(options) => options,
        Err(error) => return json_response(400, &serde_json::json!({ "error": error })),
    };
    let Some(session) = state
        .sessions
        .lock()
        .await
        .sessions
        .get(session_path.id)
        .cloned()
    else {
        return json_response(404, &serde_json::json!({ "error": "Session not found" }));
    };
    if !session_visible_to_auth(&session, auth) {
        return json_response(404, &serde_json::json!({ "error": "Session not found" }));
    }
    if !options.allow_sensitive_content && session_contains_sensitive_content(&session) {
        return json_response(
            409,
            &serde_json::json!({
                "error": "Sensitive content detected. Confirm that you want to export this session.",
                "code": "sensitive_content_detected"
            }),
        );
    }
    match options.format.as_str() {
        "markdown" => text_response(200, &session_export_text(&session, true)),
        "text" => text_response(200, &session_export_text(&session, false)),
        _ => json_response(200, &session_full_value(&session)),
    }
}

pub(super) fn session_export_text(session: &SessionRecord, markdown: bool) -> String {
    let mut lines = Vec::new();
    if markdown {
        lines.push(format!("# {}", session.title));
    } else {
        lines.push(session.title.clone());
    }
    for message in &session.messages {
        let role = message
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or("message");
        let text = message_text(message);
        if markdown {
            lines.push(format!("\n## {role}\n{text}"));
        } else {
            lines.push(format!("\n{role}:\n{text}"));
        }
    }
    lines.join("\n")
}

pub(super) fn message_text(message: &Value) -> String {
    match message.get("content") {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter_map(|block| {
                block
                    .get("text")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .or_else(|| Some(block.to_string()))
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Some(value) => value.to_string(),
        None => String::new(),
    }
}

pub(super) fn session_attachments_value(session: &SessionRecord) -> Value {
    let mut attachments = session_attachments(session);
    for attachment in &mut attachments {
        sanitize_attachment_for_read(attachment);
    }
    serde_json::json!({ "sessionId": session.id, "attachments": attachments })
}

pub(super) fn session_attachments(session: &SessionRecord) -> Vec<Value> {
    let mut attachments = Vec::new();
    for message in &session.messages {
        if let Some(values) = message.get("attachments").and_then(Value::as_array) {
            attachments.extend(values.iter().cloned());
        }
    }
    attachments
}

pub(super) fn session_attachment_extract_id(tail: &str) -> Option<String> {
    let rest = tail.strip_prefix("attachments/")?;
    let (attachment_id, suffix) = rest.split_once('/')?;
    if suffix != "extract" {
        return None;
    }
    let attachment_id = percent_decode_component(attachment_id);
    if attachment_id.is_empty() {
        None
    } else {
        Some(attachment_id)
    }
}

pub(super) async fn handle_attachment_extract(
    stream: &mut TcpStream,
    initial: &mut Vec<u8>,
    head: &RequestHead,
) -> Vec<u8> {
    let body = match read_request_body_with_limit(
        stream,
        initial,
        head,
        MAX_EXTRACT_JSON_BODY_BYTES,
    )
    .await
    {
        Ok(body) => body,
        Err(error) => return json_response(400, &serde_json::json!({ "error": error })),
    };
    let request: ExtractAttachmentRequest = match serde_json::from_slice(&body) {
        Ok(request) => request,
        Err(error) => {
            return json_response(
                400,
                &serde_json::json!({ "error": format!("invalid attachment extract request: {error}") }),
            );
        }
    };
    match tokio::task::spawn_blocking(move || extract_attachment_request(request)).await {
        Ok(Ok(output)) => attachment_extract_json_response(output.file_name.clone(), output),
        Ok(Err(error)) => json_response(400, &serde_json::json!({ "error": error })),
        Err(error) => json_response(
            500,
            &serde_json::json!({ "error": format!("Attachment extraction failed: {error}") }),
        ),
    }
}

pub(super) async fn handle_session_attachment_extract(
    head: &RequestHead,
    state: &AppState,
    session_id: &str,
    attachment_id: String,
    auth: &AuthContext,
) -> Vec<u8> {
    let should_force = head
        .query
        .get("force")
        .map(|force| matches!(force.as_str(), "1" | "true"))
        .unwrap_or(false);
    let (file_name, mime_type, content_base64) = {
        let mut sessions = state.sessions.lock().await;
        let Some(session) = sessions.sessions.get_mut(session_id) else {
            return json_response(404, &serde_json::json!({ "error": "Session not found" }));
        };
        if !session_visible_to_auth(session, auth) {
            return json_response(404, &serde_json::json!({ "error": "Session not found" }));
        }
        let Some(attachment) = find_session_attachment_mut(session, &attachment_id) else {
            return json_response(404, &serde_json::json!({ "error": "Attachment not found" }));
        };

        let file_name = attachment_string_field(attachment, &["fileName", "file_name"])
            .unwrap_or_else(|| "attachment".to_string());
        let mime_type = attachment_string_field(attachment, &["mimeType", "mime_type"]);
        if let Some(extracted_text) =
            attachment_string_field(attachment, &["extractedText", "extracted_text"])
        {
            if !should_force {
                return json_response(
                    200,
                    &serde_json::json!({
                        "fileName": file_name,
                        "format": "unknown",
                        "extractor": "native",
                        "size": attachment.get("size").and_then(Value::as_u64).unwrap_or(0),
                        "truncated": false,
                        "extractedText": extracted_text,
                        "cached": true
                    }),
                );
            }
        }
        let Some(content_base64) =
            attachment_string_field(attachment, &["contentBase64", "content_base64", "content"])
        else {
            return json_response(
                404,
                &serde_json::json!({ "error": "Attachment content not available" }),
            );
        };
        (file_name, mime_type, content_base64)
    };
    let output = match tokio::task::spawn_blocking({
        let file_name = file_name.clone();
        move || {
            extract_attachment_request(ExtractAttachmentRequest {
                file_name,
                mime_type,
                content_base64,
                max_chars: None,
            })
        }
    })
    .await
    {
        Ok(Ok(output)) => output,
        Ok(Err(error)) => {
            return json_response(400, &serde_json::json!({ "error": error }));
        }
        Err(error) => {
            return json_response(
                500,
                &serde_json::json!({ "error": format!("Attachment extraction failed: {error}") }),
            );
        }
    };
    let should_persist = {
        let mut sessions = state.sessions.lock().await;
        let Some(session) = sessions.sessions.get_mut(session_id) else {
            return attachment_extract_json_response(file_name, output);
        };
        if !session_visible_to_auth(session, auth) {
            return json_response(404, &serde_json::json!({ "error": "Session not found" }));
        }
        let Some(attachment) = find_session_attachment_mut(session, &attachment_id) else {
            return attachment_extract_json_response(file_name, output);
        };
        if let Some(object) = attachment.as_object_mut() {
            object.insert(
                "extractedText".to_string(),
                Value::String(output.extracted_text.clone()),
            );
            true
        } else {
            false
        }
    };
    if should_persist {
        persist_session_store(state).await;
    }
    attachment_extract_json_response(file_name, output)
}

pub(super) fn find_session_attachment_mut<'a>(
    session: &'a mut SessionRecord,
    attachment_id: &str,
) -> Option<&'a mut Value> {
    for message in &mut session.messages {
        let Some(attachments) = message.get_mut("attachments").and_then(Value::as_array_mut) else {
            continue;
        };
        if let Some(attachment) = attachments.iter_mut().find(|attachment| {
            attachment
                .get("id")
                .and_then(Value::as_str)
                .map(|id| id == attachment_id)
                .unwrap_or(false)
        }) {
            return Some(attachment);
        }
    }
    None
}

pub(super) fn attachment_string_field(attachment: &Value, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|key| attachment.get(*key).and_then(Value::as_str))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

pub(super) fn attachment_extract_json_response(
    file_name: String,
    output: ExtractDocumentOutput,
) -> Vec<u8> {
    json_response(
        200,
        &serde_json::json!({
            "fileName": file_name,
            "format": output.format,
            "extractor": output.extractor,
            "size": output.size_bytes,
            "truncated": output.truncated,
            "extractedText": output.extracted_text
        }),
    )
}

pub(super) fn extract_attachment_request(
    request: ExtractAttachmentRequest,
) -> Result<ExtractDocumentOutput, String> {
    let file_name = request.file_name.trim().to_string();
    if file_name.is_empty() {
        return Err("fileName is required".to_string());
    }
    let normalized = normalize_base64(&request.content_base64);
    let encoded = strip_data_url_prefix(&normalized);
    if encoded.is_empty() {
        return Err("contentBase64 is required".to_string());
    }
    if !is_valid_base64(encoded) {
        return Err("Invalid base64 content".to_string());
    }
    let bytes = BASE64_STANDARD
        .decode(encoded)
        .map_err(|_| "Invalid base64 content".to_string())?;
    extract_document_text(
        bytes,
        file_name,
        request.mime_type.filter(|value| !value.trim().is_empty()),
        request.max_chars,
    )
}

pub(super) fn normalize_base64(input: &str) -> String {
    input.chars().filter(|ch| !ch.is_whitespace()).collect()
}

pub(super) fn is_valid_base64(input: &str) -> bool {
    if input.is_empty() || input.len() % 4 == 1 {
        return false;
    }
    input
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '+' | '/' | '='))
}

pub(super) fn extract_document_text(
    bytes: Vec<u8>,
    file_name: String,
    mime_type: Option<String>,
    max_chars: Option<usize>,
) -> Result<ExtractDocumentOutput, String> {
    if bytes.len() > MAX_EXTRACT_INPUT_BYTES {
        return Err(format!(
            "Document is too large ({:.1}MB). Maximum supported size is 50MB.",
            bytes.len() as f64 / 1024.0 / 1024.0
        ));
    }
    let format = detect_document_format(&file_name, mime_type.as_deref());
    let size_bytes = bytes.len();
    let prefer_markitdown = should_prefer_markitdown();
    let mut extractor = "native".to_string();
    let mut extracted_text = if prefer_markitdown {
        match extract_with_markitdown(&bytes, &file_name, mime_type.as_deref())? {
            Some(text) => {
                extractor = "markitdown".to_string();
                text
            }
            None => String::new(),
        }
    } else {
        String::new()
    };

    if extracted_text.is_empty() {
        extracted_text = match format.as_str() {
            "text" => String::from_utf8(bytes.clone())
                .map_err(|_| "Document is not valid UTF-8 text".to_string())?,
            "pdf" => pdf_extract::extract_text_from_mem(&bytes)
                .map_err(|error| format!("Failed to extract PDF text: {error}"))?,
            "docx" => extract_zip_text(&bytes, |name| name == "word/document.xml")?,
            "pptx" => extract_zip_text(&bytes, |name| {
                name.starts_with("ppt/slides/") && name.ends_with(".xml")
            })?,
            "xlsx" => extract_zip_text(&bytes, |name| {
                name == "xl/sharedStrings.xml"
                    || (name.starts_with("xl/worksheets/") && name.ends_with(".xml"))
            })?,
            _ => String::new(),
        };
    }

    if extractor != "markitdown" && should_try_markitdown(&format, &file_name, mime_type.as_deref())
    {
        if let Some(text) = extract_with_markitdown(&bytes, &file_name, mime_type.as_deref())? {
            extracted_text = text;
            extractor = "markitdown".to_string();
        }
    };
    if extracted_text.is_empty() && format == "unknown" {
        return Err("Unsupported document format".to_string());
    }
    let max_chars = max_chars.unwrap_or(DEFAULT_EXTRACT_MAX_CHARS).max(1);
    let (extracted_text, truncated) = clamp_chars(&extracted_text, max_chars);
    Ok(ExtractDocumentOutput {
        file_name,
        format,
        extractor,
        size_bytes,
        truncated,
        extracted_text,
    })
}

pub(super) fn detect_document_format(file_name: &str, mime_type: Option<&str>) -> String {
    let lower_name = file_name.to_ascii_lowercase();
    let mime_type = mime_type.unwrap_or("").to_ascii_lowercase();
    if mime_type.starts_with("text/") {
        return "text".to_string();
    }
    if mime_type == "application/pdf" || lower_name.ends_with(".pdf") {
        return "pdf".to_string();
    }
    if mime_type == "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
        || lower_name.ends_with(".docx")
    {
        return "docx".to_string();
    }
    if mime_type == "application/vnd.openxmlformats-officedocument.presentationml.presentation"
        || lower_name.ends_with(".pptx")
    {
        return "pptx".to_string();
    }
    if mime_type == "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"
        || lower_name.ends_with(".xlsx")
    {
        return "xlsx".to_string();
    }
    for extension in [
        ".txt",
        ".md",
        ".markdown",
        ".json",
        ".yaml",
        ".yml",
        ".csv",
        ".ts",
        ".tsx",
        ".js",
        ".jsx",
        ".html",
        ".css",
        ".xml",
    ] {
        if lower_name.ends_with(extension) {
            return "text".to_string();
        }
    }
    "unknown".to_string()
}

pub(super) fn extract_zip_text<F>(bytes: &[u8], accept: F) -> Result<String, String>
where
    F: Fn(&str) -> bool,
{
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes))
        .map_err(|error| format!("Failed to read document archive: {error}"))?;
    let mut output = String::new();
    for index in 0..archive.len() {
        let mut file = archive
            .by_index(index)
            .map_err(|error| format!("Failed to read document entry: {error}"))?;
        let name = file.name().to_string();
        if !accept(&name) {
            continue;
        }
        let mut xml = String::new();
        file.read_to_string(&mut xml)
            .map_err(|error| format!("Failed to read document XML: {error}"))?;
        if !output.is_empty() {
            output.push('\n');
        }
        output.push_str(&xml_text_content(&xml));
    }
    Ok(output)
}

pub(super) fn xml_text_content(xml: &str) -> String {
    let mut text = String::new();
    let mut in_tag = false;
    for ch in xml.chars() {
        match ch {
            '<' => {
                in_tag = true;
                text.push(' ');
            }
            '>' => in_tag = false,
            _ if !in_tag => text.push(ch),
            _ => {}
        }
    }
    decode_xml_entities(&collapse_whitespace(&text))
}

pub(super) fn collapse_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub(super) fn decode_xml_entities(text: &str) -> String {
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

pub(super) fn clamp_chars(text: &str, max_chars: usize) -> (String, bool) {
    for (count, (index, _)) in text.char_indices().enumerate() {
        if count == max_chars {
            return (text[..index].to_string(), true);
        }
    }
    (text.to_string(), false)
}

pub(super) fn serve_session_attachment(session: &SessionRecord, tail: &str) -> Vec<u8> {
    let Some(attachment_id) = tail
        .strip_prefix("attachments/")
        .and_then(|rest| rest.split('/').next())
        .map(percent_decode_component)
        .filter(|value| !value.is_empty())
    else {
        return json_response(404, &serde_json::json!({ "error": "Attachment not found" }));
    };
    let Some(attachment) = session_attachments(session).into_iter().find(|attachment| {
        attachment
            .get("id")
            .and_then(Value::as_str)
            .map(|id| id == attachment_id)
            .unwrap_or(false)
    }) else {
        return json_response(404, &serde_json::json!({ "error": "Attachment not found" }));
    };
    let Some(content) = attachment.get("content").and_then(Value::as_str) else {
        return json_response(
            404,
            &serde_json::json!({ "error": "Attachment content not available" }),
        );
    };
    let encoded = content
        .split_once(',')
        .map(|(_, value)| value)
        .unwrap_or(content);
    let Ok(bytes) = BASE64_STANDARD.decode(encoded) else {
        return json_response(
            400,
            &serde_json::json!({ "error": "Attachment content is not valid base64" }),
        );
    };
    let mime = attachment
        .get("mimeType")
        .or_else(|| attachment.get("mime_type"))
        .and_then(Value::as_str)
        .unwrap_or("application/octet-stream");
    response_with_no_store(200, mime, &bytes)
}

pub(super) fn session_artifacts_value(session: &SessionRecord) -> Value {
    let artifacts = reconstruct_session_artifacts(session)
        .into_iter()
        .map(|(filename, content)| {
            serde_json::json!({
                "filename": filename,
                "content": content
            })
        })
        .collect::<Vec<_>>();
    serde_json::json!({ "sessionId": session.id, "artifacts": artifacts })
}

pub(super) fn session_artifact_access_response(
    head: &RequestHead,
    session: &SessionRecord,
) -> Vec<u8> {
    let Some(actions) = artifact_access_actions(head.query.get("actions")) else {
        return json_response(
            400,
            &serde_json::json!({ "error": "actions must include view, file, events, or zip" }),
        );
    };
    let filename = head
        .query
        .get("filename")
        .map(|value| percent_decode_component(value))
        .filter(|value| !value.trim().is_empty());
    let ttl_ms = env::var("MAESTRO_ARTIFACT_ACCESS_TTL_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(5 * 60 * 1000);
    let expires_at = now_millis().saturating_add(ttl_ms);
    let expires_at_iso =
        (chrono::Utc::now() + chrono::Duration::milliseconds(ttl_ms as i64)).to_rfc3339();
    let token_payload = format!(
        "{}:{}:{}:{}",
        session.id,
        filename.as_deref().unwrap_or(""),
        actions.join(","),
        expires_at
    );
    json_response(
        200,
        &serde_json::json!({
            "sessionId": session.id,
            "scope": Value::Null,
            "filename": filename,
            "actions": actions,
            "expiresAt": expires_at,
            "expiresAtIso": expires_at_iso,
            "token": BASE64_STANDARD.encode(token_payload)
        }),
    )
}

pub(super) fn artifact_access_actions(raw_actions: Option<&String>) -> Option<Vec<String>> {
    let decoded = raw_actions.map(|value| percent_decode_component(value))?;
    let mut actions = Vec::new();
    for action in decoded.split(',').map(str::trim) {
        if matches!(action, "view" | "file" | "events" | "zip")
            && !actions.iter().any(|existing| existing == action)
        {
            actions.push(action.to_string());
        }
    }
    if actions.is_empty() {
        None
    } else {
        Some(actions)
    }
}

pub(super) fn serve_session_artifact(
    head: &RequestHead,
    session: &SessionRecord,
    tail: &str,
) -> Vec<u8> {
    let Some(rest) = tail.strip_prefix("artifacts/") else {
        return json_response(404, &serde_json::json!({ "error": "Artifact not found" }));
    };
    let is_view = rest.ends_with("/view");
    let filename = percent_decode_component(rest.strip_suffix("/view").unwrap_or(rest));
    let artifacts = reconstruct_session_artifacts(session);
    let Some(content) = artifacts.get(&filename) else {
        return json_response(404, &serde_json::json!({ "error": "Artifact not found" }));
    };
    let mime = mime_for_path(Path::new(&filename));
    if is_view && mime.starts_with("text/html") {
        return sandboxed_artifact_viewer(&filename, content);
    }
    if query_flag(head, "download") || query_flag(head, "standalone") {
        return response_with_extra_headers(
            200,
            mime,
            content.as_bytes(),
            &format!(
                "Content-Disposition: {}\r\nCache-Control: no-store, no-cache, must-revalidate\r\n",
                attachment_content_disposition(&filename)
            ),
        );
    }
    response_with_no_store(200, mime, content.as_bytes())
}

pub(super) fn sandboxed_artifact_viewer(filename: &str, content: &str) -> Vec<u8> {
    let title = html_escape(filename);
    let srcdoc = html_escape(content);
    let body = format!(
        r#"<!doctype html>
<html>
<head>
<meta charset="utf-8">
<title>{title}</title>
<style>
html,body,iframe{{margin:0;width:100%;height:100%;border:0;background:white;}}
</style>
</head>
<body>
<iframe title="{title}" sandbox="allow-scripts allow-forms allow-popups allow-downloads" srcdoc="{srcdoc}"></iframe>
</body>
</html>"#
    );
    response_with_extra_headers(
        200,
        "text/html; charset=utf-8",
        body.as_bytes(),
        "Cache-Control: no-store, no-cache, must-revalidate\r\nContent-Security-Policy: default-src 'none'; frame-src 'self'; style-src 'unsafe-inline'; base-uri 'none'\r\n",
    )
}

pub(super) fn html_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

pub(super) fn serve_session_artifacts_zip(session: &SessionRecord) -> Vec<u8> {
    let mut artifacts = reconstruct_session_artifacts(session)
        .into_iter()
        .collect::<Vec<_>>();
    artifacts.sort_by(|left, right| left.0.cmp(&right.0));
    let zip = match build_store_zip(
        artifacts
            .iter()
            .map(|(name, content)| (name.as_str(), content.as_bytes())),
    ) {
        Ok(zip) => zip,
        Err(error) => return json_response(500, &serde_json::json!({ "error": error })),
    };
    response_with_extra_headers(
        200,
        "application/zip",
        &zip,
        &format!(
            "Content-Disposition: {}\r\nCache-Control: no-store, no-cache, must-revalidate\r\n",
            attachment_content_disposition(&format!("artifacts-{}.zip", session.id))
        ),
    )
}

pub(super) fn build_store_zip<'a, I>(entries: I) -> Result<Vec<u8>, String>
where
    I: IntoIterator<Item = (&'a str, &'a [u8])>,
{
    let entries = entries.into_iter().collect::<Vec<_>>();
    if entries.len() > u16::MAX as usize {
        return Err("Too many artifacts to archive".to_string());
    }

    let mut output = Vec::new();
    let mut central_directory = Vec::new();
    for (name, content) in &entries {
        let name_bytes = name.as_bytes();
        if name_bytes.len() > u16::MAX as usize || content.len() > u32::MAX as usize {
            return Err("Artifact archive entry is too large".to_string());
        }
        let local_header_offset = output.len();
        if local_header_offset > u32::MAX as usize {
            return Err("Artifact archive is too large".to_string());
        }
        let crc = crc32(content);
        push_u32_le(&mut output, 0x0403_4b50);
        push_u16_le(&mut output, 20);
        push_u16_le(&mut output, 0);
        push_u16_le(&mut output, 0);
        push_u16_le(&mut output, 0);
        push_u16_le(&mut output, 0);
        push_u32_le(&mut output, crc);
        push_u32_le(&mut output, content.len() as u32);
        push_u32_le(&mut output, content.len() as u32);
        push_u16_le(&mut output, name_bytes.len() as u16);
        push_u16_le(&mut output, 0);
        output.extend_from_slice(name_bytes);
        output.extend_from_slice(content);

        push_u32_le(&mut central_directory, 0x0201_4b50);
        push_u16_le(&mut central_directory, 20);
        push_u16_le(&mut central_directory, 20);
        push_u16_le(&mut central_directory, 0);
        push_u16_le(&mut central_directory, 0);
        push_u16_le(&mut central_directory, 0);
        push_u16_le(&mut central_directory, 0);
        push_u32_le(&mut central_directory, crc);
        push_u32_le(&mut central_directory, content.len() as u32);
        push_u32_le(&mut central_directory, content.len() as u32);
        push_u16_le(&mut central_directory, name_bytes.len() as u16);
        push_u16_le(&mut central_directory, 0);
        push_u16_le(&mut central_directory, 0);
        push_u16_le(&mut central_directory, 0);
        push_u16_le(&mut central_directory, 0);
        push_u32_le(&mut central_directory, 0);
        push_u32_le(&mut central_directory, local_header_offset as u32);
        central_directory.extend_from_slice(name_bytes);
    }

    let central_directory_offset = output.len();
    let central_directory_size = central_directory.len();
    if central_directory_offset > u32::MAX as usize || central_directory_size > u32::MAX as usize {
        return Err("Artifact archive is too large".to_string());
    }
    output.extend_from_slice(&central_directory);
    push_u32_le(&mut output, 0x0605_4b50);
    push_u16_le(&mut output, 0);
    push_u16_le(&mut output, 0);
    push_u16_le(&mut output, entries.len() as u16);
    push_u16_le(&mut output, entries.len() as u16);
    push_u32_le(&mut output, central_directory_size as u32);
    push_u32_le(&mut output, central_directory_offset as u32);
    push_u16_le(&mut output, 0);
    Ok(output)
}

pub(super) fn push_u16_le(output: &mut Vec<u8>, value: u16) {
    output.extend_from_slice(&value.to_le_bytes());
}

pub(super) fn push_u32_le(output: &mut Vec<u8>, value: u32) {
    output.extend_from_slice(&value.to_le_bytes());
}

pub(super) fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for byte in bytes {
        crc ^= *byte as u32;
        for _ in 0..8 {
            let mask = 0u32.wrapping_sub(crc & 1);
            crc = (crc >> 1) ^ (0xedb8_8320 & mask);
        }
    }
    !crc
}

pub(super) fn attachment_content_disposition(filename: &str) -> String {
    let safe_filename = filename
        .chars()
        .map(|ch| match ch {
            '"' | '\\' | '\r' | '\n' => '_',
            _ => ch,
        })
        .collect::<String>();
    format!("attachment; filename=\"{safe_filename}\"")
}

pub(super) fn reconstruct_session_artifacts(session: &SessionRecord) -> HashMap<String, String> {
    let mut artifacts = HashMap::new();
    for message in &session.messages {
        let Some(tools) = message.get("tools").and_then(Value::as_array) else {
            continue;
        };
        for tool in tools {
            if tool.get("name").and_then(Value::as_str) != Some("artifacts") {
                continue;
            }
            if tool.get("status").and_then(Value::as_str) != Some("completed") {
                continue;
            }
            if tool
                .get("result")
                .and_then(|result| result.get("isError"))
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                continue;
            }
            let Some(args) = tool.get("args") else {
                continue;
            };
            let command = args.get("command").and_then(Value::as_str).unwrap_or("");
            let Some(filename) = args.get("filename").and_then(Value::as_str) else {
                continue;
            };
            match command {
                "create" | "rewrite" => {
                    artifacts.insert(
                        filename.to_string(),
                        args.get("content")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                    );
                }
                "update" => {
                    if let (Some(current), Some(old), Some(new)) = (
                        artifacts.get_mut(filename),
                        args.get("old_str").and_then(Value::as_str),
                        args.get("new_str").and_then(Value::as_str),
                    ) {
                        *current = current.replacen(old, new, 1);
                    }
                }
                "delete" => {
                    artifacts.remove(filename);
                }
                _ => {}
            }
        }
    }
    artifacts
}

pub(super) fn session_summary_value(session: &SessionRecord) -> Value {
    let mut value = serde_json::json!({
        "id": session.id,
        "title": session.title,
        "createdAt": session.created_at,
        "updatedAt": session.updated_at,
        "messageCount": session.message_count
    });
    if let Some(favorite) = session.favorite {
        value["favorite"] = Value::Bool(favorite);
    }
    if !session.tags.is_empty() {
        value["tags"] = serde_json::json!(session.tags);
    }
    if let Some(log_group_id) = &session.log_group_id {
        value["logGroupId"] = Value::String(log_group_id.clone());
    }
    value
}

pub(super) fn session_full_value(session: &SessionRecord) -> Value {
    let mut value = session_summary_value(session);
    value["messages"] = Value::Array(
        session
            .messages
            .iter()
            .map(public_session_message)
            .collect(),
    );
    value
}

pub(super) fn public_session_message(message: &Value) -> Value {
    let mut message = message.clone();
    if let Some(object) = message.as_object_mut() {
        if let Some(attachments) = object.get_mut("attachments").and_then(Value::as_array_mut) {
            for attachment in attachments {
                sanitize_attachment_for_read(attachment);
            }
        }
    }
    message
}

pub(super) fn sanitize_attachment_for_read(attachment: &mut Value) {
    let Some(object) = attachment.as_object_mut() else {
        return;
    };
    let had_inline_content = object.remove("content").is_some()
        || object.remove("contentBase64").is_some()
        || object.remove("content_base64").is_some();
    if had_inline_content && !object.contains_key("contentOmitted") {
        object.insert("contentOmitted".to_string(), Value::Bool(true));
    }
}

pub(super) fn new_session_id() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0);
    let counter = SESSION_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("rust-session-{now}-{counter}")
}

pub(super) fn pending_request_resume_value(request_id: &str, payload: &Value) -> Value {
    let kind = payload
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or_else(|| {
            if payload.get("decision").is_some() {
                "approval"
            } else if payload.get("action").is_some() {
                "tool_retry"
            } else {
                "client_tool"
            }
        });
    let resolution = match kind {
        "approval" => payload
            .get("decision")
            .and_then(Value::as_str)
            .unwrap_or("approved"),
        "tool_retry" => match payload.get("action").and_then(Value::as_str) {
            Some("retry") => "retried",
            Some("skip") => "skipped",
            Some("abort") => "aborted",
            _ => "completed",
        },
        "user_input" => "answered",
        _ if payload
            .get("isError")
            .and_then(Value::as_bool)
            .unwrap_or(false) =>
        {
            "failed"
        }
        _ => "completed",
    };
    let mut request = serde_json::json!({
        "id": request_id,
        "kind": kind,
        "resolution": resolution,
        "source": "local"
    });
    if let Some(session_id) = payload.get("sessionId").and_then(Value::as_str) {
        request["sessionId"] = Value::String(session_id.to_string());
    }
    serde_json::json!({ "success": true, "request": request })
}

pub(super) fn pending_tool_response_from_payload(payload: &Value) -> (bool, Option<ToolResult>) {
    if payload
        .get("kind")
        .and_then(Value::as_str)
        .is_some_and(|kind| kind == "approval")
        || payload.get("decision").is_some()
    {
        let decision = payload
            .get("decision")
            .and_then(Value::as_str)
            .unwrap_or("approved");
        return (!matches!(decision, "denied" | "rejected" | "abort"), None);
    }

    let output = payload
        .get("content")
        .map(|content| {
            content
                .as_str()
                .map(ToString::to_string)
                .unwrap_or_else(|| content.to_string())
        })
        .unwrap_or_default();
    if payload
        .get("isError")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        (true, Some(ToolResult::failure(output)))
    } else {
        (true, Some(ToolResult::success(output)))
    }
}

#[cfg(test)]
mod lineage_tests {
    use super::*;

    #[test]
    fn log_group_ids_accept_stable_lineage_and_reject_unsafe_values() {
        let create: SessionCreateRequest =
            serde_json::from_str(r#"{"tags":["reviewed"],"logGroupId":"release:42"}"#).unwrap();
        assert_eq!(create.tags, Some(vec!["reviewed".to_string()]));
        assert_eq!(create.log_group_id.as_deref(), Some("release:42"));

        assert_eq!(
            normalize_log_group_id(Some(" deploy:release/42 ".to_string()))
                .unwrap()
                .as_deref(),
            Some("deploy:release/42")
        );
        assert!(normalize_log_group_id(Some("bad group".to_string())).is_err());
        assert!(normalize_log_group_id(Some("x".repeat(129))).is_err());

        let absent: SessionUpdateRequest = serde_json::from_str("{}").unwrap();
        assert_eq!(absent.log_group_id, None);
        let clear: SessionUpdateRequest = serde_json::from_str(r#"{"logGroupId":null}"#).unwrap();
        assert_eq!(clear.log_group_id, Some(None));
        let set: SessionUpdateRequest =
            serde_json::from_str(r#"{"logGroupId":"release:42"}"#).unwrap();
        assert_eq!(set.log_group_id, Some(Some("release:42".to_string())));
    }
}

//! Session-scoped projection of the existing native background process owner.
//! No gateway task manager or inferred foreground execution state is created.
use super::*;
use maestro_local_host::tools::background_tasks::{
    self, BackgroundTaskScope, BackgroundTaskStatus,
};

pub(super) fn scope_from_session(session: &SessionRecord) -> Option<BackgroundTaskScope> {
    let scope = BackgroundTaskScope {
        session_id: session.id.clone(),
        session_generation: session.created_at.clone(),
        owner: session.owner.clone(),
        organization_id: session.organization_id.clone(),
        workspace_id: session.workspace_id.clone(),
    };
    scope.is_valid().then_some(scope)
}

/// Call before constructing the local execution host, with the authorized session.
pub(super) async fn access_for_session(
    state: &AppState,
    session_id: Option<&str>,
    auth: &AuthContext,
) -> background_tasks::BackgroundTaskAccess {
    scope_for_session(state, session_id, auth)
        .await
        .map(background_tasks::BackgroundTaskAccess::Scoped)
        .unwrap_or(background_tasks::BackgroundTaskAccess::Denied)
}

pub(super) async fn scope_for_session(
    state: &AppState,
    session_id: Option<&str>,
    auth: &AuthContext,
) -> Option<BackgroundTaskScope> {
    let store = state.sessions.lock().await;
    let session = store.sessions.get(session_id?)?;
    if !session_visible_to_auth(session, auth) {
        return None;
    }
    scope_from_session(session)
}

fn task_values(session: &SessionRecord) -> Result<Vec<Value>, String> {
    let scope = scope_from_session(session).ok_or("Session background scope unavailable")?;
    let tasks = background_tasks::list_scoped(&scope)?;
    let mut values = Vec::new();
    let monitors = background_tasks::list_monitors();
    for task in tasks {
        let status = match task.status {
            BackgroundTaskStatus::Running => "running",
            BackgroundTaskStatus::Exited => "exited",
            BackgroundTaskStatus::Failed => "failed",
            BackgroundTaskStatus::Stopped => "stopped",
        };
        values.push(serde_json::json!({
            "id": task.id, "kind": "command", "label": task.command,
            "status": status, "completionSequence": task.completion_sequence,
            "exitCode": task.exit_code,
        }));
        // Monitor ownership is inherited from its accepted command. These are
        // output watchers; they do not claim to schedule another agent turn.
        for monitor in monitors.iter().filter(|monitor| monitor.task_id == task.id) {
            if status == "running" {
                values.push(serde_json::json!({
                    "id": monitor.id, "kind": "monitor", "label": monitor.pattern,
                    "status": "running", "completionSequence": 0, "exitCode": null,
                }));
            }
        }
    }
    Ok(values)
}

fn latest_completion(tasks: &[Value]) -> u64 {
    tasks
        .iter()
        .filter_map(|task| task["completionSequence"].as_u64())
        .max()
        .unwrap_or(0)
}

fn summary(tasks: &[Value], read_cursor: u64) -> Value {
    serde_json::json!({
        "runningCount": tasks.iter().filter(|task| task["status"] == "running").count(),
        "unseenCompletionCount": tasks.iter().filter(|task|
            task["completionSequence"].as_u64().is_some_and(|sequence| sequence > read_cursor)).count(),
        "latestCompletionSequence": latest_completion(tasks),
        "readCursor": read_cursor,
    })
}

pub(super) fn session_background_summary(session: &SessionRecord) -> Option<Value> {
    let tasks = task_values(session).ok()?;
    (!tasks.is_empty()).then(|| summary(&tasks, session.background_read_cursor))
}

pub(super) async fn handle_background_get(
    state: &AppState,
    id: &str,
    auth: &AuthContext,
) -> Vec<u8> {
    let store = state.sessions.lock().await;
    let Some(session) = store
        .sessions
        .get(id)
        .filter(|session| session_visible_to_auth(session, auth))
    else {
        return json_response(404, &serde_json::json!({ "error": "Session not found" }));
    };
    match task_values(session) {
        Ok(tasks) => json_response(
            200,
            &serde_json::json!({
                "sessionId": id, "tasks": tasks, "summary": summary(&tasks, session.background_read_cursor),
            }),
        ),
        Err(error) => json_response(503, &serde_json::json!({ "error": error })),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ReadRequest {
    through_sequence: u64,
}

fn next_read_cursor(requested: u64, recorded: u64, previous: u64) -> Result<u64, &'static str> {
    if requested > recorded.max(previous) {
        return Err("Read cursor exceeds recorded completion");
    }
    Ok(previous.max(requested))
}

pub(super) async fn handle_background_read(
    stream: &mut TcpStream,
    initial: &mut Vec<u8>,
    head: &RequestHead,
    state: &AppState,
    id: &str,
    auth: &AuthContext,
) -> Vec<u8> {
    let body = match read_request_body(stream, initial, head).await {
        Ok(body) => body,
        Err(error) => return json_response(400, &serde_json::json!({ "error": error })),
    };
    let request = match serde_json::from_slice::<ReadRequest>(&body) {
        Ok(request) => request,
        Err(_) => {
            return json_response(
                400,
                &serde_json::json!({ "error": "Invalid background read cursor" }),
            );
        }
    };
    let mut store = state.sessions.lock().await;
    let Some(session) = store
        .sessions
        .get_mut(id)
        .filter(|session| session_visible_to_auth(session, auth))
    else {
        return json_response(404, &serde_json::json!({ "error": "Session not found" }));
    };
    let tasks = match task_values(session) {
        Ok(tasks) => tasks,
        Err(error) => return json_response(503, &serde_json::json!({ "error": error })),
    };
    let previous = session.background_read_cursor;
    session.background_read_cursor = match next_read_cursor(
        request.through_sequence,
        latest_completion(&tasks),
        previous,
    ) {
        Ok(cursor) => cursor,
        Err(error) => return json_response(400, &serde_json::json!({ "error": error })),
    };
    let read_cursor = session.background_read_cursor;
    let snapshot = store.clone();
    if let Err(error) = persist_session_store_snapshot(state, &snapshot).await {
        if let Some(session) = store.sessions.get_mut(id) {
            session.background_read_cursor = previous;
        }
        return json_response(
            503,
            &serde_json::json!({ "error": format!("Could not save background read cursor: {error}") }),
        );
    }
    json_response(
        200,
        &serde_json::json!({ "sessionId": id, "readCursor": read_cursor }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn completion_is_unseen_only_after_its_saved_cursor() {
        let tasks = vec![
            serde_json::json!({"status":"running", "completionSequence":0}),
            serde_json::json!({"status":"exited", "completionSequence":8}),
            serde_json::json!({"status":"failed", "completionSequence":9}),
        ];
        assert_eq!(summary(&tasks, 8)["unseenCompletionCount"], 1);
        assert_eq!(summary(&tasks, 9)["unseenCompletionCount"], 0);
        assert_eq!(summary(&tasks, 9)["runningCount"], 1);
    }
    #[test]
    fn cursor_cannot_skip_future_completions_or_rewind_read_state() {
        assert_eq!(next_read_cursor(8, 9, 7), Ok(8));
        assert_eq!(next_read_cursor(6, 9, 8), Ok(8));
        assert!(next_read_cursor(10, 9, 8).is_err());
        // Retention may prune the read event, without making the cursor rewind.
        assert_eq!(next_read_cursor(9, 0, 9), Ok(9));
    }

    fn owned_fixture() -> (AppState, AuthContext) {
        let mut session = crate::tests::test_session_record("session-a");
        session.owner = Some("alice".into());
        session.organization_id = Some("org-a".into());
        session.workspace_id = Some("workspace-a".into());
        let auth = AuthContext {
            subject: session.owner.clone(),
            organization_id: session.organization_id.clone(),
            workspace_id: session.workspace_id.clone(),
            ..AuthContext::default()
        };
        (
            crate::tests::test_app_state_with_sessions(HashMap::from([(
                session.id.clone(),
                session,
            )])),
            auth,
        )
    }

    #[tokio::test]
    async fn roster_and_acceptance_scope_refuse_other_owners_tenants_and_deleted_sessions() {
        let (state, auth) = owned_fixture();
        let accepted = scope_for_session(&state, Some("session-a"), &auth)
            .await
            .unwrap();
        for field in ["owner", "organization", "workspace"] {
            let mut other = auth.clone();
            match field {
                "owner" => other.subject = Some("bob".into()),
                "organization" => other.organization_id = Some("org-b".into()),
                _ => other.workspace_id = Some("workspace-b".into()),
            }
            assert!(
                scope_for_session(&state, Some("session-a"), &other)
                    .await
                    .is_none()
            );
            let response = handle_background_get(&state, "session-a", &other).await;
            assert!(
                String::from_utf8(response)
                    .unwrap()
                    .starts_with("HTTP/1.1 404")
            );
        }
        state.sessions.lock().await.sessions.remove("session-a");
        assert!(
            scope_for_session(&state, Some("session-a"), &auth)
                .await
                .is_none()
        );
        assert!(
            String::from_utf8(handle_background_get(&state, "session-a", &auth).await)
                .unwrap()
                .starts_with("HTTP/1.1 404")
        );
        let mut recreated = crate::tests::test_session_record("session-a");
        recreated.owner = auth.subject.clone();
        recreated.organization_id = auth.organization_id.clone();
        recreated.workspace_id = auth.workspace_id.clone();
        recreated.created_at = "new-incarnation".into();
        state
            .sessions
            .lock()
            .await
            .sessions
            .insert("session-a".into(), recreated);
        let new_scope = scope_for_session(&state, Some("session-a"), &auth)
            .await
            .unwrap();
        assert_ne!(new_scope, accepted);
    }

    #[tokio::test]
    async fn wrong_owner_cannot_acknowledge_another_sessions_completions() {
        let (state, mut auth) = owned_fixture();
        auth.subject = Some("bob".into());
        let body = "{\"throughSequence\":9}";
        let mut initial = format!("PATCH /api/sessions/session-a/background-read HTTP/1.1\r\nContent-Length: {}\r\n\r\n{body}", body.len()).into_bytes();
        let head = parse_request_head(&initial).unwrap();
        let (_client, mut server) = crate::tests::tcp_stream_pair().await;
        let response =
            handle_background_read(&mut server, &mut initial, &head, &state, "session-a", &auth)
                .await;
        assert!(
            String::from_utf8(response)
                .unwrap()
                .starts_with("HTTP/1.1 404")
        );
        assert_eq!(
            state.sessions.lock().await.sessions["session-a"].background_read_cursor,
            0
        );
    }
}

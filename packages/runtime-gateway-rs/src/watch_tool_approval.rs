//! Gateway watch tools use the same resumable approval boundary as local tools.
//! The intermediary channel ignores client-supplied results: approval authorizes
//! the exact captured call, whose result is produced by the gateway owner.
use super::*;

pub(crate) struct WatchToolRequest<'a> {
    pub(crate) call_id: &'a str,
    pub(crate) tool: &'a str,
    pub(crate) args: &'a Value,
}

pub(crate) async fn dispatch(
    state: &AppState,
    auth: &AuthContext,
    session: Option<&str>,
    request: WatchToolRequest<'_>,
    native_sender: PendingToolResponseSender,
    mode: &str,
) -> Option<Value> {
    let WatchToolRequest {
        call_id,
        tool,
        args,
    } = request;
    if mode == "auto" {
        let result =
            match crate::pull_request_watch::capture_action_scope(state, auth, session, tool, args)
                .await
            {
                Some(scope) => {
                    crate::pull_request_watch::handle_scoped_tool(
                        state, auth, session, tool, args, scope,
                    )
                    .await
                }
                None => ToolResult::failure("Session not found"),
            };
        let _ = native_sender.send((
            call_id.into(),
            true,
            Some(result),
            ExecutionSource::RemoteClient,
            None,
        ));
        return None;
    }
    if mode == "fail" {
        let _ = native_sender.send((
            call_id.into(),
            false,
            None,
            ExecutionSource::RemoteClient,
            None,
        ));
        return Some(approval_blocked_tool_event(call_id, tool));
    }
    let accepted = session_owner(state, session, auth).await;
    let Some(accepted) = accepted else {
        let _ = native_sender.send((
            call_id.into(),
            false,
            None,
            ExecutionSource::RemoteClient,
            None,
        ));
        return Some(approval_blocked_tool_event(call_id, tool));
    };
    let scope =
        crate::pull_request_watch::capture_action_scope(state, auth, session, tool, args).await;
    let (approval_sender, mut approvals) = mpsc::unbounded_channel::<ToolResponseMessage>();
    state
        .pending_tool_responses
        .lock()
        .await
        .insert(call_id.into(), approval_sender);
    if let Some(owner) = PendingToolResponseOwner::for_request(session, auth) {
        state
            .pending_tool_response_sessions
            .lock()
            .await
            .insert(call_id.into(), owner);
    }
    let state = state.clone();
    let auth = auth.clone();
    let session = session.map(str::to_owned);
    let call_id_owned = call_id.to_owned();
    let tool_owned = tool.to_owned();
    let args_owned = args.clone();
    tokio::spawn(async move {
        let Some((_, approved, _, _, _)) = approvals.recv().await else {
            return;
        };
        let approved = approved
            && scope.is_some()
            && session_owner(&state, session.as_deref(), &auth)
                .await
                .as_ref()
                == Some(&accepted);
        let result = if approved {
            Some(
                crate::pull_request_watch::handle_scoped_tool(
                    &state,
                    &auth,
                    session.as_deref(),
                    &tool_owned,
                    &args_owned,
                    scope.expect("approved request has captured scope"),
                )
                .await,
            )
        } else {
            None
        };
        let _ = native_sender.send((
            call_id_owned,
            approved,
            result,
            ExecutionSource::RemoteClient,
            None,
        ));
    });
    Some(serde_json::json!({
        "type":"action_approval_required", "request": {
            "id":call_id, "toolName":tool, "args":args,
            "reason":"PR watch action requires approval"
        }
    }))
}

async fn session_owner(state: &AppState, id: Option<&str>, auth: &AuthContext) -> Option<Value> {
    let store = state.sessions.lock().await;
    let session = store.sessions.get(id?)?;
    session_visible_to_auth(session, auth).then(|| {
        serde_json::json!({
            "id":session.id, "createdAt":session.created_at, "owner":session.owner,
            "organizationId":session.organization_id, "workspaceId":session.workspace_id,
        })
    })
}

#[cfg(test)]
#[path = "watch_tool_approval_tests.rs"]
mod tests;

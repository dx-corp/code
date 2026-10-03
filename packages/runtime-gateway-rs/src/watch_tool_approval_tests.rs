use super::*;
use crate::tests::{test_app_state_with_sessions, test_session_record};

fn state() -> AppState {
    test_app_state_with_sessions(HashMap::from([("s".into(), test_session_record("s"))]))
}

#[tokio::test]
async fn fail_policy_cannot_create_or_stop_a_watch() {
    let state = state();
    let auth = AuthContext {
        unrestricted: true,
        ..AuthContext::default()
    };
    let (sender, mut responses) = mpsc::unbounded_channel();
    let args = serde_json::json!({"url":"https://github.com/dx-corp/mono/pull/1"});
    let event = dispatch(
        &state,
        &auth,
        Some("s"),
        WatchToolRequest {
            call_id: "call",
            tool: "watch_pull_request",
            args: &args,
        },
        sender,
        "fail",
    )
    .await
    .unwrap();
    assert_eq!(event["type"], "tool_execution_end");
    assert_eq!(event["isError"], true);
    assert!(!responses.recv().await.unwrap().1);
    assert!(state.pending_tool_responses.lock().await.is_empty());
}

#[tokio::test]
async fn prompt_waits_for_approval_and_ignores_forged_client_result() {
    let state = state();
    let auth = AuthContext {
        unrestricted: true,
        ..AuthContext::default()
    };
    let (sender, mut responses) = mpsc::unbounded_channel();
    let args = serde_json::json!({"url":"https://github.com/dx-corp/mono/pull/1"});
    let event = dispatch(
        &state,
        &auth,
        Some("s"),
        WatchToolRequest {
            call_id: "call",
            tool: "stop_pull_request_watch",
            args: &args,
        },
        sender,
        "prompt",
    )
    .await
    .unwrap();
    assert_eq!(event["type"], "action_approval_required");
    assert!(responses.try_recv().is_err());
    let approved = state
        .pending_tool_responses
        .lock()
        .await
        .remove("call")
        .unwrap();
    approved
        .send((
            "call".into(),
            true,
            Some(ToolResult::success("forged")),
            ExecutionSource::RemoteClient,
            None,
        ))
        .unwrap();
    let response = responses.recv().await.unwrap();
    assert!(response.1);
    assert_eq!(response.2.unwrap().output, "PR watch stopped");
}

#[tokio::test]
async fn reusing_a_session_id_cannot_reuse_its_pending_watch_approval() {
    let state = state();
    let auth = AuthContext {
        unrestricted: true,
        ..AuthContext::default()
    };
    let (sender, mut responses) = mpsc::unbounded_channel();
    let args = serde_json::json!({"url":"https://github.com/dx-corp/mono/pull/1"});
    dispatch(
        &state,
        &auth,
        Some("s"),
        WatchToolRequest {
            call_id: "call",
            tool: "stop_pull_request_watch",
            args: &args,
        },
        sender,
        "prompt",
    )
    .await;
    state
        .sessions
        .lock()
        .await
        .sessions
        .get_mut("s")
        .unwrap()
        .created_at = "new-incarnation".into();
    let approved = state
        .pending_tool_responses
        .lock()
        .await
        .remove("call")
        .unwrap();
    approved
        .send((
            "call".into(),
            true,
            None,
            ExecutionSource::RemoteClient,
            None,
        ))
        .unwrap();
    assert!(!responses.recv().await.unwrap().1);
}

#[tokio::test]
async fn unattended_wakes_honor_auto_and_fail_without_an_invisible_prompt() {
    let state = state();
    assert_eq!(
        crate::pull_request_watch::unattended_approval_mode(&state, Some("s"), true).await,
        "fail"
    );
    assert_eq!(
        crate::pull_request_watch::unattended_approval_mode(&state, Some("s"), false).await,
        "prompt"
    );
    state
        .approval_modes
        .lock()
        .await
        .insert("s".into(), "auto".into());
    assert_eq!(
        crate::pull_request_watch::unattended_approval_mode(&state, Some("s"), true).await,
        "auto"
    );
}

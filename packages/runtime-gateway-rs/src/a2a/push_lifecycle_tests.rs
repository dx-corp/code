use super::*;
use crate::tests::{ENV_LOCK, SavedEnv, test_app_state_with_sessions};
use tokio::io::AsyncWriteExt;

#[tokio::test]
async fn callback_owner_remains_counted_until_its_http_request_finishes() {
    let _lock = ENV_LOCK.lock().await;
    const KEYS: &[&str] = &[
        "MAESTRO_A2A_PUSH_ALLOW_PRIVATE",
        "MAESTRO_A2A_PUSH_ALLOW_INSECURE",
        "MAESTRO_A2A_PUSH_DISABLE_DELIVERY",
    ];
    let _environment = SavedEnv::capture(KEYS);
    std::env::set_var(KEYS[0], "1");
    std::env::set_var(KEYS[1], "1");
    std::env::set_var(KEYS[2], "0");
    let callback = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let config = serde_json::json!({"id":"callback", "taskId":"task", "url":format!("http://{}/callback", callback.local_addr().unwrap())});
    let task = a2a_task_with_push_notification_config(
        &serde_json::json!({"id":"task", "status":{"state":"TASK_STATE_WORKING"}}),
        config,
    )
    .unwrap();
    let state = test_app_state_with_sessions(std::collections::HashMap::new());
    dispatch_a2a_push_notifications(&task, &state);
    let (mut stream, _) = tokio::time::timeout(Duration::from_secs(2), callback.accept())
        .await
        .unwrap()
        .unwrap();
    let mut initial = Vec::new();
    let head = crate::http::read_request_head(&mut stream, &mut initial)
        .await
        .unwrap();
    let payload = crate::http::read_request_body(&mut stream, &mut initial, &head)
        .await
        .unwrap();
    assert!(
        serde_json::from_slice::<Value>(&payload)
            .unwrap()
            .get("statusUpdate")
            .is_some()
    );
    assert_eq!(state.lifecycle().active(), 1);
    state.lifecycle().begin_drain();
    assert!(state.lifecycle().admit().is_err());
    stream
        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    stream.shutdown().await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), state.lifecycle().idle())
        .await
        .unwrap();
}

#[tokio::test]
async fn cancellation_callback_continues_during_drain_after_existing_task_authorization() {
    let _lock = ENV_LOCK.lock().await;
    const KEYS: &[&str] = &[
        "MAESTRO_A2A_PUSH_ALLOW_PRIVATE",
        "MAESTRO_A2A_PUSH_ALLOW_INSECURE",
        "MAESTRO_A2A_PUSH_DISABLE_DELIVERY",
    ];
    let _environment = SavedEnv::capture(KEYS);
    std::env::set_var(KEYS[0], "1");
    std::env::set_var(KEYS[1], "1");
    std::env::set_var(KEYS[2], "0");
    let callback = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let config = serde_json::json!({"id":"callback", "taskId":"task", "url":format!("http://{}/callback", callback.local_addr().unwrap())});
    let task = a2a_task_with_push_notification_config(
        &serde_json::json!({"id":"task", "metadata":{"ownerSubject":"owner"}, "status":{"state":"TASK_STATE_WORKING"}}),
        config,
    )
    .unwrap();
    let state = test_app_state_with_sessions(std::collections::HashMap::new());
    state.a2a_tasks.lock().await.insert("task".to_owned(), task);
    state.lifecycle().begin_drain();
    assert!(
        crate::a2a::cancel_a2a_task(&state, "task", &crate::AuthContext::default())
            .await
            .is_err()
    );
    assert_eq!(state.lifecycle().active(), 0);
    let owner = crate::AuthContext {
        subject: Some("owner".to_owned()),
        ..Default::default()
    };
    crate::a2a::cancel_a2a_task(&state, "task", &owner)
        .await
        .unwrap();
    for payload_key in ["statusUpdate", "task"] {
        let (mut stream, _) = tokio::time::timeout(Duration::from_secs(2), callback.accept())
            .await
            .unwrap()
            .unwrap();
        let mut initial = Vec::new();
        let head = crate::http::read_request_head(&mut stream, &mut initial)
            .await
            .unwrap();
        let payload = crate::http::read_request_body(&mut stream, &mut initial, &head)
            .await
            .unwrap();
        let payload: Value = serde_json::from_slice(&payload).unwrap();
        assert_eq!(
            payload[payload_key]["status"]["state"],
            "TASK_STATE_CANCELED"
        );
        assert_eq!(state.lifecycle().active(), 1);
        assert!(state.lifecycle().admit().is_err());
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        stream.shutdown().await.unwrap();
    }
    tokio::time::timeout(Duration::from_secs(2), state.lifecycle().idle())
        .await
        .unwrap();
}

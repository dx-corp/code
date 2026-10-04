use super::*;

fn page_head(cursor: Option<&str>) -> RequestHead {
    RequestHead {
        method: "GET".to_string(),
        path: "/api/sessions/paged/page".to_string(),
        query: cursor
            .map(|cursor| HashMap::from([("cursor".to_string(), cursor.to_string())]))
            .unwrap_or_default(),
        headers: HashMap::new(),
    }
}

fn page_json(response: &[u8]) -> Value {
    let response = std::str::from_utf8(response).unwrap();
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    serde_json::from_str(response.split_once("\r\n\r\n").unwrap().1).unwrap()
}

async fn read_page(state: &AppState, auth: &AuthContext, cursor: Option<&str>) -> Vec<u8> {
    handle_session_get(
        &page_head(cursor),
        state,
        SessionPath {
            id: "paged",
            tail: Some("page"),
        },
        auth,
    )
    .await
}

#[tokio::test]
async fn sessions_page_retains_actionable_startup_failure_with_a_bounded_payload() {
    let mut session = tenant_session("paged", "owner", "org", "workspace");
    session.last_turn_error = Some("Run `maestro login`".into());
    let state = test_app_state_with_sessions(HashMap::from([("paged".to_string(), session)]));
    let auth = tenant_auth("owner", "org", "workspace");
    let page = page_json(&read_page(&state, &auth, None).await);
    assert_eq!(page["lastTurnError"], "Run `maestro login`");
    state
        .sessions
        .lock()
        .await
        .sessions
        .get_mut("paged")
        .unwrap()
        .last_turn_error = Some("\u{0}".repeat(100_000));
    let page = page_json(&read_page(&state, &auth, None).await);
    assert_eq!(
        page["lastTurnError"].as_str().unwrap().chars().count(),
        1024
    );
    assert!(serde_json::to_vec(&page).unwrap().len() < 16 * 1024);
    assert_eq!(
        page["messageCount"], 0,
        "failure is not an assistant answer"
    );
}

#[tokio::test]
async fn sessions_page_recent_window_and_cursor_survive_appends_without_duplicates() {
    let mut session = tenant_session("paged", "owner", "org", "workspace");
    session.messages = (0..121)
        .map(|index| serde_json::json!({ "role": "user", "content": index.to_string() }))
        .collect();
    let state = test_app_state_with_sessions(HashMap::from([("paged".to_string(), session)]));
    let auth = tenant_auth("owner", "org", "workspace");
    let recent = page_json(&read_page(&state, &auth, None).await);
    assert_eq!(recent["startIndex"], 71);
    assert_eq!(recent["messages"].as_array().unwrap().len(), 50);
    assert_eq!(recent["messages"][0]["content"], "71");
    let cursor = recent["nextCursor"].as_str().unwrap();
    state
        .sessions
        .lock()
        .await
        .sessions
        .get_mut("paged")
        .unwrap()
        .messages
        .push(serde_json::json!({ "role": "assistant", "content": "new" }));
    let earlier = page_json(&read_page(&state, &auth, Some(cursor)).await);
    assert_eq!(earlier["startIndex"], 21);
    assert_eq!(earlier["messages"][49]["content"], "70");
    assert_eq!(earlier["messageCount"], 122);
    let first = page_json(&read_page(&state, &auth, earlier["nextCursor"].as_str()).await);
    assert_eq!(first["startIndex"], 0);
    assert_eq!(first["messages"].as_array().unwrap().len(), 21);
    assert_eq!(first["nextCursor"], Value::Null);
    assert_eq!(first["hasEarlier"], false);
    let full = page_json(
        &handle_session_get(
            &page_head(None),
            &state,
            SessionPath {
                id: "paged",
                tail: None,
            },
            &auth,
        )
        .await,
    );
    assert_eq!(
        full["messages"].as_array().unwrap().len(),
        122,
        "legacy reads remain full"
    );
}

#[tokio::test]
async fn sessions_page_rechecks_owner_and_tenant_for_every_cursor() {
    let state = test_app_state_with_sessions(HashMap::from([(
        "paged".to_string(),
        tenant_session("paged", "owner", "org", "workspace"),
    )]));
    for auth in [
        tenant_auth("other", "org", "workspace"),
        tenant_auth("owner", "other", "workspace"),
        tenant_auth("owner", "org", "other"),
        AuthContext::default(),
    ] {
        let response = read_page(&state, &auth, None).await;
        assert!(
            std::str::from_utf8(&response)
                .unwrap()
                .starts_with("HTTP/1.1 404")
        );
    }
    let auth = tenant_auth("owner", "org", "workspace");
    for cursor in [
        "invalid".to_string(),
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(&(1_u8, "another-session", 1)).unwrap()),
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(&(1_u8, "paged", 500)).unwrap()),
    ] {
        assert!(
            std::str::from_utf8(&read_page(&state, &auth, Some(&cursor)).await)
                .unwrap()
                .starts_with("HTTP/1.1 400")
        );
    }
}

#[tokio::test]
async fn sessions_page_bounds_actual_public_payloads_and_marks_truncation() {
    let mut session = tenant_session("paged", "owner", "org", "workspace");
    session.messages = (0..70).map(|_| serde_json::json!({ "role": "user", "content": "\\\"💛".repeat(100_000), "attachments": [{ "contentBase64": "secret-inline" }] })).collect();
    let state = test_app_state_with_sessions(HashMap::from([("paged".to_string(), session)]));
    let auth = tenant_auth("owner", "org", "workspace");
    let page = page_json(&read_page(&state, &auth, None).await);
    let messages = page["messages"].as_array().unwrap();
    assert!(!messages.is_empty());
    assert!(
        messages.len() < 50,
        "byte limit must reduce rows, not just count them"
    );
    assert!(messages.iter().all(
        |message| serde_json::to_vec(message).unwrap().len() <= 32 * 1024
            && message["contentTruncated"] == true
    ));
    assert!(serde_json::to_vec(&page).unwrap().len() < 272 * 1024);
    assert!(
        !serde_json::to_string(&page)
            .unwrap()
            .contains("secret-inline")
    );
}

#[tokio::test]
async fn sessions_page_preserves_structured_excerpts_and_removes_all_inline_attachment_content() {
    let mut session = tenant_session("paged", "owner", "org", "workspace");
    session.messages = vec![
        serde_json::json!({ "role": "user", "content": [{ "type": "text", "text": "structured public text".repeat(10_000) }]}),
        serde_json::json!({ "role": "user", "content": "ordinary", "attachments": [{ "name": "file.txt", "content": "private-one", "contentBase64": "private-two", "content_base64": "private-three" }] }),
    ];
    let state = test_app_state_with_sessions(HashMap::from([("paged".to_string(), session)]));
    let page = page_json(&read_page(&state, &tenant_auth("owner", "org", "workspace"), None).await);
    assert!(
        page["messages"][0]["content"]
            .as_str()
            .unwrap()
            .starts_with("structured public text")
    );
    assert_eq!(page["messages"][0]["contentTruncated"], true);
    assert_eq!(
        page["messages"][1]["attachments"][0]["contentOmitted"],
        true
    );
    let serialized = serde_json::to_string(&page).unwrap();
    for private in ["private-one", "private-two", "private-three"] {
        assert!(!serialized.contains(private));
    }
}

#[tokio::test]
async fn sessions_page_message_anchor_stays_in_byte_budget_and_rejects_reused_generation() {
    let mut session = tenant_session("paged", "owner", "org", "workspace");
    let generation = session.created_at.clone();
    session.messages=(0..150).map(|index|serde_json::json!({"role":"user","content":format!("message {index} {}","😀".repeat(20_000))})).collect();
    let state = test_app_state_with_sessions(HashMap::from([("paged".into(), session)]));
    let auth = tenant_auth("owner", "org", "workspace");
    let mut head = page_head(None);
    head.query = HashMap::from([
        ("messageIndex".into(), "75".into()),
        ("sourceCreatedAt".into(), generation),
    ]);
    let response = handle_session_get(
        &head,
        &state,
        SessionPath {
            id: "paged",
            tail: Some("page"),
        },
        &auth,
    )
    .await;
    let page = page_json(&response);
    let start = page["startIndex"].as_u64().unwrap();
    let messages = page["messages"].as_array().unwrap();
    assert_eq!(start + messages.len() as u64, 76);
    assert!(
        messages.last().unwrap()["content"]
            .as_str()
            .unwrap()
            .starts_with("message 75")
    );
    assert!(serde_json::to_vec(&page).unwrap().len() <= 256 * 1024 + 16 * 1024);
    state
        .sessions
        .lock()
        .await
        .sessions
        .get_mut("paged")
        .unwrap()
        .created_at = "replacement".into();
    let rejected = handle_session_get(
        &head,
        &state,
        SessionPath {
            id: "paged",
            tail: Some("page"),
        },
        &auth,
    )
    .await;
    assert!(
        std::str::from_utf8(&rejected)
            .unwrap()
            .starts_with("HTTP/1.1 409")
    );
}

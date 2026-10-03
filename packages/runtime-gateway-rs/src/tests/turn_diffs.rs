use super::*;

#[test]
fn undo_endpoint_reads_and_consumes_tui_checkpoint_store() {
    use maestro_local_host::checkpoints::{Checkpoint, CheckpointStore, EntryKind, FileEntry};
    use sha2::{Digest, Sha256};

    let temp = unique_test_dir("maestro-undo-checkpoint");
    std::fs::create_dir_all(&temp).unwrap();
    // Match checkpoint capture: persist the resolved root, not a platform alias.
    let temp = dunce::canonicalize(temp).unwrap();
    let file = temp.join("src.txt");
    let before = b"before";
    let after = b"after";
    std::fs::write(&file, after).unwrap();
    let before_hash = Sha256::digest(before)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let after_hash = Sha256::digest(after)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();

    let store = CheckpointStore::new(&temp.join("sessions"), "session-1");
    let checkpoint_dir = store.root().join("checkpoint-1");
    std::fs::create_dir_all(checkpoint_dir.join("blobs")).unwrap();
    std::fs::write(checkpoint_dir.join("blobs").join(&before_hash), before).unwrap();
    let checkpoint = Checkpoint {
        id: "checkpoint-1".to_string(),
        created_at: "2026-08-05T00:00:00Z".to_string(),
        prompt: "edit src.txt".to_string(),
        repo_root: temp.clone(),
        head: None,
        user_turn_index: None,
        entries: vec![FileEntry {
            path: "src.txt".to_string(),
            kind: EntryKind::Modified,
            pre_blob: Some(before_hash),
            post_hash: Some(after_hash),
            post_snapshot_oversized: false,
        }],
    };
    std::fs::write(
        checkpoint_dir.join("checkpoint.json"),
        serde_json::to_vec(&checkpoint).unwrap(),
    )
    .unwrap();

    let head = RequestHead {
        method: "GET".to_string(),
        path: "/api/undo".to_string(),
        query: HashMap::from([(String::from("sessionId"), String::from("session-1"))]),
        headers: HashMap::new(),
    };
    let summary = undo_response_for_store(&head, &store);
    assert_eq!(summary["totalChanges"], 1);
    assert_eq!(summary["canUndo"], true);

    let restored = restore_undo_response_for_store(&store);
    assert_eq!(restored["success"], true, "{restored}");
    assert_eq!(std::fs::read(&file).unwrap(), before);
    assert_eq!(store.list().len(), 0);
    // A missing restore blob must not become success after other file work.
    std::fs::create_dir_all(&checkpoint_dir).unwrap();
    std::fs::write(
        checkpoint_dir.join("checkpoint.json"),
        serde_json::to_vec(&checkpoint).unwrap(),
    )
    .unwrap();
    std::fs::write(&file, after).unwrap();
    let failed = restore_undo_response_for_store(&store);
    assert_eq!(failed["success"], false);
    assert_eq!(failed["failedFiles"].as_array().unwrap().len(), 1);
    assert_eq!(std::fs::read(&file).unwrap(), after);
    assert_eq!(store.list().len(), 1);
    let _ = std::fs::remove_dir_all(temp);
}

#[tokio::test]
async fn native_turn_diff_query_preserves_completed_identity_and_session_authority() {
    let root = TestDir::new("native-turn-diff");
    for args in [
        vec!["init", "--quiet"],
        vec!["config", "user.email", "test@example.test"],
        vec!["config", "user.name", "Test"],
    ] {
        assert!(
            std::process::Command::new("git")
                .args(args)
                .current_dir(root.path())
                .status()
                .unwrap()
                .success()
        );
    }
    let session: SessionRecord = serde_json::from_value(serde_json::json!({
        "id": "native-session", "owner": "owner", "organizationId": "org", "workspaceId": "ws",
        "title": "Turn", "createdAt": "2026-10-03T00:00:00Z", "updatedAt": "2026-10-03T00:00:00Z", "messageCount": 2,
        "messages": [{"role":"user", "content":"first"}, {"role":"user", "content":"concurrent later turn"}]
    })).unwrap();
    let mut state =
        test_app_state_with_sessions(HashMap::from([(session.id.clone(), session.clone())]));
    let mut config = auth_test_config();
    config.cwd = dunce::canonicalize(root.path()).unwrap();
    config.session_store_path = root.path().join("gateway-state").join("sessions.json");
    state.config = Arc::new(config);
    let mut snapshot = crate::turn_diffs::begin_chat_snapshot(
        &state,
        Some(&session.id),
        "first",
        Some("native-session:created:1"),
    )
    .await;
    fs::write(root.path().join("result.txt"), "saved result").unwrap();
    let mut message = serde_json::json!({"role":"assistant", "content":"done"});
    crate::turn_diffs::finish_chat_snapshot(&mut snapshot, &mut message).await;
    assert_eq!(message["fileSnapshotTurnIndex"], 0);
    let mut completed = session.clone();
    completed.messages.push(message);
    state
        .sessions
        .lock()
        .await
        .sessions
        .insert(completed.id.clone(), completed.clone());
    fs::write(root.path().join("result.txt"), "later mutation").unwrap();
    let auth = AuthContext {
        subject: Some("owner".to_string()),
        organization_id: Some("org".to_string()),
        workspace_id: Some("ws".to_string()),
        ..AuthContext::default()
    };
    let mut head = csrf_head_for_path("GET", "/api/sessions/native-session/turn-diff", None);
    let response = crate::sessions::handle_session_get(
        &head,
        &state,
        session_path_from_path(&head.path).unwrap(),
        &auth,
    )
    .await;
    let text = String::from_utf8(response).unwrap();
    assert!(text.starts_with("HTTP/1.1 200"));
    assert!(text.contains("saved result"));
    assert!(!text.contains("later mutation"));
    for forbidden in [
        AuthContext {
            subject: Some("other".to_string()),
            ..auth.clone()
        },
        AuthContext {
            workspace_id: Some("other".to_string()),
            ..auth.clone()
        },
    ] {
        let response = crate::sessions::handle_session_get(
            &head,
            &state,
            session_path_from_path(&head.path).unwrap(),
            &forbidden,
        )
        .await;
        assert!(
            String::from_utf8(response)
                .unwrap()
                .starts_with("HTTP/1.1 404")
        );
    }
    head.query.insert("turnIndex".to_string(), "1".to_string());
    let response = crate::turn_diffs::session_turn_diff_response(&head, &state, &completed).await;
    assert!(
        String::from_utf8(response)
            .unwrap()
            .contains("\"availability\":\"unavailable\"")
    );
    head.query.insert("turnIndex".to_string(), "2".to_string());
    let response = crate::turn_diffs::session_turn_diff_response(&head, &state, &completed).await;
    assert!(
        String::from_utf8(response)
            .unwrap()
            .starts_with("HTTP/1.1 400")
    );
    // A public ID can be reused, but old generation and ownership cannot.
    let mut new_generation = completed.clone();
    new_generation.created_at = "2026-10-03T00:00:01Z".to_string();
    let mut new_owner = completed.clone();
    new_owner.owner = Some("new-owner".to_string());
    for recreated in [new_generation, new_owner] {
        head.query.insert("turnIndex".to_string(), "0".to_string());
        let response =
            crate::turn_diffs::session_turn_diff_response(&head, &state, &recreated).await;
        assert!(
            String::from_utf8(response)
                .unwrap()
                .contains("\"availability\":\"unavailable\"")
        );
    }
    head.query.clear();
    completed
        .messages
        .push(serde_json::json!({"role":"assistant", "fileSnapshotTurnIndex":1}));
    let response = crate::turn_diffs::session_turn_diff_response(&head, &state, &completed).await;
    assert!(
        String::from_utf8(response)
            .unwrap()
            .contains("\"availability\":\"unavailable\"")
    );
}

#[tokio::test]
async fn native_overlapping_turn_snapshots_are_unavailable_without_serializing_turns() {
    let root = TestDir::new("native-overlap-snapshots");
    assert!(
        std::process::Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(root.path())
            .status()
            .unwrap()
            .success()
    );
    let mut session = test_session_record("overlap-session");
    session.messages = vec![
        serde_json::json!({"role":"user", "content":"a"}),
        serde_json::json!({"role":"user", "content":"b"}),
    ];
    let mut state =
        test_app_state_with_sessions(HashMap::from([(session.id.clone(), session.clone())]));
    let mut config = auth_test_config();
    config.cwd = dunce::canonicalize(root.path()).unwrap();
    config.session_store_path = root.path().join("gateway-state").join("sessions.json");
    state.config = Arc::new(config);
    let mut first = crate::turn_diffs::begin_chat_snapshot(
        &state,
        Some(&session.id),
        "a",
        Some("session:created:1"),
    )
    .await;
    let mut second = crate::turn_diffs::begin_chat_snapshot(
        &state,
        Some(&session.id),
        "b",
        Some("session:created:2"),
    )
    .await;
    assert!(
        first.is_some() && second.is_some(),
        "both turns progress independently"
    );
    fs::write(root.path().join("shared.txt"), "overlapping edits").unwrap();
    let mut a = serde_json::json!({"role":"assistant", "content":"a done"});
    let mut b = serde_json::json!({"role":"assistant", "content":"b done"});
    crate::turn_diffs::finish_chat_snapshot(&mut first, &mut a).await;
    crate::turn_diffs::finish_chat_snapshot(&mut second, &mut b).await;
    assert_eq!(a["fileSnapshotAvailable"], false);
    assert_eq!(b["fileSnapshotAvailable"], false);
    assert_eq!(a["fileSnapshotTurnIndex"], 0);
    assert_eq!(b["fileSnapshotTurnIndex"], 1);
    session.messages.extend([a, b]);
    let mut head = csrf_head_for_path("GET", "/api/sessions/overlap-session/turn-diff", None);
    for index in [0, 1] {
        head.query
            .insert("turnIndex".to_string(), index.to_string());
        let response = crate::turn_diffs::session_turn_diff_response(&head, &state, &session).await;
        assert!(
            String::from_utf8(response)
                .unwrap()
                .contains("\"availability\":\"unavailable\"")
        );
    }
    // Dropping a canceled capture releases detection state and pending files.
    let canceled = crate::turn_diffs::begin_chat_snapshot(
        &state,
        Some(&session.id),
        "cancel",
        Some("session:created:2"),
    )
    .await;
    drop(canceled);
    let mut final_turn = crate::turn_diffs::begin_chat_snapshot(
        &state,
        Some(&session.id),
        "alone",
        Some("session:created:2"),
    )
    .await;
    let mut message = serde_json::json!({"role":"assistant"});
    crate::turn_diffs::finish_chat_snapshot(&mut final_turn, &mut message).await;
    assert_eq!(message["fileSnapshotAvailable"], true);
}

#[tokio::test]
async fn native_sessionless_turns_invalidate_overlapping_session_snapshots() {
    let root = TestDir::new("native-sessionless-overlap-snapshots");
    assert!(
        std::process::Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(root.path())
            .status()
            .unwrap()
            .success()
    );
    let mut session = test_session_record("sessionless-overlap-session");
    session.messages = vec![serde_json::json!({"role":"user", "content":"scoped"})];
    let mut state =
        test_app_state_with_sessions(HashMap::from([(session.id.clone(), session.clone())]));
    let mut config = auth_test_config();
    config.cwd = dunce::canonicalize(root.path()).unwrap();
    config.session_store_path = root.path().join("gateway-state").join("sessions.json");
    state.config = Arc::new(config);
    for sessionless_first in [false, true] {
        let (mut scoped, mut sessionless) = if sessionless_first {
            let sessionless =
                crate::turn_diffs::begin_chat_snapshot(&state, None, "sessionless", None).await;
            let scoped = crate::turn_diffs::begin_chat_snapshot(
                &state,
                Some(&session.id),
                "scoped",
                Some("session:created:1"),
            )
            .await;
            (scoped, sessionless)
        } else {
            let scoped = crate::turn_diffs::begin_chat_snapshot(
                &state,
                Some(&session.id),
                "scoped",
                Some("session:created:1"),
            )
            .await;
            let sessionless =
                crate::turn_diffs::begin_chat_snapshot(&state, None, "sessionless", None).await;
            (scoped, sessionless)
        };
        assert!(scoped.is_some() && sessionless.is_some());
        fs::write(root.path().join("shared.txt"), "sessionless edit").unwrap();
        let mut scoped_message = serde_json::json!({"role":"assistant", "content":"scoped done"});
        let mut sessionless_message =
            serde_json::json!({"role":"assistant", "content":"sessionless done"});
        if sessionless_first {
            crate::turn_diffs::finish_chat_snapshot(&mut sessionless, &mut sessionless_message)
                .await;
        }
        crate::turn_diffs::finish_chat_snapshot(&mut scoped, &mut scoped_message).await;
        if !sessionless_first {
            crate::turn_diffs::finish_chat_snapshot(&mut sessionless, &mut sessionless_message)
                .await;
        }
        assert_eq!(scoped_message["fileSnapshotAvailable"], false);
        assert_eq!(scoped_message["fileSnapshotTurnIndex"], 0);
        assert!(sessionless_message.get("fileSnapshotAvailable").is_none());
        assert!(sessionless_message.get("fileSnapshotTurnIndex").is_none());
        let mut completed = session.clone();
        completed.messages.push(scoped_message);
        let head = csrf_head_for_path(
            "GET",
            "/api/sessions/sessionless-overlap-session/turn-diff",
            None,
        );
        let response =
            crate::turn_diffs::session_turn_diff_response(&head, &state, &completed).await;
        assert!(
            String::from_utf8(response)
                .unwrap()
                .contains("\"availability\":\"unavailable\"")
        );
    }
    // A rejected or canceled sessionless turn releases its lease on drop.
    let canceled = crate::turn_diffs::begin_chat_snapshot(&state, None, "cancel", None).await;
    drop(canceled);
    let mut alone = crate::turn_diffs::begin_chat_snapshot(
        &state,
        Some(&session.id),
        "alone",
        Some("session:created:1"),
    )
    .await;
    let mut message = serde_json::json!({"role":"assistant"});
    crate::turn_diffs::finish_chat_snapshot(&mut alone, &mut message).await;
    assert_eq!(message["fileSnapshotAvailable"], true);
}

use super::*;

// ─────────────────────────────────────────────────────────────────────────
// Rewind Picker Tests (double-Esc on empty input)
// ─────────────────────────────────────────────────────────────────────────

/// Point the app's session manager at a temp sessions dir and give it a
/// session id so file checkpoints resolve to the fixture, not `$HOME`.
fn setup_rewind_session(app: &mut App) -> (tempfile::TempDir, std::path::PathBuf) {
    let tmp = tempdir().unwrap();
    let repo = tmp.path().join("repo");
    let sessions = tmp.path().join("sessions");
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::create_dir_all(&sessions).unwrap();
    app.session_manager = SessionManager::with_sessions_dir(
        repo.to_string_lossy().to_string(),
        sessions.to_string_lossy().to_string(),
    );
    app.state.session_id = Some("rewind-test".to_string());
    (tmp, repo)
}

/// Write a one-file checkpoint manifest (plus its pre-turn blob) directly
/// into the session's checkpoint store and set the file's current content.
fn write_rewind_checkpoint(
    app: &App,
    repo: &std::path::Path,
    id: &str,
    created_at: &str,
    pre: &str,
    post: &str,
) {
    use sha2::{Digest, Sha256};
    let hash = |content: &str| format!("{:x}", Sha256::digest(content.as_bytes()));

    let store =
        crate::checkpoints::CheckpointStore::new(app.session_manager.sessions_dir(), "rewind-test");
    let dir = store.root().join(id);
    std::fs::create_dir_all(dir.join("blobs")).unwrap();
    std::fs::write(dir.join("blobs").join(hash(pre)), pre).unwrap();
    std::fs::write(repo.join("a.rs"), post).unwrap();

    let checkpoint = crate::checkpoints::Checkpoint {
        id: id.to_string(),
        created_at: created_at.to_string(),
        prompt: format!("prompt for {id}"),
        repo_root: dunce::canonicalize(repo).unwrap(),
        head: None,
        user_turn_index: None,
        entries: vec![crate::checkpoints::FileEntry {
            path: "a.rs".to_string(),
            kind: crate::checkpoints::EntryKind::Modified,
            pre_blob: Some(hash(pre)),
            post_hash: Some(hash(post)),
            post_snapshot_oversized: false,
        }],
    };
    let bytes = serde_json::to_vec_pretty(&checkpoint).unwrap();
    std::fs::write(dir.join("checkpoint.json"), bytes).unwrap();
}

async fn press_esc(app: &mut App) {
    app.handle_key(KeyCode::Esc, CrosstermModifiers::NONE)
        .await
        .unwrap();
}

#[test]
fn ephemeral_sessions_keep_file_checkpoints_without_turn_coordinates() {
    let mut app = new_test_app();
    let (_temp, repo) = setup_rewind_session(&mut app);
    assert!(
        std::process::Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(&repo)
            .status()
            .unwrap()
            .success()
    );
    assert!(app.session_manager.writer().is_none());
    app.capture_file_checkpoint(&repo, "ephemeral edit");
    assert!(
        app.pending_checkpoint
            .as_ref()
            .is_some_and(|pending| pending.user_turn_index.is_none())
    );
    std::fs::write(repo.join("new.txt"), "created in ephemeral turn").unwrap();
    app.finalize_file_checkpoint();
    let store =
        crate::checkpoints::CheckpointStore::new(app.session_manager.sessions_dir(), "rewind-test");
    assert_eq!(store.list().len(), 1);
    let restored = crate::checkpoints::restore_latest(&store).unwrap().unwrap();
    assert!(restored.failed.is_empty());
    assert!(!repo.join("new.txt").exists());
}

#[test]
fn rewind_both_reopens_before_two_edits_and_keeps_manual_changes() {
    let mut app = new_test_app();
    let (_tmp, repo) = setup_rewind_session(&mut app);
    app.state.session_id = None;
    app.record_user_message("first edit");
    app.record_user_message("second edit");
    app.session_manager.flush().unwrap();
    let source_id = app.state.session_id.clone().unwrap();
    // The fixture helper writes to rewind-test; move its completed store to
    // this real saved session after assigning explicit turn coordinates.
    write_rewind_checkpoint(&app, &repo, "cp-0", "2026-07-24T00:00:00Z", "v0", "v1");
    write_rewind_checkpoint(&app, &repo, "cp-1", "2026-07-24T01:00:00Z", "v1", "v2");
    let fixture_store =
        crate::checkpoints::CheckpointStore::new(app.session_manager.sessions_dir(), "rewind-test");
    for index in 0..2 {
        let manifest = fixture_store
            .root()
            .join(format!("cp-{index}/checkpoint.json"));
        let mut value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&manifest).unwrap()).unwrap();
        value["user_turn_index"] = index.into();
        let mut second_file = value["entries"][0].clone();
        second_file["path"] = "b.rs".into();
        value["entries"].as_array_mut().unwrap().push(second_file);
        std::fs::write(manifest, serde_json::to_vec(&value).unwrap()).unwrap();
    }
    let source_store =
        crate::checkpoints::CheckpointStore::new(app.session_manager.sessions_dir(), &source_id);
    std::fs::rename(fixture_store.root(), source_store.root()).unwrap();
    std::fs::write(repo.join("a.rs"), "later manual edit").unwrap();
    std::fs::write(repo.join("b.rs"), "v2").unwrap();
    app.rewind_saved_turns(2, false, true);
    assert!(app.state.error.is_none(), "{:?}", app.state.error);
    assert_ne!(app.state.session_id.as_deref(), Some(source_id.as_str()));
    let branch = app.session_manager.current_session_path().unwrap();
    app.session_manager.flush().unwrap();
    let reopened = crate::session::SessionReader::read_file(branch).unwrap();
    assert_eq!(reopened.stats.user_messages, 0);
    assert_eq!(
        std::fs::read_to_string(repo.join("a.rs")).unwrap(),
        "later manual edit"
    );
    assert_eq!(std::fs::read_to_string(repo.join("b.rs")).unwrap(), "v0");
}

#[test]
fn rewind_saved_conversation_reopens_at_selected_turn_and_keeps_source() {
    let mut app = new_test_app();
    let (_tmp, _) = setup_rewind_session(&mut app);
    app.state.session_id = None;
    app.record_user_message("first request");
    app.record_user_message("abandoned request");
    app.session_manager.flush().unwrap();
    let source_path = app.session_manager.current_session_path().unwrap();
    let original = std::fs::read(&source_path).unwrap();
    let source_id = app.state.session_id.clone();
    app.rewind_turns(1, true);
    assert_eq!(app.state.session_id, source_id);
    assert_eq!(std::fs::read(&source_path).unwrap(), original);
    app.rewind_turns(1, false);
    assert!(app.state.error.is_none(), "{:?}", app.state.error);
    assert_ne!(app.state.session_id, source_id);
    app.record_user_message("replacement request");
    app.session_manager.flush().unwrap();
    let branch = app.session_manager.current_session_path().unwrap();
    let reopened = crate::session::SessionReader::read_file(&branch).unwrap();
    assert_eq!(
        reopened
            .messages
            .iter()
            .map(AppMessage::text_content)
            .collect::<Vec<_>>(),
        ["first request", "replacement request"]
    );
    assert_eq!(std::fs::read(&source_path).unwrap(), original);
    let (history, _, _) = app.agent_context_for_spawn().unwrap();
    assert_eq!(history.len(), 2);
    assert_eq!(history[1].content.as_text(), Some("replacement request"));
}

#[tokio::test]
async fn double_esc_on_empty_input_opens_rewind_picker() {
    let mut app = new_test_app();
    let (_tmp, repo) = setup_rewind_session(&mut app);
    write_rewind_checkpoint(
        &app,
        &repo,
        "cp-1",
        "2026-07-24T00:00:00Z",
        "original\n",
        "edit\n",
    );

    press_esc(&mut app).await;
    assert_eq!(app.active_modal, ActiveModal::None);
    assert_eq!(
        app.state.status.as_deref(),
        Some("Press Esc again to rewind files")
    );

    press_esc(&mut app).await;
    assert_eq!(app.active_modal, ActiveModal::RewindPicker);
    assert!(app.rewind_picker.is_visible());
}

#[test]
fn rewind_picker_conversation_uses_the_saved_turn_coordinate() {
    let mut app = new_test_app();
    let (_temp, repo) = setup_rewind_session(&mut app);
    app.state.session_id = None;
    for prompt in ["kept first request", "second request", "third request"] {
        app.record_user_message(prompt);
    }
    app.session_manager.flush().unwrap();
    let source = app.session_manager.current_session_path().unwrap();
    let original = std::fs::read(&source).unwrap();
    app.rewind_picker.show(vec![crate::checkpoints::Checkpoint {
        id: "selected".into(),
        created_at: "2026-09-10T00:00:00Z".into(),
        prompt: "second request".into(),
        repo_root: repo,
        head: None,
        user_turn_index: Some(1),
        entries: vec![],
    }]);
    app.handle_rewind_picker_key(KeyCode::Char('c')).unwrap();
    assert!(app.state.error.is_none(), "{:?}", app.state.error);
    app.session_manager.flush().unwrap();
    let child = crate::session::SessionReader::read_file(
        app.session_manager.current_session_path().unwrap(),
    )
    .unwrap();
    assert_eq!(child.stats.user_messages, 1);
    assert_eq!(std::fs::read(source).unwrap(), original);
}

#[tokio::test]
async fn rewind_picker_enter_restores_selected_checkpoint() {
    let mut app = new_test_app();
    let (_tmp, repo) = setup_rewind_session(&mut app);
    write_rewind_checkpoint(&app, &repo, "cp-1", "2026-07-24T00:00:00Z", "v0\n", "v1\n");
    write_rewind_checkpoint(&app, &repo, "cp-2", "2026-07-24T01:00:00Z", "v1\n", "v2\n");

    press_esc(&mut app).await;
    press_esc(&mut app).await;
    assert_eq!(app.active_modal, ActiveModal::RewindPicker);

    // Newest checkpoint is selected by default; Enter restores it.
    app.handle_key(KeyCode::Enter, CrosstermModifiers::NONE)
        .await
        .unwrap();

    assert_eq!(app.active_modal, ActiveModal::None);
    assert!(!app.rewind_picker.is_visible());
    assert_eq!(std::fs::read_to_string(repo.join("a.rs")).unwrap(), "v1\n");
    assert_eq!(
        app.state.status.as_deref(),
        Some("Files restored from checkpoint.")
    );

    // Only the applied checkpoint was consumed; the older one remains.
    let store =
        crate::checkpoints::CheckpointStore::new(app.session_manager.sessions_dir(), "rewind-test");
    let remaining = store.list();
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].id, "cp-1");
}

#[tokio::test]
async fn rewind_picker_esc_dismisses_without_restoring() {
    let mut app = new_test_app();
    let (_tmp, repo) = setup_rewind_session(&mut app);
    write_rewind_checkpoint(
        &app,
        &repo,
        "cp-1",
        "2026-07-24T00:00:00Z",
        "original\n",
        "edit\n",
    );

    press_esc(&mut app).await;
    press_esc(&mut app).await;
    assert_eq!(app.active_modal, ActiveModal::RewindPicker);

    press_esc(&mut app).await;
    assert_eq!(app.active_modal, ActiveModal::None);
    assert!(!app.rewind_picker.is_visible());
    // Nothing was restored and the checkpoint was not consumed.
    assert_eq!(
        std::fs::read_to_string(repo.join("a.rs")).unwrap(),
        "edit\n"
    );
    let store =
        crate::checkpoints::CheckpointStore::new(app.session_manager.sessions_dir(), "rewind-test");
    assert_eq!(store.list().len(), 1);
}

#[tokio::test]
async fn double_esc_without_checkpoints_shows_status() {
    let mut app = new_test_app();
    let (_tmp, _repo) = setup_rewind_session(&mut app);

    press_esc(&mut app).await;
    press_esc(&mut app).await;

    assert_eq!(app.active_modal, ActiveModal::None);
    assert_eq!(
        app.state.status.as_deref(),
        Some("No file checkpoints recorded for this session.")
    );
}

#[tokio::test]
async fn double_esc_rewind_picker_blocked_while_busy() {
    let mut app = new_test_app();
    let (_tmp, repo) = setup_rewind_session(&mut app);
    write_rewind_checkpoint(
        &app,
        &repo,
        "cp-1",
        "2026-07-24T00:00:00Z",
        "original\n",
        "edit\n",
    );
    app.state.busy = true;

    press_esc(&mut app).await;
    press_esc(&mut app).await;

    assert_eq!(app.active_modal, ActiveModal::None);
    assert_eq!(
        app.state.status.as_deref(),
        Some("Wait for the active response to finish before rewinding.")
    );
}

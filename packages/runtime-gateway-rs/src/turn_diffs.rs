//! Native completed-turn review reads the session checkpoint owner, never Git HEAD
//! or the current worktree. Gateway state roots and authorized session IDs scope it.
use super::*;
use maestro_local_host::checkpoints::{
    CheckpointStore, PendingTurn, begin_turn_snapshot, finalize_turn_snapshot,
};

fn snapshot_root(state: &AppState) -> &Path {
    state
        .config
        .session_store_path
        .parent()
        .unwrap_or(&state.config.cwd)
}

fn native_snapshot_key(session: &SessionRecord, workspace: &Path) -> String {
    use base64::Engine;
    use sha2::{Digest, Sha256};
    // Session IDs may be reused after deletion. Creation and ownership are
    // immutable within a generation; include the canonical workspace as well.
    let workspace = dunce::canonicalize(workspace).unwrap_or_else(|_| workspace.to_path_buf());
    let identity = serde_json::json!([
        session.id,
        session.created_at,
        session.owner,
        session.organization_id,
        session.workspace_id,
        workspace
    ]);
    format!(
        "native-{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(Sha256::digest(identity.to_string().as_bytes()))
    )
}

/// Ephemeral ambiguity detection only. Turns never wait for each other.
#[derive(Default)]
pub(super) struct NativeSnapshotRegistry {
    active: std::sync::Mutex<Vec<std::sync::Weak<std::sync::atomic::AtomicBool>>>,
}

struct CaptureLease {
    registry: Arc<NativeSnapshotRegistry>,
    ambiguous: Arc<std::sync::atomic::AtomicBool>,
}

impl NativeSnapshotRegistry {
    fn acquire(self: &Arc<Self>) -> CaptureLease {
        use std::sync::atomic::{AtomicBool, Ordering};
        let mut active = self.active.lock().expect("snapshot registry lock");
        active.retain(|entry| entry.strong_count() > 0);
        let ambiguous = Arc::new(AtomicBool::new(!active.is_empty()));
        for other in active.iter().filter_map(std::sync::Weak::upgrade) {
            other.store(true, Ordering::SeqCst);
        }
        active.push(Arc::downgrade(&ambiguous));
        CaptureLease {
            registry: self.clone(),
            ambiguous,
        }
    }
}

impl CaptureLease {
    fn complete(&self) -> bool {
        let mut active = self.registry.active.lock().expect("snapshot registry lock");
        let this = Arc::downgrade(&self.ambiguous);
        active.retain(|entry| !entry.ptr_eq(&this) && entry.strong_count() > 0);
        self.ambiguous.load(std::sync::atomic::Ordering::SeqCst)
    }
}

impl Drop for CaptureLease {
    fn drop(&mut self) {
        self.complete();
    }
}

pub(super) struct ChatSnapshot {
    capture: Option<SessionSnapshot>,
    lease: CaptureLease,
}

struct SessionSnapshot {
    turn_index: usize,
    pending: Option<PendingTurn>,
    store: CheckpointStore,
}

pub(super) async fn begin_chat_snapshot(
    state: &AppState,
    session_id: Option<&str>,
    prompt: &str,
    scope: Option<&str>,
) -> Option<ChatSnapshot> {
    // Every accepted native turn can edit this workspace, including turns with
    // no durable session. Hold its lease until completion or error/drop.
    let mut snapshot = ChatSnapshot {
        capture: None,
        lease: state.native_snapshot_registry.acquire(),
    };
    let Some(session_id) = session_id else {
        return Some(snapshot);
    };
    let sessions = state.sessions.lock().await;
    let Some(session) = sessions.sessions.get(session_id) else {
        return Some(snapshot);
    };
    // The accepted message count identifies this submission even when another
    // actor appends a message before capture. Do not reassign it to the tail.
    let message_count = scope
        .and_then(|scope| scope.rsplit(':').next()?.parse::<usize>().ok())
        .unwrap_or(session.messages.len());
    let Some(turn_index) = session
        .messages
        .iter()
        .take(message_count)
        .filter(|message| message.get("role").and_then(Value::as_str) == Some("user"))
        .count()
        .checked_sub(1)
    else {
        return Some(snapshot);
    };
    let checkpoint_key = native_snapshot_key(session, &state.config.cwd);
    drop(sessions);
    let cwd = state.config.cwd.clone();
    let root = snapshot_root(state).to_path_buf();
    let prompt = prompt.to_string();
    let store = CheckpointStore::new(&root, &checkpoint_key);
    let pending = tokio::task::spawn_blocking(move || {
        let mut pending = begin_turn_snapshot(&cwd, &root, &checkpoint_key, &prompt)?;
        pending.user_turn_index = Some(turn_index);
        Some(pending)
    })
    .await
    .ok()
    .flatten();
    snapshot.capture = Some(SessionSnapshot {
        turn_index,
        pending,
        store,
    });
    Some(snapshot)
}

pub(super) async fn finish_chat_snapshot(snapshot: &mut Option<ChatSnapshot>, message: &mut Value) {
    let Some(snapshot) = snapshot.take() else {
        return;
    };
    let Some(capture) = snapshot.capture else {
        return;
    };
    let mut saved = None;
    if let Some(pending) = capture.pending {
        if !snapshot
            .lease
            .ambiguous
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            match tokio::task::spawn_blocking(move || finalize_turn_snapshot(pending)).await {
                Ok(Ok(checkpoint)) => saved = checkpoint,
                outcome => tracing::warn!(?outcome, "completed turn checkpoint could not be saved"),
            }
        }
    }
    let ambiguous = snapshot.lease.complete();
    if ambiguous {
        if let Some(checkpoint) = &saved {
            if let Err(error) = capture.store.remove(&checkpoint.id) {
                tracing::warn!(%error, "ambiguous snapshot cleanup failed");
            }
        }
    }
    message["fileSnapshotAvailable"] = serde_json::json!(saved.is_some() && !ambiguous);
    // Completion identity is durable even when file capture failed. Latest-turn
    // reads therefore report unavailable instead of showing an older turn.
    message["fileSnapshotTurnIndex"] = serde_json::json!(capture.turn_index);
}

pub(super) async fn session_turn_diff_response(
    head: &RequestHead,
    state: &AppState,
    session: &SessionRecord,
) -> Vec<u8> {
    let turn_index = match head.query.get("turnIndex") {
        Some(value) => match value.parse::<usize>() {
            Ok(index) => Some(index),
            Err(_) => {
                return json_response(400, &serde_json::json!({"error": "Invalid turn index"}));
            }
        },
        None => session
            .messages
            .iter()
            .rev()
            .find(|message| message.get("role").and_then(Value::as_str) == Some("assistant"))
            .and_then(|message| message.get("fileSnapshotTurnIndex").and_then(Value::as_u64))
            .and_then(|index| usize::try_from(index).ok()),
    };
    let user_turns = session
        .messages
        .iter()
        .filter(|message| message.get("role").and_then(Value::as_str) == Some("user"))
        .count();
    if turn_index.is_some_and(|index| index >= user_turns) {
        return json_response(
            400,
            &serde_json::json!({"error": "Turn range is unavailable"}),
        );
    }
    let completed_snapshot = turn_index.is_some_and(|index| {
        session.messages.iter().any(|message| {
            message.get("role").and_then(Value::as_str) == Some("assistant")
                && message.get("fileSnapshotTurnIndex").and_then(Value::as_u64)
                    == Some(index as u64)
                && message
                    .get("fileSnapshotAvailable")
                    .and_then(Value::as_bool)
                    == Some(true)
        })
    });
    let store = CheckpointStore::new(
        snapshot_root(state),
        &native_snapshot_key(session, &state.config.cwd),
    );
    let cwd = state.config.cwd.clone();
    let snapshot = if let Some(turn_index) = turn_index.filter(|_| completed_snapshot) {
        tokio::task::spawn_blocking(move || store.turn_snapshot(Some(turn_index), &cwd)).await
    } else {
        Ok(Ok(None))
    };
    match snapshot {
        Ok(Ok(Some((index, files)))) if index < user_turns => json_response(
            200,
            &serde_json::json!({
                "sessionId": session.id, "workspacePath": state.config.cwd,
                "turnIndex": index, "availability": "ready", "files": files,
            }),
        ),
        Ok(Ok(_)) => json_response(
            200,
            &serde_json::json!({
                "sessionId": session.id, "workspacePath": state.config.cwd,
                "turnIndex": turn_index, "availability": "unavailable", "files": [],
            }),
        ),
        error => json_response(
            500,
            &serde_json::json!({"error": format!("Cannot read turn checkpoint: {error:?}")}),
        ),
    }
}

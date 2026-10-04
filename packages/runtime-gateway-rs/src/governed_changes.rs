//! Saved receipt bytes, never a second workspace execution path.
use super::*;
use crate::governed_native::Admission;

pub(crate) fn receipt(execution: &Value, admission: &Admission) -> Result<Option<Value>, String> {
    let output = &execution["output"]["safeOutput"];
    let Some(changes) = output.get("nativeChanges") else {
        return Ok(None);
    };
    let key = maestro_runtime_contracts::native_code::checkpoint_key(
        &admission.organization_id,
        &admission.workspace_id,
        &admission.admission_id,
    );
    if output["nativeCheckpointKey"] != key || !valid_changes(changes) {
        return Err("Hosted saved changes do not match the verified execution receipt".into());
    }
    Ok(Some(serde_json::json!({"toolExecutionId":execution["id"],
        "platformAdmissionId":admission.admission_id,"nativeCheckpointKey":key,"changes":changes})))
}

fn valid_changes(changes: &Value) -> bool {
    if !matches!(serde_json::to_vec(changes), Ok(bytes) if bytes.len() <= 60 * 1024) {
        return false;
    }
    let Some(files) = changes["files"].as_array() else {
        return false;
    };
    if !matches!(
        changes["availability"].as_str(),
        Some("ready" | "unavailable")
    ) || (changes["availability"] == "unavailable" && !files.is_empty())
    {
        return false;
    }
    let mut paths = HashSet::new();
    files.iter().all(|file| {
        let Some(path) = file["path"].as_str() else {
            return false;
        };
        !path.is_empty()
            && path.len() <= 4096
            && !path.starts_with('/')
            && !path.contains(['\\', '\0'])
            && !path.split('/').any(|part| matches!(part, "" | "." | ".."))
            && paths.insert(path)
            && matches!(
                file["kind"].as_str(),
                Some("created" | "modified" | "deleted")
            )
            && matches!(
                file["availability"].as_str(),
                Some("patch" | "binary" | "truncated" | "unavailable")
            )
            && ["beforeContent", "afterContent"].iter().all(|field| {
                file.get(field)
                    .is_some_and(|value| value.is_null() || value.is_string())
            })
    })
}

/// Save after every terminal command, before asking the model to continue.
/// Later inference failure or Stop cannot erase an already completed effect.
pub(crate) async fn save(
    state: &AppState,
    admission: &Admission,
    scope: Option<&str>,
    receipt: Value,
) -> Result<(), String> {
    let count = scope
        .and_then(|scope| scope.rsplit(':').next()?.parse::<usize>().ok())
        .ok_or("Hosted saved changes are missing their accepted message coordinate")?;
    let mut store = state.sessions.lock().await;
    let session = store
        .sessions
        .get_mut(&admission.native_session_id)
        .filter(|session| session.created_at == admission.native_session_created_at)
        .ok_or("Hosted saved changes session generation is unavailable")?;
    let turn_index = session
        .messages
        .iter()
        .take(count)
        .filter(|message| message["role"] == "user")
        .count()
        .checked_sub(1)
        .ok_or("Hosted saved changes user turn is unavailable")?;
    let message = session
        .messages
        .get_mut(
            count
                .checked_sub(1)
                .ok_or("Invalid accepted message coordinate")?,
        )
        .filter(|message| message["role"] == "user")
        .ok_or("Hosted saved changes accepted message is unavailable")?;
    let previous = message.get("governedChanges").cloned();
    let previous_index = message.get("governedChangesTurnIndex").cloned();
    message["governedChanges"] = receipt;
    message["governedChangesTurnIndex"] = serde_json::json!(turn_index);
    if let Err(error) = persist_session_store_snapshot(state, &store).await {
        let message = &mut store
            .sessions
            .get_mut(&admission.native_session_id)
            .expect("locked session")
            .messages[count - 1];
        if let Some(previous) = previous {
            message["governedChanges"] = previous;
        } else {
            message
                .as_object_mut()
                .expect("message object")
                .remove("governedChanges");
        }
        if let Some(previous) = previous_index {
            message["governedChangesTurnIndex"] = previous;
        } else {
            message
                .as_object_mut()
                .expect("message object")
                .remove("governedChangesTurnIndex");
        }
        return Err(error);
    }
    Ok(())
}

pub(crate) fn saved(session: &SessionRecord, index: usize) -> Option<Value> {
    let receipt = session
        .messages
        .iter()
        .filter(|message| message["role"] == "user")
        .nth(index)?
        .get("governedChanges")?;
    valid_changes(&receipt["changes"]).then(|| receipt.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn completed_effect_bytes_are_durable_and_ordinal_survives_later_failure() {
        let root = tempfile::tempdir().unwrap();
        let mut session = create_session_record(None, Some("human".into()));
        session.id = "session".into();
        session.created_at = "created".into();
        session.messages = vec![
            serde_json::json!({"role":"user","content":"first"}),
            serde_json::json!({"role":"assistant","content":"done"}),
            serde_json::json!({"role":"user","content":"edit"}),
        ];
        let mut state = crate::tests::test_app_state_with_sessions(HashMap::from([(
            session.id.clone(),
            session,
        )]));
        let mut config = (*state.config).clone();
        config.session_store_path = root.path().join("sessions.json");
        state.config = Arc::new(config);
        let admission: Admission = serde_json::from_value(serde_json::json!({"admissionId":"admission","organizationId":"org","workspaceId":"workspace","subject":"human","runnerSessionId":"runner","applicationId":"deixic","agentId":"maestro","actorId":"human","gatewayEpoch":"epoch","nativeSessionId":"session","nativeSessionCreatedAt":"created","nativeTurnId":"turn","requestSha256":"digest","state":"active","expiresAt":"2100-01-01T00:00:00Z"})).unwrap();
        let value = serde_json::json!({"toolExecutionId":"execution","platformAdmissionId":"admission","changes":{"availability":"ready","files":[]}});
        save(&state, &admission, Some("session:created:3"), value.clone())
            .await
            .unwrap();
        let (restored, valid) = load_session_store(&state.config.session_store_path).await;
        assert!(valid);
        assert_eq!(
            restored.sessions["session"].messages[2]["governedChangesTurnIndex"],
            1
        );
        assert_eq!(saved(&restored.sessions["session"], 1), Some(value));
        state.session_store_persist_enabled = false;
        let previous = state.sessions.lock().await.sessions["session"].messages[2].clone();
        assert!(
            save(
                &state,
                &admission,
                Some("session:created:3"),
                serde_json::json!({"invalid":"new"})
            )
            .await
            .is_err()
        );
        assert_eq!(
            state.sessions.lock().await.sessions["session"].messages[2],
            previous
        );
        state
            .sessions
            .lock()
            .await
            .sessions
            .get_mut("session")
            .unwrap()
            .created_at = "replacement".into();
        assert!(
            save(
                &state,
                &admission,
                Some("session:created:3"),
                serde_json::json!({})
            )
            .await
            .is_err()
        );
    }

    #[test]
    fn saved_changes_reject_oversize_and_unsafe_paths() {
        let mut changes = serde_json::json!({"availability":"ready","files":[{"path":"new.txt","kind":"created","availability":"patch","beforeContent":"","afterContent":"saved"}]});
        assert!(valid_changes(&changes));
        changes["files"][0]["path"] = serde_json::json!("../secret");
        assert!(!valid_changes(&changes));
        changes["files"][0]["path"] = serde_json::json!("new.txt");
        changes["files"][0]["afterContent"] = serde_json::json!("a".repeat(60 * 1024));
        assert!(!valid_changes(&changes));
    }
}

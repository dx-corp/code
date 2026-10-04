//! Last observed request context, separate from cumulative billable usage.
use super::*;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct NativeContextSnapshot {
    pub(crate) provider: String,
    pub(crate) model: String,
    pub(crate) used_tokens: Option<u64>,
    pub(crate) max_tokens: Option<u64>,
    pub(crate) source: ContextSource,
    pub(crate) observed_at: String,
    pub(crate) compaction: Option<ContextCompaction>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) enum ContextSource {
    ProviderReported,
    Estimated,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ContextCompaction {
    pub(crate) tokens_before: u64,
    pub(crate) first_kept_entry_index: usize,
    pub(crate) automatic: bool,
    pub(crate) observed_at: String,
}

fn context_max(provider: &str, model: &str) -> Option<u64> {
    let id = if model.starts_with(&format!("{provider}/")) {
        model.to_string()
    } else {
        format!("{provider}/{model}")
    };
    maestro_local_host::model_facts_generated::model_facts(&id)
        .map(|facts| u64::from(facts.context_window))
        .filter(|max| *max > 0)
}

fn project(
    previous: Option<&NativeContextSnapshot>,
    event: &FromAgent,
    provider: &str,
    model: &str,
) -> Option<NativeContextSnapshot> {
    let same_model =
        previous.filter(|snapshot| snapshot.provider == provider && snapshot.model == model);
    match event {
        // Calibration is emitted for the sealed primary request before any
        // auxiliary summarization usage. ResponseEnd may include both calls;
        // CodexUsageState and headless Codex usage also carry aggregate totals.
        FromAgent::ContextCalibration { observation } if provider != "openai-codex" => {
            Some(NativeContextSnapshot {
                provider: provider.into(),
                model: model.into(),
                used_tokens: Some(observation.observed_input_tokens),
                max_tokens: context_max(provider, model),
                source: ContextSource::ProviderReported,
                observed_at: now_rfc3339(),
                compaction: same_model.and_then(|snapshot| snapshot.compaction.clone()),
            })
        }
        FromAgent::Compaction {
            tokens_before,
            first_kept_entry_index,
            auto,
            timestamp,
            ..
        } => Some(NativeContextSnapshot {
            provider: provider.into(),
            model: model.into(),
            used_tokens: None,
            max_tokens: context_max(provider, model),
            source: ContextSource::Estimated,
            observed_at: timestamp.clone(),
            compaction: Some(ContextCompaction {
                tokens_before: *tokens_before,
                first_kept_entry_index: *first_kept_entry_index,
                automatic: *auto,
                observed_at: timestamp.clone(),
            }),
        }),
        _ => None,
    }
}

/// Captures only real runtime events and checks the accepted session generation.
pub(crate) async fn record_context_event(
    state: &AppState,
    session_id: Option<&str>,
    session_created_at: &str,
    event: &FromAgent,
    provider: &str,
    model: &str,
) {
    let Some(id) = session_id else {
        return;
    };
    let mut store = state.sessions.lock().await;
    let Some(session) = store
        .sessions
        .get_mut(id)
        .filter(|s| s.created_at == session_created_at)
    else {
        return;
    };
    let Some(snapshot) = project(session.native_context.as_ref(), event, provider, model) else {
        return;
    };
    let previous = session.native_context.replace(snapshot);
    if let Err(error) = persist_session_store_snapshot(state, &store).await {
        if let Some(session) = store.sessions.get_mut(id) {
            session.native_context = previous;
        }
        tracing::warn!(%error,"native context observation could not be persisted");
    }
}

/// Records one dex-loop model call's prompt size. The kernel's `Usage`
/// counts every prompt token, cached or not, so it is the same observation
/// the native actor's context calibration carried; it goes through the same
/// projection so the meter cannot drift between runtimes.
pub(crate) async fn record_observed_input(
    state: &AppState,
    session_id: Option<&str>,
    session_created_at: &str,
    observed_input_tokens: u64,
    provider: &str,
    model: &str,
) {
    if observed_input_tokens == 0 {
        return;
    }
    let Ok(event) = serde_json::from_value::<FromAgent>(serde_json::json!({
        "type": "context_calibration",
        "observation": {
            "request_id": "dex-loop",
            "generation": 0,
            "estimated_input_tokens": observed_input_tokens,
            "observed_input_tokens": observed_input_tokens
        }
    })) else {
        return;
    };
    record_context_event(
        state,
        session_id,
        session_created_at,
        &event,
        provider,
        model,
    )
    .await;
}

pub(super) fn response(session: &SessionRecord, query: &HashMap<String, String>) -> Vec<u8> {
    if query
        .get("sourceCreatedAt")
        .is_some_and(|generation| generation != &session.created_at)
    {
        return json_response(
            409,
            &serde_json::json!({"error":"Session generation changed"}),
        );
    }
    json_response(
        200,
        &serde_json::json!({"sessionId":session.id,"sourceCreatedAt":session.created_at,"context":session.native_context}),
    )
}

#[cfg(test)]
mod session_context_tests {
    use super::*;
    fn measured(usage: maestro_runtime::TokenUsage) -> FromAgent {
        serde_json::from_value(serde_json::json!({
            "type":"context_calibration",
            "observation":{
                "request_id":"request",
                "generation":1,
                "estimated_input_tokens":100,
                "observed_input_tokens":usage.input_tokens + usage.cache_read_tokens + usage.cache_write_tokens
            }
        }))
        .unwrap()
    }
    #[test]
    fn session_context_uses_latest_request_and_excludes_output_and_lifetime_totals() {
        let first = project(
            None,
            &measured(maestro_runtime::TokenUsage {
                input_tokens: 100,
                output_tokens: 900,
                cache_read_tokens: 30,
                cache_write_tokens: 20,
                ..Default::default()
            }),
            "anthropic",
            "claude-sonnet-4",
        )
        .unwrap();
        assert_eq!(first.used_tokens, Some(150));
        assert_eq!(first.source, ContextSource::ProviderReported);
        assert_eq!(first.max_tokens, Some(200_000));
        let last = project(
            Some(&first),
            &measured(maestro_runtime::TokenUsage {
                input_tokens: 5,
                ..Default::default()
            }),
            "anthropic",
            "claude-sonnet-4",
        )
        .unwrap();
        assert_eq!(last.used_tokens, Some(5));
        // A primary request plus a semantic-summary bill must not replace the
        // measured primary context with an aggregate of two model requests.
        assert!(
            project(
                Some(&last),
                &FromAgent::ResponseEnd {
                    response_id: "response".into(),
                    usage: Some(maestro_runtime::TokenUsage {
                        input_tokens: 80_005,
                        output_tokens: 900,
                        ..Default::default()
                    }),
                },
                "anthropic",
                "claude-sonnet-4",
            )
            .is_none()
        );
        assert!(
            project(
                Some(&last),
                &measured(maestro_runtime::TokenUsage {
                    input_tokens: 999_999,
                    ..Default::default()
                }),
                "openai-codex",
                "gpt-5.6"
            )
            .is_none()
        );
        assert_eq!(
            project(
                None,
                &measured(maestro_runtime::TokenUsage::default()),
                "unknown",
                "unknown"
            )
            .unwrap()
            .max_tokens,
            None
        );
    }
    #[test]
    fn session_context_compaction_invalidates_stale_meter_retains_real_observation() {
        let before = project(
            None,
            &measured(maestro_runtime::TokenUsage {
                input_tokens: 190_000,
                ..Default::default()
            }),
            "anthropic",
            "claude-sonnet-4",
        )
        .unwrap();
        let compact = FromAgent::Compaction {
            summary: "summary".into(),
            first_kept_entry_index: 9,
            tokens_before: 190_000,
            auto: true,
            custom_instructions: None,
            continuation: None,
            timestamp: "2026-10-03T00:00:00Z".into(),
        };
        let after = project(Some(&before), &compact, "anthropic", "claude-sonnet-4").unwrap();
        assert_eq!(after.used_tokens, None);
        assert_eq!(after.source, ContextSource::Estimated);
        assert_eq!(after.compaction.as_ref().unwrap().tokens_before, 190_000);
        let observed = project(
            Some(&after),
            &measured(maestro_runtime::TokenUsage {
                input_tokens: 1_500,
                ..Default::default()
            }),
            "anthropic",
            "claude-sonnet-4",
        )
        .unwrap();
        assert_eq!(observed.used_tokens, Some(1_500));
        assert_eq!(observed.compaction, after.compaction);
        let restored: NativeContextSnapshot =
            serde_json::from_value(serde_json::to_value(observed.clone()).unwrap()).unwrap();
        assert_eq!(restored, observed);
    }

    #[tokio::test]
    async fn session_context_persists_last_snapshot_and_rejects_deleted_or_reused_generation() {
        let root = std::env::temp_dir().join(new_session_id());
        let mut session = create_session_record(None, Some("owner".into()));
        session.id = "observed".into();
        let generation = session.created_at.clone();
        let mut state = crate::tests::test_app_state_with_sessions(HashMap::from([(
            "observed".into(),
            session,
        )]));
        let mut config = (*state.config).clone();
        config.session_store_path = root.join("sessions.json");
        state.config = Arc::new(config);
        let event = measured(maestro_runtime::TokenUsage {
            input_tokens: 42,
            ..Default::default()
        });
        record_context_event(
            &state,
            Some("observed"),
            &generation,
            &event,
            "anthropic",
            "claude-sonnet-4",
        )
        .await;
        let (restored, valid) = load_session_store(&state.config.session_store_path).await;
        assert!(valid);
        assert_eq!(
            restored.sessions["observed"]
                .native_context
                .as_ref()
                .unwrap()
                .used_tokens,
            Some(42)
        );
        let mut replacement = create_session_record(None, Some("owner".into()));
        replacement.id = "observed".into();
        replacement.created_at = "new-generation".into();
        state
            .sessions
            .lock()
            .await
            .sessions
            .insert("observed".into(), replacement);
        record_context_event(
            &state,
            Some("observed"),
            &generation,
            &event,
            "anthropic",
            "claude-sonnet-4",
        )
        .await;
        assert!(
            state.sessions.lock().await.sessions["observed"]
                .native_context
                .is_none()
        );
        state.sessions.lock().await.sessions.clear();
        record_context_event(
            &state,
            Some("observed"),
            &generation,
            &event,
            "anthropic",
            "claude-sonnet-4",
        )
        .await;
        assert!(state.sessions.lock().await.sessions.is_empty());
        tokio::fs::remove_dir_all(root).await.unwrap();
    }
}

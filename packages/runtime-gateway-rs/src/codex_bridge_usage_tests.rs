use super::*;

#[tokio::test]
async fn usage_prices_the_execution_model_after_the_selection_changes() {
    let state = crate::tests::test_app_state_with_sessions(HashMap::new());
    {
        let mut selected = state.selected_model.lock().await;
        selected.provider = "anthropic".into();
        selected.id = "claude-sonnet-4".into();
    }
    let chat: ChatRequest = serde_json::from_value(serde_json::json!({
        "messages": []
    }))
    .unwrap();
    let (provider, model) = usage_provider_model(&chat, &state, "openai/gpt-4o").await;
    assert_eq!(provider, "openai");
    assert_eq!(model, "gpt-4o");
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("usage.json");
    persist_usage_entry(
        &path,
        Some("session-a"),
        &provider,
        &model,
        &TokenUsage {
            input_tokens: 1_000,
            output_tokens: 100,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            cost: None,
        },
    )
    .await;
    let rows: Value = serde_json::from_slice(&tokio::fs::read(&path).await.unwrap()).unwrap();
    assert_eq!(rows[0]["provider"], "openai");
    assert_eq!(rows[0]["model"], "gpt-4o");
    assert_eq!(rows[0]["costSource"], "modelPriced");
    let rates = maestro_local_host::model_catalog::bundled_rates("openai/gpt-4o").unwrap();
    assert_eq!(
        rows[0]["pricingRates"]["inputPerMillion"],
        rates.input_per_million
    );
}

#[tokio::test]
async fn codex_bridge_usage_persistence_retains_absence_and_provenance() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("nested/usage.json");
    let mut usage = TokenUsage {
        input_tokens: 1_000,
        output_tokens: 100,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
        cost: None,
    };
    persist_usage_entry(&path, Some("session-a"), "local", "unlisted", &usage).await;
    usage.cost = Some(0.0);
    persist_usage_entry(&path, Some("session-a"), "local", "unlisted", &usage).await;
    usage.cost = None;
    persist_usage_entry(&path, Some("session-b"), "openai", "gpt-4o", &usage).await;

    let rows: Value = serde_json::from_slice(&tokio::fs::read(&path).await.unwrap()).unwrap();
    assert!(rows[0]["cost"].is_null());
    assert_eq!(rows[0]["costSource"], "unpriced");
    assert_eq!(rows[0]["sessionId"], "session-a");
    assert_eq!(rows[1]["cost"], 0.0);
    assert_eq!(rows[1]["costSource"], "providerReported");
    assert_eq!(rows[2]["costSource"], "modelPriced");
    assert!(rows[2]["pricingVersion"].as_u64().unwrap() > 0);
    assert!(rows[2]["pricingRates"].is_object());
    let snapshot = crate::local::usage_snapshot(&path).await;
    assert_eq!(snapshot["summary"]["totalRequests"], 3);
    assert_eq!(snapshot["summary"]["unpricedRequests"], 1);
    assert_eq!(snapshot["summary"]["knownCostRequests"], 2);
    assert_eq!(snapshot["summary"]["totalCost"], rows[2]["cost"]);
    assert_eq!(snapshot["summary"]["totalTokens"], 3_300);
}

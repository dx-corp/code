use super::*;
use serde_json::json;

#[tokio::test]
async fn local_usage_roundtrip_keeps_known_zero_and_missing_cost_distinct() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("usage.json");
    let rows = json!([
        {"provider":"openai", "model":"test", "tokensInput":10, "cost":0.0, "costSource":"providerReported"},
        {"provider":"openai", "model":"test", "tokensOutput":20, "cost":0.25, "costSource":"modelPriced"},
        {"provider":"openai", "model":"test", "tokensCacheRead":3, "cost":null, "costSource":"unpriced"},
        {"provider":"local", "model":"test", "tokensCacheWrite":4},
        {"provider":"openai", "model":"legacy", "cost":0.5},
        {"provider":"openai", "model":"legacy", "cost":0.0}
    ]);
    tokio::fs::write(&path, serde_json::to_vec(&rows).unwrap())
        .await
        .unwrap();
    let snapshot = usage_snapshot(&path).await;
    let summary = &snapshot["summary"];
    assert_eq!(summary["totalCost"], 0.75);
    assert_eq!(summary["totalRequests"], 6);
    assert_eq!(summary["unpricedRequests"], 2);
    assert_eq!(summary["knownCostRequests"], 4);
    assert_eq!(summary["costComplete"], false);
    assert_eq!(summary["totalTokens"], 37);
    assert_eq!(
        summary["costSources"],
        json!({"providerReported":1,"modelPriced":1,"legacyRecorded":2,"unpriced":2})
    );
    let bucket = &summary["byModel"]["openai/test"];
    assert_eq!(bucket["cost"], 0.25);
    assert_eq!(bucket["unpricedRequests"], 1);
    assert_eq!(bucket["knownCostRequests"], 2);
    assert_eq!(bucket["costComplete"], false);
    assert_eq!(summary["byProvider"]["local"]["unpricedRequests"], 1);
}

#[tokio::test]
async fn local_usage_all_unpriced_has_no_claim_of_complete_zero_cost() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("usage.json");
    tokio::fs::write(
        &path,
        br#"[{"provider":"local","cost":null,"costSource":"unpriced"}]"#,
    )
    .await
    .unwrap();
    let summary = &usage_snapshot(&path).await["summary"];
    assert_eq!(summary["totalCost"], 0.0);
    assert_eq!(summary["unpricedRequests"], 1);
    assert_eq!(summary["knownCostRequests"], 0);
    assert_eq!(summary["costComplete"], false);
}

#[tokio::test]
async fn local_usage_explicit_unpriced_or_future_source_is_not_counted_as_known() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("usage.json");
    tokio::fs::write(&path, br#"[{"cost":5,"costSource":"unpriced"},{"cost":5,"costSource":"futureSource"},{"cost":null,"costSource":"providerReported"}]"#).await.unwrap();
    let summary = &usage_snapshot(&path).await["summary"];
    assert_eq!(summary["totalCost"], 0.0);
    assert_eq!(summary["unpricedRequests"], 3);
}

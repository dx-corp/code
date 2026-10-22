use super::new_test_app;
use crate::agent::FromAgent;
use crate::bug_report;
use crate::session::SessionManager;

#[tokio::test]
async fn crash_diag_bug_report_suggestion_keeps_a_redacted_trace() {
    let temp = tempfile::tempdir().unwrap();
    let mut app = new_test_app();
    app.session_manager = SessionManager::with_sessions_dir("/tmp", temp.path());
    app.ensure_session_started().unwrap();
    let secret = "sk-abcdefghijklmnopqrst";
    app.handle_agent_message(FromAgent::Error {
        message: format!("private provider payload {secret}"),
        fatal: false,
        terminal: true,
        retryable: false,
    })
    .await
    .unwrap();
    let first = bug_report::load(app.session_manager.current_session_path().as_deref())
        .unwrap()
        .unwrap();
    assert!(!first.description.contains("private provider payload"));
    assert!(!first.description.contains(secret));
    assert!(first.include_diagnostics);
    assert_eq!(first.context.evidence.len(), 1);
    assert_eq!(first.context.evidence[0].kind, "trace");
    assert!(first.context.evidence[0].text.starts_with("class=other "));
    assert!(
        first.context.evidence[0]
            .text
            .contains("private provider payload")
    );
    assert!(!first.context.evidence[0].text.contains(secret));
    app.suggest_bug_report("another private payload");
    let again = bug_report::load(app.session_manager.current_session_path().as_deref())
        .unwrap()
        .unwrap();
    assert_eq!(first.id, again.id);
    assert_eq!(first.context.evidence, again.context.evidence);
}

#[tokio::test]
async fn crash_diag_bug_report_trace_names_admission_and_gateway_request() {
    let temp = tempfile::tempdir().unwrap();
    let mut app = new_test_app();
    app.session_manager = SessionManager::with_sessions_dir("/tmp", temp.path());
    app.ensure_session_started().unwrap();
    app.handle_agent_message(FromAgent::TurnStarted)
        .await
        .unwrap();
    app.handle_agent_message(FromAgent::ManagedGatewayReceipt {
        request_id: "req-9".into(),
        record_id: "rec-9".into(),
        lineage_id: "maestro-turn-v2:abc".into(),
        record_status: "completed".into(),
        provider_prompt_sha256: None,
        provider_tools_sha256: None,
        provider_tool_count: None,
    })
    .await
    .unwrap();
    let secret = "sk-abcdefghijklmnopqrst";
    app.handle_agent_message(FromAgent::Error {
        message: format!(
            "Agent error: provider admission denied for chat request req-9: lease {secret}"
        ),
        fatal: false,
        terminal: true,
        retryable: false,
    })
    .await
    .unwrap();
    let report = bug_report::load(app.session_manager.current_session_path().as_deref())
        .unwrap()
        .unwrap();
    assert_eq!(report.context.evidence.len(), 1);
    assert_eq!(report.context.evidence[0].kind, "trace");
    assert_eq!(report.context.evidence[0].source_id, "req-9");
    assert!(
        report.context.evidence[0]
            .text
            .starts_with("class=admission request=req-9 record=rec-9 lineage=maestro-turn-v2:abc ")
    );
    assert!(!report.context.evidence[0].text.contains(secret));
    assert!(!report.description.contains("provider admission denied"));
}

#[tokio::test]
async fn crash_diag_next_turn_does_not_reuse_the_previous_gateway_request() {
    let temp = tempfile::tempdir().unwrap();
    let mut app = new_test_app();
    app.session_manager = SessionManager::with_sessions_dir("/tmp", temp.path());
    app.ensure_session_started().unwrap();
    app.handle_agent_message(FromAgent::ManagedGatewayReceipt {
        request_id: "req-old".into(),
        record_id: "rec-old".into(),
        lineage_id: "line-old".into(),
        record_status: "completed".into(),
        provider_prompt_sha256: None,
        provider_tools_sha256: None,
        provider_tool_count: None,
    })
    .await
    .unwrap();
    app.handle_agent_message(FromAgent::TurnStarted)
        .await
        .unwrap();
    app.handle_agent_message(FromAgent::Error {
        message: "Agent error: boom".into(),
        fatal: false,
        terminal: true,
        retryable: false,
    })
    .await
    .unwrap();
    let report = bug_report::load(app.session_manager.current_session_path().as_deref())
        .unwrap()
        .unwrap();
    assert!(
        report.context.evidence[0]
            .text
            .starts_with("class=agent_error ")
    );
    assert!(!report.context.evidence[0].text.contains("req-old"));
    assert_ne!(report.context.evidence[0].source_id, "req-old");
}

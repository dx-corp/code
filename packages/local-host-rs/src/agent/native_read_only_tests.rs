//! Integration coverage for the TUI-owned read-only execution host.
//!
//! The runtime owns read-only call classification and wave orchestration. The
//! tests here keep the concrete `ToolExecutor`/`BatchExecutor` boundary
//! covered, including parallel starts, credential redaction, cancellation,
//! receipts, and terminal events.

use std::sync::Arc;

use maestro_runtime::agent::NativeReadOnlyToolCall;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::native_host::LocalNativeExecutionHost;
use super::{
    CredentialVault, ExecutionPhase, FromAgent, NativeExecutionHostHandle, NativeModelRoute,
    ToolOutcome,
};
use crate::hooks::IntegratedHookSystem;
use crate::tools::ToolExecutor;

fn test_host(executor: Arc<ToolExecutor>, cwd: &str) -> NativeExecutionHostHandle {
    LocalNativeExecutionHost::compose(
        executor,
        IntegratedHookSystem::new(cwd),
        |_, _| Err("read-only integration test host has no model resolver".to_owned()),
        |_| NativeModelRoute::DirectProvider,
        None,
    )
}

/// Mark `workspace` trusted the same way a real user would (global config,
/// keyed on the canonical workspace path), by pointing `$HOME` at a
/// throwaway directory containing only that trust grant.
///
/// The inline tool in this test is self-authored by the test itself, so
/// simulating a trusted workspace is the correct fixture. The process-wide
/// environment lock prevents other tests from observing the temporary home.
async fn mark_workspace_trusted(workspace: &std::path::Path) -> impl Drop {
    struct HomeGuard {
        _lock: tokio::sync::OwnedMutexGuard<()>,
        previous_home: Option<String>,
        _fake_home: tempfile::TempDir,
    }

    impl Drop for HomeGuard {
        fn drop(&mut self) {
            match &self.previous_home {
                Some(home) => std::env::set_var("HOME", home),
                None => std::env::remove_var("HOME"),
            }
        }
    }

    let lock = crate::config::test_process_env_lock_async().await;
    let fake_home = tempfile::tempdir().unwrap();
    let canonical = dunce::canonicalize(workspace).unwrap_or_else(|_| workspace.to_path_buf());
    let composer_dir = fake_home.path().join(".composer");
    std::fs::create_dir_all(&composer_dir).unwrap();
    std::fs::write(
        composer_dir.join("config.toml"),
        format!(
            "[projects.\"{}\"]\ntrust_level = \"trusted\"\n",
            canonical.display()
        ),
    )
    .unwrap();

    let previous_home = std::env::var("HOME").ok();
    std::env::set_var("HOME", fake_home.path());

    HomeGuard {
        _lock: lock,
        previous_home,
        _fake_home: fake_home,
    }
}

#[tokio::test]
async fn tui_native_host_read_only_wave_runs_parallel_with_redacted_receipts() {
    let temp = tempfile::tempdir().unwrap();
    let _home_guard = mark_workspace_trusted(temp.path()).await;
    let composer_dir = temp.path().join(".composer");
    std::fs::create_dir_all(&composer_dir).unwrap();
    std::fs::write(
        composer_dir.join("tools.json"),
        r#"{
            "tools": [{
                "name": "read_probe",
                "description": "Delayed read-only probe for native batching tests",
                "command": "sleep 0.08; printf GITHUB_TOKEN=ghs_123456789012345678901234567890123456",
                "parameters": {
                    "phase": {"type": "string"},
                    "index": {"type": "number"}
                },
                "annotations": {
                    "readOnly": true
                }
            }]
        }"#,
    )
    .unwrap();

    let credential_vault = CredentialVault::new();
    let executor = Arc::new(ToolExecutor::with_credential_vault(
        temp.path().to_str().unwrap(),
        credential_vault.clone(),
    ));
    let host = test_host(Arc::clone(&executor), temp.path().to_str().unwrap());
    let calls: Vec<NativeReadOnlyToolCall> = (0..4)
        .map(|index| NativeReadOnlyToolCall {
            call_id: format!("inspect-{index}"),
            tool_name: "read_probe".to_owned(),
            args: serde_json::json!({
                "phase": "inspect",
                "index": index
            }),
        })
        .collect();

    let (tx, mut rx) = mpsc::unbounded_channel();
    let results = host.execute_read_only_wave(&calls, &tx, None).await;

    assert_eq!(results.len(), 4);
    assert!(
        results
            .values()
            .all(|result| matches!(result.outcome, ToolOutcome::Succeeded { .. }))
    );
    assert!(
        results
            .values()
            .all(|result| result.model_content().contains("{{CRED|"))
    );
    assert!(results.values().all(|result| {
        credential_vault
            .resolve_all(&result.model_content())
            .contains("ghs_123456789012345678901234567890123456")
    }));

    let mut starts = 0;
    let mut ends = 0;
    let mut event_order = Vec::new();
    while let Ok(event) = rx.try_recv() {
        match event {
            FromAgent::ToolStart { .. } => {
                starts += 1;
                event_order.push("start");
            }
            FromAgent::ToolEnd { .. } => {
                ends += 1;
                event_order.push("end");
            }
            _ => {}
        }
    }
    assert_eq!(starts, 4);
    assert_eq!(ends, 4);
    let first_end = event_order
        .iter()
        .position(|event| *event == "end")
        .expect("read-only wave should emit a ToolEnd event");
    let starts_before_first_end = event_order[..first_end]
        .iter()
        .filter(|event| **event == "start")
        .count();
    assert_eq!(
        starts_before_first_end, 4,
        "read-only wave should start every member before the first completion"
    );
}

#[tokio::test]
async fn tui_native_host_cancelled_read_only_wave_retains_queued_receipts_and_events() {
    let temp = tempfile::tempdir().unwrap();
    let executor = Arc::new(ToolExecutor::new(temp.path().to_str().unwrap()));
    let host = test_host(Arc::clone(&executor), temp.path().to_str().unwrap());
    let calls = vec![NativeReadOnlyToolCall {
        call_id: "inspect-0".to_owned(),
        tool_name: "glob".to_owned(),
        args: serde_json::json!({"pattern": "*.rs"}),
    }];
    let (tx, mut rx) = mpsc::unbounded_channel();
    let cancel_token = CancellationToken::new();
    cancel_token.cancel();

    let results = host
        .execute_read_only_wave(&calls, &tx, Some(cancel_token))
        .await;

    let execution = results.get("inspect-0").expect("cancelled call result");
    assert!(matches!(
        execution.outcome,
        ToolOutcome::Cancelled {
            phase: ExecutionPhase::Queued
        }
    ));

    let events: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
    assert!(events.iter().any(|event| matches!(
        event,
        FromAgent::ToolEnd {
            call_id,
            success: false,
            receipt: Some(receipt),
            ..
        } if call_id == "inspect-0" && receipt.call_id == "inspect-0"
    )));
}

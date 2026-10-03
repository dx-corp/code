//! Exploration preserves nested hook admission, rewriting, and projection.

use super::*;
use crate::hooks::{HookResult, PostToolUseHook, PreToolUseHook};

struct ExploreHookAudit {
    rewrite_to: String,
    pre_calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    post_calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl PreToolUseHook for ExploreHookAudit {
    fn on_pre_tool_use(&self, input: &crate::hooks::PreToolUseInput) -> HookResult {
        self.pre_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let path = input
            .tool_input
            .get("file_path")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        if path.ends_with("blocked.txt") {
            return HookResult::Block {
                reason: "blocked by explore test hook".to_string(),
            };
        }
        if path.ends_with("rewrite.txt") {
            return HookResult::ModifyInput {
                new_input: serde_json::json!({"file_path": self.rewrite_to}),
            };
        }
        HookResult::Continue
    }

    fn matches(&self, tool_name: &str) -> bool {
        tool_name.eq_ignore_ascii_case("read")
    }
}

impl PostToolUseHook for ExploreHookAudit {
    fn on_post_tool_use(&self, _input: &crate::hooks::PostToolUseInput) -> HookResult {
        self.post_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        HookResult::Continue
    }

    fn matches(&self, tool_name: &str) -> bool {
        tool_name.eq_ignore_ascii_case("read")
    }
}

#[tokio::test]
async fn explore_runs_nested_tool_hooks_for_each_operation() {
    let dir = tempfile::tempdir().unwrap();
    let allowed = dir.path().join("allowed.txt");
    let rewrite = dir.path().join("rewrite.txt");
    let blocked = dir.path().join("blocked.txt");
    std::fs::write(&allowed, "allowed content").unwrap();
    std::fs::write(&rewrite, "rewrite should not be read directly").unwrap();

    let pre_calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let post_calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let audit = std::sync::Arc::new(ExploreHookAudit {
        rewrite_to: allowed.display().to_string(),
        pre_calls: std::sync::Arc::clone(&pre_calls),
        post_calls: std::sync::Arc::clone(&post_calls),
    });
    let mut hooks = crate::hooks::IntegratedHookSystem::new(&dir.path().display().to_string());
    hooks.registry.register_pre_tool_use(audit.clone());
    hooks.registry.register_post_tool_use(audit);

    let executor = ToolExecutor::new(dir.path().to_string_lossy().to_string());
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let execution = executor
        .execute_with_receipt_cancellable_inline_env(
            "explore",
            &serde_json::json!({
                "operations": [
                    {"tool": "read", "args": {"file_path": allowed}},
                    {"tool": "read", "args": {"file_path": rewrite}},
                    {"tool": "read", "args": {"file_path": blocked}}
                ]
            }),
            Some(&tx),
            "explore-hooks",
            ToolExecutionOptions {
                cancel: tokio_util::sync::CancellationToken::new(),
                approved_inline_env: None,
                hooks: Some(&mut hooks),
                emit_terminal_event: true,
            },
        )
        .await;

    let output = execution.to_legacy().output;
    let results: Vec<serde_json::Value> =
        serde_json::from_str(&output).expect("explore output should be JSON");
    assert_eq!(results.len(), 3);
    assert!(results[0]["success"].as_bool().unwrap());
    assert!(results[1]["success"].as_bool().unwrap());
    let duplicate_read_outputs = [
        results[0]["output"].as_str().unwrap(),
        results[1]["output"].as_str().unwrap(),
    ];
    assert_eq!(
        duplicate_read_outputs
            .iter()
            .filter(|output| output.contains("allowed content"))
            .count(),
        1,
        "one concurrent read must retain the full file contents"
    );
    assert_eq!(
        duplicate_read_outputs
            .iter()
            .filter(|output| output.starts_with("Unchanged since previous read:"))
            .count(),
        1,
        "the duplicate concurrent read must use the session projection"
    );
    assert!(!results[2]["success"].as_bool().unwrap());
    assert!(
        results[2]["error"]
            .as_str()
            .unwrap()
            .contains("blocked by explore test hook")
    );
    assert_eq!(
        pre_calls.load(std::sync::atomic::Ordering::SeqCst),
        3,
        "every nested operation must run PreToolUse"
    );
    assert_eq!(
        post_calls.load(std::sync::atomic::Ordering::SeqCst),
        2,
        "PostToolUse must run for each operation that actually executed"
    );
    assert!(
        std::iter::from_fn(|| rx.try_recv().ok()).any(|event| matches!(
            event,
            FromAgent::HookBlocked { call_id, tool, .. }
                if call_id == "explore-hooks:explore:2" && tool == "read"
        ))
    );
}

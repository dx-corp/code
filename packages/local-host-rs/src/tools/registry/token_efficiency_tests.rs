use super::*;

#[tokio::test]
async fn repeated_full_text_read_returns_an_unchanged_marker() {
    let dir = tempfile::tempdir().unwrap();
    let file_path = dir.path().join("stable.txt");
    std::fs::write(&file_path, "stable contents\n").unwrap();
    let executor = ToolExecutor::new(dir.path().to_str().unwrap());
    let args = serde_json::json!({
        "path": "stable.txt",
        "lineNumbers": false,
        "wrapInCodeFence": false,
        "withDiagnostics": false,
    });

    let first = executor.execute("read", &args, None, "read-first").await;
    let second = executor.execute("read", &args, None, "read-second").await;

    assert_eq!(first.output, "stable contents");
    assert_eq!(second.output, "Unchanged since previous read: stable.txt");
    assert_eq!(
        (executor.cache_stats().hits, executor.cache_stats().misses),
        (1, 1)
    );
}

#[tokio::test]
async fn changed_full_text_read_returns_a_unified_diff() {
    let dir = tempfile::tempdir().unwrap();
    let file_path = dir.path().join("changing.txt");
    std::fs::write(&file_path, "one\ntwo\n").unwrap();
    let executor = ToolExecutor::new(dir.path().to_str().unwrap());
    let args = serde_json::json!({
        "path": "changing.txt",
        "lineNumbers": false,
        "wrapInCodeFence": false,
        "withDiagnostics": false,
    });
    let first = executor.execute("read", &args, None, "read-first").await;
    assert_eq!(first.output, "one\ntwo");

    std::fs::write(&file_path, "one\nthree\n").unwrap();
    executor.clear_cache();
    let second = executor.execute("read", &args, None, "read-second").await;

    assert!(
        second
            .output
            .starts_with("Diff since previous read: changing.txt\n")
    );
    assert!(second.output.contains("-two"), "{}", second.output);
    assert!(second.output.contains("+three"), "{}", second.output);
}

#[tokio::test]
async fn partial_reads_are_not_replaced_by_session_diffs() {
    let dir = tempfile::tempdir().unwrap();
    let file_path = dir.path().join("partial.txt");
    std::fs::write(&file_path, "one\ntwo\nthree\n").unwrap();
    let executor = ToolExecutor::new(dir.path().to_str().unwrap());
    let args = serde_json::json!({
        "path": "partial.txt",
        "offset": 2,
        "limit": 1,
        "lineNumbers": false,
        "wrapInCodeFence": false,
        "withDiagnostics": false,
    });

    let first = executor.execute("read", &args, None, "read-first").await;
    let second = executor.execute("read", &args, None, "read-second").await;

    assert_eq!(first.output, "two");
    assert_eq!(second.output, "two");
}

#[tokio::test]
async fn cached_full_read_uses_the_session_projection() {
    let dir = tempfile::tempdir().unwrap();
    let file_path = dir.path().join("cache_test.txt");
    std::fs::write(&file_path, "cached content").unwrap();
    let executor = ToolExecutor::new(dir.path().to_str().unwrap());
    let args = serde_json::json!({"path": "cache_test.txt"});

    let first = executor.execute("read", &args, None, "call-1").await;
    assert!(first.success);
    assert_eq!(
        (executor.cache_stats().hits, executor.cache_stats().misses),
        (0, 1)
    );

    let second = executor.execute("read", &args, None, "call-2").await;
    assert!(second.success);
    assert_eq!(
        second.output,
        "Unchanged since previous read: cache_test.txt"
    );
    assert_eq!(
        (executor.cache_stats().hits, executor.cache_stats().misses),
        (1, 1)
    );
}

use super::*;

#[test]
fn session_info_title() {
    let info = SessionInfo {
        parent_session: None,
        id: "abc123".to_string(),
        path: PathBuf::from("/tmp/test.jsonl"),
        cwd: "/tmp".to_string(),
        model: "anthropic/claude-3".to_string(),
        thinking_level: ThinkingLevel::Medium,
        timestamp: "2024-01-15T10:30:00Z".to_string(),
        stats: SessionStats::default(),
        meta: None,
        preview: None,
        modified: None,
    };

    assert!(info.title().contains("abc123"));
    assert_eq!(info.short_id(), "abc123");
}

#[test]
fn test_session_info_clone() {
    let info = SessionInfo {
        parent_session: None,
        id: "test".to_string(),
        path: PathBuf::from("/test"),
        cwd: "/cwd".to_string(),
        model: "model".to_string(),
        thinking_level: ThinkingLevel::Medium,
        timestamp: "2024".to_string(),
        stats: SessionStats::default(),
        meta: None,
        preview: None,
        modified: None,
    };

    let cloned = info.clone();
    assert_eq!(cloned.id, info.id);
    assert_eq!(cloned.cwd, info.cwd);
}

#[test]
fn test_session_info_debug() {
    let info = SessionInfo {
        parent_session: None,
        id: "test".to_string(),
        path: PathBuf::from("/test"),
        cwd: "/cwd".to_string(),
        model: "model".to_string(),
        thinking_level: ThinkingLevel::Medium,
        timestamp: "2024".to_string(),
        stats: SessionStats::default(),
        meta: None,
        preview: None,
        modified: None,
    };

    let debug = format!("{:?}", info);
    assert!(debug.contains("test"));
}

// ============================================================
// Prune Sessions Tests
// ============================================================

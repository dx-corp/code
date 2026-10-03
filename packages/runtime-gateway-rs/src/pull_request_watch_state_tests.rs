use super::*;

fn snapshot() -> Snapshot {
    Snapshot {
        head: "abc".into(),
        state: PullRequestState::Open,
        checks: BTreeMap::from([("required".into(), CheckState::Pending)]),
        required_checks: BTreeSet::from(["required".into()]),
        comments: BTreeSet::from(["issue:1".into()]),
        conflicting: Some(false),
    }
}

#[test]
fn pull_request_watch_state_accepts_only_canonical_github_pull_urls() {
    let target = PullRequestRef::parse("https://github.com/dx-corp/mono/pull/123").unwrap();
    assert_eq!(target.repo, "dx-corp/mono");
    assert_eq!(target.number, 123);
    for invalid in [
        "https://github.com.evil.test/dx-corp/mono/pull/1",
        "http://github.com/dx-corp/mono/pull/1",
        "https://github.com/dx-corp/mono/pull/1?x=1",
        "https://github.com/dx-corp/mono/pull/1/commits",
        "https://github.com/../mono/pull/1",
        "https://github.com/dx-corp/mono/pull/0",
    ] {
        assert!(PullRequestRef::parse(invalid).is_err(), "{invalid}");
    }
}

#[test]
fn pull_request_watch_state_failed_checks_notify_once_even_if_initially_failed() {
    let mut current = snapshot();
    current.checks.insert("required".into(), CheckState::Failed);
    let mut baseline = WatchBaseline::new(&current);
    assert!(
        reduce(&mut baseline, Ok(current.clone()))
            .prompt
            .unwrap()
            .contains("Failed checks")
    );
    assert!(reduce(&mut baseline, Ok(current.clone())).prompt.is_none());
    current
        .checks
        .insert("required".into(), CheckState::Pending);
    reduce(&mut baseline, Ok(current.clone()));
    current.checks.insert("required".into(), CheckState::Failed);
    assert!(reduce(&mut baseline, Ok(current)).prompt.is_some());
}

#[test]
fn pull_request_watch_state_green_requires_nonempty_exact_required_checks() {
    let current = snapshot();
    let mut baseline = WatchBaseline::new(&current);
    let mut next = current.clone();
    next.checks.insert("advisory".into(), CheckState::Passed);
    let transition = reduce(&mut baseline, Ok(next.clone()));
    assert!(
        !transition
            .prompt
            .unwrap()
            .contains("Required checks passed")
    );
    next.checks.insert("required".into(), CheckState::Passed);
    assert!(
        reduce(&mut baseline, Ok(next.clone()))
            .prompt
            .unwrap()
            .contains("Required checks passed")
    );
    assert!(reduce(&mut baseline, Ok(next)).prompt.is_none());
    let mut empty = snapshot();
    empty.required_checks.clear();
    empty.checks.clear();
    let mut baseline = WatchBaseline::new(&empty);
    assert!(reduce(&mut baseline, Ok(empty)).prompt.is_none());
}

#[test]
fn pull_request_watch_state_head_resets_check_health_and_comment_limit() {
    let mut current = snapshot();
    let mut baseline = WatchBaseline::new(&current);
    baseline.comments_only_wakes = 9;
    current.head = "def".into();
    let transition = reduce(&mut baseline, Ok(current));
    assert!(transition.prompt.unwrap().contains("Head changed"));
    assert!(!transition.stop);
    assert_eq!(baseline.comments_only_wakes, 0);
}

#[test]
fn pull_request_watch_state_unknown_mergeability_preserves_known_conflict() {
    let mut current = snapshot();
    current.conflicting = Some(true);
    let mut baseline = WatchBaseline::new(&current);
    current.conflicting = None;
    assert!(reduce(&mut baseline, Ok(current.clone())).prompt.is_none());
    assert_eq!(baseline.conflicting, Some(true));
    current.conflicting = Some(false);
    assert!(
        reduce(&mut baseline, Ok(current))
            .prompt
            .unwrap()
            .contains("Conflict resolved")
    );
}

#[test]
fn pull_request_watch_state_comments_deduplicate_by_id_and_stop_after_ten_wakes() {
    let mut current = snapshot();
    let mut baseline = WatchBaseline::new(&current);
    for number in 1..=10 {
        current.comments.insert(format!("inline:{number}"));
        let transition = reduce(&mut baseline, Ok(current.clone()));
        assert!(transition.prompt.is_some());
        assert_eq!(transition.stop, number == 10);
        if number < 10 {
            assert!(reduce(&mut baseline, Ok(current.clone())).prompt.is_none());
        }
    }
}

#[test]
fn pull_request_watch_state_reads_fail_closed_and_stop_after_fifteen_errors() {
    let mut baseline = WatchBaseline::new(&snapshot());
    for number in 1..=15 {
        let transition = reduce(&mut baseline, Err("permission denied".into()));
        assert_eq!(transition.stop, number == 15);
        assert_eq!(transition.prompt.is_some(), number == 15);
    }
    let mut baseline = WatchBaseline::new(&snapshot());
    reduce(&mut baseline, Err("temporary".into()));
    reduce(&mut baseline, Ok(snapshot()));
    assert_eq!(baseline.consecutive_errors, 0);
}

#[test]
fn pull_request_watch_state_terminal_status_stops_without_duplicate_wakes() {
    for state in [PullRequestState::Merged, PullRequestState::Closed] {
        let mut current = snapshot();
        let mut baseline = WatchBaseline::new(&current);
        current.state = state;
        let transition = reduce(&mut baseline, Ok(current.clone()));
        assert!(transition.stop);
        assert!(transition.prompt.is_some());
        let transition = reduce(&mut baseline, Ok(current));
        assert!(transition.stop);
        assert!(transition.prompt.is_none());
    }
}

#[test]
fn pull_request_watch_state_comment_reader_keeps_equal_timestamps_and_excludes_self() {
    let payload = serde_json::json!([[
        {"id":1,"created_at":"same","user":{"login":"other"}},
        {"id":2,"created_at":"same","user":{"login":"other"}},
        {"id":3,"created_at":"same","user":{"login":"SELF"}}
    ]]);
    let comments = comment_ids(&payload, "inline", "self").unwrap();
    assert_eq!(
        comments,
        BTreeSet::from(["inline:1".into(), "inline:2".into()])
    );
}

#[test]
fn pull_request_watch_state_missing_required_check_prevents_green_claim() {
    let mut current = snapshot();
    current.checks.insert("required".into(), CheckState::Passed);
    current.required_checks.insert("not-yet-created".into());
    let mut baseline = WatchBaseline::new(&current);
    assert!(reduce(&mut baseline, Ok(current)).prompt.is_none());
    assert!(!baseline.required_green);
}

#[test]
fn pull_request_watch_state_check_parser_rejects_conflicts_and_missing_fields() {
    let value = serde_json::json!([
        {"name":"one", "bucket":"pass"},
        {"name":"two", "bucket":"pending"},
        {"name":"three", "bucket":"skipping"},
        {"name":"four", "bucket":"cancel"}
    ]);
    let checks = check_states(&value).unwrap();
    assert_eq!(checks["one"], CheckState::Passed);
    assert_eq!(checks["two"], CheckState::Pending);
    assert_eq!(checks["three"], CheckState::Unknown);
    assert_eq!(checks["four"], CheckState::Failed);
    assert!(check_states(&serde_json::json!([{"name":"one"}])).is_err());
    assert!(
        check_states(&serde_json::json!([
            {"name":"one", "bucket":"pass"},
            {"name":"one", "bucket":"fail"}
        ]))
        .is_err()
    );
}

#[test]
fn pull_request_watch_state_green_comments_still_hit_comments_only_limit() {
    let mut current = snapshot();
    current.checks.insert("required".into(), CheckState::Passed);
    let mut baseline = WatchBaseline::new(&current);
    reduce(&mut baseline, Ok(current.clone()));
    for number in 1..=10 {
        current.comments.insert(format!("review:{number}"));
        let transition = reduce(&mut baseline, Ok(current.clone()));
        assert_eq!(transition.stop, number == 10);
    }
}

#[test]
fn pull_request_watch_state_check_progress_resets_comments_only_limit() {
    let mut current = snapshot();
    let mut baseline = WatchBaseline::new(&current);
    baseline.comments_only_wakes = 9;
    current
        .checks
        .insert("advisory".into(), CheckState::Pending);
    reduce(&mut baseline, Ok(current));
    assert_eq!(baseline.comments_only_wakes, 0);
}

#[tokio::test]
async fn pull_request_watch_state_output_capture_enforces_limit_without_truncation() {
    assert_eq!(read_bounded(&b"1234"[..], 4).await.unwrap(), b"1234");
    assert!(read_bounded(&b"12345"[..], 4).await.is_err());
}

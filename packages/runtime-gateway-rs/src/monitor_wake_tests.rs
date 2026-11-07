use super::*;
use maestro_local_host::tools::background_tasks::{MonitorNotification, MonitorNotificationKind};

fn scope(generation: &str) -> BackgroundTaskScope {
    BackgroundTaskScope {
        session_id: "s".into(),
        session_generation: generation.into(),
        owner: Some("u".into()),
        organization_id: Some("o".into()),
        workspace_id: Some("w".into()),
    }
}

fn auth(subject: &str) -> AuthContext {
    AuthContext {
        subject: Some(subject.into()),
        organization_id: Some("o".into()),
        workspace_id: Some("w".into()),
        ..AuthContext::default()
    }
}

fn notification(scope: &BackgroundTaskScope, kind: MonitorNotificationKind) -> MonitorNotification {
    MonitorNotification {
        scope: Some(scope.clone()),
        monitor_id: "m".into(),
        task_id: "t".into(),
        description: "build".into(),
        kind,
        timestamp_ms: 0,
    }
}

fn output(scope: &BackgroundTaskScope, line: &str) -> MonitorNotification {
    notification(
        scope,
        MonitorNotificationKind::Lines {
            lines: vec![line.into()],
            omitted: 0,
        },
    )
}

fn exited(scope: &BackgroundTaskScope) -> MonitorNotification {
    notification(
        scope,
        MonitorNotificationKind::Exited {
            exit_code: Some(0),
            events: 1,
        },
    )
}

fn owners_with(scope: &BackgroundTaskScope) -> MonitorOwners {
    let owners = MonitorOwners::default();
    owners.register("s", scope.clone(), auth("u"));
    owners
}

#[test]
fn idle_output_wakes_but_busy_output_waits_for_the_next_turn() {
    let scope = scope("generation-one");
    let owners = owners_with(&scope);
    owners.absorb("s", true, vec![output(&scope, "compiling")]);
    assert!(
        owners.wake_candidate("s").is_none(),
        "output during a turn must not start another"
    );
    owners.absorb("s", false, vec![output(&scope, "linking")]);
    assert!(owners.wake_candidate("s").is_some());
}

#[test]
fn a_terminal_notice_during_a_turn_still_wakes_after_it() {
    let scope = scope("generation-one");
    let owners = owners_with(&scope);
    owners.absorb("s", true, vec![output(&scope, "compiling"), exited(&scope)]);
    let (_, _, through) = owners.wake_candidate("s").expect("terminal wake");
    assert_eq!(through, 2);
}

#[test]
fn a_turn_delivers_every_pending_note_and_acknowledges_only_those() {
    let scope = scope("generation-one");
    let owners = owners_with(&scope);
    owners.absorb(
        "s",
        true,
        vec![output(&scope, "one"), output(&scope, "two")],
    );
    let (block, delivery) = owners.take_for_turn("s");
    let block = block.expect("pending notes");
    assert!(
        block.contains("| one") && block.contains("| two"),
        "{block}"
    );
    assert_eq!(
        block
            .matches("[tool_notification kind=\"monitor\"]")
            .count(),
        2
    );

    // A note that arrives during the turn is not acknowledged by it.
    owners.absorb("s", true, vec![exited(&scope)]);
    owners.acknowledge(&delivery);
    let (rest, _) = owners.take_for_turn("s");
    let rest = rest.expect("the later note stays pending");
    assert!(
        rest.contains("event: exited") && !rest.contains("| one"),
        "{rest}"
    );

    // An unacknowledged (failed) turn keeps everything for the next one.
    let (again, _) = owners.take_for_turn("s");
    assert_eq!(again, Some(rest));
}

#[test]
fn a_failed_wake_disarms_instead_of_retrying_hot() {
    let scope = scope("generation-one");
    let owners = owners_with(&scope);
    owners.absorb("s", false, vec![output(&scope, "ready")]);
    let (_, _, through) = owners.wake_candidate("s").unwrap();
    owners.disarm_through("s", through);
    assert!(owners.wake_candidate("s").is_none());
    assert!(
        owners.take_for_turn("s").0.is_some(),
        "notes wait for the next turn"
    );
}

#[test]
fn notifications_join_only_their_own_scope_generation() {
    let current = scope("generation-two");
    let owners = owners_with(&current);
    owners.absorb("s", false, vec![output(&scope("generation-one"), "stale")]);
    assert!(owners.take_for_turn("s").0.is_none());
    assert!(owners.wake_candidate("s").is_none());
}

#[test]
fn pending_notes_are_bounded_and_shed_output_before_terminal_notices() {
    let scope = scope("generation-one");
    let owners = owners_with(&scope);
    owners.absorb("s", true, vec![exited(&scope)]);
    let flood = (0..MAX_PENDING + 10)
        .map(|index| output(&scope, &format!("line {index}")))
        .collect();
    owners.absorb("s", true, flood);
    let (block, _) = owners.take_for_turn("s");
    let block = block.unwrap();
    assert_eq!(
        block
            .matches("[tool_notification kind=\"monitor\"]")
            .count(),
        MAX_PENDING
    );
    assert!(block.contains("event: exited"));
}

#[test]
fn owners_without_activity_or_pending_notes_are_pruned() {
    let scope = scope("generation-prune-only");
    let owners = owners_with(&scope);
    owners.absorb("s", true, vec![output(&scope, "kept")]);
    owners.prune();
    assert_eq!(owners.sessions(), vec!["s".to_string()]);
    let (_, delivery) = owners.take_for_turn("s");
    owners.acknowledge(&delivery);
    owners.prune();
    assert!(owners.sessions().is_empty());
}

#[tokio::test]
async fn a_busy_session_is_never_claimed_for_a_wake() {
    let state = crate::tests::test_app_state_with_sessions(HashMap::new());
    let scope = scope("generation-one");
    state
        .pull_request_watches
        .monitors
        .register("s", scope.clone(), auth("u"));
    state
        .pull_request_watches
        .monitors
        .absorb("s", false, vec![output(&scope, "ready")]);
    let turn = state.pull_request_watches.enter(Some("s")).unwrap();
    assert!(state.pull_request_watches.is_active("s"));
    assert!(state.pull_request_watches.claim_idle("s").is_none());
    drop(turn);
    assert!(!state.pull_request_watches.is_active("s"));
    assert!(state.pull_request_watches.claim_idle("s").is_some());
}

#[tokio::test]
async fn only_the_session_owner_registers_its_monitors() {
    let session: SessionRecord = serde_json::from_value(serde_json::json!({
        "id": "s", "owner": "u", "organizationId": "o", "workspaceId": "w",
        "title": "t", "createdAt": "generation-one", "updatedAt": "now", "messageCount": 0
    }))
    .unwrap();
    let state = crate::tests::test_app_state_with_sessions(HashMap::from([("s".into(), session)]));
    assert_eq!(
        crate::background::scope_for_session(&state, Some("s"), &auth("u")).await,
        Some(scope("generation-one"))
    );
    assert_eq!(
        crate::background::scope_for_session(&state, Some("s"), &auth("intruder")).await,
        None
    );
    // No line watch or queued notification for this scope: nothing to keep.
    register(&state, "s", &auth("u")).await;
    assert!(state.pull_request_watches.monitors.sessions().is_empty());
}

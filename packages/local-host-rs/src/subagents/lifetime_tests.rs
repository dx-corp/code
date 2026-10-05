//! Child ownership contracts exercised through native launch, wait, and resume.
use super::*;
use crate::ai::{ScriptedBlock, ScriptedClient, ScriptedResponse, StopReason, UnifiedClient};
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::Notify;

const WAIT: Duration = Duration::from_secs(30);
const MODEL: &str = "scripted-replay/maestro-replay-v1";

#[derive(Debug, PartialEq, Eq)]
enum Probe {
    Started,
    CompletionHeld,
    Closed,
}

struct LifetimeFactory {
    client: ScriptedClient,
    probes: mpsc::UnboundedSender<Probe>,
    snapshot_gate: Option<Arc<Notify>>,
    calls: AtomicUsize,
}

impl ChildAgentFactory for LifetimeFactory {
    fn spawn(
        &self,
        request: ChildLaunchRequest,
    ) -> anyhow::Result<(NativeAgent, mpsc::UnboundedReceiver<FromAgent>)> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        assert!(
            SUBAGENT_TOOL_NAMES
                .iter()
                .all(|name| !request.allowed_tools.contains(*name))
        );
        let hook_log = PathBuf::from(&request.config.cwd).join("child-hook-events.log");
        let (agent, mut source) = crate::agent::NativeAgent::new_with_test_client(
            request.config,
            UnifiedClient::Scripted(self.client.clone()),
        )?;
        agent.set_hook_log_file(hook_log.display().to_string())?;
        let (sender, events) = mpsc::unbounded_channel();
        let probes = self.probes.clone();
        let gate = self.snapshot_gate.clone();
        tokio::spawn(async move {
            let mut started = false;
            let mut gate_used = false;
            while let Some(event) = source.recv().await {
                if !started && matches!(&event, FromAgent::ResponseChunk { .. }) {
                    started = true;
                    let _ = probes.send(Probe::Started);
                }
                // Hold the first real terminal/checkpoint event as a barrier.
                // The lifecycle owner must retain its lease and scheduler slot
                // until this arrives and the native actor has shut down.
                if started
                    && !gate_used
                    && matches!(
                        &event,
                        FromAgent::TurnCompleted { .. } | FromAgent::ConversationSnapshot { .. }
                    )
                {
                    if let Some(gate) = &gate {
                        gate_used = true;
                        let _ = probes.send(Probe::CompletionHeld);
                        gate.notified().await;
                    }
                }
                let _ = sender.send(event);
            }
            let _ = probes.send(Probe::Closed);
        });
        Ok((agent.into_runtime(), events))
    }
}

fn fixture(
    root: &Path,
    responses: Vec<ScriptedResponse>,
    snapshot_gate: Option<Arc<Notify>>,
) -> (
    SubagentManager,
    Arc<LifetimeFactory>,
    mpsc::UnboundedReceiver<Probe>,
) {
    let (sender, probes) = mpsc::unbounded_channel();
    let factory = Arc::new(LifetimeFactory {
        client: ScriptedClient::new(MODEL, responses),
        probes: sender,
        snapshot_gate,
        calls: AtomicUsize::new(0),
    });
    let mut manager = SubagentManager::with_root(root.into(), root.join("records"))
        .with_child_agent_factory(factory.clone());
    manager.runtime = Arc::new(RuntimeRegistry::with_capacity(1));
    (manager, factory, probes)
}

fn pending_response() -> ScriptedResponse {
    ScriptedResponse {
        blocks: vec![
            ScriptedBlock::Text("Child is running".into()),
            ScriptedBlock::Pending,
        ],
        stop_reason: StopReason::EndTurn,
        error: None,
    }
}

fn spawn_args(background: bool) -> serde_json::Value {
    serde_json::json!({
        "task": "Inspect the child lifetime fixture",
        "backend": "native", "role": "explore", "isolation": "shared",
        "model": MODEL, "run_in_background": background,
        "timeout_ms": 120_000,
    })
}

async fn probe(receiver: &mut mpsc::UnboundedReceiver<Probe>, expected: Probe) {
    assert_eq!(
        tokio::time::timeout(WAIT, receiver.recv()).await.unwrap(),
        Some(expected)
    );
}

fn isolated_runtime(future: impl std::future::Future<Output = ()>) {
    let home = tempfile::tempdir().unwrap();
    // Call only in the dedicated test process, before starting its runtime.
    // Neither credential discovery nor managed setup may use developer state.
    for name in crate::credential_mode::TEST_IDENTITY_ENV_VARS {
        std::env::remove_var(name);
    }
    std::env::set_var("HOME", home.path());
    std::env::set_var("MAESTRO_HOME", home.path());
    std::env::set_var("MAESTRO_OAUTH_STORAGE_MODE", "file");
    assert!(matches!(
        crate::credential_mode::detect().unwrap(),
        crate::credential_mode::DetectedMode::Byok
    ));
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(future);
}

fn assert_actor_cleanup(root: &Path, attempts: usize) {
    let log = std::fs::read_to_string(root.join("child-hook-events.log")).unwrap();
    assert_eq!(
        log.lines()
            .filter(|line| line.contains("SessionEnd"))
            .count(),
        attempts,
        "each native actor must finish its shutdown hook exactly once before the caller receives its result: {log}"
    );
}

#[test]
fn foreground_token_links_immediately_and_child_cancel_does_not_cancel_parent() {
    let parent = CancellationToken::new();
    let foreground = child_run_token(false, Some(&parent));
    let background = child_run_token(true, Some(&parent));
    foreground.cancel();
    assert!(!parent.is_cancelled());
    let sibling = child_run_token(false, Some(&parent));
    parent.cancel();
    assert!(
        sibling.is_cancelled(),
        "parent cancellation must not need a spawned watcher poll"
    );
    assert!(child_run_token(false, Some(&parent)).is_cancelled());
    assert!(!background.is_cancelled());
    assert!(!child_run_token(true, Some(&parent)).is_cancelled());
}

#[test]
fn already_cancelled_foreground_parent_prevents_child_factory_admission() {
    if crate::config::test_reexec_for_process_isolation() {
        return;
    }
    isolated_runtime(async {
        let root = tempfile::tempdir().unwrap();
        let (manager, factory, _probes) = fixture(root.path(), vec![pending_response()], None);
        let mut record = super::tests::control_receipt_record(root.path());
        record.status = SubagentStatus::Queued;
        record.started_at_ms = None;
        let parent = CancellationToken::new();
        parent.cancel();
        let (_, control_rx) = mpsc::channel(1);
        let parent_vault = CredentialVault::new();
        let terminal = manager
            .run_child(
                record,
                ChildRun {
                    prompt: "Must not construct an actor".into(),
                    history: None,
                    sandbox_policy: None,
                    token: child_run_token(false, Some(&parent)),
                    control_rx,
                },
                ChildLaunch {
                    lease: None,
                    credential_vault: CredentialVault::new(),
                    parent_credential_generation: parent_vault.generation(),
                    parent_credential_vault: parent_vault,
                },
            )
            .await
            .unwrap();
        assert_eq!(terminal.status, SubagentStatus::Cancelled);
        assert_eq!(factory.calls.load(Ordering::SeqCst), 0);
        assert_eq!(manager.runtime.available_permits(), 1);
        assert_eq!(
            manager.load_record(&terminal.id).unwrap().status,
            SubagentStatus::Cancelled
        );
    });
}

#[test]
fn parent_abort_cancels_foreground_while_waiting_for_a_scheduler_slot() {
    if crate::config::test_reexec_for_process_isolation() {
        return;
    }
    isolated_runtime(async {
        let root = tempfile::tempdir().unwrap();
        let (manager, factory, _probes) = fixture(root.path(), vec![pending_response()], None);
        let occupied = manager.runtime.acquire_permit().await.unwrap();
        let mut record = super::tests::control_receipt_record(root.path());
        record.status = SubagentStatus::Queued;
        record.started_at_ms = None;
        let parent = CancellationToken::new();
        let (_, control_rx) = mpsc::channel(1);
        let vault = CredentialVault::new();
        let run = manager.run_child(
            record,
            ChildRun {
                prompt: "Wait for scheduler capacity".into(),
                history: None,
                sandbox_policy: None,
                token: child_run_token(false, Some(&parent)),
                control_rx,
            },
            ChildLaunch {
                lease: None,
                credential_vault: CredentialVault::new(),
                parent_credential_generation: vault.generation(),
                parent_credential_vault: vault,
            },
        );
        tokio::pin!(run);
        // Poll the real admission boundary before cancelling. No timing sleeps
        // or model calls are needed to prove the child is queued.
        tokio::select! {
            biased;
            result = &mut run => panic!("queued child completed before cancellation: {result:?}"),
            () = std::future::ready(()) => {}
        }
        parent.cancel();
        let terminal = tokio::time::timeout(WAIT, run).await.unwrap().unwrap();
        assert_eq!(terminal.status, SubagentStatus::Cancelled);
        assert_eq!(factory.calls.load(Ordering::SeqCst), 0);
        assert_eq!(
            manager.runtime.available_permits(),
            0,
            "the other run still owns its slot"
        );
        drop(occupied);
        assert_eq!(manager.runtime.available_permits(), 1);
    });
}

#[test]
fn parent_abort_cancels_foreground_spawn_and_resume_after_actor_cleanup() {
    if crate::config::test_reexec_for_process_isolation() {
        return;
    }
    isolated_runtime(async {
        let root = tempfile::tempdir().unwrap();
        let (manager, factory, mut probes) = fixture(
            root.path(),
            vec![pending_response(), pending_response()],
            None,
        );
        let vault = CredentialVault::new();
        let parent = CancellationToken::new();
        let launch_manager = manager.clone();
        let launch_parent = parent.clone();
        let launch_vault = vault.clone();
        let launch = tokio::spawn(async move {
            launch_manager
                .spawn(
                    &spawn_args(false),
                    "spawn-call",
                    None,
                    launch_vault,
                    Some(&launch_parent),
                )
                .await
        });
        probe(&mut probes, Probe::Started).await;
        parent.cancel();
        let result = tokio::time::timeout(WAIT, launch).await.unwrap().unwrap();
        assert!(!result.success);
        assert_actor_cleanup(root.path(), 1);
        let id = result.details.as_ref().unwrap()["subagentId"]
            .as_str()
            .unwrap()
            .to_owned();
        assert_eq!(
            manager.load_record(&id).unwrap().status,
            SubagentStatus::Cancelled
        );
        probe(&mut probes, Probe::Closed).await;
        assert!(manager.runtime.get(&id).is_none());
        assert_eq!(manager.runtime.available_permits(), 1);
        assert!(!SubagentManager::execution_lease_is_held(
            &manager.load_record(&id).unwrap()
        ));

        let parent = CancellationToken::new();
        let resume_parent = parent.clone();
        let resume_manager = manager.clone();
        let resume_id = id.clone();
        let resume = tokio::spawn(async move {
            resume_manager.resume(&serde_json::json!({
                "subagent_id": resume_id, "task": "Resume the fixture", "run_in_background": false,
            }), "resume-call", None, vault, Some(&resume_parent)).await
        });
        probe(&mut probes, Probe::Started).await;
        parent.cancel();
        let result = tokio::time::timeout(WAIT, resume).await.unwrap().unwrap();
        assert!(!result.success);
        assert_actor_cleanup(root.path(), 2);
        probe(&mut probes, Probe::Closed).await;
        let terminal = manager.load_record(&id).unwrap();
        assert_eq!(terminal.status, SubagentStatus::Cancelled);
        assert_eq!(terminal.attempt, 2);
        assert_eq!(factory.calls.load(Ordering::SeqCst), 2);
        assert_eq!(manager.runtime.available_permits(), 1);
        assert!(manager.runtime.get(&id).is_none());
    });
}

#[test]
fn background_child_survives_parent_and_wait_cancel_and_relays_result_once() {
    if crate::config::test_reexec_for_process_isolation() {
        return;
    }
    isolated_runtime(async {
        let root = tempfile::tempdir().unwrap();
        let gate = Arc::new(Notify::new());
        let (manager, factory, mut probes) = fixture(
            root.path(),
            vec![ScriptedResponse::text("Saved child answer")],
            Some(gate.clone()),
        );
        let parent = CancellationToken::new();
        let spawned = manager
            .spawn(
                &spawn_args(true),
                "spawn-call",
                None,
                CredentialVault::new(),
                Some(&parent),
            )
            .await;
        assert!(spawned.success);
        let id = spawned.details.as_ref().unwrap()["subagentId"]
            .as_str()
            .unwrap()
            .to_owned();
        probe(&mut probes, Probe::Started).await;
        probe(&mut probes, Probe::CompletionHeld).await;
        parent.cancel();
        let child_token = manager.runtime.get(&id).unwrap();
        assert!(!child_token.is_cancelled());
        let args = serde_json::json!({"subagent_id": id, "timeout_ms": 30_000});
        let waiter = CancellationToken::new();
        waiter.cancel();
        let wait = manager.wait(&args, Some(&waiter)).await;
        assert!(!wait.success);
        assert_eq!(wait.details.unwrap()["cancelled"], true);
        assert!(!child_token.is_cancelled());
        assert_eq!(
            manager.load_record(&id).unwrap().status,
            SubagentStatus::Running
        );
        assert_eq!(manager.runtime.available_permits(), 0);
        assert!(SubagentManager::execution_lease_is_held(
            &manager.load_record(&id).unwrap()
        ));
        assert!(
            manager.poll_lifecycle_events().is_empty(),
            "no result before the terminal checkpoint and cleanup"
        );

        gate.notify_one();
        let completed = tokio::time::timeout(WAIT, manager.wait(&args, None))
            .await
            .unwrap();
        assert!(completed.success, "{}", completed.output);
        assert_actor_cleanup(root.path(), 1);
        probe(&mut probes, Probe::Closed).await;
        let mut terminal = manager.load_record(&id).unwrap();
        assert_eq!(terminal.status, SubagentStatus::Completed);
        assert_eq!(
            terminal.result.as_ref().unwrap().output,
            "Saved child answer"
        );
        assert!(terminal.snapshot_attempt.is_some());
        assert_eq!(manager.runtime.available_permits(), 1);
        assert!(manager.runtime.get(&id).is_none());
        assert!(!SubagentManager::execution_lease_is_held(&terminal));
        // A lifecycle retry uses the existing mailbox idempotency key.
        manager
            .publish_lifecycle_notification(&mut terminal)
            .unwrap();
        let events = manager.poll_lifecycle_events();
        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0].summary,
            Some(super::handoff::notification("Saved child answer"))
        );
        manager.acknowledge_lifecycle_event(&events[0]).unwrap();
        manager
            .publish_lifecycle_notification(&mut terminal)
            .unwrap();
        assert!(manager.poll_lifecycle_events().is_empty());
        assert_eq!(factory.calls.load(Ordering::SeqCst), 1);
    });
}

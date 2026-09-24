use anyhow::{Context, Result, bail};
use serde::Deserialize;

use super::*;
use crate::a2a_cli::peer_message::{
    ComputerHandoffPackageReference, DEFAULT_PEER_MESSAGE_INTERVAL_MS,
    DEFAULT_PEER_MESSAGE_WAIT_MS, start_handoff, wait_for_peer_message,
};
use crate::a2a_cli::{A2ATask, extract_task_text, is_action_required_state, is_failed_state};
use crate::commands::A2aComputerHandoffSelection;
use crate::tools::orb_delegation::OrbConsoleAction;

pub(super) enum A2aHandoffEvent {
    Accepted {
        peer: String,
        task_id: String,
        package_id: Option<String>,
        ledger_warning: Option<String>,
    },
    Finished {
        peer: String,
        task: Box<A2ATask>,
        ledger_warning: Option<String>,
    },
    Failed {
        peer: String,
        task_id: Option<String>,
        error: String,
    },
}

impl App {
    pub(super) fn handle_live_handoff(
        &mut self,
        peer: Option<String>,
        text: String,
        computer_package: Option<A2aComputerHandoffSelection>,
    ) {
        let tx = self.a2a_handoff_tx.clone();
        let wake = self.loop_wake.clone();
        let tool_executor = Arc::clone(&self.tool_executor);
        let requested_peer = peer
            .clone()
            .unwrap_or_else(|| self.state.locale.translate("default peer").into());
        self.state.status = Some(self.state.locale.format(
            "Handing work to {0} ...",
            std::slice::from_ref(&(requested_peer)),
        ));
        tokio::spawn(async move {
            let emit = |event| {
                let _ = tx.send(event);
                wake.signal();
            };
            let package = match computer_package {
                Some(selection) => match create_computer_package(tool_executor, selection).await {
                    Ok(package) => Some(package),
                    Err(error) => {
                        emit(A2aHandoffEvent::Failed {
                            peer: requested_peer,
                            task_id: None,
                            error: format!("could not create Computer package: {error:#}"),
                        });
                        return;
                    }
                },
                None => None,
            };

            let pending = match start_handoff(peer.clone(), text, package.as_ref()).await {
                Ok(pending) => pending,
                Err(error) => {
                    emit(A2aHandoffEvent::Failed {
                        peer: requested_peer,
                        task_id: None,
                        error: format!("could not send A2A handoff: {error:#}"),
                    });
                    return;
                }
            };
            let peer = pending.peer.clone();
            let task_id = pending.task.id.clone();
            emit(A2aHandoffEvent::Accepted {
                peer: peer.clone(),
                task_id: task_id.clone(),
                package_id: package.as_ref().map(|package| package.package_id.clone()),
                ledger_warning: pending.ledger_warning.clone(),
            });

            match wait_for_peer_message(
                &pending,
                DEFAULT_PEER_MESSAGE_WAIT_MS,
                DEFAULT_PEER_MESSAGE_INTERVAL_MS,
            )
            .await
            {
                Ok(completed) => {
                    emit(A2aHandoffEvent::Finished {
                        peer,
                        task: Box::new(completed.task),
                        ledger_warning: completed.ledger_warning,
                    });
                }
                Err(error) => {
                    emit(A2aHandoffEvent::Failed {
                        peer,
                        task_id: Some(task_id),
                        error: format!("could not follow A2A handoff: {error:#}"),
                    });
                }
            }
        });
    }

    /// Apply handoff progress without blocking input or agent streaming.
    pub(super) fn poll_a2a_handoffs(&mut self) -> bool {
        let mut applied = false;
        while let Ok(event) = self.a2a_handoff_rx.try_recv() {
            applied = true;
            match event {
                A2aHandoffEvent::Accepted {
                    peer,
                    task_id,
                    package_id,
                    ledger_warning,
                } => {
                    self.state.status = Some(self.state.locale.format(
                        "Handoff to {0} accepted as {1}",
                        &[(peer).clone(), (task_id).clone()],
                    ));
                    let package = package_id
                        .map(|package_id| {
                            self.state.locale.format(
                                " with Computer package `{0}`",
                                std::slice::from_ref(&(package_id)),
                            )
                        })
                        .unwrap_or_default();
                    self.state.add_system_message(self.state.locale.format("Handoff sent to `{0}` as task `{1}`{2}. Deixic Code is following it in the background.", &[(peer).clone(), (task_id).clone(), (package).clone()]));
                    self.report_a2a_ledger_warning(ledger_warning);
                }
                A2aHandoffEvent::Finished {
                    peer,
                    task,
                    ledger_warning,
                } => {
                    let task_id = task.id.clone();
                    let state = task.status.state.clone();
                    let response = extract_task_text(&task);
                    if is_failed_state(&state) {
                        self.state.error = Some(
                            self.state.locale.format(
                                "Handoff to {0} failed ({1}, state {2}){3}",
                                &[
                                    (peer).clone(),
                                    (task_id).clone(),
                                    (state).clone(),
                                    (response
                                        .as_deref()
                                        .map(|text| format!(": {text}"))
                                        .unwrap_or_default())
                                    .clone(),
                                ],
                            ),
                        );
                    } else if is_action_required_state(&state) {
                        self.state.add_system_message(
                            self.state.locale.format(
                                "Handoff `{0}` from `{1}` needs input{2}",
                                &[
                                    (task_id).clone(),
                                    (peer).clone(),
                                    (response
                                        .as_deref()
                                        .map(|text| format!(":\n\n{text}"))
                                        .unwrap_or_default())
                                    .clone(),
                                ],
                            ),
                        );
                    } else {
                        self.state.status =
                            Some(self.state.locale.format(
                                "Handoff from {0} completed",
                                std::slice::from_ref(&(peer)),
                            ));
                        self.state.add_system_message(
                            self.state.locale.format(
                                "## Handoff response from {0}\n\n{1}\n\nTask: `{2}`",
                                &[
                                    (peer).clone(),
                                    (response
                                        .as_deref()
                                        .unwrap_or("Completed without a text response."))
                                    .to_string(),
                                    (task_id).clone(),
                                ],
                            ),
                        );
                    }
                    self.report_a2a_ledger_warning(ledger_warning);
                }
                A2aHandoffEvent::Failed {
                    peer,
                    task_id,
                    error,
                } => {
                    self.state.error = Some(match task_id {
                        Some(task_id) => self.state.locale.format(
                            "Handoff to {0} task {1} stopped: {2}",
                            &[(peer).clone(), (task_id).clone(), (error).clone()],
                        ),
                        None => self.state.locale.format(
                            "Handoff to {0} failed: {1}",
                            &[(peer).clone(), (error).clone()],
                        ),
                    });
                }
            }
        }
        applied
    }

    fn report_a2a_ledger_warning(&mut self, warning: Option<String>) {
        if let Some(warning) = warning {
            self.state.add_system_message(self.state.locale.format(
                "A2A task ledger warning: {0}",
                std::slice::from_ref(&(warning)),
            ));
        }
    }
}

#[derive(Deserialize)]
struct ComputerHandoffManifest {
    package_id: String,
    package_digest: String,
    target_thread_id: String,
}

async fn create_computer_package(
    tool_executor: Arc<ToolExecutor>,
    selection: A2aComputerHandoffSelection,
) -> Result<ComputerHandoffPackageReference> {
    let expected_target = selection.target_thread_id.clone();
    let result = tool_executor
        .run_orb_console(OrbConsoleAction::HandoffCreate {
            source_id: selection.source_task_id,
            target_thread_id: selection.target_thread_id,
            files: selection.files,
            artifact_ids: selection.artifact_ids,
            include_diff: selection.include_diff,
        })
        .await;
    if !result.success {
        bail!(
            "{}",
            result
                .error
                .unwrap_or_else(|| "hosted Computer rejected the package".into())
        );
    }
    extract_computer_package_reference(result.details.as_ref(), &expected_target)
}

fn extract_computer_package_reference(
    details: Option<&serde_json::Value>,
    expected_target: &str,
) -> Result<ComputerHandoffPackageReference> {
    let manifest = details
        .and_then(|details| details.pointer("/handoffPackage/manifest"))
        .cloned()
        .context("Computer response omitted the handoff package manifest")?;
    let manifest: ComputerHandoffManifest = serde_json::from_value(manifest)
        .context("Computer returned an invalid package manifest")?;
    if manifest.package_id.len() != 64
        || !manifest
            .package_id
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
        || manifest.package_digest != format!("sha256:{}", manifest.package_id)
    {
        bail!("Computer returned an invalid package identity or digest");
    }
    if manifest.target_thread_id != expected_target {
        bail!(
            "Computer addressed the package to {}, expected {}",
            manifest.target_thread_id,
            expected_target
        );
    }
    Ok(ComputerHandoffPackageReference {
        package_id: manifest.package_id,
        package_digest: manifest.package_digest,
        target_thread_id: manifest.target_thread_id,
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn extracts_validated_computer_package_reference() {
        let package_id = "a".repeat(64);
        let package_digest = format!("sha256:{package_id}");
        let details = json!({
            "handoffPackage": {
                "manifest": {
                    "package_id": package_id,
                    "package_digest": package_digest,
                    "target_thread_id": "thread-2"
                }
            }
        });
        assert_eq!(
            extract_computer_package_reference(Some(&details), "thread-2").unwrap(),
            ComputerHandoffPackageReference {
                package_id,
                package_digest,
                target_thread_id: "thread-2".into(),
            }
        );
    }

    #[test]
    fn rejects_missing_or_misdirected_computer_package_reference() {
        assert!(extract_computer_package_reference(None, "thread-2").is_err());
        let package_id = "a".repeat(64);
        let package_digest = format!("sha256:{package_id}");
        let details = json!({
            "handoffPackage": {
                "manifest": {
                    "package_id": package_id,
                    "package_digest": package_digest,
                    "target_thread_id": "thread-3"
                }
            }
        });
        assert!(extract_computer_package_reference(Some(&details), "thread-2").is_err());

        let bad_digest = json!({
            "handoffPackage": {
                "manifest": {
                    "package_id": package_id,
                    "package_digest": "sha256:wrong",
                    "target_thread_id": "thread-2"
                }
            }
        });
        assert!(extract_computer_package_reference(Some(&bad_digest), "thread-2").is_err());
    }
}

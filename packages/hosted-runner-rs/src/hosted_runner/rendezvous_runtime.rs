//! Runtime-side authority and latency bookkeeping for outbound rendezvous.

use std::sync::Arc;
use std::time::{Duration, Instant};

use thiserror::Error;
use tokio_util::sync::CancellationToken;

use super::config::{HostedRunnerRendezvousConfig, HostedRunnerWorkloadIdentityConfig};
use super::rendezvous_carrier::{
    RendezvousCarrier, RendezvousCarrierConfig, RendezvousCarrierError, TracingRendezvousMetrics,
};
use super::rendezvous_protocol::{
    HostToRunnerFrame, RendezvousAck, RendezvousCommandError, RendezvousCommandOutcome,
    RendezvousExecution, RendezvousIdentity, RendezvousLatencyError, RendezvousLatencyMilestones,
    RendezvousLatencySample, RendezvousLifecycle, RendezvousLifecycleError, RendezvousMode,
    RendezvousRequest, RendezvousRequestDisposition, RunnerToHostFrame,
};
use super::workload_identity::ReloadableClientIdentity;
use super::{
    ConnectionRole, HostedRunnerHeadlessMessageContext, HostedRunnerHeadlessMessageExecution,
    HostedRunnerHeadlessMessageResult, SharedRunner, ToAgentMessage,
};

pub const FIRST_COMMAND_LATENCY_METRIC: &str =
    "maestro_rendezvous_activation_to_first_command_seconds";
pub const FIRST_FRAME_LATENCY_METRIC: &str =
    "maestro_rendezvous_first_command_to_first_frame_seconds";
pub const ACTIVATION_TO_FIRST_FRAME_METRIC: &str =
    "maestro_rendezvous_activation_to_first_frame_seconds";

/// A shadow activation may advertise eligibility, but never command authority.
#[must_use]
pub const fn candidate_eligible_at(mode: RendezvousMode, accepted_at: Instant) -> Option<Instant> {
    match mode {
        RendezvousMode::OutboundShadow => Some(accepted_at),
        RendezvousMode::Inbound | RendezvousMode::Outbound => None,
    }
}

/// Applies one successfully executed logical request to the replay ledger and
/// records the first command/frame latency pair. Callers must invoke this only
/// after command execution succeeds.
pub fn process_request(
    lifecycle: &mut RendezvousLifecycle,
    request: &RendezvousRequest,
    activation_at: Instant,
    first_command_at: Instant,
    first_frame_at: Instant,
) -> Result<(RendezvousAck, RendezvousLatencySample), RendezvousRuntimeError> {
    if lifecycle.active_activation_id() != Some(request.activation_id) {
        return Err(RendezvousLifecycleError::ActiveActivationMismatch {
            expected: request.activation_id,
            active: lifecycle.active_activation_id(),
        }
        .into());
    }
    lifecycle.record_applied_request(request.sequence, request.idempotency_key.clone())?;
    let mut milestones = RendezvousLatencyMilestones::started(activation_at);
    milestones.record_first_command(first_command_at)?;
    let sample = milestones.record_first_frame(first_frame_at)?;
    Ok((
        RendezvousAck {
            activation_id: request.activation_id,
            sequence: request.sequence,
            idempotency_key: request.idempotency_key.clone(),
            result: None,
            error: None,
        },
        sample,
    ))
}

#[derive(Debug, Error)]
pub enum RendezvousRuntimeError {
    #[error(transparent)]
    Lifecycle(#[from] RendezvousLifecycleError),
    #[error(transparent)]
    Latency(#[from] RendezvousLatencyError),
    #[error(transparent)]
    Carrier(#[from] RendezvousCarrierError),
    #[error("rendezvous command payload is invalid")]
    InvalidCommand,
    #[error("rendezvous command execution failed: {0}")]
    Execution(String),
}

impl RendezvousRuntimeError {
    fn wire_error(&self) -> RendezvousCommandError {
        let (code, retryable, message) = match self {
            Self::InvalidCommand => (
                "invalid_command",
                false,
                "rendezvous command payload is invalid",
            ),
            Self::Execution(_) => (
                "execution_failed",
                false,
                "rendezvous command execution failed",
            ),
            Self::Lifecycle(_) => ("lifecycle_rejected", false, "rendezvous lifecycle rejected"),
            Self::Latency(_) => ("latency_invalid", false, "rendezvous latency was invalid"),
            Self::Carrier(_) => ("transport_error", true, "rendezvous transport failed"),
        };
        RendezvousCommandError {
            code: code.to_string(),
            message: message.to_string(),
            retryable,
        }
    }
}

fn command_outcome(result: &HostedRunnerHeadlessMessageResult) -> RendezvousCommandOutcome {
    RendezvousCommandOutcome {
        execution: match result.execution {
            HostedRunnerHeadlessMessageExecution::TransportOnly => {
                RendezvousExecution::TransportOnly
            }
            HostedRunnerHeadlessMessageExecution::RuntimeHandled => {
                RendezvousExecution::RuntimeHandled
            }
        },
        message: result.message.clone(),
        idempotency_finalized: result.idempotency_finalized,
    }
}

/// Runs the identity-rotating outbound carrier. The lifecycle survives
/// reconnects, preserving replay/idempotency state until Platform rotates the
/// bootstrap activation identity and restarts the resident.
pub(super) async fn run(
    config: HostedRunnerRendezvousConfig,
    workload: HostedRunnerWorkloadIdentityConfig,
    client_identity: ReloadableClientIdentity,
    shared: SharedRunner,
    shutdown: CancellationToken,
) {
    let mut lifecycle = RendezvousLifecycle::new(
        config.mode,
        RendezvousIdentity {
            organization_id: workload.organization_id,
            workspace_id: workload.workspace_id,
            sandbox_id: workload.sandbox_id,
            placement_generation: workload.placement_generation,
            runner_session_id: shared.config.runner_session_id.clone(),
        },
        config.nonce.clone(),
    );
    lifecycle.set_runtime_ready(true);
    let mut backoff = Duration::from_millis(250);
    loop {
        // Outbound authority is a runtime grant, never a configuration fact.
        // The grant is set only after the carrier receives the Platform accepted frame.
        // It is revoked before every reconnect or shutdown path.
        shared.set_rendezvous_outbound_authority(false);
        let Some((tls_config, identity_cancellation, identity_expires_at)) =
            client_identity.snapshot(chrono::Utc::now()).await
        else {
            tokio::select! {
                () = shutdown.cancelled() => {
                    shared.set_rendezvous_outbound_authority(false);
                    return;
                },
                () = tokio::time::sleep(backoff) => {}
            }
            backoff = (backoff * 2).min(Duration::from_secs(5));
            continue;
        };
        let endpoint = match tokio::net::lookup_host(config.endpoint.as_str()).await {
            Ok(mut endpoints) => match endpoints.next() {
                Some(endpoint) => endpoint,
                None => {
                    tokio::time::sleep(backoff).await;
                    continue;
                }
            },
            Err(_) => {
                tokio::time::sleep(backoff).await;
                continue;
            }
        };
        let server_name = match rustls::pki_types::ServerName::try_from(config.server_name.clone())
        {
            Ok(server_name) => server_name,
            Err(_) => return,
        };
        let carrier = RendezvousCarrier::new(
            RendezvousCarrierConfig {
                endpoint,
                server_name,
                tls_config,
                identity_cancellation: identity_cancellation.clone(),
                phase_timeout: Duration::from_secs(10),
            },
            Arc::new(TracingRendezvousMetrics),
        );
        let activation_at = Instant::now();
        if let Ok(mut connection) = carrier.connect(&mut lifecycle, config.activation_id).await {
            backoff = Duration::from_millis(250);
            let outbound_authority = config.mode == RendezvousMode::Outbound
                && lifecycle.command_authority()
                    == Some(super::rendezvous_protocol::CommandAuthority::Outbound);
            shared.set_rendezvous_outbound_authority(outbound_authority);
            if let Some(_accepted_at) = candidate_eligible_at(config.mode, Instant::now()) {
                tracing::info!(
                    target: "maestro.hosted",
                    event = "candidate_eligible_at",
                    activation_id = %config.activation_id,
                    mode = "outbound_shadow",
                    candidate_eligible_at = %chrono::Utc::now().to_rfc3339(),
                    "Outbound shadow candidate is eligible; inbound remains authoritative"
                );
                let observed = tokio::select! {
                    () = shutdown.cancelled() => {
                        shared.set_rendezvous_outbound_authority(false);
                        return;
                    },
                    frame = connection.recv() => frame,
                };
                if matches!(observed, Ok(HostToRunnerFrame::Request(_))) {
                    tracing::warn!(
                        target: "maestro.hosted",
                        event = "shadow_command_rejected",
                        activation_id = %config.activation_id,
                        "Outbound shadow sent a command while inbound is authoritative"
                    );
                }
            } else {
                loop {
                    let frame = tokio::select! {
                        () = shutdown.cancelled() => {
                            shared.set_rendezvous_outbound_authority(false);
                            return;
                        },
                        frame = connection.recv() => frame,
                    };
                    let HostToRunnerFrame::Request(request) = (match frame {
                        Ok(frame) => frame,
                        Err(_) => break,
                    }) else {
                        break;
                    };
                    let disposition = match lifecycle
                        .request_disposition(request.sequence, &request.idempotency_key)
                    {
                        Ok(disposition) => disposition,
                        Err(_) => break,
                    };
                    let command_at = Instant::now();
                    let command_outcome = if disposition == RendezvousRequestDisposition::Execute {
                        let outcome =
                            match execute_request(&shared, &request, identity_expires_at).await {
                                Ok(outcome) => outcome,
                                Err(error) => {
                                    let failure = RunnerToHostFrame::Ack(RendezvousAck {
                                        activation_id: request.activation_id,
                                        sequence: request.sequence,
                                        idempotency_key: request.idempotency_key.clone(),
                                        result: None,
                                        error: Some(error.wire_error()),
                                    });
                                    let _ = connection.send(&failure).await;
                                    // The failed sequence was not recorded as applied. Close so
                                    // the host cannot advance past a failed contiguous sequence.
                                    break;
                                }
                            };
                        Some(outcome)
                    } else {
                        None
                    };
                    let frame_at = Instant::now();
                    let (mut ack, sample) = match process_request(
                        &mut lifecycle,
                        &request,
                        activation_at,
                        command_at,
                        frame_at,
                    ) {
                        Ok(result) => result,
                        Err(_) => break,
                    };
                    ack.result = command_outcome;
                    record_latency(sample);
                    if connection.send(&RunnerToHostFrame::Ack(ack)).await.is_err() {
                        break;
                    }
                }
            }
        }
        // A closed or failed carrier must immediately return command
        // authority to the inbound path while the next connection is fenced.
        shared.set_rendezvous_outbound_authority(false);
        tokio::select! {
            () = shutdown.cancelled() => {
                shared.set_rendezvous_outbound_authority(false);
                return;
            },
            () = tokio::time::sleep(backoff) => {}
        }
        backoff = (backoff * 2).min(Duration::from_secs(5));
    }
}

async fn execute_request(
    shared: &SharedRunner,
    request: &RendezvousRequest,
    identity_expires_at: chrono::DateTime<chrono::Utc>,
) -> Result<RendezvousCommandOutcome, RendezvousRuntimeError> {
    let message: ToAgentMessage = serde_json::from_value(request.payload.clone())
        .map_err(|_| RendezvousRuntimeError::InvalidCommand)?;
    let context = HostedRunnerHeadlessMessageContext {
        session_id: shared.config.runner_session_id.clone(),
        connection_id: format!("rendezvous:{}", request.activation_id),
        subscription_id: None,
        role: ConnectionRole::Controller,
        controller_connection_id: None,
        client_protocol_version: Some(super::HEADLESS_PROTOCOL_VERSION.to_string()),
        client_info: None,
        capabilities: None,
        opt_out_notifications: None,
        lease_expires_at: identity_expires_at.to_rfc3339(),
        workspace_root: shared.config.workspace_root.clone(),
        response_idempotency_key: Some(request.idempotency_key.clone()),
    };
    let _mutation = shared.mutation_lifecycle.lock().await;
    let result = shared
        .message_executor
        .execute_async(&context, message)
        .await
        .map_err(|error| RendezvousRuntimeError::Execution(error.to_string()))?;
    let mut state = shared
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let outcome = command_outcome(&result);
    for message in result.messages {
        shared.publish_message(&mut state, message);
    }
    Ok(outcome)
}

fn record_latency(sample: RendezvousLatencySample) {
    for (metric, duration) in [
        (
            FIRST_COMMAND_LATENCY_METRIC,
            sample.activation_to_first_command,
        ),
        (
            FIRST_FRAME_LATENCY_METRIC,
            sample.first_command_to_first_frame,
        ),
        (
            ACTIVATION_TO_FIRST_FRAME_METRIC,
            sample.activation_to_first_frame,
        ),
    ] {
        tracing::info!(
            target: "maestro.hosted",
            metric,
            value = duration.as_secs_f64(),
            "Hosted runner rendezvous latency"
        );
    }
}

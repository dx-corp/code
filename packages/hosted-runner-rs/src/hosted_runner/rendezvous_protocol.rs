//! Transport-neutral frames and activation lifecycle for hosted-runner rendezvous.
//!
//! A carrier (mTLS stream, in-memory test transport, or a future alternative)
//! owns I/O. This module owns the versioned wire shape and the fail-closed
//! command-authority rules shared by every carrier.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use uuid::Uuid;

/// Opaque possession proof minted by Platform for one durable activation intent.
#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct RendezvousNonce(String);

impl RendezvousNonce {
    /// Nonces are bootstrap secrets: reject empty, oversized, or control-bearing values.
    pub fn parse(value: impl Into<String>) -> Result<Self, RendezvousLifecycleError> {
        let value = value.into();
        if value.is_empty()
            || value.len() > 256
            || !value.bytes().all(|byte| byte.is_ascii_graphic())
        {
            return Err(RendezvousLifecycleError::InvalidNonce);
        }
        Ok(Self(value))
    }
}

impl std::fmt::Debug for RendezvousNonce {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("RendezvousNonce(<redacted>)")
    }
}

/// Version negotiated by both ends of a hosted-runner rendezvous stream.
pub const RENDEZVOUS_PROTOCOL_VERSION: &str = "evalops.maestro.hosted-runner-rendezvous.v1";

/// Hard upper bound advertised for requests awaiting acknowledgement.
pub const MAX_IN_FLIGHT_REQUESTS: u16 = 128;

/// Bounded replay history retained by the transport-neutral lifecycle.
pub const MAX_REPLAY_RECORDS: usize = 4096;

/// Rollout modes, ordered operationally as inbound, shadow, then outbound.
#[derive(Debug, Clone, Copy, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RendezvousMode {
    /// The existing inbound listener is authoritative; no outbound stream opens.
    Inbound,
    /// The outbound stream opens for observation while inbound remains authoritative.
    OutboundShadow,
    /// Only the outbound stream is command-authoritative.
    Outbound,
}

impl RendezvousMode {
    /// Returns the one command-authoritative path selected by this mode.
    #[must_use]
    pub const fn command_authority(self) -> Option<CommandAuthority> {
        match self {
            Self::Inbound | Self::OutboundShadow => Some(CommandAuthority::Inbound),
            Self::Outbound => Some(CommandAuthority::Outbound),
        }
    }

    /// Whether requests received on the outbound stream may be admitted.
    #[must_use]
    pub const fn outbound_commands_enabled(self) -> bool {
        matches!(self, Self::Outbound)
    }
}

/// Mutually exclusive command paths.
#[derive(Debug, Clone, Copy, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandAuthority {
    /// Existing inbound hosted-runner HTTP path.
    Inbound,
    /// New outbound rendezvous stream.
    Outbound,
}

/// Workload identity bound to an outbound stream and placement generation.
#[derive(Debug, Clone, Deserialize, Eq, PartialEq, Serialize)]
pub struct RendezvousIdentity {
    pub organization_id: String,
    pub workspace_id: String,
    pub sandbox_id: Uuid,
    pub placement_generation: u64,
    pub runner_session_id: String,
}

impl RendezvousIdentity {
    fn validate(&self) -> Result<(), RendezvousLifecycleError> {
        if self.organization_id.trim().is_empty() {
            return Err(RendezvousLifecycleError::InvalidIdentity("organization_id"));
        }
        if self.workspace_id.trim().is_empty() {
            return Err(RendezvousLifecycleError::InvalidIdentity("workspace_id"));
        }
        if self.placement_generation == 0 {
            return Err(RendezvousLifecycleError::InvalidIdentity(
                "placement_generation",
            ));
        }
        if self.runner_session_id.trim().is_empty() {
            return Err(RendezvousLifecycleError::InvalidIdentity(
                "runner_session_id",
            ));
        }
        Ok(())
    }
}

/// First runner-to-host frame on each newly established carrier.
#[derive(Debug, Clone, Deserialize, Eq, PartialEq, Serialize)]
pub struct RendezvousOpen {
    pub protocol_version: String,
    pub activation_id: Uuid,
    pub rendezvous_nonce: RendezvousNonce,
    pub identity: RendezvousIdentity,
    pub mode: RendezvousMode,
    pub resume_after_sequence: Option<u64>,
    pub max_in_flight_requests: u16,
}

/// Host acceptance of the identity-bound activation.
#[derive(Debug, Clone, Deserialize, Eq, PartialEq, Serialize)]
pub struct RendezvousAccepted {
    pub activation_id: Uuid,
    pub outbound_commands_enabled: bool,
    pub replay_from_sequence: u64,
}

/// A logical request sent by the host on the accepted rendezvous stream.
#[derive(Debug, Clone, Deserialize, PartialEq, Serialize)]
pub struct RendezvousRequest {
    pub activation_id: Uuid,
    pub sequence: u64,
    pub idempotency_key: String,
    pub payload: Value,
}

/// Additive result carried by an acknowledgement when the hosted runner has
/// executed a command. Older hosts continue to accept an Ack without this
/// field, so wire rollout can be staged independently from command routing.
#[derive(Debug, Clone, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RendezvousExecution {
    TransportOnly,
    RuntimeHandled,
}

#[derive(Debug, Clone, Deserialize, Eq, PartialEq, Serialize)]
pub struct RendezvousCommandOutcome {
    pub execution: RendezvousExecution,
    pub message: String,
    pub idempotency_finalized: bool,
}

#[derive(Debug, Clone, Deserialize, Eq, PartialEq, Serialize)]
pub struct RendezvousCommandError {
    pub code: String,
    pub message: String,
    pub retryable: bool,
}

/// Acknowledgement for an applied or deduplicated logical request.
#[derive(Debug, Clone, Deserialize, Eq, PartialEq, Serialize)]
pub struct RendezvousAck {
    pub activation_id: Uuid,
    pub sequence: u64,
    pub idempotency_key: String,
    /// Optional for compatibility with the original Ack-only rendezvous
    /// contract. A response never carries both result and error.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<RendezvousCommandOutcome>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<RendezvousCommandError>,
}

impl RendezvousAck {
    #[must_use]
    pub fn has_valid_outcome(&self) -> bool {
        !(self.result.is_some() && self.error.is_some())
    }
}

/// Why an activation was closed and must no longer admit requests.
#[derive(Debug, Clone, Copy, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RevocationReason {
    RuntimeUnavailable,
    Rotated,
    Shutdown,
}

/// Terminal frame for a particular activation ID.
#[derive(Debug, Clone, Deserialize, Eq, PartialEq, Serialize)]
pub struct RendezvousClose {
    pub activation_id: Uuid,
    pub reason: RevocationReason,
}

/// Frames emitted by the hosted runner.
#[derive(Debug, Clone, Deserialize, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RunnerToHostFrame {
    Open(RendezvousOpen),
    Ack(RendezvousAck),
    Close(RendezvousClose),
}

/// Frames emitted by the stable runner host.
#[derive(Debug, Clone, Deserialize, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HostToRunnerFrame {
    Accepted(RendezvousAccepted),
    Request(RendezvousRequest),
    Close(RendezvousClose),
}

/// Atomic result of replacing one activation with another.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct RendezvousRotation {
    pub close: RendezvousClose,
    pub open: RendezvousOpen,
}

/// Whether a request needs execution or is an exact replay already applied.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum RendezvousRequestDisposition {
    Execute,
    Replay,
}

/// Paired latency deltas for one reverse-activation attempt.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct RendezvousLatencySample {
    pub activation_to_first_command: Duration,
    pub first_command_to_first_frame: Duration,
    pub activation_to_first_frame: Duration,
}

/// Invalid ordering of latency milestones.
#[derive(Debug, Clone, Copy, Error, Eq, PartialEq)]
pub enum RendezvousLatencyError {
    #[error("first command timestamp precedes activation")]
    CommandBeforeActivation,
    #[error("first frame was recorded before the first command")]
    FirstCommandMissing,
    #[error("first frame timestamp precedes first command")]
    FrameBeforeCommand,
}

/// Records the first command and first frame for exactly one activation.
#[derive(Debug, Clone)]
pub struct RendezvousLatencyMilestones {
    activation_started_at: Instant,
    first_command_at: Option<Instant>,
    first_frame_at: Option<Instant>,
}

impl RendezvousLatencyMilestones {
    /// Starts a paired latency measurement at outbound activation.
    #[must_use]
    pub const fn started(activation_started_at: Instant) -> Self {
        Self {
            activation_started_at,
            first_command_at: None,
            first_frame_at: None,
        }
    }

    /// Records the first command; later calls are idempotent.
    pub fn record_first_command(&mut self, at: Instant) -> Result<(), RendezvousLatencyError> {
        if self.first_command_at.is_some() {
            return Ok(());
        }
        if at
            .checked_duration_since(self.activation_started_at)
            .is_none()
        {
            return Err(RendezvousLatencyError::CommandBeforeActivation);
        }
        self.first_command_at = Some(at);
        Ok(())
    }

    /// Records the first frame and returns the stable paired sample.
    pub fn record_first_frame(
        &mut self,
        at: Instant,
    ) -> Result<RendezvousLatencySample, RendezvousLatencyError> {
        if let Some(sample) = self.sample() {
            return Ok(sample);
        }
        let first_command_at = self
            .first_command_at
            .ok_or(RendezvousLatencyError::FirstCommandMissing)?;
        if at.checked_duration_since(first_command_at).is_none() {
            return Err(RendezvousLatencyError::FrameBeforeCommand);
        }
        self.first_frame_at = Some(at);
        Ok(RendezvousLatencySample {
            activation_to_first_command: first_command_at
                .duration_since(self.activation_started_at),
            first_command_to_first_frame: at.duration_since(first_command_at),
            activation_to_first_frame: at.duration_since(self.activation_started_at),
        })
    }

    /// Returns the sample after both milestones have been recorded.
    #[must_use]
    pub fn sample(&self) -> Option<RendezvousLatencySample> {
        let first_command_at = self.first_command_at?;
        let first_frame_at = self.first_frame_at?;
        Some(RendezvousLatencySample {
            activation_to_first_command: first_command_at
                .duration_since(self.activation_started_at),
            first_command_to_first_frame: first_frame_at.duration_since(first_command_at),
            activation_to_first_frame: first_frame_at.duration_since(self.activation_started_at),
        })
    }
}

/// Rejected lifecycle transitions. All rejection paths leave authority unchanged.
#[derive(Debug, Clone, Error, Eq, PartialEq)]
pub enum RendezvousLifecycleError {
    #[error("runtime is not ready")]
    RuntimeNotReady,
    #[error("outbound rendezvous is disabled in inbound mode")]
    OutboundDisabled,
    #[error("rendezvous activation {active} is already active")]
    ActivationConflict { active: Uuid },
    #[error("rendezvous activation {expected} is not active; active activation is {active:?}")]
    ActiveActivationMismatch {
        expected: Uuid,
        active: Option<Uuid>,
    },
    #[error("no rendezvous activation is active")]
    NoActiveActivation,
    #[error("rendezvous activation has not been accepted")]
    ActivationNotAccepted,
    #[error("outbound requests are disabled in this rollout mode")]
    OutboundCommandsDisabled,
    #[error("host command-authority grant mismatch: expected {expected}, received {received}")]
    AcceptanceAuthorityMismatch { expected: bool, received: bool },
    #[error("host replay start mismatch: expected {expected}, received {received}")]
    ReplayStartMismatch { expected: u64, received: u64 },
    #[error("request sequence gap: expected {expected}, received {received}")]
    SequenceGap { expected: u64, received: u64 },
    #[error("request sequence {sequence} reused with different idempotency metadata")]
    SequenceConflict { sequence: u64 },
    #[error("idempotency key must not be empty")]
    EmptyIdempotencyKey,
    #[error("rendezvous nonce must be 1..=256 visible ASCII bytes")]
    InvalidNonce,
    #[error("rendezvous identity field {0} is invalid")]
    InvalidIdentity(&'static str),
}

/// Transport-neutral activation, replay, rotation, and readiness state.
#[derive(Debug, Clone)]
pub struct RendezvousLifecycle {
    mode: RendezvousMode,
    identity: RendezvousIdentity,
    rendezvous_nonce: RendezvousNonce,
    runtime_ready: bool,
    active_activation_id: Option<Uuid>,
    accepted: bool,
    last_applied_sequence: Option<u64>,
    replay_records: VecDeque<(u64, String)>,
}

impl RendezvousLifecycle {
    /// Creates a lifecycle with no ready runtime and no active stream.
    #[must_use]
    pub fn new(
        mode: RendezvousMode,
        identity: RendezvousIdentity,
        rendezvous_nonce: RendezvousNonce,
    ) -> Self {
        Self {
            mode,
            identity,
            rendezvous_nonce,
            runtime_ready: false,
            active_activation_id: None,
            accepted: false,
            last_applied_sequence: None,
            replay_records: VecDeque::new(),
        }
    }

    /// Changes runtime readiness, synchronously revoking an activation on loss.
    pub fn set_runtime_ready(&mut self, ready: bool) -> Option<RendezvousClose> {
        if self.runtime_ready == ready {
            return None;
        }
        self.runtime_ready = ready;
        if ready {
            return None;
        }
        self.accepted = false;
        self.active_activation_id
            .take()
            .map(|activation_id| RendezvousClose {
                activation_id,
                reason: RevocationReason::RuntimeUnavailable,
            })
    }

    /// Opens or idempotently re-opens an activation for the current mode.
    pub fn open(
        &mut self,
        activation_id: Uuid,
    ) -> Result<RendezvousOpen, RendezvousLifecycleError> {
        if !self.runtime_ready {
            return Err(RendezvousLifecycleError::RuntimeNotReady);
        }
        if self.mode == RendezvousMode::Inbound {
            return Err(RendezvousLifecycleError::OutboundDisabled);
        }
        self.identity.validate()?;
        match self.active_activation_id {
            Some(active) if active != activation_id => {
                return Err(RendezvousLifecycleError::ActivationConflict { active });
            }
            Some(_) => {
                self.accepted = false;
            }
            None => {
                self.active_activation_id = Some(activation_id);
                self.accepted = false;
            }
        }
        Ok(self.open_frame(activation_id))
    }

    /// Applies host acceptance only when its replay and authority grant match.
    pub fn accept(
        &mut self,
        accepted: &RendezvousAccepted,
    ) -> Result<(), RendezvousLifecycleError> {
        if self.active_activation_id != Some(accepted.activation_id) {
            return Err(RendezvousLifecycleError::ActiveActivationMismatch {
                expected: accepted.activation_id,
                active: self.active_activation_id,
            });
        }
        let commands_expected = self.mode.outbound_commands_enabled();
        if accepted.outbound_commands_enabled != commands_expected {
            return Err(RendezvousLifecycleError::AcceptanceAuthorityMismatch {
                expected: commands_expected,
                received: accepted.outbound_commands_enabled,
            });
        }
        let replay_expected = self.last_applied_sequence.map_or(1, |last| last + 1);
        if accepted.replay_from_sequence != replay_expected {
            return Err(RendezvousLifecycleError::ReplayStartMismatch {
                expected: replay_expected,
                received: accepted.replay_from_sequence,
            });
        }
        self.accepted = true;
        Ok(())
    }

    /// Atomically closes the current activation and opens its replacement.
    pub fn rotate(
        &mut self,
        current: Uuid,
        replacement: Uuid,
    ) -> Result<RendezvousRotation, RendezvousLifecycleError> {
        if !self.runtime_ready {
            return Err(RendezvousLifecycleError::RuntimeNotReady);
        }
        if self.active_activation_id != Some(current) {
            return Err(RendezvousLifecycleError::ActiveActivationMismatch {
                expected: current,
                active: self.active_activation_id,
            });
        }
        self.active_activation_id = Some(replacement);
        self.accepted = false;
        Ok(RendezvousRotation {
            close: RendezvousClose {
                activation_id: current,
                reason: RevocationReason::Rotated,
            },
            open: self.open_frame(replacement),
        })
    }

    /// Closes only the named activation, making stale close frames harmless.
    pub fn close(&mut self, activation_id: Uuid) -> bool {
        if self.active_activation_id != Some(activation_id) {
            return false;
        }
        self.active_activation_id = None;
        self.accepted = false;
        true
    }

    /// Records an applied request with contiguous, idempotent replay semantics.
    pub fn record_applied_request(
        &mut self,
        sequence: u64,
        idempotency_key: impl Into<String>,
    ) -> Result<(), RendezvousLifecycleError> {
        let idempotency_key = idempotency_key.into();
        match self.request_disposition(sequence, &idempotency_key)? {
            RendezvousRequestDisposition::Replay => return Ok(()),
            RendezvousRequestDisposition::Execute => {}
        }
        self.last_applied_sequence = Some(sequence);
        self.replay_records.push_back((sequence, idempotency_key));
        while self.replay_records.len() > MAX_REPLAY_RECORDS {
            self.replay_records.pop_front();
        }
        Ok(())
    }

    /// Classifies replay before executing a command so reconnects cannot
    /// duplicate a request that was already durably applied by this runtime.
    pub fn request_disposition(
        &self,
        sequence: u64,
        idempotency_key: &str,
    ) -> Result<RendezvousRequestDisposition, RendezvousLifecycleError> {
        if self.active_activation_id.is_none() {
            return Err(RendezvousLifecycleError::NoActiveActivation);
        }
        if !self.mode.outbound_commands_enabled() {
            return Err(RendezvousLifecycleError::OutboundCommandsDisabled);
        }
        if !self.accepted {
            return Err(RendezvousLifecycleError::ActivationNotAccepted);
        }
        if idempotency_key.trim().is_empty() {
            return Err(RendezvousLifecycleError::EmptyIdempotencyKey);
        }
        let expected = self.last_applied_sequence.map_or(1, |last| last + 1);
        if sequence < expected {
            return match self
                .replay_records
                .iter()
                .find(|(recorded_sequence, _)| *recorded_sequence == sequence)
            {
                Some((_, recorded_key)) if recorded_key == idempotency_key => {
                    Ok(RendezvousRequestDisposition::Replay)
                }
                _ => Err(RendezvousLifecycleError::SequenceConflict { sequence }),
            };
        }
        if sequence > expected {
            return Err(RendezvousLifecycleError::SequenceGap {
                expected,
                received: sequence,
            });
        }
        Ok(RendezvousRequestDisposition::Execute)
    }

    /// Returns the currently authoritative path, or none while unavailable.
    #[must_use]
    pub const fn command_authority(&self) -> Option<CommandAuthority> {
        if !self.runtime_ready {
            return None;
        }
        match self.mode {
            RendezvousMode::Inbound | RendezvousMode::OutboundShadow => {
                Some(CommandAuthority::Inbound)
            }
            RendezvousMode::Outbound if self.active_activation_id.is_some() && self.accepted => {
                Some(CommandAuthority::Outbound)
            }
            RendezvousMode::Outbound => None,
        }
    }

    /// Returns the activation currently allowed to exchange frames.
    #[must_use]
    pub const fn active_activation_id(&self) -> Option<Uuid> {
        self.active_activation_id
    }

    fn open_frame(&self, activation_id: Uuid) -> RendezvousOpen {
        RendezvousOpen {
            protocol_version: RENDEZVOUS_PROTOCOL_VERSION.to_string(),
            activation_id,
            rendezvous_nonce: self.rendezvous_nonce.clone(),
            identity: self.identity.clone(),
            mode: self.mode,
            resume_after_sequence: self.last_applied_sequence,
            max_in_flight_requests: MAX_IN_FLIGHT_REQUESTS,
        }
    }
}

#[cfg(test)]
mod tests;

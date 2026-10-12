//! Outbound mTLS carrier for the hosted-runner rendezvous protocol.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rustls::pki_types::ServerName;
use thiserror::Error;
use tokio::io::{ReadHalf, WriteHalf};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_rustls::{TlsConnector, client::TlsStream};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::headless::{AsyncFrameReader, AsyncFrameWriter, FramingError};

use super::rendezvous_protocol::{
    HostToRunnerFrame, RendezvousLifecycle, RendezvousLifecycleError, RunnerToHostFrame,
};

/// Histogram emitted once for each outbound connection phase.
pub const CONNECT_DURATION_METRIC: &str = "maestro_rendezvous_connect_duration_seconds";

/// Open-to-Accepted duration, including runner-host validation and durable CAS.
pub const ACTIVATION_CAS_DURATION_METRIC: &str =
    "maestro_rendezvous_activation_cas_duration_seconds";

/// Counter emitted for reconnect attempts and terminal outcomes.
pub const RECONNECT_COUNTER_METRIC: &str = "maestro_rendezvous_reconnect_total";

/// Counter emitted for bounded queue admission and removal outcomes.
pub const QUEUE_COUNTER_METRIC: &str = "maestro_rendezvous_queue_total";

/// Stable instrumentation boundary for production tracing and test recorders.
pub trait RendezvousMetricSink: Send + Sync {
    fn observe_connect_duration(
        &self,
        phase: &'static str,
        outcome: &'static str,
        duration: Duration,
    );

    fn observe_activation_cas_duration(&self, outcome: &'static str, duration: Duration);

    fn increment_counter(&self, metric: &'static str, outcome: &'static str);
}

/// Emits metric-shaped tracing events for the configured telemetry exporter.
#[derive(Debug, Default)]
pub struct TracingRendezvousMetrics;

impl RendezvousMetricSink for TracingRendezvousMetrics {
    fn observe_connect_duration(
        &self,
        phase: &'static str,
        outcome: &'static str,
        duration: Duration,
    ) {
        tracing::info!(
            metric = CONNECT_DURATION_METRIC,
            phase,
            outcome,
            value = duration.as_secs_f64(),
            "hosted runner rendezvous connect phase"
        );
    }

    fn observe_activation_cas_duration(&self, outcome: &'static str, duration: Duration) {
        tracing::info!(
            metric = ACTIVATION_CAS_DURATION_METRIC,
            outcome,
            value = duration.as_secs_f64(),
            "hosted runner rendezvous activation CAS"
        );
    }

    fn increment_counter(&self, metric: &'static str, outcome: &'static str) {
        tracing::info!(
            metric,
            outcome,
            value = 1_u64,
            "hosted runner rendezvous counter"
        );
    }
}

/// Network and identity material for one stable runner-host rendezvous endpoint.
pub struct RendezvousCarrierConfig {
    /// Stable runner-host address resolved by the caller.
    pub endpoint: SocketAddr,
    /// DNS identity required from the runner-host server certificate.
    pub server_name: ServerName<'static>,
    /// Strict client configuration built from a dedicated short-lived
    /// ClientAuth-only leaf with the authoritative Maestro tuple URI SAN.
    pub tls_config: Arc<rustls::ClientConfig>,
    /// Revoked when that short-lived identity expires or rotates.
    pub identity_cancellation: CancellationToken,
    /// Independent upper bound for TCP, TLS, open, and accepted phases.
    pub phase_timeout: Duration,
}

/// Failure to establish or use an outbound rendezvous stream.
#[derive(Debug, Error)]
pub enum RendezvousCarrierError {
    #[error("rendezvous lifecycle rejected the transition: {0}")]
    Lifecycle(#[from] RendezvousLifecycleError),
    #[error("rendezvous identity was revoked")]
    IdentityRevoked,
    #[error("rendezvous activation was revoked")]
    ActivationRevoked,
    #[error("rendezvous {phase} phase timed out")]
    Timeout { phase: &'static str },
    #[error("rendezvous {phase} I/O failed: {source}")]
    Io {
        phase: &'static str,
        #[source]
        source: std::io::Error,
    },
    #[error("rendezvous framing failed: {0}")]
    Framing(#[from] FramingError),
    #[error("runner host did not send accepted as its first frame")]
    UnexpectedFirstFrame,
}

/// Establishes identity-bound outbound streams without selecting rollout authority.
pub struct RendezvousCarrier {
    config: RendezvousCarrierConfig,
    metrics: Arc<dyn RendezvousMetricSink>,
    active_connection: Mutex<Option<CancellationToken>>,
}

impl RendezvousCarrier {
    #[must_use]
    pub fn new(config: RendezvousCarrierConfig, metrics: Arc<dyn RendezvousMetricSink>) -> Self {
        Self {
            config,
            metrics,
            active_connection: Mutex::new(None),
        }
    }

    /// Opens TCP and mTLS, sends the protocol open frame, then crosses the accept barrier.
    pub async fn connect(
        &self,
        lifecycle: &mut RendezvousLifecycle,
        activation_id: Uuid,
    ) -> Result<RendezvousConnection, RendezvousCarrierError> {
        let previous = lifecycle.active_activation_id();
        let reconnecting = previous.is_some();
        if reconnecting {
            self.metrics
                .increment_counter(RECONNECT_COUNTER_METRIC, "attempt");
        }
        let open = match previous {
            Some(current) if current != activation_id => {
                match lifecycle.rotate(current, activation_id) {
                    Ok(rotation) => rotation.open,
                    Err(error) => {
                        self.metrics
                            .increment_counter(RECONNECT_COUNTER_METRIC, "error");
                        return Err(error.into());
                    }
                }
            }
            _ => match lifecycle.open(activation_id) {
                Ok(open) => open,
                Err(error) => {
                    if reconnecting {
                        self.metrics
                            .increment_counter(RECONNECT_COUNTER_METRIC, "error");
                    }
                    return Err(error.into());
                }
            },
        };
        let activation_cancellation = CancellationToken::new();
        if let Some(previous) = self
            .active_connection
            .lock()
            .expect("rendezvous carrier mutex poisoned")
            .replace(activation_cancellation.clone())
        {
            previous.cancel();
        }
        let result = self
            .connect_inner(lifecycle, open, activation_cancellation.clone())
            .await;
        if reconnecting {
            self.metrics.increment_counter(
                RECONNECT_COUNTER_METRIC,
                if result.is_ok() { "success" } else { "error" },
            );
        }
        if result.is_err() {
            lifecycle.close(activation_id);
            activation_cancellation.cancel();
        }
        result
    }

    /// Immediately makes the active carrier unusable after readiness loss or shutdown.
    pub fn revoke_active(&self) {
        if let Some(active) = self
            .active_connection
            .lock()
            .expect("rendezvous carrier mutex poisoned")
            .take()
        {
            active.cancel();
        }
    }

    async fn connect_inner(
        &self,
        lifecycle: &mut RendezvousLifecycle,
        open: super::rendezvous_protocol::RendezvousOpen,
        activation_cancellation: CancellationToken,
    ) -> Result<RendezvousConnection, RendezvousCarrierError> {
        let tcp_started = Instant::now();
        let tcp = tokio::select! {
            () = self.config.identity_cancellation.cancelled() => {
                self.record_phase("tcp", "revoked", tcp_started);
                return Err(RendezvousCarrierError::IdentityRevoked);
            }
            result = tokio::time::timeout(
                self.config.phase_timeout,
                TcpStream::connect(self.config.endpoint),
            ) => match result {
                Ok(Ok(stream)) => stream,
                Ok(Err(source)) => {
                    self.record_phase("tcp", "error", tcp_started);
                    return Err(RendezvousCarrierError::Io { phase: "tcp", source });
                }
                Err(_) => {
                    self.record_phase("tcp", "timeout", tcp_started);
                    return Err(RendezvousCarrierError::Timeout { phase: "tcp" });
                }
            }
        };
        self.record_phase("tcp", "success", tcp_started);

        let tls_started = Instant::now();
        let connector = TlsConnector::from(self.config.tls_config.clone());
        let tls = tokio::select! {
            () = self.config.identity_cancellation.cancelled() => {
                self.record_phase("tls", "revoked", tls_started);
                return Err(RendezvousCarrierError::IdentityRevoked);
            }
            result = tokio::time::timeout(
                self.config.phase_timeout,
                connector.connect(self.config.server_name.clone(), tcp),
            ) => match result {
                Ok(Ok(stream)) => stream,
                Ok(Err(source)) => {
                    self.record_phase("tls", "error", tls_started);
                    return Err(RendezvousCarrierError::Io { phase: "tls", source });
                }
                Err(_) => {
                    self.record_phase("tls", "timeout", tls_started);
                    return Err(RendezvousCarrierError::Timeout { phase: "tls" });
                }
            }
        };
        self.record_phase("tls", "success", tls_started);

        let (read, write) = tokio::io::split(tls);
        let mut reader = AsyncFrameReader::new(read);
        let mut writer = AsyncFrameWriter::new(write);
        let activation_started = Instant::now();
        let open_started = activation_started;
        if let Err(error) = self
            .phase_timeout("open", writer.write_message(&RunnerToHostFrame::Open(open)))
            .await
        {
            self.record_phase("open", error.outcome(), open_started);
            self.metrics
                .observe_activation_cas_duration(error.outcome(), activation_started.elapsed());
            return Err(error);
        }
        self.record_phase("open", "success", open_started);

        let accepted_started = Instant::now();
        let mut frame: serde_json::Value = match self
            .phase_timeout("accepted", reader.read_message())
            .await
        {
            Ok(frame) => frame,
            Err(error) => {
                self.record_phase("accepted", error.outcome(), accepted_started);
                self.metrics
                    .observe_activation_cas_duration(error.outcome(), activation_started.elapsed());
                return Err(error);
            }
        };
        // N-1 shadow hosts did not send an authority bit. Treat omission as
        // false: shadow remains inbound-authoritative and outbound fails closed.
        if frame.get("type").and_then(serde_json::Value::as_str) == Some("accepted")
            && frame.get("outbound_commands_enabled").is_none()
        {
            frame["outbound_commands_enabled"] = serde_json::Value::Bool(false);
        }
        let frame: HostToRunnerFrame = match serde_json::from_value(frame) {
            Ok(frame) => frame,
            Err(error) => {
                self.record_phase("accepted", "error", accepted_started);
                self.metrics
                    .observe_activation_cas_duration("error", activation_started.elapsed());
                return Err(RendezvousCarrierError::Framing(FramingError::Json(error)));
            }
        };
        let HostToRunnerFrame::Accepted(accepted) = frame else {
            self.record_phase("accepted", "error", accepted_started);
            self.metrics
                .observe_activation_cas_duration("error", activation_started.elapsed());
            return Err(RendezvousCarrierError::UnexpectedFirstFrame);
        };
        if let Err(error) = lifecycle.accept(&accepted) {
            self.record_phase("accepted", "error", accepted_started);
            self.metrics
                .observe_activation_cas_duration("error", activation_started.elapsed());
            return Err(error.into());
        }
        self.record_phase("accepted", "success", accepted_started);
        self.metrics
            .observe_activation_cas_duration("success", activation_started.elapsed());

        Ok(RendezvousConnection {
            reader,
            writer,
            identity_cancellation: self.config.identity_cancellation.clone(),
            activation_cancellation,
        })
    }

    async fn phase_timeout<T>(
        &self,
        phase: &'static str,
        operation: impl std::future::Future<Output = Result<T, FramingError>>,
    ) -> Result<T, RendezvousCarrierError> {
        tokio::select! {
            () = self.config.identity_cancellation.cancelled() => {
                Err(RendezvousCarrierError::IdentityRevoked)
            }
            result = tokio::time::timeout(self.config.phase_timeout, operation) => match result {
                Ok(Ok(value)) => Ok(value),
                Ok(Err(error)) => Err(RendezvousCarrierError::Framing(error)),
                Err(_) => Err(RendezvousCarrierError::Timeout { phase }),
            }
        }
    }

    fn record_phase(&self, phase: &'static str, outcome: &'static str, started: Instant) {
        self.metrics
            .observe_connect_duration(phase, outcome, started.elapsed());
    }
}

/// Non-blocking admission error for the rendezvous request queue.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum RendezvousQueueError {
    Full,
    Closed,
}

/// Sender side of the protocol-sized bounded queue.
pub struct RendezvousQueueSender<T> {
    sender: mpsc::Sender<T>,
    metrics: Arc<dyn RendezvousMetricSink>,
}

impl<T> Clone for RendezvousQueueSender<T> {
    fn clone(&self) -> Self {
        Self {
            sender: self.sender.clone(),
            metrics: self.metrics.clone(),
        }
    }
}

impl<T> RendezvousQueueSender<T> {
    pub fn try_send(&self, value: T) -> Result<(), RendezvousQueueError> {
        match self.sender.try_send(value) {
            Ok(()) => {
                self.metrics
                    .increment_counter(QUEUE_COUNTER_METRIC, "enqueued");
                Ok(())
            }
            Err(mpsc::error::TrySendError::Full(_)) => {
                self.metrics
                    .increment_counter(QUEUE_COUNTER_METRIC, "dropped_full");
                Err(RendezvousQueueError::Full)
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                self.metrics
                    .increment_counter(QUEUE_COUNTER_METRIC, "dropped_closed");
                Err(RendezvousQueueError::Closed)
            }
        }
    }
}

/// Receiver side of the protocol-sized bounded queue.
pub struct RendezvousQueueReceiver<T> {
    receiver: mpsc::Receiver<T>,
    metrics: Arc<dyn RendezvousMetricSink>,
}

impl<T> RendezvousQueueReceiver<T> {
    pub async fn recv(&mut self) -> Option<T> {
        let value = self.receiver.recv().await;
        if value.is_some() {
            self.metrics
                .increment_counter(QUEUE_COUNTER_METRIC, "dequeued");
        }
        value
    }
}

/// Creates a queue whose capacity matches the negotiated protocol bound.
pub fn bounded_rendezvous_queue<T>(
    metrics: Arc<dyn RendezvousMetricSink>,
) -> (RendezvousQueueSender<T>, RendezvousQueueReceiver<T>) {
    let (sender, receiver) = mpsc::channel(usize::from(
        super::rendezvous_protocol::MAX_IN_FLIGHT_REQUESTS,
    ));
    (
        RendezvousQueueSender {
            sender,
            metrics: metrics.clone(),
        },
        RendezvousQueueReceiver { receiver, metrics },
    )
}

impl RendezvousCarrierError {
    const fn outcome(&self) -> &'static str {
        match self {
            Self::IdentityRevoked => "revoked",
            Self::Timeout { .. } => "timeout",
            _ => "error",
        }
    }
}

/// Accepted bidirectional rendezvous stream bound to one identity generation.
pub struct RendezvousConnection {
    reader: AsyncFrameReader<ReadHalf<TlsStream<TcpStream>>>,
    writer: AsyncFrameWriter<WriteHalf<TlsStream<TcpStream>>>,
    identity_cancellation: CancellationToken,
    activation_cancellation: CancellationToken,
}

impl RendezvousConnection {
    pub async fn recv(&mut self) -> Result<HostToRunnerFrame, RendezvousCarrierError> {
        tokio::select! {
            () = self.identity_cancellation.cancelled() => {
                Err(RendezvousCarrierError::IdentityRevoked)
            }
            () = self.activation_cancellation.cancelled() => {
                Err(RendezvousCarrierError::ActivationRevoked)
            }
            result = self.reader.read_message() => result.map_err(Into::into),
        }
    }

    pub async fn send(&mut self, frame: &RunnerToHostFrame) -> Result<(), RendezvousCarrierError> {
        tokio::select! {
            () = self.identity_cancellation.cancelled() => {
                Err(RendezvousCarrierError::IdentityRevoked)
            }
            () = self.activation_cancellation.cancelled() => {
                Err(RendezvousCarrierError::ActivationRevoked)
            }
            result = self.writer.write_message(frame) => result.map_err(Into::into),
        }
    }
}

#[cfg(test)]
mod tests;

use std::io;
use std::net::SocketAddr;

use serde::{Deserialize, Serialize};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use super::{DrainRequest, ResponseBody, SharedRunner, handle_drain};

pub struct HostedRunnerHandle {
    pub(super) local_addr: SocketAddr,
    pub(super) shared: SharedRunner,
    pub(super) shutdown: CancellationToken,
    pub(super) task: JoinHandle<()>,
    pub(super) identity_task: Option<JoinHandle<()>>,
    pub(super) tls: bool,
}

impl HostedRunnerHandle {
    #[must_use]
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    #[must_use]
    pub fn base_url(&self) -> String {
        let scheme = if self.tls { "https" } else { "http" };
        format!("{scheme}://{}", self.local_addr)
    }

    pub async fn drain_for_shutdown(
        &self,
        reason: impl Into<String>,
        requested_by: impl Into<String>,
    ) -> io::Result<serde_json::Value> {
        let response = handle_drain(
            self.shared.clone(),
            DrainRequest {
                reason: Some(reason.into()),
                requested_by: Some(requested_by.into()),
                export_paths: Some(vec![".".to_string()]),
            },
        )
        .await
        .map_err(|error| io::Error::other(error.message))?;
        match response {
            ResponseBody::Json { status, body } if status < 400 => Ok(body),
            ResponseBody::Json { status, body } => Err(io::Error::other(format!(
                "hosted runner drain returned status {status}: {body}"
            ))),
            ResponseBody::Bytes { .. } | ResponseBody::Sse { .. } => Err(io::Error::other(
                "hosted runner drain returned an unexpected stream response",
            )),
        }
    }

    pub async fn shutdown(self) {
        let _ = self.shared.stop_event_pump().await;
        self.shutdown.cancel();
        let _ = self.task.await;
        if let Some(identity_task) = self.identity_task {
            let _ = identity_task.await;
        }
        self.shared.stop_thread_persistence_recovery().await;
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostedRunnerIdentity {
    pub protocol_version: String,
    pub runner_session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_instance_id: Option<String>,
    pub ready: bool,
    pub draining: bool,
}

//! Transient rendezvous with the trusted controller for invocation authority.
use super::protocol::FromAgent;
use maestro_ai::managed_authorization::{
    ManagedAuthorizationProvider, ManagedAuthorizationRenewal,
};
use maestro_runtime_contracts::{ManagedGatewayCredential, ManagedInferenceAuthorization};
use std::{
    collections::HashMap,
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
};
use tokio::sync::{mpsc, oneshot};

type Reply = oneshot::Sender<ManagedAuthorizationRenewal>;

#[derive(Clone)]
pub struct ManagedAuthorizationCoordinator {
    pending: Arc<Mutex<HashMap<String, Reply>>>,
    events: mpsc::UnboundedSender<FromAgent>,
}

impl ManagedAuthorizationCoordinator {
    pub(super) fn new(events: mpsc::UnboundedSender<FromAgent>) -> Self {
        Self {
            pending: Arc::default(),
            events,
        }
    }

    /// Only the host's authenticated result adapter may answer a pending request.
    pub fn respond(
        &self,
        request_id: &str,
        authorization: ManagedInferenceAuthorization,
        gateway_credential: Option<ManagedGatewayCredential>,
    ) -> anyhow::Result<()> {
        authorization.validate().map_err(anyhow::Error::msg)?;
        if let Some(credential) = &gateway_credential {
            credential.validate().map_err(anyhow::Error::msg)?;
        }
        let reply = self
            .pending
            .lock()
            .map_err(|_| anyhow::anyhow!("authorization coordinator poisoned"))?
            .remove(request_id)
            .ok_or_else(|| anyhow::anyhow!("authorization request is no longer pending"))?;
        reply
            .send(ManagedAuthorizationRenewal {
                authorization,
                gateway_credential,
            })
            .map_err(|_| anyhow::anyhow!("authorization request was cancelled"))
    }
}

struct PendingGuard<'a> {
    owner: &'a ManagedAuthorizationCoordinator,
    id: String,
}
impl Drop for PendingGuard<'_> {
    fn drop(&mut self) {
        self.owner
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.id);
    }
}

impl ManagedAuthorizationProvider for ManagedAuthorizationCoordinator {
    fn renew(
        &self,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<ManagedAuthorizationRenewal>> + Send + '_>>
    {
        Box::pin(async move {
            let id = uuid::Uuid::new_v4().to_string();
            let (tx, rx) = oneshot::channel();
            self.pending
                .lock()
                .map_err(|_| anyhow::anyhow!("authorization coordinator poisoned"))?
                .insert(id.clone(), tx);
            let guard = PendingGuard { owner: self, id };
            self.events
                .send(FromAgent::ManagedAuthorizationRequest {
                    request_id: guard.id.clone(),
                })
                .map_err(|_| anyhow::anyhow!("authorization controller disconnected"))?;
            rx.await
                .map_err(|_| anyhow::anyhow!("authorization controller dropped the request"))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn cancelled_renewal_removes_pending_authority() {
        let (tx, mut events) = mpsc::unbounded_channel();
        let coordinator = Arc::new(ManagedAuthorizationCoordinator::new(tx));
        let waiter = {
            let coordinator = coordinator.clone();
            tokio::spawn(async move { coordinator.renew().await })
        };
        let FromAgent::ManagedAuthorizationRequest { request_id } = events.recv().await.unwrap()
        else {
            panic!("expected authorization request")
        };
        assert!(
            coordinator
                .respond(
                    "another-request",
                    ManagedInferenceAuthorization::new("opaque"),
                    None,
                )
                .is_err()
        );
        assert_eq!(coordinator.pending.lock().unwrap().len(), 1);
        waiter.abort();
        let _ = waiter.await;
        assert!(coordinator.pending.lock().unwrap().is_empty());
        assert!(
            coordinator
                .respond(
                    &request_id,
                    ManagedInferenceAuthorization::new("opaque"),
                    None
                )
                .is_err()
        );
    }

    #[tokio::test]
    async fn a_result_can_only_satisfy_its_request_once() {
        let (tx, mut events) = mpsc::unbounded_channel();
        let coordinator = Arc::new(ManagedAuthorizationCoordinator::new(tx));
        let waiter = {
            let coordinator = coordinator.clone();
            tokio::spawn(async move { coordinator.renew().await })
        };
        let FromAgent::ManagedAuthorizationRequest { request_id } = events.recv().await.unwrap()
        else {
            panic!("expected authorization request")
        };
        coordinator
            .respond(
                &request_id,
                ManagedInferenceAuthorization::new("opaque"),
                None,
            )
            .unwrap();
        assert_eq!(
            waiter.await.unwrap().unwrap().authorization.into_inner(),
            "opaque"
        );
        assert!(
            coordinator
                .respond(
                    &request_id,
                    ManagedInferenceAuthorization::new("other"),
                    None
                )
                .is_err()
        );
    }
}

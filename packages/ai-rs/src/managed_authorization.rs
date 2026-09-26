//! The trusted host supplies a new opaque capability for each HTTP invocation.
use std::{future::Future, pin::Pin};

use maestro_runtime_contracts::{ManagedGatewayCredential, ManagedInferenceAuthorization};

/// One-use, ephemeral response from the trusted managed inference controller.
#[derive(Debug)]
pub struct ManagedAuthorizationRenewal {
    pub authorization: ManagedInferenceAuthorization,
    pub gateway_credential: Option<ManagedGatewayCredential>,
}

/// Renewal is separate from logical-request hooks: transport retries consume
/// authority too. The client never signs or broadens the returned capability.
pub trait ManagedAuthorizationProvider: Send + Sync {
    fn renew(
        &self,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<ManagedAuthorizationRenewal>> + Send + '_>>;
}

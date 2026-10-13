//! Native child construction, independent of mailbox and journal management.

use crate::agent::tool_host::ToolHost;
use crate::agent::{CredentialVault, FromAgent, NativeAgentConfig};
use maestro_runtime::agent::NativeAgent;
use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::mpsc;

/// Already-resolved local child configuration. The lifecycle owner retains
/// leases, history, transcript persistence, cancellation, and terminal records.
pub struct ChildLaunchRequest {
    pub config: NativeAgentConfig,
    pub allowed_tools: HashSet<String>,
    pub credential_vault: CredentialVault,
    pub mailbox_identity: String,
}

/// A composing host supplies a child actor without owning its durable lifecycle.
/// Returning an error must not leave a detached child running.
pub trait ChildAgentFactory: Send + Sync {
    fn spawn(
        &self,
        request: ChildLaunchRequest,
    ) -> anyhow::Result<(NativeAgent, mpsc::UnboundedReceiver<FromAgent>)>;
}

/// Standard local tools, hooks, credentials, and policy for native children.
pub struct LocalChildAgentFactory {
    tool_host: Arc<dyn ToolHost>,
}

impl LocalChildAgentFactory {
    pub fn new(tool_host: Arc<dyn ToolHost>) -> Self {
        Self { tool_host }
    }
}

impl ChildAgentFactory for LocalChildAgentFactory {
    fn spawn(
        &self,
        request: ChildLaunchRequest,
    ) -> anyhow::Result<(NativeAgent, mpsc::UnboundedReceiver<FromAgent>)> {
        crate::agent::NativeAgent::new_with_allowed_tools_and_credential_vault_runner(
            Arc::clone(&self.tool_host),
            request.config,
            &request.allowed_tools,
            request.credential_vault,
            request.mailbox_identity,
        )
        .map(|(agent, events)| (agent.into_runtime(), events))
    }
}

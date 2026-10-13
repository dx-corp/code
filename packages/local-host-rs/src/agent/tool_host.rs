//! Seam between the local host and the concrete tool implementation.
//!
//! The host asks this trait for the three things it needs from the tool layer:
//! composing the native execution host, and listing tool definitions for
//! capability and subagent policy decisions. [`BuiltInToolHost`] is the
//! in-tree implementation. Each agent and dex-loop execution host requires
//! an explicit `Arc<dyn ToolHost>` at construction; host selection is never
//! shared process state.

use std::path::Path;
use std::sync::{Arc, RwLock};

use anyhow::Result;
use maestro_runtime::agent::{NativeExecutionHostHandle, ToolDefinition};

use super::{NativeAgentConfig, build_local_host};
use crate::agent::credential_store::CredentialVault;

/// Inputs for composing one native execution host.
pub struct LocalHostBuild<'a> {
    pub config: &'a NativeAgentConfig,
    pub credential_vault: CredentialVault,
    /// True for an injected test client that bypasses Identity verification.
    pub skip_identity_verification: bool,
    pub subagent_parent_scope_id: Option<String>,
    pub mailbox_identity: Option<String>,
    pub telemetry_identity_scope: Arc<RwLock<Option<crate::telemetry::TelemetryIdentityScope>>>,
}

pub trait ToolHost: Send + Sync {
    /// Compose the tool executor, hooks, firewall and model resolver for one actor.
    /// The owned receiver lets composition pass this same host to child agents.
    fn build_local_host(
        self: Arc<Self>,
        build: LocalHostBuild<'_>,
    ) -> Result<NativeExecutionHostHandle>;

    /// Definitions of the built-in tool registry.
    fn registry_tool_definitions(&self) -> Vec<ToolDefinition>;

    /// Definitions an executor rooted at `cwd` exposes.
    fn executor_tool_definitions(&self, cwd: &Path) -> Vec<ToolDefinition>;
}

/// The tool implementation that lives in this crate.
pub struct BuiltInToolHost;

impl ToolHost for BuiltInToolHost {
    fn build_local_host(
        self: Arc<Self>,
        build: LocalHostBuild<'_>,
    ) -> Result<NativeExecutionHostHandle> {
        build_local_host(
            self,
            build.config,
            build.credential_vault,
            build.skip_identity_verification,
            build.subagent_parent_scope_id,
            build.mailbox_identity,
            build.telemetry_identity_scope,
        )
    }

    fn registry_tool_definitions(&self) -> Vec<ToolDefinition> {
        crate::tools::ToolRegistry::new().tools().cloned().collect()
    }

    fn executor_tool_definitions(&self, cwd: &Path) -> Vec<ToolDefinition> {
        crate::tools::ToolExecutor::new(cwd.to_string_lossy().to_string())
            .tool_definitions()
            .cloned()
            .collect()
    }
}

#[cfg(test)]
mod tests;

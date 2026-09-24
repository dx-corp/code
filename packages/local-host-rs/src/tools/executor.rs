//! The tool-execution boundary.
//!
//! [`ToolExecutor`] is the seam between "the agent decided to run this tool"
//! and "something ran it". Everything above the seam -- approval, hooks, the
//! action firewall, sandbox-policy denial, the result cache, credential
//! vaulting, and receipt construction -- stays in the agent process by
//! construction. Everything below the seam is a single invocation that
//! produces one [`ToolResult`] and may stream output while it runs.
//!
//! [`InProcessExecutor`] is the implementation that runs the invocation on
//! the current process's tool stack. A second implementation that runs it in
//! a supervised child process plugs into the same trait.
//!
//! The boundary takes a fully resolved request, runs it, streams output, and
//! answers cancellation while the host retains policy and transcript state.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::agent::{FromAgent, ToolResult};
use crate::sandbox::SandboxPolicy;

/// Where a tool invocation runs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolIsolation {
    /// The invocation runs on the agent's own tool stack, in the agent's
    /// process.
    #[default]
    InProcess,
    /// The invocation runs in a separate operating-system process.
    Process,
}

/// Which of a tool's two output streams a chunk came from.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputStream {
    #[default]
    Stdout,
    Stderr,
}

/// The tools that may run outside the agent process.
///
/// The list is deliberately short and closed. A tool qualifies only when its
/// entire effect is filesystem or network I/O that a child process can
/// perform on the parent's behalf. Anything that reads or writes agent state
/// -- approvals, hooks, `ask_user`, `todo`, subagent lifecycle,
/// MCP sessions, mailbox, goal and harness context -- is absent from this
/// list and stays in-process regardless of configuration.
pub const PROCESS_ISOLATABLE_TOOLS: [&str; 8] = [
    "bash",
    "web_fetch",
    "read",
    "write",
    "edit",
    "glob",
    "grep",
    "list",
];

/// Whether `tool` is allowed to run outside the agent process.
///
/// Comparison is ASCII-case-insensitive because the dispatcher accepts the
/// capitalized aliases (`Bash`, `Read`, `WebFetch`) the models emit.
#[must_use]
pub fn is_process_isolatable(tool: &str) -> bool {
    let normalized = tool.to_ascii_lowercase();
    let normalized = match normalized.as_str() {
        "webfetch" => "web_fetch",
        "ls" => "list",
        other => other,
    };
    PROCESS_ISOLATABLE_TOOLS.contains(&normalized)
}

/// The sandbox decision the agent already made, in a form that survives a
/// process boundary.
///
/// `None` means the agent applied no native sandbox to this executor, which
/// is not the same as [`SandboxPolicy::DangerFullAccess`]: the latter is an
/// explicit policy the agent chose, the former is the absence of one. A child
/// process must reproduce that distinction exactly, so the snapshot carries
/// the `Option` rather than collapsing it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxPolicySnapshot {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy: Option<SandboxPolicy>,
}

impl SandboxPolicySnapshot {
    /// Snapshot an executor's configured policy.
    #[must_use]
    pub fn from_policy(policy: Option<SandboxPolicy>) -> Self {
        Self { policy }
    }

    /// The policy this snapshot carries, if the agent configured one.
    #[must_use]
    pub fn policy(&self) -> Option<&SandboxPolicy> {
        self.policy.as_ref()
    }

    /// Whether the agent configured no native sandbox for this executor.
    #[must_use]
    pub fn is_unrestricted(&self) -> bool {
        self.policy.is_none()
    }
}

/// One fully-resolved tool call, ready to run.
///
/// Every field is owned and serializable so the same value can be handed to
/// an in-process executor or written to a child process's stdin.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolInvocation {
    /// The agent's identifier for this call. Output chunks and results are
    /// routed back by this value.
    pub call_id: String,
    /// Registry tool name, as the model emitted it.
    pub tool: String,
    /// Validated tool arguments.
    pub args: serde_json::Value,
    /// Working directory the tool runs in.
    pub cwd: PathBuf,
    /// Extra environment entries the tool must see, on top of the executor's
    /// own environment. Ordered because approval-scoped inline environments
    /// are order-sensitive.
    #[serde(default)]
    pub env: Vec<(String, String)>,
    /// The sandbox decision the agent already made.
    #[serde(default)]
    pub sandbox: SandboxPolicySnapshot,
    /// Wall-clock bound for this invocation, if the caller set one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<Duration>,
}

impl ToolInvocation {
    /// Build an invocation with no extra environment, no sandbox snapshot,
    /// and no timeout.
    pub fn new(
        call_id: impl Into<String>,
        tool: impl Into<String>,
        args: serde_json::Value,
        cwd: impl Into<PathBuf>,
    ) -> Self {
        Self {
            call_id: call_id.into(),
            tool: tool.into(),
            args,
            cwd: cwd.into(),
            env: Vec::new(),
            sandbox: SandboxPolicySnapshot::default(),
            timeout: None,
        }
    }

    #[must_use]
    pub fn with_env(mut self, env: Vec<(String, String)>) -> Self {
        self.env = env;
        self
    }

    #[must_use]
    pub fn with_sandbox(mut self, sandbox: SandboxPolicySnapshot) -> Self {
        self.sandbox = sandbox;
        self
    }

    #[must_use]
    pub fn with_timeout(mut self, timeout: Option<Duration>) -> Self {
        self.timeout = timeout;
        self
    }
}

/// Where a running tool's incremental output goes.
///
/// This wraps the agent's own event channel so an executor never has to know
/// how the UI consumes output. An executor that produces no incremental
/// output simply ignores the sink.
#[derive(Clone, Debug)]
pub struct OutputSink {
    events: mpsc::UnboundedSender<FromAgent>,
}

impl OutputSink {
    #[must_use]
    pub fn new(events: mpsc::UnboundedSender<FromAgent>) -> Self {
        Self { events }
    }

    /// The underlying agent event channel.
    ///
    /// [`InProcessExecutor`] hands this straight to the existing dispatcher,
    /// which already emits `FromAgent::ToolOutput` itself; that keeps the
    /// in-process path byte-identical to what it was before the seam existed.
    #[must_use]
    pub fn events(&self) -> &mpsc::UnboundedSender<FromAgent> {
        &self.events
    }

    /// Forward one chunk of tool output.
    ///
    /// Both streams become `FromAgent::ToolOutput`, which is what the
    /// in-process bash tool already does; the `stream` argument exists so a
    /// transport that distinguishes them (the child-process wire protocol)
    /// does not have to lose the distinction before it reaches this point.
    pub fn emit(&self, call_id: &str, _stream: OutputStream, chunk: impl Into<String>) {
        let content = chunk.into();
        if content.is_empty() {
            return;
        }
        let _ = self.events.send(FromAgent::ToolOutput {
            call_id: call_id.to_string(),
            content,
        });
    }
}

/// Runs one tool invocation.
///
/// Implementations must not panic: a failed invocation is a
/// [`ToolResult`] with `success: false`, never an unwind. Implementations
/// must also honor `cancel` -- an in-process implementation by racing the
/// token, an out-of-process implementation by killing the child.
#[async_trait]
pub trait ToolExecutor: Send + Sync {
    /// Run `invocation` to completion, or until `cancel` fires.
    async fn execute(
        &self,
        invocation: ToolInvocation,
        cancel: CancellationToken,
        output: Option<OutputSink>,
    ) -> ToolResult;

    /// Where this executor runs invocations.
    fn isolation(&self) -> ToolIsolation;
}

/// Runs invocations on the current process's tool stack.
///
/// This is a thin adapter over [`crate::tools::ToolExecutor`] (the registry
/// dispatcher). It calls the dispatcher's raw entry point, because the policy,
/// cache, and receipt layers above the seam have already run by the time an
/// invocation reaches an executor.
pub struct InProcessExecutor {
    registry: Arc<crate::tools::ToolExecutor>,
}

impl InProcessExecutor {
    /// Build an executor backed by a fresh registry rooted at `cwd`.
    #[must_use]
    pub fn new(cwd: impl Into<String>) -> Self {
        Self {
            registry: Arc::new(crate::tools::ToolExecutor::new(cwd)),
        }
    }

    /// Build an executor backed by an existing registry.
    #[must_use]
    pub fn from_registry(registry: Arc<crate::tools::ToolExecutor>) -> Self {
        Self { registry }
    }

    /// The registry this executor dispatches through.
    #[must_use]
    pub fn registry(&self) -> &Arc<crate::tools::ToolExecutor> {
        &self.registry
    }
}

impl std::fmt::Debug for InProcessExecutor {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("InProcessExecutor")
            .field("cwd", &self.registry.cwd())
            .finish()
    }
}

#[async_trait]
impl ToolExecutor for InProcessExecutor {
    async fn execute(
        &self,
        invocation: ToolInvocation,
        cancel: CancellationToken,
        output: Option<OutputSink>,
    ) -> ToolResult {
        self.registry
            .dispatch_invocation(&invocation, output.as_ref().map(OutputSink::events), cancel)
            .await
    }

    fn isolation(&self) -> ToolIsolation {
        ToolIsolation::InProcess
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_isolatable_covers_aliases_and_excludes_stateful_tools() {
        for tool in [
            "bash",
            "Bash",
            "read",
            "Read",
            "WebFetch",
            "web_fetch",
            "ls",
        ] {
            assert!(is_process_isolatable(tool), "{tool} should be isolatable");
        }
        for tool in [
            "ask_user",
            "todo",
            "spawn_subagent",
            "wait_subagent",
            "update_goal",
            "send_mailbox",
            "mcp_list_resources",
            "explore",
        ] {
            assert!(
                !is_process_isolatable(tool),
                "{tool} must stay in the agent process"
            );
        }
    }

    #[test]
    fn invocation_round_trips_through_json() {
        let invocation = ToolInvocation::new(
            "call-1",
            "bash",
            serde_json::json!({"command": "echo hi"}),
            "/workspace",
        )
        .with_env(vec![("FOO".to_string(), "bar".to_string())])
        .with_sandbox(SandboxPolicySnapshot::from_policy(Some(
            SandboxPolicy::ReadOnly,
        )))
        .with_timeout(Some(Duration::from_millis(1500)));

        let encoded = serde_json::to_string(&invocation).expect("invocation should serialize");
        let decoded: ToolInvocation =
            serde_json::from_str(&encoded).expect("invocation should deserialize");
        assert_eq!(decoded, invocation);
    }

    #[test]
    fn absent_sandbox_policy_is_distinct_from_danger_full_access() {
        let absent = SandboxPolicySnapshot::from_policy(None);
        let explicit = SandboxPolicySnapshot::from_policy(Some(SandboxPolicy::DangerFullAccess));
        assert!(absent.is_unrestricted());
        assert!(!explicit.is_unrestricted());
        assert_ne!(absent, explicit);
    }

    #[tokio::test]
    async fn in_process_executor_runs_a_read_and_reports_its_isolation() {
        let directory = std::env::temp_dir().join(format!(
            "maestro-in-process-executor-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock should follow the Unix epoch")
                .as_nanos()
        ));
        std::fs::create_dir_all(&directory).expect("temp directory should be created");
        let file = directory.join("fixture.txt");
        std::fs::write(&file, "in-process-executor\n").expect("fixture should be written");

        let executor = InProcessExecutor::new(directory.display().to_string());
        assert_eq!(executor.isolation(), ToolIsolation::InProcess);

        let result = executor
            .execute(
                ToolInvocation::new(
                    "call-read",
                    "read",
                    serde_json::json!({"file_path": file.display().to_string()}),
                    &directory,
                ),
                CancellationToken::new(),
                None,
            )
            .await;

        assert!(result.success, "read failed: {result:?}");
        assert!(result.output.contains("in-process-executor"));
        std::fs::remove_dir_all(&directory).expect("temp directory should be removed");
    }

    #[tokio::test]
    async fn in_process_executor_streams_output_through_the_sink() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let sink = OutputSink::new(tx);
        sink.emit("call-1", OutputStream::Stderr, "partial");
        sink.emit("call-1", OutputStream::Stdout, "");

        let event = rx.try_recv().expect("one chunk should have been forwarded");
        match event {
            FromAgent::ToolOutput { call_id, content } => {
                assert_eq!(call_id, "call-1");
                assert_eq!(content, "partial");
            }
            other => panic!("unexpected event: {other:?}"),
        }
        assert!(rx.try_recv().is_err(), "empty chunks must not be forwarded");
    }
}

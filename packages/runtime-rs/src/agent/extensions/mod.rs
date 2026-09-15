//! Agent extension registry and its declared hook points.
//!
//! The native agent loop in `src/agent/native.rs` used to grow a new `if` or
//! `match` arm every time the loop needed a new behavior. This module replaces
//! that with a fixed set of hook points and an ordered list of tenants that are
//! invoked at each one. New loop behavior lands as an [`AgentExtension`]
//! implementation registered in [`ExtensionRegistry::with_default_tenants`],
//! not as another branch in `native.rs`.
//!
//! Ordered registration and per-tenant isolation keep extension failures local.
//! These hooks fire on a synchronous, single
//! threaded loop, so registration order is the only ordering a tenant needs.
//!
//! # Hook points
//!
//! | Hook | Fired from | Verdict honored |
//! | --- | --- | --- |
//! | [`AgentExtension::on_user_turn_start`] | the three `AgentCommand` arms that discard conversation state (`ClearHistory`, `ReplaceHistory`, `ReplaceHistoryPreservingCredentials`) | no |
//! | [`AgentExtension::on_tool_call_planned`] | before every tool call, including the re-check applied to a deferred call after an approval boundary | yes |
//! | [`AgentExtension::on_tool_result`] | after a tool call produced its model-facing result | no (payload is mutable) |
//! | [`AgentExtension::on_tool_batch_end`] | after a whole batch of tool results is assembled, before it enters history | no (payload is mutable) |
//! | [`AgentExtension::on_assistant_text_delta`] | on each streamed assistant text delta | yes |
//! | [`AgentExtension::on_turn_end`] | once per `run_loop` call, on every exit path | no |
//!
//! # Verdict merge
//!
//! Tenants run in registration order and every tenant runs, even after one has
//! already voted to block. The registry merges their verdicts as:
//!
//! 1. any [`ExtensionVerdict::Block`] wins, and the first block in registration
//!    order supplies the reason;
//! 2. otherwise the first [`ExtensionVerdict::Steer`] in registration order wins;
//! 3. otherwise [`ExtensionVerdict::Proceed`].
//!
//! # Tenant isolation
//!
//! A hook signature has no `Result`, so the one way a tenant can be wrong about
//! its own verdict is to return a `Block` or `Steer` whose text is empty: the
//! agent loop would then show the user or the model an empty reason. The
//! registry rejects such a verdict, downgrades it to `Proceed`, counts it in
//! [`ExtensionStats::errors`], stores it in [`ExtensionStats::last_error`], and
//! logs it once per tenant. The remaining tenants still run.
//!
//! A tenant must not panic. The release profile in `Cargo.toml`
//! sets `panic = "abort"`, so a panic inside a hook terminates the process and
//! no registry-level guard can catch it.

pub mod doom_loop;
pub(crate) mod model_dynamics;

use std::time::Instant;

use crate::agent::safety::SafetyConfig;
pub use doom_loop::DoomLoopExtension;

/// State handed to [`AgentExtension::on_user_turn_start`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnStartContext {
    /// Identifier of the turn the runner is currently executing, or the last
    /// one it executed when the reset arrives while the agent is idle.
    pub turn_id: String,
    /// Count of turns the runner has started, including the current one.
    pub turn_index: u64,
}

/// State handed to [`AgentExtension::on_tool_call_planned`].
///
/// Carries only plain data. No field borrows runner state, so a tenant cannot
/// reach back into `native.rs` internals.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCallContext {
    /// Identifier of the turn this call belongs to.
    pub turn_id: String,
    /// Provider-assigned tool-use identifier.
    pub call_id: String,
    /// Tool name as the model asked for it.
    pub tool_name: String,
    /// Key-sorted JSON rendering of `args`, stable across map iteration order.
    pub args_hash: String,
    /// Credential-vaulted arguments. Secrets are already replaced by references.
    pub args: serde_json::Value,
    /// Count of tool calls the runner has planned in this turn before this one.
    pub call_index: u64,
}

/// State handed to [`AgentExtension::on_tool_result`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolResultContext {
    /// Trusted local edit receipt; absent for unknown or provider-native results.
    pub edit: Option<LocalEditResult>,
    /// Identifier of the turn this call belongs to.
    pub turn_id: String,
    /// Provider-assigned tool-use identifier.
    pub call_id: String,
    /// Tool name as the model asked for it.
    pub tool_name: String,
    /// Key-sorted JSON rendering of `args`.
    pub args_hash: String,
    /// Credential-vaulted arguments.
    pub args: serde_json::Value,
    /// Whether the runner is about to report this result to the model as an error.
    pub is_error: bool,
    /// Wall-clock duration the runner measured for this call.
    pub duration_ms: u64,
}

/// Content-free local edit outcome used for repair suggestions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalEditResult {
    pub path: String,
    pub text_not_found: bool,
}

/// Immutable completion of a tool executed inside the provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeToolResultContext {
    pub turn_id: String,
    pub call_id: String,
    pub success: bool,
}

/// State handed to [`AgentExtension::on_tool_batch_end`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchEndContext {
    /// Identifier of the turn this batch belongs to.
    pub turn_id: String,
    /// Number of tool results in the batch.
    pub batch_size: u64,
    /// How many of those results are reported to the model as errors.
    pub error_count: u64,
}

/// State handed to [`AgentExtension::on_turn_end`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnEndContext {
    /// Identifier of the turn that just ended.
    pub turn_id: String,
    /// Number of tool calls the runner planned during the turn.
    pub tool_calls: u64,
    /// Whether the turn ended by error or cancellation rather than completing.
    pub interrupted: bool,
}

/// The model-facing half of a tool result, which a tenant may amend in place.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolResultPayload {
    /// Text the model will see for this tool call.
    pub content: String,
    /// Whether the model will see this result flagged as an error.
    pub is_error: bool,
}

/// What a tenant wants the agent loop to do at a hook point that takes a verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExtensionVerdict {
    /// Continue unchanged.
    Proceed,
    /// Stop the action under inspection and report `reason`.
    ///
    /// At [`AgentExtension::on_tool_call_planned`] the tool does not run and
    /// `reason` becomes an error tool result. At
    /// [`AgentExtension::on_assistant_text_delta`] the turn ends after the
    /// current provider response completes.
    Block {
        /// Why the action was stopped. Shown to the user and to the model.
        reason: String,
    },
    /// Let the loop continue but push `message` at the model.
    ///
    /// At [`AgentExtension::on_tool_call_planned`] the tool does not run and
    /// `message` becomes a non-error tool result. At
    /// [`AgentExtension::on_assistant_text_delta`] `message` is queued as a
    /// steering prompt for the next turn.
    Steer {
        /// Text handed to the model.
        message: String,
    },
}

impl ExtensionVerdict {
    /// Text carried by a `Block` or `Steer`, if any.
    #[must_use]
    pub fn text(&self) -> Option<&str> {
        match self {
            Self::Proceed => None,
            Self::Block { reason } => Some(reason.as_str()),
            Self::Steer { message } => Some(message.as_str()),
        }
    }
}

/// One behavior of the agent loop, registered as a tenant of the loop rather
/// than written as a branch inside it.
///
/// Every hook has a default no-op body, so a tenant implements only the hooks it
/// needs. Implementations must be `Send + Sync`: the registry lives on the
/// runner, which runs on a Tokio task, and the runner is also borrowed across
/// awaits by side-question paths that require its immutable fields to be
/// shareable.
pub trait AgentExtension: Send + Sync {
    /// Stable identifier used in logs and in [`ExtensionStats`].
    fn name(&self) -> &'static str;

    /// The runner discarded conversation state for a new user turn.
    fn on_user_turn_start(&mut self, cx: &TurnStartContext) {
        let _ = cx;
    }

    /// The model asked for a tool call and the runner has not run it yet.
    fn on_tool_call_planned(&mut self, cx: &ToolCallContext) -> ExtensionVerdict {
        let _ = cx;
        ExtensionVerdict::Proceed
    }

    /// A tool call produced the result the model is about to see.
    fn on_tool_result(&mut self, cx: &ToolResultContext, result: &mut ToolResultPayload) {
        let _ = (cx, result);
    }

    /// Observe a provider-owned tool result; its effects and output are already complete.
    fn on_native_tool_result(&mut self, cx: &NativeToolResultContext) {
        let _ = cx;
    }

    /// A whole batch of tool results is assembled and about to enter history.
    ///
    /// `last_result` is the final result in the batch, which is where a tenant
    /// appends text meant to be read after the batch.
    fn on_tool_batch_end(&mut self, cx: &BatchEndContext, last_result: &mut ToolResultPayload) {
        let _ = (cx, last_result);
    }

    /// The provider streamed one assistant text delta.
    fn on_assistant_text_delta(&mut self, delta: &str) -> ExtensionVerdict {
        let _ = delta;
        ExtensionVerdict::Proceed
    }

    /// One `run_loop` call finished, whether it completed or was interrupted.
    fn on_turn_end(&mut self, cx: &TurnEndContext) {
        let _ = cx;
    }
}

/// Per-tenant accounting the registry keeps for diagnostics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtensionStats {
    /// The tenant's [`AgentExtension::name`].
    pub name: &'static str,
    /// How many hook invocations this tenant has received.
    pub calls: u64,
    /// Total time spent inside this tenant's hooks, in microseconds.
    pub total_duration_us: u64,
    /// How many malformed verdicts this tenant returned.
    pub errors: u64,
    /// The most recent malformed-verdict description, if any.
    pub last_error: Option<String>,
}

impl ExtensionStats {
    fn new(name: &'static str) -> Self {
        Self {
            name,
            calls: 0,
            total_duration_us: 0,
            errors: 0,
            last_error: None,
        }
    }
}

struct RegisteredExtension {
    extension: Box<dyn AgentExtension>,
    stats: ExtensionStats,
}

/// Ordered list of [`AgentExtension`] tenants plus the dispatch that invokes them.
///
/// An empty registry is a no-op at every hook point, so wiring a hook into the
/// agent loop is safe before any tenant exists for it.
pub struct ExtensionRegistry {
    tenants: Vec<RegisteredExtension>,
}

impl std::fmt::Debug for ExtensionRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExtensionRegistry")
            .field(
                "tenants",
                &self
                    .tenants
                    .iter()
                    .map(|tenant| tenant.stats.name)
                    .collect::<Vec<_>>(),
            )
            .finish()
    }
}

impl ExtensionRegistry {
    /// An empty registry. Every hook is a no-op.
    #[must_use]
    pub fn new() -> Self {
        Self {
            tenants: Vec::new(),
        }
    }

    /// The registry the native agent runs with.
    ///
    /// Registration order here is the order tenants observe every hook.
    #[must_use]
    pub fn with_default_tenants() -> Self {
        Self::with_default_tenants_and_safety(SafetyConfig::default())
    }

    /// [`Self::with_default_tenants`] with explicit doom-loop thresholds.
    #[must_use]
    pub fn with_default_tenants_and_safety(safety: SafetyConfig) -> Self {
        let mut registry = Self::new();
        registry.register(Box::new(DoomLoopExtension::with_config(safety)));
        registry
    }

    /// Append a tenant. It observes every later hook after all tenants already
    /// registered.
    pub fn register(&mut self, extension: Box<dyn AgentExtension>) {
        let stats = ExtensionStats::new(extension.name());
        self.tenants.push(RegisteredExtension { extension, stats });
    }

    /// Tenant names in registration order.
    #[must_use]
    pub fn names(&self) -> Vec<&'static str> {
        self.tenants
            .iter()
            .map(|tenant| tenant.stats.name)
            .collect()
    }

    /// Per-tenant accounting in registration order.
    #[must_use]
    pub fn stats(&self) -> Vec<ExtensionStats> {
        self.tenants
            .iter()
            .map(|tenant| tenant.stats.clone())
            .collect()
    }

    /// Accounting for one tenant by name.
    #[must_use]
    pub fn stats_for(&self, name: &str) -> Option<ExtensionStats> {
        self.tenants
            .iter()
            .find(|tenant| tenant.stats.name == name)
            .map(|tenant| tenant.stats.clone())
    }

    /// Number of registered tenants.
    #[must_use]
    pub fn len(&self) -> usize {
        self.tenants.len()
    }

    /// Whether no tenant is registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.tenants.is_empty()
    }

    /// Fire [`AgentExtension::on_user_turn_start`] on every tenant, in order.
    pub fn on_user_turn_start(&mut self, cx: &TurnStartContext) {
        for tenant in &mut self.tenants {
            let started = Instant::now();
            tenant.extension.on_user_turn_start(cx);
            record_timing(&mut tenant.stats, started);
        }
    }

    /// Fire [`AgentExtension::on_tool_call_planned`] on every tenant and merge
    /// the verdicts. Every tenant runs even once one has voted to block.
    #[must_use]
    pub fn on_tool_call_planned(&mut self, cx: &ToolCallContext) -> ExtensionVerdict {
        let mut merge = VerdictMerge::default();
        for tenant in &mut self.tenants {
            let started = Instant::now();
            let verdict = tenant.extension.on_tool_call_planned(cx);
            record_timing(&mut tenant.stats, started);
            merge.accept(verdict, &mut tenant.stats, "on_tool_call_planned");
        }
        merge.finish()
    }

    /// Fire [`AgentExtension::on_tool_result`] on every tenant, in order. Each
    /// tenant sees the payload as the tenants before it left it.
    pub fn on_tool_result(&mut self, cx: &ToolResultContext, result: &mut ToolResultPayload) {
        for tenant in &mut self.tenants {
            let started = Instant::now();
            tenant.extension.on_tool_result(cx, result);
            record_timing(&mut tenant.stats, started);
        }
    }

    /// Announce a provider-owned completion without offering to rewrite its output.
    pub fn on_native_tool_result(&mut self, cx: &NativeToolResultContext) {
        for tenant in &mut self.tenants {
            let started = Instant::now();
            tenant.extension.on_native_tool_result(cx);
            record_timing(&mut tenant.stats, started);
        }
    }

    /// Fire [`AgentExtension::on_tool_batch_end`] on every tenant, in order.
    pub fn on_tool_batch_end(&mut self, cx: &BatchEndContext, last_result: &mut ToolResultPayload) {
        for tenant in &mut self.tenants {
            let started = Instant::now();
            tenant.extension.on_tool_batch_end(cx, last_result);
            record_timing(&mut tenant.stats, started);
        }
    }

    /// Fire [`AgentExtension::on_assistant_text_delta`] on every tenant and
    /// merge the verdicts.
    #[must_use]
    pub fn on_assistant_text_delta(&mut self, delta: &str) -> ExtensionVerdict {
        let mut merge = VerdictMerge::default();
        for tenant in &mut self.tenants {
            let started = Instant::now();
            let verdict = tenant.extension.on_assistant_text_delta(delta);
            record_timing(&mut tenant.stats, started);
            merge.accept(verdict, &mut tenant.stats, "on_assistant_text_delta");
        }
        merge.finish()
    }

    /// Fire [`AgentExtension::on_turn_end`] on every tenant, in order.
    pub fn on_turn_end(&mut self, cx: &TurnEndContext) {
        for tenant in &mut self.tenants {
            let started = Instant::now();
            tenant.extension.on_turn_end(cx);
            record_timing(&mut tenant.stats, started);
        }
    }
}

impl Default for ExtensionRegistry {
    fn default() -> Self {
        Self::new()
    }
}

fn record_timing(stats: &mut ExtensionStats, started: Instant) {
    stats.calls = stats.calls.saturating_add(1);
    stats.total_duration_us = stats
        .total_duration_us
        .saturating_add(started.elapsed().as_micros() as u64);
}

/// Accumulates tenant verdicts under the documented merge rule.
#[derive(Default)]
struct VerdictMerge {
    block: Option<String>,
    steer: Option<String>,
}

impl VerdictMerge {
    fn accept(&mut self, verdict: ExtensionVerdict, stats: &mut ExtensionStats, hook: &str) {
        // A verdict that stops or redirects the loop with no text would show an
        // empty reason to the user and to the model. Isolate the tenant that
        // produced it instead of propagating it.
        if verdict.text().is_some_and(|text| text.trim().is_empty()) {
            let description = format!("{hook} returned an empty verdict text");
            if stats.last_error.is_none() {
                tracing::warn!(
                    target: "maestro.agent.extensions",
                    event = "agent_extension_isolated",
                    extension = stats.name,
                    hook,
                    "extension returned an empty verdict text; treating it as Proceed",
                );
            }
            stats.errors = stats.errors.saturating_add(1);
            stats.last_error = Some(description);
            return;
        }

        match verdict {
            ExtensionVerdict::Proceed => {}
            ExtensionVerdict::Block { reason } => {
                if self.block.is_none() {
                    self.block = Some(reason);
                }
            }
            ExtensionVerdict::Steer { message } => {
                if self.steer.is_none() {
                    self.steer = Some(message);
                }
            }
        }
    }

    fn finish(self) -> ExtensionVerdict {
        if let Some(reason) = self.block {
            return ExtensionVerdict::Block { reason };
        }
        if let Some(message) = self.steer {
            return ExtensionVerdict::Steer { message };
        }
        ExtensionVerdict::Proceed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::Mutex;

    /// Records the order in which hooks reached each tenant and returns a fixed
    /// verdict from the verdict-taking hooks.
    struct ScriptedExtension {
        name: &'static str,
        log: Arc<Mutex<Vec<String>>>,
        verdict: ExtensionVerdict,
        appends: Option<&'static str>,
    }

    impl ScriptedExtension {
        fn new(name: &'static str, log: &Arc<Mutex<Vec<String>>>) -> Self {
            Self {
                name,
                log: Arc::clone(log),
                verdict: ExtensionVerdict::Proceed,
                appends: None,
            }
        }

        fn with_verdict(mut self, verdict: ExtensionVerdict) -> Self {
            self.verdict = verdict;
            self
        }

        fn appending(mut self, text: &'static str) -> Self {
            self.appends = Some(text);
            self
        }

        fn note(&self, hook: &str) {
            self.log
                .lock()
                .expect("extension log poisoned")
                .push(format!("{}:{hook}", self.name));
        }
    }

    impl AgentExtension for ScriptedExtension {
        fn name(&self) -> &'static str {
            self.name
        }

        fn on_user_turn_start(&mut self, _cx: &TurnStartContext) {
            self.note("on_user_turn_start");
        }

        fn on_tool_call_planned(&mut self, _cx: &ToolCallContext) -> ExtensionVerdict {
            self.note("on_tool_call_planned");
            self.verdict.clone()
        }

        fn on_tool_result(&mut self, _cx: &ToolResultContext, result: &mut ToolResultPayload) {
            self.note("on_tool_result");
            if let Some(text) = self.appends {
                result.content.push_str(text);
            }
        }

        fn on_tool_batch_end(
            &mut self,
            _cx: &BatchEndContext,
            last_result: &mut ToolResultPayload,
        ) {
            self.note("on_tool_batch_end");
            if let Some(text) = self.appends {
                last_result.content.push_str(text);
            }
        }

        fn on_assistant_text_delta(&mut self, _delta: &str) -> ExtensionVerdict {
            self.note("on_assistant_text_delta");
            self.verdict.clone()
        }

        fn on_turn_end(&mut self, _cx: &TurnEndContext) {
            self.note("on_turn_end");
        }
    }

    fn tool_call_context() -> ToolCallContext {
        ToolCallContext {
            turn_id: "turn-1".to_string(),
            call_id: "call-1".to_string(),
            tool_name: "bash".to_string(),
            args_hash: "{\"command\":\"ls\"}".to_string(),
            args: serde_json::json!({"command": "ls"}),
            call_index: 0,
        }
    }

    fn tool_result_context() -> ToolResultContext {
        ToolResultContext {
            edit: None,
            turn_id: "turn-1".to_string(),
            call_id: "call-1".to_string(),
            tool_name: "bash".to_string(),
            args_hash: "{\"command\":\"ls\"}".to_string(),
            args: serde_json::json!({"command": "ls"}),
            is_error: false,
            duration_ms: 3,
        }
    }

    fn batch_end_context() -> BatchEndContext {
        BatchEndContext {
            turn_id: "turn-1".to_string(),
            batch_size: 2,
            error_count: 0,
        }
    }

    #[test]
    fn registry_is_send_and_sync_for_async_runner_borrows() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<ExtensionRegistry>();
    }

    #[test]
    fn empty_registry_is_a_no_op_at_every_hook() {
        let mut registry = ExtensionRegistry::new();
        assert!(registry.is_empty());

        let mut payload = ToolResultPayload {
            content: "output".to_string(),
            is_error: false,
        };
        registry.on_user_turn_start(&TurnStartContext {
            turn_id: "turn-1".to_string(),
            turn_index: 1,
        });
        assert_eq!(
            registry.on_tool_call_planned(&tool_call_context()),
            ExtensionVerdict::Proceed
        );
        registry.on_tool_result(&tool_result_context(), &mut payload);
        registry.on_tool_batch_end(&batch_end_context(), &mut payload);
        assert_eq!(
            registry.on_assistant_text_delta("hello"),
            ExtensionVerdict::Proceed
        );
        registry.on_turn_end(&TurnEndContext {
            turn_id: "turn-1".to_string(),
            tool_calls: 0,
            interrupted: false,
        });

        assert_eq!(payload.content, "output");
        assert!(!payload.is_error);
    }

    #[test]
    fn tenants_run_in_registration_order_at_every_hook() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut registry = ExtensionRegistry::new();
        registry.register(Box::new(ScriptedExtension::new("first", &log)));
        registry.register(Box::new(ScriptedExtension::new("second", &log)));
        registry.register(Box::new(ScriptedExtension::new("third", &log)));

        assert_eq!(registry.names(), vec!["first", "second", "third"]);

        let mut payload = ToolResultPayload::default();
        registry.on_user_turn_start(&TurnStartContext {
            turn_id: "turn-1".to_string(),
            turn_index: 1,
        });
        let _ = registry.on_tool_call_planned(&tool_call_context());
        registry.on_tool_result(&tool_result_context(), &mut payload);
        registry.on_tool_batch_end(&batch_end_context(), &mut payload);
        let _ = registry.on_assistant_text_delta("hello");
        registry.on_turn_end(&TurnEndContext {
            turn_id: "turn-1".to_string(),
            tool_calls: 1,
            interrupted: false,
        });

        let observed = log.lock().expect("extension log poisoned").clone();
        assert_eq!(
            observed,
            vec![
                "first:on_user_turn_start",
                "second:on_user_turn_start",
                "third:on_user_turn_start",
                "first:on_tool_call_planned",
                "second:on_tool_call_planned",
                "third:on_tool_call_planned",
                "first:on_tool_result",
                "second:on_tool_result",
                "third:on_tool_result",
                "first:on_tool_batch_end",
                "second:on_tool_batch_end",
                "third:on_tool_batch_end",
                "first:on_assistant_text_delta",
                "second:on_assistant_text_delta",
                "third:on_assistant_text_delta",
                "first:on_turn_end",
                "second:on_turn_end",
                "third:on_turn_end",
            ]
        );
    }

    #[test]
    fn payload_edits_compose_in_registration_order() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut registry = ExtensionRegistry::new();
        registry.register(Box::new(
            ScriptedExtension::new("first", &log).appending("-first"),
        ));
        registry.register(Box::new(
            ScriptedExtension::new("second", &log).appending("-second"),
        ));

        let mut payload = ToolResultPayload {
            content: "base".to_string(),
            is_error: false,
        };
        registry.on_tool_result(&tool_result_context(), &mut payload);
        assert_eq!(payload.content, "base-first-second");
    }

    #[test]
    fn any_block_wins_over_steer_and_proceed() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut registry = ExtensionRegistry::new();
        registry.register(Box::new(ScriptedExtension::new("proceeds", &log)));
        registry.register(Box::new(
            ScriptedExtension::new("steers", &log).with_verdict(ExtensionVerdict::Steer {
                message: "try something else".to_string(),
            }),
        ));
        registry.register(Box::new(
            ScriptedExtension::new("blocks", &log).with_verdict(ExtensionVerdict::Block {
                reason: "stop".to_string(),
            }),
        ));

        assert_eq!(
            registry.on_tool_call_planned(&tool_call_context()),
            ExtensionVerdict::Block {
                reason: "stop".to_string()
            }
        );

        // Every tenant still observed the hook, including the two registered
        // before the blocker.
        let observed = log.lock().expect("extension log poisoned").clone();
        assert_eq!(observed.len(), 3);
    }

    #[test]
    fn first_block_in_registration_order_supplies_the_reason() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut registry = ExtensionRegistry::new();
        registry.register(Box::new(
            ScriptedExtension::new("early", &log).with_verdict(ExtensionVerdict::Block {
                reason: "early reason".to_string(),
            }),
        ));
        registry.register(Box::new(ScriptedExtension::new("late", &log).with_verdict(
            ExtensionVerdict::Block {
                reason: "late reason".to_string(),
            },
        )));

        assert_eq!(
            registry.on_assistant_text_delta("hello"),
            ExtensionVerdict::Block {
                reason: "early reason".to_string()
            }
        );
    }

    #[test]
    fn first_steer_wins_when_nothing_blocks() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut registry = ExtensionRegistry::new();
        registry.register(Box::new(ScriptedExtension::new("proceeds", &log)));
        registry.register(Box::new(
            ScriptedExtension::new("early", &log).with_verdict(ExtensionVerdict::Steer {
                message: "early message".to_string(),
            }),
        ));
        registry.register(Box::new(ScriptedExtension::new("late", &log).with_verdict(
            ExtensionVerdict::Steer {
                message: "late message".to_string(),
            },
        )));

        assert_eq!(
            registry.on_tool_call_planned(&tool_call_context()),
            ExtensionVerdict::Steer {
                message: "early message".to_string()
            }
        );
    }

    #[test]
    fn an_empty_verdict_text_isolates_that_tenant_only() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut registry = ExtensionRegistry::new();
        registry.register(Box::new(
            ScriptedExtension::new("broken", &log).with_verdict(ExtensionVerdict::Block {
                reason: "   ".to_string(),
            }),
        ));
        registry.register(Box::new(
            ScriptedExtension::new("healthy", &log).with_verdict(ExtensionVerdict::Steer {
                message: "keep going".to_string(),
            }),
        ));

        // The broken tenant's block is discarded; the healthy tenant still votes.
        assert_eq!(
            registry.on_tool_call_planned(&tool_call_context()),
            ExtensionVerdict::Steer {
                message: "keep going".to_string()
            }
        );

        let broken = registry.stats_for("broken").expect("broken tenant stats");
        assert_eq!(broken.errors, 1);
        assert_eq!(
            broken.last_error.as_deref(),
            Some("on_tool_call_planned returned an empty verdict text")
        );

        let healthy = registry.stats_for("healthy").expect("healthy tenant stats");
        assert_eq!(healthy.errors, 0);
        assert!(healthy.last_error.is_none());
    }

    #[test]
    fn isolated_tenant_keeps_receiving_hooks() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut registry = ExtensionRegistry::new();
        registry.register(Box::new(
            ScriptedExtension::new("broken", &log).with_verdict(ExtensionVerdict::Block {
                reason: String::new(),
            }),
        ));

        for _ in 0..3 {
            assert_eq!(
                registry.on_tool_call_planned(&tool_call_context()),
                ExtensionVerdict::Proceed
            );
        }

        let broken = registry.stats_for("broken").expect("broken tenant stats");
        assert_eq!(broken.errors, 3);
        assert_eq!(broken.calls, 3);
    }

    #[test]
    fn registry_counts_hook_invocations_per_tenant() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut registry = ExtensionRegistry::new();
        registry.register(Box::new(ScriptedExtension::new("first", &log)));
        registry.register(Box::new(ScriptedExtension::new("second", &log)));

        let _ = registry.on_tool_call_planned(&tool_call_context());
        let _ = registry.on_assistant_text_delta("hello");
        registry.on_turn_end(&TurnEndContext {
            turn_id: "turn-1".to_string(),
            tool_calls: 1,
            interrupted: false,
        });

        let stats = registry.stats();
        assert_eq!(stats.len(), 2);
        for tenant in stats {
            assert_eq!(tenant.calls, 3);
            assert_eq!(tenant.errors, 0);
        }
    }

    #[test]
    fn default_registry_registers_the_doom_loop_tenant() {
        let registry = ExtensionRegistry::with_default_tenants();
        assert_eq!(registry.names(), vec![doom_loop::DOOM_LOOP_EXTENSION]);
    }
}

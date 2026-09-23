//! Batch Tool Execution
//!
//! This module provides parallel execution of multiple tool calls.
//! It allows running multiple tools concurrently with configurable
//! limits on parallelism and proper error handling.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use futures::future::join_all;
use tokio::sync::{Semaphore, mpsc};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use super::details::BatchDetails;
use super::registry::ToolExecutor;
use crate::agent::{
    CredentialVault, ExecutionPhase, ExecutionSource, FromAgent, ToolExecution, ToolResult,
};

/// Bounded grace period `execute_with_cancel_at_generation` gives an
/// already-started call its own copy of `cancel_token` (see the per-call
/// `Some(cancel_token.clone())` forwarded below) to run its own
/// cancellation-aware cleanup to completion, before hard-abandoning it.
///
/// A bare `select! { .. => .., () = cancel_token.cancelled() => .. }` (or
/// `JoinSet::abort_all`) is not a fair race once the call holds its own
/// copy of the same token: `cancelled()` resolves in a single poll, while a
/// tool's own nested cleanup (e.g. `BashTool::execute_with_cancellation`
/// killing a process tree, then awaiting its reap) needs several polls
/// across real wall-clock time, so the bare cancellation path would win
/// almost every time and abandon that cleanup mid-way -- exactly the
/// `kill_on_drop`-kills-only-the-shell failure mode forwarding the token
/// alone does not fix. Mirrors `agent::native::race_with_cancellation_grace`
/// (same window, same rationale), duplicated here rather than shared
/// because that helper is private to `native.rs` and threaded through
/// `run_loop_cancellable`'s single top-level future, not this module's
/// per-call/per-task shape.
const BATCH_CANCELLATION_GRACE_PERIOD: std::time::Duration = std::time::Duration::from_secs(3);

/// A single tool call in a batch
#[derive(Debug, Clone)]
pub struct BatchToolCall {
    /// Unique identifier for this call
    pub call_id: String,
    /// Tool name to execute
    pub tool_name: String,
    /// Arguments for the tool
    pub args: serde_json::Value,
}

impl BatchToolCall {
    /// Create a new batch tool call
    pub fn new(
        call_id: impl Into<String>,
        tool_name: impl Into<String>,
        args: serde_json::Value,
    ) -> Self {
        Self {
            call_id: call_id.into(),
            tool_name: tool_name.into(),
            args,
        }
    }
}

/// Result of a single tool call in a batch
#[derive(Debug, Clone)]
pub struct BatchToolResult {
    /// Call ID that matches the input
    pub call_id: String,
    /// Tool name that was executed
    pub tool_name: String,
    /// The result from the tool
    pub result: ToolResult,
    /// Typed execution evidence when the batch path retained it.
    pub execution: Option<ToolExecution>,
}

impl BatchToolResult {
    fn from_execution(call_id: String, tool_name: String, execution: ToolExecution) -> Self {
        let result = execution.to_legacy();
        Self {
            call_id,
            tool_name,
            result,
            execution: Some(execution),
        }
    }

    fn cancelled(call_id: String, tool_name: String, phase: ExecutionPhase) -> Self {
        let execution = ToolExecution::cancelled(
            call_id.clone(),
            tool_name.clone(),
            ExecutionSource::Native,
            phase,
        )
        .with_managed_policy(crate::safety::managed_policy_metadata());
        Self::from_execution(call_id, tool_name, execution)
    }
}

/// Configuration for batch execution
#[derive(Debug, Clone)]
pub struct BatchConfig {
    /// Maximum number of concurrent tool executions
    pub max_concurrency: usize,
    /// Whether to continue executing remaining tools after a failure
    pub continue_on_error: bool,
    /// Whether to emit individual tool events
    pub emit_events: bool,
}

impl Default for BatchConfig {
    fn default() -> Self {
        Self {
            max_concurrency: 4,
            continue_on_error: true,
            emit_events: true,
        }
    }
}

impl BatchConfig {
    /// Create a new batch config with max concurrency
    #[must_use]
    pub fn with_concurrency(mut self, max: usize) -> Self {
        self.max_concurrency = max.max(1);
        self
    }

    /// Configure whether to continue after errors
    #[must_use]
    pub fn continue_on_error(mut self, cont: bool) -> Self {
        self.continue_on_error = cont;
        self
    }

    /// Configure event emission
    #[must_use]
    pub fn emit_events(mut self, emit: bool) -> Self {
        self.emit_events = emit;
        self
    }
}

/// Batch executor for running multiple tools in parallel
///
/// Note: The `BatchExecutor` shares one cached `ToolExecutor` across
/// validation and parallel execution tasks to avoid rebuilding registries on
/// every read-only wave member.
pub struct BatchExecutor {
    /// Configuration
    config: BatchConfig,
    /// Cached executor for validation (avoids repeated registry building)
    executor: Arc<ToolExecutor>,
}

impl BatchExecutor {
    /// Create a batch runner around an already-warm tool executor.
    ///
    /// Native agent read waves use this constructor so every call shares the
    /// session's registry, MCP state, Bash process bookkeeping, and result
    /// cache. Rebuilding a `ToolExecutor` for each wave is needlessly
    /// expensive and discards useful warm state.
    pub(crate) fn from_shared_executor(executor: Arc<ToolExecutor>, config: BatchConfig) -> Self {
        Self { config, executor }
    }

    /// Create a new batch executor with the given working directory
    pub fn new(cwd: impl Into<String>) -> Self {
        Self::with_credential_vault(cwd, CredentialVault::new())
    }

    /// Create a new batch executor using a caller-provided credential vault.
    pub fn with_credential_vault(
        cwd: impl Into<String>,
        credential_vault: CredentialVault,
    ) -> Self {
        let cwd = cwd.into();
        Self {
            executor: Arc::new(ToolExecutor::with_credential_vault(&cwd, credential_vault)),
            config: BatchConfig::default(),
        }
    }

    /// Create with custom configuration
    pub fn with_config(cwd: impl Into<String>, config: BatchConfig) -> Self {
        Self::with_config_and_credential_vault(cwd, config, CredentialVault::new())
    }

    /// Create a batch executor with custom configuration and a shared vault.
    pub fn with_config_and_credential_vault(
        cwd: impl Into<String>,
        config: BatchConfig,
        credential_vault: CredentialVault,
    ) -> Self {
        let cwd = cwd.into();
        Self {
            executor: Arc::new(ToolExecutor::with_credential_vault(&cwd, credential_vault)),
            config,
        }
    }

    /// Execute multiple tools in parallel
    ///
    /// Returns results in the same order as the input calls.
    /// Uses a semaphore to limit concurrency.
    pub async fn execute(
        &self,
        calls: Vec<BatchToolCall>,
        event_tx: Option<mpsc::UnboundedSender<FromAgent>>,
    ) -> Vec<BatchToolResult> {
        self.execute_at_generation(calls, event_tx, self.executor.credential_generation())
            .await
    }

    pub(crate) async fn execute_at_generation(
        &self,
        calls: Vec<BatchToolCall>,
        event_tx: Option<mpsc::UnboundedSender<FromAgent>>,
        generation: u64,
    ) -> Vec<BatchToolResult> {
        if calls.is_empty() {
            return Vec::new();
        }

        if !self.config.continue_on_error {
            if let Some(ref tx) = event_tx {
                if self.config.emit_events {
                    let _ = tx.send(FromAgent::BatchStart { total: calls.len() });
                }
            }

            let mut results = Vec::with_capacity(calls.len());
            let mut failed = false;

            for call in calls {
                if failed {
                    results.push(BatchToolResult {
                        call_id: call.call_id,
                        tool_name: call.tool_name,
                        result: ToolResult::failure("Skipped due to previous error in batch"),
                        execution: None,
                    });
                    continue;
                }

                let result = self
                    .executor
                    .execute_at_generation(
                        &call.tool_name,
                        &call.args,
                        if self.config.emit_events {
                            event_tx.as_ref()
                        } else {
                            None
                        },
                        &call.call_id,
                        generation,
                        None,
                    )
                    .await;

                if !result.success {
                    failed = true;
                }

                results.push(BatchToolResult {
                    call_id: call.call_id,
                    tool_name: call.tool_name,
                    result,
                    execution: None,
                });
            }

            if let Some(ref tx) = event_tx {
                if self.config.emit_events {
                    let successes = results.iter().filter(|r| r.result.success).count();
                    let failures = results.len() - successes;
                    let _ = tx.send(FromAgent::BatchEnd {
                        total: results.len(),
                        successes,
                        failures,
                    });
                }
            }

            return results;
        }

        // Send batch start event
        if let Some(ref tx) = event_tx {
            if self.config.emit_events {
                let _ = tx.send(FromAgent::BatchStart { total: calls.len() });
            }
        }

        let semaphore = Arc::new(Semaphore::new(self.config.max_concurrency));
        let mut handles = Vec::with_capacity(calls.len());

        for call in calls {
            let permit = semaphore.clone().acquire_owned().await.unwrap();
            let executor = Arc::clone(&self.executor);
            let event_tx_clone = event_tx.clone();
            let emit_events = self.config.emit_events;

            let call_id = call.call_id.clone();
            let tool_name = call.tool_name.clone();
            let args = call.args.clone();

            handles.push(tokio::spawn(async move {
                let result = executor
                    .execute_at_generation(
                        &tool_name,
                        &args,
                        if emit_events {
                            event_tx_clone.as_ref()
                        } else {
                            None
                        },
                        &call_id,
                        generation,
                        None,
                    )
                    .await;

                // Release permit when done
                drop(permit);

                BatchToolResult {
                    call_id,
                    tool_name,
                    result,
                    execution: None,
                }
            }));
        }

        // Wait for all tasks to complete
        let results: Vec<BatchToolResult> = join_all(handles)
            .await
            .into_iter()
            .filter_map(std::result::Result::ok)
            .collect();

        // Send batch end event
        if let Some(ref tx) = event_tx {
            if self.config.emit_events {
                let successes = results.iter().filter(|r| r.result.success).count();
                let failures = results.len() - successes;
                let _ = tx.send(FromAgent::BatchEnd {
                    total: results.len(),
                    successes,
                    failures,
                });
            }
        }

        results
    }

    /// Execute multiple tools in parallel and abort outstanding work when cancelled.
    ///
    /// Returns one result per requested call in input order. Calls that never
    /// complete because cancellation wins are surfaced as failed tool results so
    /// the caller can close out the batch cleanly.
    pub async fn execute_with_cancel(
        &self,
        calls: Vec<BatchToolCall>,
        event_tx: Option<mpsc::UnboundedSender<FromAgent>>,
        cancel_token: CancellationToken,
    ) -> Vec<BatchToolResult> {
        self.execute_with_cancel_at_generation(
            calls,
            event_tx,
            cancel_token,
            self.executor.credential_generation(),
        )
        .await
    }

    pub(crate) async fn execute_with_cancel_at_generation(
        &self,
        calls: Vec<BatchToolCall>,
        event_tx: Option<mpsc::UnboundedSender<FromAgent>>,
        cancel_token: CancellationToken,
        generation: u64,
    ) -> Vec<BatchToolResult> {
        let total = calls.len();
        if calls.is_empty() {
            return Vec::new();
        }

        if !self.config.continue_on_error {
            if let Some(ref tx) = event_tx {
                if self.config.emit_events {
                    let _ = tx.send(FromAgent::BatchStart { total });
                }
            }

            let mut results = Vec::with_capacity(total);
            let mut failed = false;
            let mut cancelled = false;

            for call in calls {
                if cancelled || cancel_token.is_cancelled() {
                    cancelled = true;
                    let result = BatchToolResult::cancelled(
                        call.call_id,
                        call.tool_name,
                        ExecutionPhase::Queued,
                    );
                    emit_batch_completion(event_tx.as_ref(), self.config.emit_events, &result);
                    results.push(result);
                    continue;
                }

                if failed {
                    results.push(BatchToolResult {
                        call_id: call.call_id,
                        tool_name: call.tool_name,
                        result: ToolResult::failure("Skipped due to previous error in batch"),
                        execution: None,
                    });
                    continue;
                }

                if self.config.emit_events {
                    if let Some(tx) = event_tx.as_ref() {
                        let _ = tx.send(FromAgent::ToolStart {
                            call_id: call.call_id.clone(),
                        });
                    }
                }
                // Forward `cancel_token` into the per-call `cancel` slot too
                // (not just this `select!`): a call whose own tool (bash,
                // web_fetch, an inline tool) never observes the token
                // relies solely on the abandoned future's `Drop` for
                // cleanup -- which for a child process only kills the
                // immediate shell (`kill_on_drop`), not any descendants it
                // spawned.
                //
                // `call_id`/`tool_name` are cloned up front (rather than
                // borrowed from `call`) so they can still be moved into the
                // result below after `call_execution` -- which borrows
                // `call.args` -- is done being polled.
                let call_id = call.call_id.clone();
                let tool_name = call.tool_name.clone();
                // Scoped so `call_execution` (which borrows `call_id`,
                // `tool_name`, and `call.args`) is dropped before they are
                // moved into `BatchToolResult::from_execution` below.
                let execution = {
                    let call_execution = self.executor.execute_with_receipt_at_generation(
                        &tool_name,
                        &call.args,
                        None,
                        &call_id,
                        generation,
                        Some(cancel_token.clone()),
                    );
                    tokio::pin!(call_execution);
                    tokio::select! {
                        execution = &mut call_execution => execution,
                        () = cancel_token.cancelled() => {
                            cancelled = true;
                            // See `BATCH_CANCELLATION_GRACE_PERIOD`: give
                            // the call's own cancellation-aware cleanup
                            // (now that it holds the same token) a bounded
                            // window to run to completion instead of
                            // dropping it -- and whatever cleanup it was
                            // mid-way through -- the instant `cancelled()`
                            // resolves.
                            match tokio::time::timeout(
                                BATCH_CANCELLATION_GRACE_PERIOD,
                                &mut call_execution,
                            )
                            .await
                            {
                                Ok(execution) => execution,
                                Err(_) => ToolExecution::cancelled(
                                    &call_id,
                                    &tool_name,
                                    ExecutionSource::Native,
                                    ExecutionPhase::Running,
                                )
                                .with_managed_policy(crate::safety::managed_policy_metadata()),
                            }
                        }
                    }
                };
                let result = BatchToolResult::from_execution(call_id, tool_name, execution);

                if !result.result.success {
                    failed = true;
                }
                emit_batch_completion(event_tx.as_ref(), self.config.emit_events, &result);
                results.push(result);
            }

            if let Some(ref tx) = event_tx {
                if self.config.emit_events {
                    let successes = results.iter().filter(|r| r.result.success).count();
                    let failures = results.len() - successes;
                    let _ = tx.send(FromAgent::BatchEnd {
                        total: results.len(),
                        successes,
                        failures,
                    });
                }
            }

            return results;
        }

        if let Some(ref tx) = event_tx {
            if self.config.emit_events {
                let _ = tx.send(FromAgent::BatchStart { total });
            }
        }

        let call_metadata: Vec<(String, String)> = calls
            .iter()
            .map(|call| (call.call_id.clone(), call.tool_name.clone()))
            .collect();
        let semaphore = Arc::new(Semaphore::new(self.config.max_concurrency));
        let mut task_set: JoinSet<(usize, BatchToolResult)> = JoinSet::new();
        let mut result_slots: Vec<Option<BatchToolResult>> =
            std::iter::repeat_with(|| None).take(total).collect();
        let mut started = vec![false; total];
        let mut cancelled = cancel_token.is_cancelled();

        if !cancelled {
            for (index, call) in calls.into_iter().enumerate() {
                let permit = tokio::select! {
                    permit = semaphore.clone().acquire_owned() => permit,
                    () = cancel_token.cancelled() => {
                        cancelled = true;
                        break;
                    }
                };

                let Ok(permit) = permit else {
                    cancelled = true;
                    break;
                };

                if self.config.emit_events {
                    if let Some(tx) = event_tx.as_ref() {
                        let _ = tx.send(FromAgent::ToolStart {
                            call_id: call.call_id.clone(),
                        });
                    }
                }
                started[index] = true;
                let executor = Arc::clone(&self.executor);
                let call_id = call.call_id;
                let tool_name = call.tool_name;
                let args = call.args;
                // Forward this call's own copy of the shared token so its
                // tool's cancellation-aware cleanup (process-tree kill,
                // etc.) has something to observe; see
                // `drain_with_grace_period_then_abort` for why the task set
                // gives it a bounded window to actually run before this
                // task is hard-aborted.
                let call_cancel_token = cancel_token.clone();

                task_set.spawn(async move {
                    let execution = executor
                        .execute_with_receipt_at_generation(
                            &tool_name,
                            &args,
                            None,
                            &call_id,
                            generation,
                            Some(call_cancel_token),
                        )
                        .await;

                    drop(permit);

                    (
                        index,
                        BatchToolResult::from_execution(call_id, tool_name, execution),
                    )
                });
            }
        }

        if cancelled {
            drain_with_grace_period_then_abort(&mut task_set, &mut result_slots).await;
        }

        while !task_set.is_empty() {
            if cancelled {
                record_joined_batch_result(task_set.join_next().await, &mut result_slots);
                continue;
            }

            tokio::select! {
                result = task_set.join_next() => {
                    record_joined_batch_result(result, &mut result_slots);
                }
                () = cancel_token.cancelled() => {
                    cancelled = true;
                    drain_with_grace_period_then_abort(&mut task_set, &mut result_slots).await;
                }
            }
        }

        for (index, slot) in result_slots.iter_mut().enumerate() {
            if slot.is_none() {
                let (call_id, tool_name) = &call_metadata[index];
                let phase = if started[index] {
                    ExecutionPhase::Running
                } else {
                    ExecutionPhase::Queued
                };
                *slot = Some(BatchToolResult::cancelled(
                    call_id.clone(),
                    tool_name.clone(),
                    phase,
                ));
            }
        }

        let results: Vec<BatchToolResult> = result_slots.into_iter().flatten().collect();

        if self.config.emit_events {
            for result in &results {
                emit_batch_completion(event_tx.as_ref(), true, result);
            }
        }

        if let Some(ref tx) = event_tx {
            if self.config.emit_events {
                let successes = results.iter().filter(|r| r.result.success).count();
                let failures = results.len() - successes;
                let _ = tx.send(FromAgent::BatchEnd {
                    total: results.len(),
                    successes,
                    failures,
                });
            }
        }

        results
    }

    /// Execute multiple tools in parallel and return detailed execution info
    ///
    /// Returns results along with batch-level details including timing,
    /// success rates, and per-tool durations.
    pub async fn execute_with_details(
        &self,
        calls: Vec<BatchToolCall>,
        event_tx: Option<mpsc::UnboundedSender<FromAgent>>,
    ) -> (Vec<BatchToolResult>, BatchDetails) {
        self.execute_with_details_at_generation(
            calls,
            event_tx,
            self.executor.credential_generation(),
        )
        .await
    }

    pub(crate) async fn execute_with_details_at_generation(
        &self,
        calls: Vec<BatchToolCall>,
        event_tx: Option<mpsc::UnboundedSender<FromAgent>>,
        generation: u64,
    ) -> (Vec<BatchToolResult>, BatchDetails) {
        let start_time = Instant::now();
        let total = calls.len();

        if calls.is_empty() {
            let details = BatchDetails::new(0)
                .with_results(0, 0)
                .with_duration(0)
                .with_concurrency(self.config.max_concurrency);
            return (Vec::new(), details);
        }

        if !self.config.continue_on_error {
            if let Some(ref tx) = event_tx {
                if self.config.emit_events {
                    let _ = tx.send(FromAgent::BatchStart { total });
                }
            }

            let mut results = Vec::with_capacity(total);
            let mut tool_durations = HashMap::new();
            let mut failed = false;

            for call in calls {
                if failed {
                    tool_durations.insert(call.call_id.clone(), 0);
                    results.push(BatchToolResult {
                        call_id: call.call_id,
                        tool_name: call.tool_name,
                        result: ToolResult::failure("Skipped due to previous error in batch"),
                        execution: None,
                    });
                    continue;
                }

                let tool_start = Instant::now();
                let result = self
                    .executor
                    .execute_at_generation(
                        &call.tool_name,
                        &call.args,
                        if self.config.emit_events {
                            event_tx.as_ref()
                        } else {
                            None
                        },
                        &call.call_id,
                        generation,
                        None,
                    )
                    .await;
                let duration_ms = tool_start.elapsed().as_millis() as u64;
                tool_durations.insert(call.call_id.clone(), duration_ms);

                if !result.success {
                    failed = true;
                }

                results.push(BatchToolResult {
                    call_id: call.call_id,
                    tool_name: call.tool_name,
                    result,
                    execution: None,
                });
            }

            let duration_ms = start_time.elapsed().as_millis() as u64;
            let successes = results.iter().filter(|r| r.result.success).count();
            let failures = results.len() - successes;

            let details = BatchDetails::new(total)
                .with_results(successes, failures)
                .with_duration(duration_ms)
                .with_concurrency(self.config.max_concurrency)
                .with_tool_durations(tool_durations)
                .with_backpressure_count(0);

            if let Some(ref tx) = event_tx {
                if self.config.emit_events {
                    let _ = tx.send(FromAgent::BatchEnd {
                        total: results.len(),
                        successes,
                        failures,
                    });
                }
            }

            return (results, details);
        }

        // Send batch start event
        if let Some(ref tx) = event_tx {
            if self.config.emit_events {
                let _ = tx.send(FromAgent::BatchStart { total });
            }
        }

        let semaphore = Arc::new(Semaphore::new(self.config.max_concurrency));
        let mut handles = Vec::with_capacity(calls.len());

        for call in calls {
            let permit = semaphore.clone().acquire_owned().await.unwrap();
            let executor = Arc::clone(&self.executor);
            let event_tx_clone = event_tx.clone();
            let emit_events = self.config.emit_events;

            let call_id = call.call_id.clone();
            let tool_name = call.tool_name.clone();
            let args = call.args.clone();

            handles.push(tokio::spawn(async move {
                let tool_start = Instant::now();
                let result = executor
                    .execute_at_generation(
                        &tool_name,
                        &args,
                        if emit_events {
                            event_tx_clone.as_ref()
                        } else {
                            None
                        },
                        &call_id,
                        generation,
                        None,
                    )
                    .await;

                let duration_ms = tool_start.elapsed().as_millis() as u64;

                // Release permit when done
                drop(permit);

                (
                    BatchToolResult {
                        call_id: call_id.clone(),
                        tool_name,
                        result,
                        execution: None,
                    },
                    call_id,
                    duration_ms,
                )
            }));
        }

        // Wait for all tasks to complete
        let task_results: Vec<_> = join_all(handles)
            .await
            .into_iter()
            .filter_map(std::result::Result::ok)
            .collect();

        // Separate results from durations
        let executor_reuse_count = task_results.len();
        let mut results = Vec::with_capacity(task_results.len());
        let mut tool_durations = HashMap::new();

        for (result, call_id, duration) in task_results {
            tool_durations.insert(call_id, duration);
            results.push(result);
        }

        // Calculate stats
        let duration_ms = start_time.elapsed().as_millis() as u64;
        let successes = results.iter().filter(|r| r.result.success).count();
        let failures = results.len() - successes;
        let backpressure_count = total.saturating_sub(self.config.max_concurrency);

        // Build details
        let mut details = BatchDetails::new(total)
            .with_results(successes, failures)
            .with_duration(duration_ms)
            .with_concurrency(self.config.max_concurrency)
            .with_tool_durations(tool_durations)
            .with_executor_reuse_count(executor_reuse_count)
            .with_backpressure_count(backpressure_count);

        if self.config.continue_on_error {
            details = details.with_continue_on_error();
        }

        // Send batch end event
        if let Some(ref tx) = event_tx {
            if self.config.emit_events {
                let _ = tx.send(FromAgent::BatchEnd {
                    total: results.len(),
                    successes,
                    failures,
                });
            }
        }

        (results, details)
    }

    /// Execute tools sequentially (useful for dependent operations)
    pub async fn execute_sequential(
        &self,
        calls: Vec<BatchToolCall>,
        event_tx: Option<mpsc::UnboundedSender<FromAgent>>,
    ) -> Vec<BatchToolResult> {
        self.execute_sequential_at_generation(
            calls,
            event_tx,
            self.executor.credential_generation(),
        )
        .await
    }

    pub(crate) async fn execute_sequential_at_generation(
        &self,
        calls: Vec<BatchToolCall>,
        event_tx: Option<mpsc::UnboundedSender<FromAgent>>,
        generation: u64,
    ) -> Vec<BatchToolResult> {
        let mut results = Vec::with_capacity(calls.len());
        let mut failed = false;

        for call in calls {
            if failed {
                results.push(BatchToolResult {
                    call_id: call.call_id,
                    tool_name: call.tool_name,
                    result: ToolResult::failure("Skipped due to previous error in batch"),
                    execution: None,
                });
                continue;
            }

            let result = self
                .executor
                .execute_at_generation(
                    &call.tool_name,
                    &call.args,
                    event_tx.as_ref(),
                    &call.call_id,
                    generation,
                    None,
                )
                .await;

            let success = result.success;
            results.push(BatchToolResult {
                call_id: call.call_id,
                tool_name: call.tool_name,
                result,
                execution: None,
            });

            // Stop on first error if configured
            if !success && !self.config.continue_on_error {
                failed = true;
            }
        }

        results
    }

    /// Check which tools require approval
    pub fn check_approvals(&self, calls: &[BatchToolCall]) -> Vec<(String, bool)> {
        calls
            .iter()
            .map(|call| {
                let needs_approval = self.executor.requires_approval(&call.tool_name, &call.args);
                (call.call_id.clone(), needs_approval)
            })
            .collect()
    }

    /// Filter calls that require approval
    pub fn filter_needs_approval<'a>(&self, calls: &'a [BatchToolCall]) -> Vec<&'a BatchToolCall> {
        calls
            .iter()
            .filter(|call| self.executor.requires_approval(&call.tool_name, &call.args))
            .collect()
    }

    /// Validate all calls and return any with missing required fields
    pub fn validate_calls(&self, calls: &[BatchToolCall]) -> HashMap<String, Vec<String>> {
        let mut errors = HashMap::new();

        for call in calls {
            let missing = self.executor.missing_required(&call.tool_name, &call.args);
            if !missing.is_empty() {
                errors.insert(call.call_id.clone(), missing);
            }
        }

        errors
    }
}

fn record_joined_batch_result(
    result: Option<Result<(usize, BatchToolResult), tokio::task::JoinError>>,
    result_slots: &mut [Option<BatchToolResult>],
) {
    if let Some(Ok((index, result))) = result {
        if let Some(slot) = result_slots.get_mut(index) {
            *slot = Some(result);
        }
    }
}

/// Drain already-spawned tasks for up to [`BATCH_CANCELLATION_GRACE_PERIOD`]
/// before hard-aborting whatever is left.
///
/// `JoinSet::abort_all` drops each task's future in place without executing
/// any more of its own logic -- including a cancelled bash/inline call's own
/// branch body that calls `kill_process_tree` -- so aborting the instant
/// cancellation fires would defeat forwarding `cancel_token` into each
/// spawned call's own `cancel` slot (see the call sites above): the task
/// would never get a chance to run that cleanup at all. Every remaining task
/// already holds its own copy of the same token (it is what triggered this
/// drain), so within the grace window each one's own cancellation-aware
/// cleanup can finish and be recorded normally through `join_next`; only
/// whatever is still running once the deadline passes gets hard-aborted.
async fn drain_with_grace_period_then_abort(
    task_set: &mut JoinSet<(usize, BatchToolResult)>,
    result_slots: &mut [Option<BatchToolResult>],
) {
    let grace_deadline = tokio::time::Instant::now() + BATCH_CANCELLATION_GRACE_PERIOD;
    while !task_set.is_empty() {
        match tokio::time::timeout_at(grace_deadline, task_set.join_next()).await {
            Ok(result) => record_joined_batch_result(result, result_slots),
            Err(_) => break,
        }
    }
    task_set.abort_all();
}

fn emit_batch_completion(
    event_tx: Option<&mpsc::UnboundedSender<FromAgent>>,
    emit_events: bool,
    result: &BatchToolResult,
) {
    if !emit_events {
        return;
    }
    let Some(tx) = event_tx else {
        return;
    };

    if !result.result.output.is_empty() {
        let _ = tx.send(FromAgent::ToolOutput {
            call_id: result.call_id.clone(),
            content: result.result.output.clone(),
        });
    }
    let _ = tx.send(FromAgent::ToolEnd {
        call_id: result.call_id.clone(),
        success: result.result.success,
        result: Some(result.result.clone()),
        receipt: result
            .execution
            .as_ref()
            .map(|execution| execution.receipt.clone()),
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_batch_tool_call_new() {
        let call = BatchToolCall::new("id1", "read", json!({"file_path": "/test.txt"}));
        assert_eq!(call.call_id, "id1");
        assert_eq!(call.tool_name, "read");
    }

    #[test]
    fn test_batch_config_default() {
        let config = BatchConfig::default();
        assert_eq!(config.max_concurrency, 4);
        assert!(config.continue_on_error);
        assert!(config.emit_events);
    }

    #[test]
    fn test_batch_config_builder() {
        let config = BatchConfig::default()
            .with_concurrency(8)
            .continue_on_error(false)
            .emit_events(false);

        assert_eq!(config.max_concurrency, 8);
        assert!(!config.continue_on_error);
        assert!(!config.emit_events);
    }

    #[test]
    fn test_batch_config_min_concurrency() {
        let config = BatchConfig::default().with_concurrency(0);
        assert_eq!(config.max_concurrency, 1); // Minimum is 1
    }

    #[tokio::test]
    async fn test_batch_executor_empty() {
        let batch = BatchExecutor::new("/tmp");

        let results = batch.execute(vec![], None).await;
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn batch_executor_uses_the_injected_credential_vault() {
        let dir = tempfile::tempdir().unwrap();
        let vault = CredentialVault::new();
        let batch =
            BatchExecutor::with_credential_vault(dir.path().to_str().unwrap(), vault.clone());
        let token = ["ghs", "123456789012345678901234567890123456"].join("_");
        let calls = vec![BatchToolCall::new(
            "call-1",
            "bash",
            json!({"command": format!("echo GITHUB_TOKEN={token}")}),
        )];

        let results = batch.execute(calls, None).await;

        assert_eq!(results.len(), 1);
        assert!(results[0].result.output.contains("{{CRED|"));
        assert!(
            vault
                .resolve_all(&results[0].result.output)
                .contains(&token)
        );
    }

    #[tokio::test]
    async fn batch_rotation_does_not_vault_queued_old_session_output() {
        let vault = CredentialVault::new();
        let batch = BatchExecutor::with_config_and_credential_vault(
            "/tmp",
            BatchConfig::default().with_concurrency(1),
            vault.clone(),
        );
        let token = ["ghp", "123456789012345678901234567890123456"].join("_");
        let (tx, mut rx) = mpsc::unbounded_channel();
        let run = batch.execute(
            vec![
                BatchToolCall::new("blocking", "bash", json!({"command": "sleep 0.2"})),
                BatchToolCall::new(
                    "stale",
                    "bash",
                    json!({"command": format!("printf 'GITHUB_TOKEN={token}'")}),
                ),
            ],
            Some(tx),
        );
        tokio::pin!(run);

        loop {
            tokio::select! {
                results = &mut run => panic!("batch completed before rotation: {results:?}"),
                event = rx.recv() => match event {
                    Some(FromAgent::ToolStart { call_id }) if call_id == "blocking" => break,
                    Some(_) => {}
                    None => panic!("batch closed the event channel before starting the tool"),
                }
            }
        }
        vault.clear();

        let results = run.await;
        let stale = results
            .iter()
            .find(|result| result.call_id == "stale")
            .expect("queued call should complete");
        assert!(
            stale
                .result
                .output
                .contains("[REDACTED:api_key:portable-export]")
        );
        assert!(!stale.result.output.contains(&token));
        assert_eq!(vault.stats().count, 0);
    }

    #[test]
    fn test_batch_executor_check_approvals() {
        let batch = BatchExecutor::new("/tmp");

        let calls = vec![
            BatchToolCall::new("1", "read", json!({"file_path": "/test.txt"})),
            BatchToolCall::new(
                "2",
                "write",
                json!({"file_path": "/test.txt", "content": "x"}),
            ),
        ];

        let approvals = batch.check_approvals(&calls);
        assert_eq!(approvals.len(), 2);

        // Read doesn't need approval
        assert!(!approvals.iter().find(|(id, _)| id == "1").unwrap().1);
        // Write needs approval
        assert!(approvals.iter().find(|(id, _)| id == "2").unwrap().1);
    }

    #[test]
    fn test_batch_executor_validate_calls() {
        let batch = BatchExecutor::new("/tmp");

        let calls = vec![
            BatchToolCall::new("1", "read", json!({})), // Missing file_path
            BatchToolCall::new("2", "read", json!({"file_path": "/test.txt"})), // Valid
        ];

        let errors = batch.validate_calls(&calls);
        assert!(errors.contains_key("1"));
        assert!(!errors.contains_key("2"));
    }

    #[test]
    fn test_batch_executor_filter_needs_approval() {
        let batch = BatchExecutor::new("/tmp");

        let calls = vec![
            BatchToolCall::new("1", "read", json!({"file_path": "/test.txt"})),
            BatchToolCall::new(
                "2",
                "write",
                json!({"file_path": "/test.txt", "content": "x"}),
            ),
            BatchToolCall::new("3", "glob", json!({"pattern": "*.rs"})),
        ];

        let needs_approval = batch.filter_needs_approval(&calls);
        assert_eq!(needs_approval.len(), 1);
        assert_eq!(needs_approval[0].call_id, "2");
    }

    #[tokio::test]
    async fn test_batch_executor_with_details_empty() {
        let batch = BatchExecutor::new("/tmp");

        let (results, details) = batch.execute_with_details(vec![], None).await;
        assert!(results.is_empty());
        assert_eq!(details.total, 0);
        assert_eq!(details.successes, 0);
        assert_eq!(details.failures, 0);
        assert!(details.duration_ms.is_some());
    }

    #[tokio::test]
    async fn test_batch_executor_with_details_tracks_timing() {
        let batch = BatchExecutor::new("/tmp");

        // Use glob which should succeed quickly
        let calls = vec![BatchToolCall::new("1", "glob", json!({"pattern": "*.rs"}))];

        let (results, details) = batch.execute_with_details(calls, None).await;

        assert_eq!(results.len(), 1);
        assert_eq!(details.total, 1);
        assert!(details.duration_ms.is_some());

        // Should have tool durations
        let tool_durations = details.tool_durations.as_ref().unwrap();
        assert!(tool_durations.contains_key("1"));
    }

    #[tokio::test]
    async fn test_batch_executor_reuses_cached_executor_for_parallel_tasks() {
        let config = BatchConfig::default()
            .with_concurrency(2)
            .emit_events(false);
        let batch = BatchExecutor::with_config("/tmp", config);
        let calls = vec![
            BatchToolCall::new("1", "glob", json!({"pattern": "*.rs"})),
            BatchToolCall::new("2", "glob", json!({"pattern": "*.toml"})),
        ];

        let (results, details) = batch.execute_with_details(calls, None).await;

        assert_eq!(results.len(), 2);
        assert_eq!(results[0].call_id, "1");
        assert_eq!(results[1].call_id, "2");
        assert_eq!(details.executor_reuse_count, Some(2));
        assert_eq!(details.backpressure_count, Some(0));
    }

    #[tokio::test]
    async fn test_batch_executor_reports_backpressure_count() {
        let config = BatchConfig::default()
            .with_concurrency(2)
            .emit_events(false);
        let batch = BatchExecutor::with_config("/tmp", config);
        let calls = vec![
            BatchToolCall::new("1", "glob", json!({"pattern": "*.rs"})),
            BatchToolCall::new("2", "glob", json!({"pattern": "*.toml"})),
            BatchToolCall::new("3", "glob", json!({"pattern": "*.md"})),
            BatchToolCall::new("4", "glob", json!({"pattern": "*.json"})),
        ];

        let (results, details) = batch.execute_with_details(calls, None).await;

        assert_eq!(results.len(), 4);
        assert_eq!(details.max_concurrency, Some(2));
        assert_eq!(details.backpressure_count, Some(2));
    }

    #[tokio::test]
    async fn test_batch_executor_cancel_closes_batch_with_failures() {
        let config = BatchConfig::default().with_concurrency(2);
        let batch = BatchExecutor::with_config("/tmp", config);
        let calls = vec![
            BatchToolCall::new("1", "glob", json!({"pattern": "*.rs"})),
            BatchToolCall::new("2", "glob", json!({"pattern": "*.toml"})),
            BatchToolCall::new("3", "glob", json!({"pattern": "*.json"})),
        ];
        let cancel_token = CancellationToken::new();
        cancel_token.cancel();
        let (tx, mut rx) = mpsc::unbounded_channel();

        let results = batch
            .execute_with_cancel(calls, Some(tx), cancel_token)
            .await;

        assert_eq!(results.len(), 3);
        assert!(results.iter().all(|result| !result.result.success));
        assert!(results.iter().all(|result| {
            result
                .result
                .error
                .as_deref()
                .is_some_and(|error| error.contains("cancelled"))
        }));
        assert!(results.iter().all(|result| matches!(
            result
                .execution
                .as_ref()
                .map(|execution| &execution.outcome),
            Some(crate::agent::ToolOutcome::Cancelled {
                phase: ExecutionPhase::Queued
            })
        )));

        let mut saw_start = false;
        let mut saw_end = false;
        let mut terminal_receipts = 0;
        while let Ok(event) = rx.try_recv() {
            match event {
                FromAgent::BatchStart { total } => {
                    assert_eq!(total, 3);
                    saw_start = true;
                }
                FromAgent::BatchEnd {
                    total,
                    successes,
                    failures,
                } => {
                    assert_eq!(total, 3);
                    assert_eq!(successes, 0);
                    assert_eq!(failures, 3);
                    saw_end = true;
                }
                FromAgent::ToolEnd {
                    success: false,
                    receipt: Some(receipt),
                    ..
                } if matches!(&receipt.details, crate::agent::ToolReceiptDetails::None) => {
                    terminal_receipts += 1;
                }
                _ => {}
            }
        }

        assert!(saw_start);
        assert!(saw_end);
        assert_eq!(terminal_receipts, 3);
    }

    #[test]
    fn cancelled_batch_result_retains_execution_phase() {
        let queued = BatchToolResult::cancelled(
            "queued".to_string(),
            "glob".to_string(),
            ExecutionPhase::Queued,
        );
        let running = BatchToolResult::cancelled(
            "running".to_string(),
            "glob".to_string(),
            ExecutionPhase::Running,
        );

        assert!(matches!(
            queued
                .execution
                .as_ref()
                .map(|execution| &execution.outcome),
            Some(crate::agent::ToolOutcome::Cancelled {
                phase: ExecutionPhase::Queued
            })
        ));
        assert!(matches!(
            running
                .execution
                .as_ref()
                .map(|execution| &execution.outcome),
            Some(crate::agent::ToolOutcome::Cancelled {
                phase: ExecutionPhase::Running
            })
        ));
    }

    #[tokio::test]
    async fn cancelling_started_batch_tool_lets_bash_own_cancellation_cleanup_win() {
        let batch = BatchExecutor::with_config("/tmp", BatchConfig::default().with_concurrency(1));
        let cancel_token = CancellationToken::new();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let run = batch.execute_with_cancel(
            vec![BatchToolCall::new(
                "running",
                "bash",
                json!({"command": "sleep 30"}),
            )],
            Some(tx),
            cancel_token.clone(),
        );
        tokio::pin!(run);

        loop {
            tokio::select! {
                result = &mut run => panic!("batch completed before cancellation: {result:?}"),
                event = rx.recv() => match event {
                    Some(FromAgent::ToolStart { call_id }) if call_id == "running" => break,
                    Some(_) => {}
                    None => panic!("batch closed the event channel before starting the tool"),
                }
            }
        }
        cancel_token.cancel();
        let results = tokio::time::timeout(std::time::Duration::from_secs(5), run)
            .await
            .expect("cancellation must resolve well within its own grace period");

        // Before forwarding `cancel_token` into the per-call `cancel` slot
        // (and giving it a grace period to actually run), this always came
        // back as the generic `ToolOutcome::Cancelled { phase: Running }`
        // placeholder -- `BashTool::execute_with_cancellation` never saw the
        // token at all, so it could never run its own process-tree kill
        // before the batch executor gave up on it. Now bash's own
        // cancellation-aware branch has a real, bounded window to run to
        // completion, and its specific "Command cancelled" result (not a
        // generic placeholder) is what gets surfaced.
        assert!(
            !results[0].result.success,
            "cancelled bash call must not report success: {:?}",
            results[0].result
        );
        assert_eq!(
            results[0].result.error.as_deref(),
            Some("Command cancelled"),
            "the batch result must be bash's own cancellation-aware outcome, not a generic placeholder: {:?}",
            results[0].result
        );
        let events: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, FromAgent::ToolEnd { .. }))
                .count(),
            1
        );
    }

    /// Same fix, exercised through the *sequential*
    /// (`continue_on_error(false)`) branch of `execute_with_cancel_at_generation`,
    /// which is a materially different code path (no `JoinSet`, one call at
    /// a time via a plain `select!`) with its own separate grace-period
    /// handling.
    #[tokio::test]
    async fn cancelling_sequential_batch_tool_lets_bash_own_cancellation_cleanup_win() {
        let batch =
            BatchExecutor::with_config("/tmp", BatchConfig::default().continue_on_error(false));
        let cancel_token = CancellationToken::new();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let run = batch.execute_with_cancel(
            vec![BatchToolCall::new(
                "running",
                "bash",
                json!({"command": "sleep 30"}),
            )],
            Some(tx),
            cancel_token.clone(),
        );
        tokio::pin!(run);

        loop {
            tokio::select! {
                result = &mut run => panic!("batch completed before cancellation: {result:?}"),
                event = rx.recv() => match event {
                    Some(FromAgent::ToolStart { call_id }) if call_id == "running" => break,
                    Some(_) => {}
                    None => panic!("batch closed the event channel before starting the tool"),
                }
            }
        }
        cancel_token.cancel();
        let results = tokio::time::timeout(std::time::Duration::from_secs(5), run)
            .await
            .expect("cancellation must resolve well within its own grace period");

        assert!(
            !results[0].result.success,
            "cancelled bash call must not report success: {:?}",
            results[0].result
        );
        assert_eq!(
            results[0].result.error.as_deref(),
            Some("Command cancelled"),
            "the batch result must be bash's own cancellation-aware outcome, not a generic placeholder: {:?}",
            results[0].result
        );
    }

    #[tokio::test]
    async fn test_cancelled_batch_drain_preserves_completed_results() {
        let mut task_set = JoinSet::new();
        task_set.spawn(async {
            (
                0,
                BatchToolResult {
                    call_id: "1".to_string(),
                    tool_name: "glob".to_string(),
                    result: ToolResult::success("completed before cancellation"),
                    execution: None,
                },
            )
        });

        let mut result_slots = vec![None];
        record_joined_batch_result(task_set.join_next().await, &mut result_slots);

        let result = result_slots.into_iter().next().flatten().unwrap();
        assert_eq!(result.call_id, "1");
        assert!(result.result.success);
        assert_eq!(result.result.output, "completed before cancellation");
    }

    #[tokio::test]
    async fn test_batch_executor_with_details_config() {
        let config = BatchConfig::default()
            .with_concurrency(2)
            .continue_on_error(true);
        let batch = BatchExecutor::with_config("/tmp", config);

        let (_, details) = batch.execute_with_details(vec![], None).await;

        assert_eq!(details.max_concurrency, Some(2));
        // Empty batch doesn't set continue_on_error since it's only set on non-empty
    }
}

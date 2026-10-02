//! The sandbox proposes calls; the existing Dex ports retain all authority.
//! The wrapper itself uses the mutation ledger, so a resumed script never
//! replays effects, including when a crash loses its final projected output.

use futures_util::{StreamExt, stream::FuturesUnordered};
use serde_json::Value;

use super::*;
use crate::Output;

pub(super) fn spec() -> ToolSpec {
    ToolSpec {
        name: ToolName::new(agent_codemode::TOOL_NAME),
        label: "Composing tools".into(),
        description: agent_codemode::DESCRIPTION.into(),
        schema: agent_codemode::schema(),
        // Scripts can contain mutations. Never prefetch or rerun a wrapper.
        read_only: false,
        core: true,
        governance: GovernanceClass::Plain,
        executor: ExecutorKind::InProcess,
    }
}

impl<L, M, T, E, S, C> Engine<L, M, T, E, S, C>
where
    L: Log,
    M: Model,
    T: Tools,
    E: Effects,
    S: Sanitizer,
    C: Compactor,
{
    pub(super) async fn run_codemode(
        &self,
        ctx: &mut Context,
        parent: &ProposedCall,
        cancel: &CancellationToken,
        run_started: Instant,
    ) -> Result<ToolResult, Fenced> {
        let Some(code) = parent.args.get("code").and_then(Value::as_str) else {
            return Ok(ToolResult::error("invalid call: code must be a string"));
        };
        // Discovery follows normal tools.search exposure. Client and User
        // executors need an ordinary conversational call, not a VM continuation.
        let catalog = self
            .offered(ctx)
            .into_iter()
            .filter(|entry| {
                !matches!(entry.executor, ExecutorKind::Client | ExecutorKind::User)
                    && entry.name.as_str() != agent_codemode::TOOL_NAME
                    && entry.name.as_str() != TOOLS_SEARCH
            })
            .map(|entry| agent_codemode::Tool {
                name: entry.name.to_string(),
                description: entry.description,
                schema: entry.schema,
            })
            .collect();
        let deadline = self
            .call_expires_at(run_started)
            .min(Instant::now() + Duration::from_secs(60));
        let mut session = agent_codemode::Session::start(
            code.to_owned(),
            catalog,
            cancel,
            deadline.saturating_duration_since(Instant::now()),
        );
        let mut uncertain = false;
        while let Some(event) = session.next().await {
            match event {
                agent_codemode::Event::Done(report) => {
                    let content = report.content();
                    // A script may catch an error but cannot turn an uncertain
                    // effect into success. Keep that state in the outer ledger.
                    return Ok(if uncertain {
                        ToolResult::unknown(content)
                    } else if report.error.is_some() {
                        ToolResult::error(content)
                    } else {
                        ToolResult::text(content)
                    });
                }
                agent_codemode::Event::Calls { calls, reply } => {
                    let proposals: Vec<_> = calls
                        .iter()
                        .map(|request| {
                            ProposedCall::new(
                                CallId::new(format!(
                                    "{}:codemode:{}",
                                    parent.id.as_str(),
                                    request.index
                                )),
                                ToolName::new(&request.name),
                                request.args.clone(),
                                parent.principal.clone(),
                            )
                        })
                        .collect();
                    self.emit(
                        ctx,
                        vec![Event::CodeModeCallsProposed {
                            parent: parent.id.clone(),
                            calls: proposals.clone(),
                        }],
                    )
                    .await?;
                    let mut responses = Vec::new();
                    let mut reads = Vec::new();
                    for (request, call) in calls.iter().zip(&proposals) {
                        let Some(entry) = self.offered_spec(ctx, &call.tool).filter(|entry| {
                            entry.name.as_str() != agent_codemode::TOOL_NAME
                                && entry.name.as_str() != TOOLS_SEARCH
                                && !matches!(
                                    entry.executor,
                                    ExecutorKind::Client | ExecutorKind::User
                                )
                        }) else {
                            let result = ToolResult::error(
                                "unavailable in codemode; call conversational tools directly",
                            );
                            self.finish(ctx, call, result.clone()).await?;
                            responses.push((
                                request.index,
                                self.codemode_response(ctx, call, result, cancel, deadline)
                                    .await,
                            ));
                            continue;
                        };
                        // Reads before an effect complete first. This also lets
                        // owner policy consult evidence from the previous wave.
                        if !entry.read_only {
                            self.codemode_reads(ctx, &mut reads, &mut responses, cancel, deadline)
                                .await?;
                        }
                        let refusal = if cancel.is_cancelled() || Instant::now() >= deadline {
                            Some(ToolResult::error(NOT_RUN_INTERRUPTED))
                        } else if let Err(reason) = validate_args(&entry, &call.args) {
                            Some(ToolResult::error(reason))
                        } else if ctx.has_stalled_call(call) {
                            Some(ToolResult::error(
                                "not executed: the identical call failed three times without progress; change the inputs or approach, or report the blocker",
                            ))
                        } else if !entry.read_only && ctx.has_uncertain_call(call) {
                            Some(ToolResult::error(UNCERTAIN_REPEAT))
                        } else {
                            None
                        };
                        if let Some(result) = refusal {
                            self.finish(ctx, call, result.clone()).await?;
                            responses.push((
                                request.index,
                                self.codemode_response(ctx, call, result, cancel, deadline)
                                    .await,
                            ));
                            continue;
                        }
                        let verdict = tokio::select! {
                            biased;
                            () = cancel.cancelled() => Verdict::Deny(NOT_RUN_INTERRUPTED.into()),
                            () = tokio::time::sleep_until(deadline) => Verdict::Deny(NOT_RUN_WALL.into()),
                            verdict = self.tools.policy(ctx, call) => verdict,
                        };
                        let refused = match verdict {
                            Verdict::Deny(reason) => {
                                Some(ToolResult::error(format!("denied: {reason}")))
                            }
                            // Confirmation has to bind an ordinary proposal and
                            // answer. A script cannot manufacture that transition.
                            Verdict::NeedsConfirmation { .. } => Some(ToolResult::error(
                                "needs confirmation: call this tool directly in conversation",
                            )),
                            Verdict::NeedsApproval { approval, summary } => {
                                self.auto_approve(ctx, call, approval, summary).await?;
                                None
                            }
                            Verdict::Confirmed { approval, summary } => {
                                self.record_grant(
                                    ctx,
                                    call,
                                    approval,
                                    summary,
                                    call.principal.clone(),
                                )
                                .await?;
                                None
                            }
                            Verdict::Allow => None,
                        };
                        if let Some(result) = refused {
                            self.finish(ctx, call, result.clone()).await?;
                            responses.push((
                                request.index,
                                self.codemode_response(ctx, call, result, cancel, deadline)
                                    .await,
                            ));
                        } else if entry.read_only {
                            reads.push((request.index, call.clone(), entry));
                        } else {
                            let result = self
                                .codemode_mutation(ctx, call, &entry, cancel, deadline)
                                .await?;
                            uncertain |= result.outcome == Outcome::Unknown;
                            responses.push((
                                request.index,
                                self.codemode_response(ctx, call, result, cancel, deadline)
                                    .await,
                            ));
                        }
                    }
                    self.codemode_reads(ctx, &mut reads, &mut responses, cancel, deadline)
                        .await?;
                    // A cancelled VM may have left while admitted effects were
                    // settling. Their durable outcomes were still recorded above.
                    let _ = reply.send(responses);
                }
            }
        }
        Ok(ToolResult::unknown(
            "script ended before its final result was recorded",
        ))
    }

    async fn codemode_response(
        &self,
        ctx: &Context,
        call: &ProposedCall,
        result: ToolResult,
        cancel: &CancellationToken,
        deadline: Instant,
    ) -> Result<Value, String> {
        if result.outcome != Outcome::Succeeded {
            return response(result);
        }
        let resolved = tokio::select! {
            biased;
            () = cancel.cancelled() => return Err(READ_INTERRUPTED.into()),
            () = tokio::time::sleep_until(deadline) => return Err(DEADLINE_READ.into()),
            result = self.tools.resolve_codemode_result(ctx, call, &result, 256 * 1024) => result?,
        };
        response(resolved)
    }

    async fn codemode_reads(
        &self,
        ctx: &mut Context,
        reads: &mut Vec<(usize, ProposedCall, ToolSpec)>,
        responses: &mut agent_codemode::Reply,
        cancel: &CancellationToken,
        deadline: Instant,
    ) -> Result<(), Fenced> {
        let reads = std::mem::take(reads);
        if reads.is_empty() {
            return Ok(());
        }
        self.emit(
            ctx,
            reads
                .iter()
                .map(|(_, call, entry)| started(call, entry))
                .collect(),
        )
        .await?;
        let mut pending = FuturesUnordered::new();
        let thread = ctx.thread().clone();
        for (index, call, _) in reads {
            let thread = thread.clone();
            pending.push(async move {
                let result = tokio::select! {
                    biased;
                    () = cancel.cancelled() => ToolResult::error(READ_INTERRUPTED),
                    () = tokio::time::sleep_until(deadline) => ToolResult::error(DEADLINE_READ),
                    result = self.tools.run(&thread, &call, cancel) => result,
                };
                (index, call, result)
            });
        }
        while let Some((index, call, result)) = pending.next().await {
            self.finish(ctx, &call, result.clone()).await?;
            responses.push((
                index,
                self.codemode_response(ctx, &call, result, cancel, deadline)
                    .await,
            ));
        }
        Ok(())
    }

    async fn codemode_mutation(
        &self,
        ctx: &mut Context,
        call: &ProposedCall,
        entry: &ToolSpec,
        cancel: &CancellationToken,
        deadline: Instant,
    ) -> Result<ToolResult, Fenced> {
        let result = match self.effects.claim(call).await? {
            Claim::Existing(result) => self.settle_claim(&call.id, result).await?,
            Claim::Granted => {
                let result = if cancel.is_cancelled() || Instant::now() >= deadline {
                    ToolResult::error(NOT_RUN_WALL)
                } else {
                    self.emit(ctx, vec![started(call, entry)]).await?;
                    // Interrupt reaches the executor; a mutation settles even
                    // if the VM has stopped, preserving its claim and receipt.
                    match tokio::time::timeout_at(
                        deadline,
                        self.tools.run(ctx.thread(), call, cancel),
                    )
                    .await
                    {
                        Ok(result) => result,
                        Err(_) => ToolResult::unknown(DEADLINE_MUTATION),
                    }
                };
                self.effects.record(&call.id, &result).await?;
                result
            }
        };
        self.finish(ctx, call, result.clone()).await?;
        Ok(result)
    }
}

fn response(result: ToolResult) -> Result<Value, String> {
    let value = match result.output {
        Output::Text(text) => serde_json::from_str(&text).unwrap_or(Value::String(text)),
        Output::Ref(reference) => serde_json::json!({"output_ref": reference.as_str()}),
    };
    if result.outcome == Outcome::Succeeded {
        Ok(value)
    } else {
        Err(value.to_string())
    }
}

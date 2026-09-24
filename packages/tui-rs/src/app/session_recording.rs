use super::*;
use crate::agent::{ExecutionStatus, ToolExecution};

fn session_contains_entry_id(path: &std::path::Path, id: &str) -> Result<bool> {
    let raw = std::fs::read_to_string(path)?;
    for line in raw.lines().filter(|line| !line.trim().is_empty()) {
        let entry: SessionEntry = serde_json::from_str(line)?;
        if matches!(entry, SessionEntry::Custom(custom) if custom.id.as_deref() == Some(id)) {
            return Ok(true);
        }
    }
    Ok(false)
}

fn read_request_cache(
    path: &std::path::Path,
) -> Option<maestro_context::token_counting::RequestCacheSnapshot> {
    use std::io::BufRead;
    let file = std::fs::File::open(path).ok()?;
    let mut latest = None;
    for line in std::io::BufReader::new(file).lines() {
        let Ok(line) = line else { return None };
        if let Ok(SessionEntry::Custom(entry)) = serde_json::from_str(&line) {
            if entry.custom_type == "request_cache_snapshot_v1" {
                latest = entry
                    .data
                    .and_then(|data| serde_json::from_value(data).ok());
            }
        }
    }
    latest
}

impl App {
    pub(super) fn new_session_event(
        &self,
        lane: maestro_runtime_contracts::SessionEventLane,
        kind: &str,
        phase: maestro_runtime_contracts::SessionEventPhase,
    ) -> Option<maestro_runtime_contracts::SessionEvent> {
        let session_id = self.state.session_id.as_deref()?;
        Some(maestro_runtime_contracts::SessionEvent::new(
            uuid::Uuid::new_v4().to_string(),
            Utc::now().to_rfc3339(),
            session_id,
            lane,
            kind,
            phase,
        ))
    }

    pub(super) fn record_session_event(&mut self, event: maestro_runtime_contracts::SessionEvent) {
        if self.session_manager.writer().is_none() {
            return;
        }
        let Ok(data) = serde_json::to_value(&event) else {
            return;
        };
        self.write_session_entry(SessionEntry::Custom(CustomEntry {
            id: Some(event.event_id.clone()),
            parent_id: None,
            timestamp: event.timestamp.clone(),
            custom_type: maestro_runtime_contracts::SESSION_EVENT_CUSTOM_TYPE.into(),
            data: Some(data),
        }));
    }

    pub(super) fn record_context_budget_snapshot(
        &mut self,
        phase: maestro_context::ContextBudgetPhase,
        compaction_id: Option<String>,
    ) {
        if self.session_manager.writer().is_none() {
            return;
        }
        let Some(agent) = &self.native_agent else {
            return;
        };
        let audit = agent.runtime_audit_snapshot();
        let Some(request) = audit.request_context else {
            return;
        };
        let timestamp = Utc::now().to_rfc3339();
        let snapshot_id = uuid::Uuid::new_v4().to_string();
        let confidence = match maestro_context::token_counting::count_tokens_with_metadata(
            "",
            Some(&request.model),
        )
        .confidence
        {
            maestro_context::token_counting::CountConfidence::Measured => {
                maestro_context::BudgetCountConfidence::Measured
            }
            maestro_context::token_counting::CountConfidence::Estimated => {
                maestro_context::BudgetCountConfidence::Estimated
            }
        };
        let snapshot = maestro_context::ContextBudgetSnapshot {
            schema_version: maestro_context::CONTEXT_BUDGET_SCHEMA.into(),
            snapshot_id: snapshot_id.clone(),
            timestamp: timestamp.clone(),
            model: request.model,
            phase,
            compaction_id,
            context_window: audit.context_window,
            system_prompt: request.system,
            tool_schemas: request.tools.iter().map(|(_, tokens)| tokens).sum(),
            tool_results: request.tool_results,
            conversation: request.conversation,
            other: request.other,
            response_reserve: u64::from(audit.max_output_tokens),
            safety_margin: maestro_runtime::agent::REQUEST_CONTEXT_SAFETY_TOKENS,
            confidence,
        };
        let Ok(data) = serde_json::to_value(&snapshot) else {
            return;
        };
        self.write_session_entry(SessionEntry::Custom(CustomEntry {
            id: Some(snapshot_id),
            parent_id: None,
            timestamp,
            custom_type: maestro_context::CONTEXT_BUDGET_CUSTOM_TYPE.into(),
            data: Some(data),
        }));
    }

    pub(super) fn record_agent_session_events(&mut self, message: &FromAgent) {
        use maestro_runtime_contracts::{SessionEventLane as Lane, SessionEventPhase as Phase};

        macro_rules! record {
            ($event:expr) => {
                if let Some(event) = $event {
                    self.record_session_event(event);
                }
            };
        }
        match message {
            FromAgent::ResponseStart { response_id } => {
                let mut event =
                    self.new_session_event(Lane::Model, "model.request.started", Phase::Started);
                if let Some(event) = &mut event {
                    event.correlation_id = Some(response_id.clone());
                }
                record!(event);
            }
            FromAgent::TurnStarted => {
                record!(self.new_session_event(Lane::User, "turn.started", Phase::Started,));
            }
            FromAgent::RequestContextPrepared { .. } => {
                self.record_context_budget_snapshot(
                    maestro_context::ContextBudgetPhase::PreparedRequest,
                    None,
                );
            }
            FromAgent::ResponseEnd { response_id, .. } => {
                let mut event = self.new_session_event(
                    Lane::Model,
                    "model.response.completed",
                    Phase::Completed,
                );
                if let Some(event) = &mut event {
                    event.correlation_id = Some(response_id.clone());
                }
                record!(event);
            }
            FromAgent::TurnCompleted { response_id, .. } => {
                let mut event =
                    self.new_session_event(Lane::User, "turn.completed", Phase::Completed);
                if let Some(event) = &mut event {
                    event.correlation_id = Some(response_id.clone());
                }
                record!(event);
            }
            FromAgent::TurnInterrupted { response_id, .. } => {
                let mut event =
                    self.new_session_event(Lane::User, "turn.cancelled", Phase::Cancelled);
                if let Some(event) = &mut event {
                    event.correlation_id = Some(response_id.clone());
                }
                record!(event);
            }
            FromAgent::ToolCall {
                call_id,
                tool,
                requires_approval: true,
                ..
            } => {
                let mut event = self.new_session_event(
                    Lane::Tools,
                    "tool.approval.requested",
                    Phase::Requested,
                );
                if let Some(event) = &mut event {
                    event.correlation_id = Some(call_id.clone());
                    event.name = Some(tool.clone());
                }
                record!(event);
            }
            FromAgent::ToolStart { call_id } => {
                let mut event = self.new_session_event(Lane::Tools, "tool.started", Phase::Started);
                if let Some(event) = &mut event {
                    event.correlation_id = Some(call_id.clone());
                }
                record!(event);
            }
            FromAgent::RequestRetryScheduled {
                attempt,
                delay_ms,
                rate_limited,
            } => {
                let mut event =
                    self.new_session_event(Lane::Runtime, "retry.scheduled", Phase::Requested);
                if let Some(event) = &mut event {
                    event.attempt = Some(*attempt);
                    event.delay_ms = Some(*delay_ms);
                }
                record!(event);
                if *rate_limited {
                    let mut event =
                        self.new_session_event(Lane::Runtime, "rate_limit.hit", Phase::Info);
                    if let Some(event) = &mut event {
                        event.attempt = Some(*attempt);
                        event.delay_ms = Some(*delay_ms);
                    }
                    record!(event);
                }
            }
            FromAgent::RequestRetryObservation => {
                record!(self.new_session_event(Lane::Runtime, "retry.started", Phase::Started,));
            }
            FromAgent::CompactionMeasured { duration_ms } => {
                let mut event =
                    self.new_session_event(Lane::Runtime, "compaction.measured", Phase::Completed);
                if let Some(event) = &mut event {
                    event.duration_ms = Some(*duration_ms);
                }
                record!(event);
            }
            FromAgent::Compaction { auto, .. } => {
                let mut event =
                    self.new_session_event(Lane::Runtime, "compaction.completed", Phase::Completed);
                if let Some(event) = &mut event {
                    event.automatic = Some(*auto);
                }
                record!(event);
            }
            FromAgent::CodexTransportReceipt {
                transport_restarted: true,
                ..
            } => record!(self.new_session_event(
                Lane::Runtime,
                "session.restarted",
                Phase::Completed,
            )),
            _ => {}
        }
    }

    pub(super) fn restore_request_cache(&self, agent: &NativeAgent) {
        let snapshot = self
            .session_manager
            .current_session_path()
            .and_then(|path| read_request_cache(&path));
        agent.restore_request_cache(snapshot);
    }

    pub(super) fn persist_request_cache(&mut self) {
        let Some(snapshot) = self
            .native_agent
            .as_ref()
            .and_then(|agent| agent.runtime_audit_snapshot().request_cache)
        else {
            return;
        };
        let Ok(data) = serde_json::to_value(snapshot) else {
            return;
        };
        self.write_session_entry(SessionEntry::Custom(CustomEntry {
            id: Some(uuid::Uuid::new_v4().to_string()),
            parent_id: None,
            timestamp: Utc::now().to_rfc3339(),
            custom_type: "request_cache_snapshot_v1".into(),
            data: Some(data),
        }));
    }

    pub(super) fn active_session_count(&self) -> Option<usize> {
        let sessions = self.session_manager.list_all_sessions().ok()?;
        let cutoff = SystemTime::now().checked_sub(Duration::from_hours(1))?;
        let mut count = 0usize;
        for session in sessions {
            if let Some(modified) = session.modified {
                if modified >= cutoff {
                    count += 1;
                }
            }
        }
        Some(count)
    }

    pub(super) fn ensure_session_started(&mut self) -> Result<()> {
        if self.session_resume_failed {
            self.state.error = Some(
                self.state
                    .locale
                    .translate("Session resume failed; use /new to start a new session.")
                    .to_string(),
            );
            bail!("Session resume failed");
        }

        if self.session_manager.writer().is_some() || self.session_manager.is_ephemeral_session() {
            return Ok(());
        }

        // The session owner captures the workspace at startup. Persist that
        // same scope so resume never points tools at a different directory.
        let cwd = self.session_manager.cwd().to_owned();
        let session_id = uuid::Uuid::new_v4().to_string();
        let model = if !self.current_model.is_empty() {
            self.current_model.clone()
        } else {
            self.state
                .model
                .clone()
                .unwrap_or_else(|| self.state.locale.translate("unknown").to_string())
        };
        if self.current_model.is_empty() {
            self.current_model = model.clone();
        }
        let policy_model = policy_model_id(&model);
        if let Some(reason) = check_model_allowed(&policy_model) {
            self.state.error = Some(reason.clone());
            bail!(reason);
        }
        let tools = ToolRegistry::new()
            .tools()
            .map(|tool| ToolInfo {
                name: tool.tool.name.clone(),
                label: None,
                description: Some(tool.tool.description.clone()),
            })
            .collect::<Vec<_>>();
        let unified_context_manifest =
            crate::context_cli::load_unified_context_manifest_json(std::path::Path::new(&cwd))
                .context("Failed to capture the session context manifest")?;

        let header = SessionHeader {
            version: Some(2),
            id: session_id.clone(),
            timestamp: Utc::now().to_rfc3339(),
            cwd,
            model: policy_model,
            subject: None,
            model_metadata: None,
            thinking_level: self.current_thinking_level,
            system_prompt: None,
            prompt_metadata: None,
            prompt_context_manifest: None,
            unified_context_manifest: Some(Box::new(unified_context_manifest)),
            tools,
            branched_from: None,
            parent_session: None,
        };

        if let Err(error) = self.session_manager.start_session(header) {
            self.state.error = Some(super::format_session_persistence_error(
                self.state.locale.translate("start the session"),
                &error,
            ));
            return Err(anyhow::Error::new(error).context("Failed to start session"));
        }
        self.flush_session();

        self.state.session_id = Some(session_id.clone());
        if let Some(event) = self.new_session_event(
            maestro_runtime_contracts::SessionEventLane::Runtime,
            "session.created",
            maestro_runtime_contracts::SessionEventPhase::Completed,
        ) {
            self.record_session_event(event);
        }
        // The session now has an id, so replace the placeholder scope with the
        // id-derived one. Children can only be started by a model turn, and a
        // turn cannot begin before this point, so no child is ever stamped with
        // the placeholder.
        self.adopt_session_context(Some(&session_id), "new");
        self.session_started_at = SystemTime::now();
        self.session_resume_failed = false;
        self.usage_tracker = crate::usage::UsageTracker::with_session(session_id.clone());
        self.usage_tracker.set_model(self.current_model.clone());

        if let Some(agent) = &self.native_agent {
            agent.send_session_info(
                &std::env::current_dir()
                    .map_or_else(|_| ".".to_string(), |p| p.to_string_lossy().to_string()),
                Some(session_id),
                self.current_git_branch.clone(),
            );
        }

        Ok(())
    }

    /// Durably record a guardian auto-adjudication outcome.
    ///
    /// This is the audit record for every guardian review: without it, the
    /// only trace of an auto-approved tool call was a transcript banner
    /// (`state.add_system_message`) and an in-memory, size-bounded
    /// `ToolHistory` entry that ages out and is never persisted -- neither
    /// survives a restart or a `--replay`, and neither distinguishes "the
    /// guardian allowed this silently" from an ordinary human approval or
    /// static-allowlist auto-approval after the fact. Writing a
    /// `SessionEntry::Custom("guardian_decision", ...)` entry makes the
    /// decision, its stated reason, and the tool/args it covered part of
    /// the same append-only session log everything else is audited through.
    pub(super) fn record_guardian_decision(
        &mut self,
        call_id: &str,
        tool: &str,
        args_summary: &str,
        outcome: &str,
        reason: &str,
    ) {
        let entry = SessionEntry::Custom(CustomEntry {
            id: Some(uuid::Uuid::new_v4().to_string()),
            parent_id: None,
            timestamp: Utc::now().to_rfc3339(),
            custom_type: "guardian_decision".to_string(),
            data: Some(serde_json::json!({
                "callId": call_id,
                "tool": tool,
                "argsSummary": args_summary,
                "outcome": outcome,
                "reason": reason,
                "managedPolicy": crate::safety::managed_policy_metadata(),
            })),
        });
        self.write_session_entry(entry);
        self.flush_session();
    }

    pub(super) fn write_session_entry(&mut self, entry: SessionEntry) -> bool {
        let error = {
            let Some(writer) = self.session_manager.writer() else {
                return self.session_manager.is_ephemeral_session();
            };
            writer.write_entry(entry).err()
        };
        if let Some(err) = error {
            self.state.error = Some(super::format_session_persistence_error(
                self.state.locale.translate("persist session data"),
                err,
            ));
            return false;
        }
        true
    }

    pub(super) fn flush_session(&mut self) -> bool {
        if let Err(err) = self.session_manager.flush() {
            self.state.error = Some(super::format_session_persistence_error(
                self.state.locale.translate("flush the session transcript"),
                err,
            ));
            return false;
        }
        true
    }

    /// Persist the fact that the host applied a lifecycle notification before
    /// the mailbox delivery is acknowledged. A failed write deliberately
    /// leaves the message delivered-but-unacknowledged so its lease can expire
    /// and a later process can replay it.
    pub(super) fn subagent_lifecycle_application_exists(
        &mut self,
        event: &crate::tools::SubagentLifecycleEvent,
    ) -> Option<bool> {
        if self.ensure_session_started().is_err() {
            return None;
        }
        if self.session_manager.is_ephemeral_session() {
            return Some(
                self.ephemeral_lifecycle_applications
                    .contains(&event.mailbox_message_id),
            );
        }
        let Some(session_path) = self.session_manager.current_session_path() else {
            self.state.error = Some(
                self.state
                    .locale
                    .translate(
                        "Failed to persist subagent lifecycle application: no active session path",
                    )
                    .to_string(),
            );
            return None;
        };
        match session_contains_entry_id(&session_path, &event.mailbox_message_id) {
            Ok(found) => Some(found),
            Err(error) => {
                self.state.error = Some(super::format_session_persistence_error(
                    self.state
                        .locale
                        .translate("check subagent lifecycle application"),
                    error,
                ));
                None
            }
        }
    }

    pub(super) fn record_subagent_lifecycle_application(
        &mut self,
        event: &crate::tools::SubagentLifecycleEvent,
        content: String,
        agent_note: String,
    ) -> bool {
        if self.session_manager.is_ephemeral_session() {
            self.ephemeral_lifecycle_applications
                .insert(event.mailbox_message_id.clone());
            return true;
        }
        let entry = SessionEntry::Custom(CustomEntry {
            id: Some(event.mailbox_message_id.clone()),
            parent_id: None,
            timestamp: Utc::now().to_rfc3339(),
            custom_type: "subagent_lifecycle_applied".to_string(),
            data: Some(serde_json::json!({
                "content": content,
                "agentNote": agent_note,
                "projection": maestro_runtime::DelegationEvent::from_subagent_lifecycle(
                    &event.mailbox_message_id,
                    &event.subagent_id,
                    event.attempt,
                    &format!("{:?}", event.status).to_ascii_lowercase(),
                    event.summary.as_deref(),
                    event.error.as_deref(),
                ),
                "event": event,
            })),
        });
        let write_error = self
            .session_manager
            .writer()
            .and_then(|writer| writer.write_entry(entry).err());
        if let Some(error) = write_error {
            self.state.error = Some(super::format_session_persistence_error(
                self.state
                    .locale
                    .translate("persist subagent lifecycle application"),
                error,
            ));
            return false;
        }
        let (status, phase) = match event.status {
            crate::tools::SubagentStatus::Queued => (
                "queued",
                maestro_runtime_contracts::SessionEventPhase::Requested,
            ),
            crate::tools::SubagentStatus::Running => (
                "running",
                maestro_runtime_contracts::SessionEventPhase::Started,
            ),
            crate::tools::SubagentStatus::Completed => (
                "completed",
                maestro_runtime_contracts::SessionEventPhase::Completed,
            ),
            crate::tools::SubagentStatus::Failed => (
                "failed",
                maestro_runtime_contracts::SessionEventPhase::Failed,
            ),
            crate::tools::SubagentStatus::Cancelled => (
                "cancelled",
                maestro_runtime_contracts::SessionEventPhase::Cancelled,
            ),
            crate::tools::SubagentStatus::TimedOut => (
                "timed_out",
                maestro_runtime_contracts::SessionEventPhase::Cancelled,
            ),
            crate::tools::SubagentStatus::Interrupted => (
                "interrupted",
                maestro_runtime_contracts::SessionEventPhase::Cancelled,
            ),
        };
        let mut timeline_event = self.new_session_event(
            maestro_runtime_contracts::SessionEventLane::Worker,
            &format!("worker.{status}"),
            phase,
        );
        if let Some(timeline_event) = &mut timeline_event {
            timeline_event.correlation_id = Some(event.subagent_id.clone());
            timeline_event.attempt = Some(event.attempt);
        }
        if let Some(timeline_event) = timeline_event {
            self.record_session_event(timeline_event);
        }
        if let Err(error) = self.session_manager.flush() {
            self.state.error = Some(super::format_session_persistence_error(
                self.state
                    .locale
                    .translate("flush subagent lifecycle application"),
                error,
            ));
            return false;
        }
        true
    }

    /// Persist confirmation that the native runner appended a lifecycle note.
    pub(super) fn record_subagent_lifecycle_agent_note_delivered(
        &mut self,
        application_id: &str,
    ) -> bool {
        if self.session_manager.is_ephemeral_session() {
            return true;
        }
        let entry = SessionEntry::Custom(CustomEntry {
            id: Some(format!("{application_id}:agent-note-delivered")),
            parent_id: None,
            timestamp: Utc::now().to_rfc3339(),
            custom_type: "subagent_lifecycle_agent_note_delivered".to_string(),
            data: Some(serde_json::json!({
                "applicationId": application_id,
            })),
        });
        let Some(writer) = self.session_manager.writer() else {
            self.state.error = Some(
                self.state.locale.translate("Failed to persist subagent lifecycle agent-note delivery: no active session writer")
                    .to_string(),
            );
            return false;
        };
        let write_error = writer.write_entry(entry).err();
        if let Some(error) = write_error {
            self.state.error = Some(super::format_session_persistence_error(
                self.state
                    .locale
                    .translate("persist subagent lifecycle agent-note delivery"),
                error,
            ));
            return false;
        }
        if let Err(error) = self.session_manager.flush() {
            self.state.error = Some(super::format_session_persistence_error(
                self.state
                    .locale
                    .translate("flush subagent lifecycle agent-note delivery"),
                error,
            ));
            return false;
        }
        true
    }

    /// Persist proof that a successful native model turn consumed a note.
    pub(super) fn record_subagent_lifecycle_agent_note_consumed(
        &mut self,
        application_id: &str,
    ) -> bool {
        if self.session_manager.is_ephemeral_session() {
            return true;
        }
        let entry = SessionEntry::Custom(CustomEntry {
            id: Some(format!("{application_id}:agent-note-consumed")),
            parent_id: None,
            timestamp: Utc::now().to_rfc3339(),
            custom_type: "subagent_lifecycle_agent_note_consumed".to_string(),
            data: Some(serde_json::json!({
                "applicationId": application_id,
            })),
        });
        let Some(writer) = self.session_manager.writer() else {
            self.state.error = Some(
                self.state.locale.translate("Failed to persist subagent lifecycle agent-note consumption: no active session writer")
                    .to_string(),
            );
            return false;
        };
        let write_error = writer.write_entry(entry).err();
        if let Some(error) = write_error {
            self.state.error = Some(super::format_session_persistence_error(
                self.state
                    .locale
                    .translate("persist subagent lifecycle agent-note consumption"),
                error,
            ));
            return false;
        }
        if let Err(error) = self.session_manager.flush() {
            self.state.error = Some(super::format_session_persistence_error(
                self.state
                    .locale
                    .translate("flush subagent lifecycle agent-note consumption"),
                error,
            ));
            return false;
        }
        true
    }

    pub(super) fn record_user_message(&mut self, content: &str) {
        if self.ensure_session_started().is_err() {
            return;
        }

        let entry = SessionEntry::Message(MessageEntry {
            id: None,
            parent_id: None,
            timestamp: Utc::now().to_rfc3339(),
            message: AppMessage::User {
                content: MessageContent::Text(content.to_string()),
                attachments: None,
                timestamp: system_time_to_millis(SystemTime::now()),
            },
        });
        self.write_session_entry(entry);
        self.flush_session();
    }

    pub(super) fn record_assistant_message(
        &mut self,
        response_id: &str,
        usage: Option<crate::agent::TokenUsage>,
    ) -> bool {
        if self.ensure_session_started().is_err() {
            return false;
        }

        let Some(message) = self
            .state
            .messages
            .iter()
            .find(|m| m.id == response_id && m.role == MessageRole::Assistant)
            .cloned()
        else {
            return false;
        };

        let mut blocks = Vec::new();
        if !message.thinking.is_empty() {
            blocks.push(SessionContentBlock::Thinking {
                text: message.thinking.clone(),
                signature: None,
            });
        }
        if !message.content.is_empty() {
            blocks.push(SessionContentBlock::Text {
                text: message.content.clone(),
            });
        }
        for call in &message.tool_calls {
            blocks.push(SessionContentBlock::ToolCall {
                id: call.call_id.clone(),
                name: call.tool.clone(),
                args: call.args.clone(),
                // Pin the tool identity this call was issued against so a
                // resume cannot dispatch a different tool of the same name.
                contract: crate::tools::tool_call_contract::stamp(&call.call_id, &call.tool),
                gemini_context: None,
            });
        }

        if self
            .pending_assistant_content
            .as_ref()
            .is_some_and(|(id, _)| id == response_id)
        {
            let (_, content) = self
                .pending_assistant_content
                .take()
                .expect("matching response");
            blocks = content
                .into_iter()
                .filter_map(|block| match block {
                    crate::ai::ContentBlock::Text { text } => {
                        Some(SessionContentBlock::Text { text })
                    }
                    crate::ai::ContentBlock::Thinking {
                        thinking,
                        signature,
                    } => Some(SessionContentBlock::Thinking {
                        text: thinking,
                        signature,
                    }),
                    crate::ai::ContentBlock::ToolUse {
                        id,
                        name,
                        input,
                        gemini_context,
                    } => Some(SessionContentBlock::ToolCall {
                        contract: crate::tools::tool_call_contract::stamp(&id, &name),
                        id,
                        name,
                        args: input,
                        gemini_context,
                    }),
                    _ => None,
                })
                .collect();
        }

        let usage = usage
            .as_ref()
            .map(to_session_usage)
            .or_else(|| message.usage.as_ref().map(to_session_usage));

        let entry = SessionEntry::Message(MessageEntry {
            id: None,
            parent_id: None,
            timestamp: Utc::now().to_rfc3339(),
            message: AppMessage::Assistant {
                content: blocks,
                api: self.state.provider.clone(),
                provider: self.state.provider.clone(),
                model: Some(policy_model_id(&self.current_model)),
                usage,
                stop_reason: None,
                timestamp: system_time_to_millis(message.timestamp),
            },
        });
        let wrote = self.write_session_entry(entry);
        let flushed = self.flush_session();
        wrote && flushed
    }

    pub(super) fn record_tool_result(
        &mut self,
        call_id: &str,
        tool: &str,
        result: &ToolResult,
        execution: Option<&ToolExecution>,
    ) {
        if execution.is_some_and(|execution| {
            matches!(execution.receipt.status, ExecutionStatus::Cancelled { .. })
        }) {
            let note = result
                .error
                .clone()
                .unwrap_or_else(|| result.output.clone());
            self.tool_history
                .cancel_with_details(call_id, note, result.details.clone());
        } else if result.success {
            self.tool_history.complete_with_details(
                call_id,
                result.output.clone(),
                result.details.clone(),
            );
        } else {
            let error = result
                .error
                .clone()
                .unwrap_or_else(|| result.output.clone());
            self.tool_history
                .fail_with_details(call_id, error, result.details.clone());
        }

        if self.ensure_session_started().is_err() {
            return;
        }

        let content = if result.success {
            result.output.clone()
        } else {
            result
                .error
                .clone()
                .unwrap_or_else(|| result.output.clone())
        };

        let entry = SessionEntry::Message(MessageEntry {
            id: None,
            parent_id: None,
            timestamp: Utc::now().to_rfc3339(),
            message: AppMessage::ToolResult {
                tool_call_id: call_id.to_string(),
                tool_name: tool.to_string(),
                content,
                details: result.details.clone(),
                receipt: execution.map(|execution| execution.receipt.clone()),
                is_error: !result.success,
                timestamp: system_time_to_millis(SystemTime::now()),
            },
        });
        self.write_session_entry(entry);
        self.flush_session();
    }

    pub(super) fn record_model_change(&mut self, model: &str) {
        if self.session_manager.writer().is_none() {
            return;
        }

        let entry = SessionEntry::ModelChange(ModelChange {
            timestamp: Utc::now().to_rfc3339(),
            model: policy_model_id(model),
            model_metadata: None,
        });
        self.write_session_entry(entry);
    }

    pub(super) fn record_thinking_level_change(&mut self, level: ThinkingLevel) {
        if self.session_manager.writer().is_none() {
            return;
        }

        let entry = SessionEntry::ThinkingLevelChange(ThinkingLevelChange {
            timestamp: Utc::now().to_rfc3339(),
            thinking_level: level,
        });
        self.write_session_entry(entry);
    }

    pub(super) fn record_compaction_entry(
        &mut self,
        summary: String,
        first_kept_entry_index: usize,
        tokens_before: u64,
        auto: bool,
        custom_instructions: Option<String>,
        continuation: Option<crate::agent::compaction::ContinuationRecord>,
    ) {
        if self.ensure_session_started().is_err() {
            return;
        }

        let entry = SessionEntry::Compaction(CompactionEntry {
            id: None,
            parent_id: None,
            timestamp: Utc::now().to_rfc3339(),
            summary,
            first_kept_entry_id: None,
            first_kept_entry_index: Some(first_kept_entry_index),
            tokens_before,
            auto,
            custom_instructions,
            continuation,
        });
        self.write_session_entry(entry);
    }

    pub(super) fn record_side_question(
        &mut self,
        id: String,
        question: String,
        answer: String,
        error: Option<String>,
    ) {
        if self.ensure_session_started().is_err() {
            return;
        }
        self.write_session_entry(SessionEntry::SideQuestion(SideQuestionEntry {
            id,
            timestamp: Utc::now().to_rfc3339(),
            question,
            answer,
            error,
        }));
        self.flush_session();
    }

    pub(super) fn hydrate_usage_from_session(&mut self, session: &ParsedSession) {
        self.usage_tracker = crate::usage::UsageTracker::with_session(session.id());
        self.usage_tracker.set_model(session.header.model.clone());

        for entry in &session.usage_entries {
            let usage = crate::headless::TokenUsage {
                input_tokens: entry.usage.input,
                output_tokens: entry.usage.output,
                cache_read_tokens: entry.usage.cache_read,
                cache_write_tokens: entry.usage.cache_write,
                cost: entry.usage.cost.as_ref().map(|c| c.total),
                total_tokens: None,
                model_id: None,
                provider: None,
            };
            let _ = self.usage_tracker.add_turn_for_model(&entry.model, &usage);
        }
    }
}

#[cfg(test)]
mod lifecycle_receipt_tests {
    use super::*;

    #[test]
    fn cache_snapshot_reopens_only_the_selected_session() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("session.jsonl");
        let snapshot = maestro_context::token_counting::RequestCacheSnapshot::from_request(
            &maestro_ai::RequestConfig::default(),
            42,
        );
        let entry = SessionEntry::Custom(CustomEntry {
            id: Some("cache-fixture".into()),
            parent_id: None,
            timestamp: "2026-09-08T00:00:00Z".into(),
            custom_type: "request_cache_snapshot_v1".into(),
            data: Some(serde_json::to_value(&snapshot).unwrap()),
        });
        std::fs::write(
            &path,
            format!("{}\n", serde_json::to_string(&entry).unwrap()),
        )
        .unwrap();
        assert_eq!(read_request_cache(&path), Some(snapshot));
        assert_eq!(
            read_request_cache(&directory.path().join("another-session.jsonl")),
            None
        );
        std::fs::write(
            &path,
            "{\"type\":\"custom\",\"customType\":\"request_cache_snapshot_v1\",\"data\":false}\n",
        )
        .unwrap();
        assert_eq!(read_request_cache(&path), None);
    }

    #[test]
    fn session_entry_id_prevents_lifecycle_replay_duplication() {
        let directory = tempfile::tempdir().expect("session directory");
        let path = directory.path().join("session.jsonl");
        let entry = SessionEntry::Custom(CustomEntry {
            id: Some("m-lifecycle".to_string()),
            parent_id: None,
            timestamp: "2026-08-08T00:00:00Z".to_string(),
            custom_type: "subagent_lifecycle_applied".to_string(),
            data: Some(serde_json::json!({ "content": "finished" })),
        });
        std::fs::write(
            &path,
            format!(
                "{}\n",
                serde_json::to_string(&entry).expect("serialize entry")
            ),
        )
        .expect("persist lifecycle entry");

        assert!(session_contains_entry_id(&path, "m-lifecycle").expect("scan session"));
        assert!(!session_contains_entry_id(&path, "m-other").expect("scan other id"));
    }
}

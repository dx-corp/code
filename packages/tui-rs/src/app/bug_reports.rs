use super::*;
use crate::bug_report::{self, BugReport, DraftStatus, FeedbackClient, ReportEvidence};
use anyhow::ensure;
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use std::path::{Path, PathBuf};

#[derive(Default)]
pub(super) struct FeedbackUi {
    draft: Option<BugReport>,
    observed_session: Option<PathBuf>,
    path: Option<PathBuf>,
    queue: Vec<(PathBuf, BugReport)>,
    candidates: Vec<ReportEvidence>,
    mode: FeedbackMode,
    cursor: usize,
    scroll: u16,
    editor: String,
    editor_cursor: usize,
    error: Option<String>,
    cards_shown: usize,
    pub quick_send: bool,
    send: Option<tokio::sync::oneshot::Receiver<(PathBuf, BugReport, Result<String>)>>,
    gateway_request_id: Option<String>,
    gateway_record_id: Option<String>,
    gateway_lineage_id: Option<String>,
}

#[derive(Default, PartialEq)]
enum FeedbackMode {
    #[default]
    Review,
    Queue,
    Evidence,
    Edit(&'static str),
}

impl FeedbackUi {
    pub(super) fn note_gateway_receipt(
        &mut self,
        request_id: &str,
        record_id: &str,
        lineage_id: &str,
    ) {
        self.gateway_request_id = bug_report::correlation_token(request_id);
        self.gateway_record_id = bug_report::correlation_token(record_id);
        self.gateway_lineage_id = bug_report::correlation_token(lineage_id);
    }

    pub(super) fn clear_gateway_receipt(&mut self) {
        self.gateway_request_id = None;
        self.gateway_record_id = None;
        self.gateway_lineage_id = None;
    }

    fn gateway_correlation(&self) -> String {
        let mut parts = Vec::new();
        if let Some(id) = &self.gateway_request_id {
            parts.push(format!("request={id}"));
        }
        if let Some(id) = &self.gateway_record_id {
            parts.push(format!("record={id}"));
        }
        if let Some(id) = &self.gateway_lineage_id {
            parts.push(format!("lineage={id}"));
        }
        parts.join(" ")
    }

    pub fn card_visible(&self) -> bool {
        self.cards_shown <= 3
            && std::env::var("MAESTRO_FEEDBACK_DRAFTS").as_deref() != Ok("quiet")
            && self.draft.as_ref().is_some_and(|d| {
                !d.hidden && matches!(d.status, DraftStatus::Draft | DraftStatus::Reviewed)
            })
    }
    pub fn render_card(&self, frame: &mut ratatui::Frame, area: Rect, input_height: u16) {
        if !self.card_visible() {
            if self.draft.as_ref().is_some_and(|d| {
                matches!(
                    d.status,
                    DraftStatus::Draft | DraftStatus::Reviewed | DraftStatus::Sending
                )
            }) && area.height > input_height + 1
            {
                let badge = Rect::new(
                    area.x,
                    area.y + area.height - input_height - 2,
                    area.width,
                    1,
                );
                frame.render_widget(Clear, badge);
                frame.render_widget(
                    Paragraph::new(maestro_ui::localization::tr(
                        "Feedback drafts · /feedback to review",
                    )),
                    badge,
                );
            }
            return;
        }
        let Some(draft) = &self.draft else {
            return;
        };
        let height = 4.min(area.height.saturating_sub(input_height + 1));
        if height < 3 {
            return;
        }
        let card = Rect::new(
            area.x,
            area.y + area.height.saturating_sub(input_height + 1 + height),
            area.width,
            height,
        );
        frame.render_widget(Clear, card);
        frame.render_widget(
            Paragraph::new(maestro_ui::localization::format(
                "{0}\n1 Review · 2 Review and send · 0 Hide · /feedback Queue",
                &[draft
                    .description
                    .lines()
                    .next()
                    .unwrap_or("Feedback draft")
                    .to_string()],
            ))
            .block(
                Block::default()
                    .title(maestro_ui::localization::tr(" Bug report drafted "))
                    .borders(Borders::ALL),
            ),
            card,
        );
    }
    pub fn render(&self, frame: &mut ratatui::Frame, area: Rect) {
        let popup = Rect::new(
            area.x + 1,
            area.y + 1,
            area.width.saturating_sub(2),
            area.height.saturating_sub(2),
        );
        frame.render_widget(Clear, popup);
        let content = match &self.mode {
            FeedbackMode::Queue => {
                let rows = self
                    .queue
                    .iter()
                    .enumerate()
                    .map(|(i, (_, d))| {
                        format!(
                            "{} {}",
                            if i == self.cursor { ">" } else { " " },
                            d.description.lines().next().unwrap_or("Draft")
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                maestro_ui::localization::format(
                    "Saved drafts ({0})\n↑/↓ Select · Enter Review · w Write report · Esc Close\n\n{1}",
                    &[
                        (self.queue.len()).to_string(),
                        (if rows.is_empty() {
                            "No saved drafts."
                        } else {
                            &rows
                        })
                        .to_string(),
                    ],
                )
            }
            FeedbackMode::Evidence => {
                let rows = self
                    .candidates
                    .iter()
                    .enumerate()
                    .map(|(i, item)| {
                        let selected = self.draft.as_ref().is_some_and(|d| {
                            d.context
                                .evidence
                                .iter()
                                .any(|e| e.source_id == item.source_id && e.kind == item.kind)
                        });
                        format!(
                            "{} [{}] {} [{}]\n{}",
                            if i == self.cursor { ">" } else { " " },
                            if selected { "x" } else { " " },
                            item.kind,
                            item.source_id,
                            item.text
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n\n");
                maestro_ui::localization::format(
                    "Choose evidence · ↑/↓ Select · Space Toggle · Esc Review\nOnly checked items will be sent. Known credential patterns are redacted; review the text.\n\n{0}",
                    std::slice::from_ref(&(rows)),
                )
            }
            FeedbackMode::Edit(field) => {
                let mut text = self.editor.clone();
                text.insert(self.editor_cursor.min(text.len()), '▏');
                maestro_ui::localization::format(
                    "Edit {0} · Enter Save · Shift+Enter New line · Esc Cancel\n\n{1}",
                    &[(field).to_string(), (text).clone()],
                )
            }
            FeedbackMode::Review => maestro_ui::localization::format(
                "e Edit description · x Expected · r Reproduction · d Version/model\nv Choose evidence · s Send · a Export · 0 Discard · Esc Queue\n{0}\n\n{1}",
                &[
                    (if self.quick_send {
                        "Press 2 again to send the reviewed report."
                    } else {
                        "↑/↓ Scroll. Sending shares the fields below with product support."
                    })
                    .to_string(),
                    (self
                        .draft
                        .as_ref()
                        .map(BugReport::preview)
                        .unwrap_or_else(|| "No draft. Press e to write one.".into()))
                    .clone(),
                ],
            ),
        };
        let content = if let Some(error) = &self.error {
            format!("{error}\n\n{content}")
        } else {
            content
        };
        frame.render_widget(
            Paragraph::new(content)
                .wrap(Wrap { trim: false })
                .scroll((self.scroll, 0))
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(maestro_ui::localization::tr(" Product feedback ")),
                ),
            popup,
        );
    }
}

impl App {
    fn feedback_save(&mut self, path: &Path, report: &BugReport) -> Result<()> {
        if self.session_manager.current_session_path().as_deref() == Some(path) {
            bug_report::save(&mut self.session_manager, report)
        } else {
            // Use the existing cross-process session lock. Another live session
            // remains its writer; it cannot be edited behind that writer's back.
            let mut writer = crate::session::SessionWriter::open_existing(path)?;
            if let Some(latest) = bug_report::load_all(path)?
                .into_iter()
                .find(|d| d.id == report.id)
            {
                if let Some(expected) = self
                    .feedback_ui
                    .draft
                    .as_ref()
                    .filter(|d| d.id == report.id)
                {
                    ensure!(
                        &latest == expected
                            || matches!(report.status, DraftStatus::Sent { .. })
                                && latest.status == DraftStatus::Sending,
                        "This draft changed in another session. Reopen /feedback before editing or sending."
                    );
                }
            }
            writer.write_entry(crate::session::SessionEntry::Custom(
                crate::session::CustomEntry {
                    id: Some(uuid::Uuid::new_v4().to_string()),
                    parent_id: None,
                    timestamp: chrono::Utc::now().to_rfc3339(),
                    custom_type: "product_issue_draft_v1".into(),
                    data: Some(serde_json::to_value(report)?),
                },
            ))?;
            writer.flush()?;
            Ok(())
        }
    }

    fn feedback_queue(&mut self) -> Result<()> {
        self.session_manager.flush()?;
        let mut queue = Vec::new();
        for session in self.session_manager.list_all_sessions()? {
            // The existing session retention remains the owner. Queue views
            // omit drafts after 30 days without modifying conversation history.
            for draft in bug_report::load_all(&session.path)? {
                if !matches!(
                    draft.status,
                    DraftStatus::Sent { .. } | DraftStatus::Dismissed
                ) && chrono::Utc::now()
                    .timestamp()
                    .saturating_sub(draft.created_at)
                    <= 30 * 86400
                {
                    queue.push((session.path.clone(), draft));
                }
            }
        }
        queue.sort_by_key(|(_, d)| std::cmp::Reverse(d.created_at));
        self.feedback_ui.queue = queue;
        self.feedback_ui.mode = FeedbackMode::Queue;
        self.feedback_ui.cursor = 0;
        self.feedback_ui.scroll = 0;
        self.active_modal = ActiveModal::Feedback;
        Ok(())
    }

    pub(super) fn accept_feedback_tool(&mut self, tool: &str, result: &ToolResult) {
        if tool != "draft_feedback"
            || !result.success
            || std::env::var("MAESTRO_FEEDBACK_DRAFTS").as_deref() == Ok("off")
        {
            return;
        }
        let report = result
            .details
            .as_ref()
            .and_then(|d| d.get("feedback_draft"))
            .and_then(|d| {
                let mut draft = BugReport::new(d["description"].as_str()?).ok()?;
                draft
                    .edit(None, Some(d["expected_behavior"].as_str()?), None)
                    .ok()?;
                draft
                    .set_reproduction(d["context"]["reproduction_steps"].as_str()?)
                    .ok()?;
                Some(draft)
            });
        let Some(mut report) = report else {
            return;
        };
        // Only the built-in draft result is accepted, and authority is reset.
        report.status = DraftStatus::Draft;
        report.destination = None;
        report.context.evidence.clear();
        report.include_diagnostics = false;
        let result = (|| -> Result<()> {
            let path = self
                .session_manager
                .current_session_path()
                .context("Feedback drafts require a saved session.")?;
            let saved = bug_report::load_all(&path)?;
            if saved.iter().any(|d| {
                d.description == report.description
                    && d.expected_behavior == report.expected_behavior
            }) {
                return Ok(());
            }
            ensure!(
                saved
                    .iter()
                    .filter(|d| !matches!(
                        d.status,
                        DraftStatus::Sent { .. } | DraftStatus::Dismissed
                    ))
                    .count()
                    < 10,
                "The session already has 10 feedback drafts. Review them with /feedback."
            );
            report.context.model = self.state.model.clone().unwrap_or_default();
            self.feedback_save(&path, &report)?;
            self.feedback_ui.cards_shown += 1;
            // A running tool never changes a report the user is reviewing.
            if self.active_modal != ActiveModal::Feedback {
                self.feedback_ui.draft = Some(report);
                self.feedback_ui.path = Some(path);
            }
            self.state.add_system_message(
                self.state
                    .locale
                    .translate("Feedback draft saved locally. /feedback to review the queue.")
                    .into(),
            );
            Ok(())
        })();
        if let Err(error) = result {
            self.state.add_system_message(
                self.state
                    .locale
                    .format("Could not save feedback draft: {0}", &[(error).to_string()]),
            );
        }
    }

    pub(super) fn suggest_bug_report(&mut self, terminal_error: &str) {
        if std::env::var("MAESTRO_FEEDBACK_DRAFTS").as_deref() == Ok("off") {
            return;
        }
        if self.session_manager.writer().is_none() {
            return;
        }
        if !matches!(
            bug_report::load(self.session_manager.current_session_path().as_deref()),
            Ok(None)
        ) {
            return;
        }
        let report = bug_report::draft_tool(
            serde_json::json!({"description":"A Deixic Code turn ended with an unrecoverable error.","expected_behavior":"The requested turn completes or provides a recoverable error.","reproduction_steps":"Describe the request and select relevant evidence in /feedback."}),
        );
        self.accept_feedback_tool("draft_feedback", &report);
        self.attach_unrecoverable_trace(terminal_error);
    }

    /// The model draft stays diagnostics-off. The client attaches the trace after that reset.
    fn attach_unrecoverable_trace(&mut self, terminal_error: &str) {
        let Some(path) = self.session_manager.current_session_path() else {
            return;
        };
        let Ok(Some(mut report)) = bug_report::load(Some(path.as_path())) else {
            return;
        };
        if report.description != "A Deixic Code turn ended with an unrecoverable error."
            || !matches!(report.status, DraftStatus::Draft)
        {
            return;
        }
        let text = bug_report::terminal_error_trace(
            terminal_error,
            &self.feedback_ui.gateway_correlation(),
        );
        if text.is_empty() {
            return;
        }
        let source_id = self
            .feedback_ui
            .gateway_request_id
            .clone()
            .or_else(|| {
                self.state
                    .session_id
                    .as_deref()
                    .map(str::trim)
                    .filter(|id| {
                        !id.is_empty() && id.len() <= 256 && !id.chars().any(char::is_whitespace)
                    })
                    .map(str::to_owned)
            })
            .unwrap_or_else(|| "unrecoverable-turn".to_owned());
        report.include_diagnostics = true;
        if report
            .set_evidence(vec![ReportEvidence {
                kind: "trace".into(),
                source_id,
                text,
            }])
            .is_err()
        {
            return;
        }
        if self.feedback_save(&path, &report).is_err() {
            return;
        }
        if self
            .feedback_ui
            .draft
            .as_ref()
            .is_some_and(|draft| draft.id == report.id)
        {
            self.feedback_ui.draft = Some(report);
        }
    }

    fn feedback_candidates(&mut self) -> Result<()> {
        let path = self
            .feedback_ui
            .path
            .as_deref()
            .context("No draft session")?;
        // Parse persisted records only. Never include thinking, credentials,
        // environment, working directory, or arbitrary custom entries.
        let reader = std::io::BufReader::new(std::fs::File::open(path)?);
        use std::io::BufRead;
        let mut candidates = std::collections::VecDeque::new();
        for (index, line) in reader.lines().enumerate() {
            let entry: serde_json::Value = serde_json::from_str(&line?)?;
            if entry["type"].as_str() != Some("message") {
                continue;
            }
            let message = &entry["message"];
            let role = message["role"].as_str().unwrap_or("");
            if !matches!(role, "user" | "assistant" | "toolResult" | "tool_result") {
                continue;
            }
            let raw = match &message["content"] {
                serde_json::Value::String(text) => text.clone(),
                serde_json::Value::Array(parts) => parts
                    .iter()
                    .filter(|part| part["type"].as_str() == Some("text"))
                    .filter_map(|part| part["text"].as_str())
                    .collect::<Vec<_>>()
                    .join("\n"),
                _ => continue,
            };
            if raw.is_empty() {
                continue;
            }
            let text = bug_report::redact(&raw);
            // Do not silently cut evidence: long items stay outside this bounded picker.
            if text.len() > 4000 {
                continue;
            }
            let fallback = format!("entry:{}", index + 1);
            let id = entry["id"]
                .as_str()
                .or_else(|| message["toolCallId"].as_str())
                .unwrap_or(&fallback);
            candidates.push_back(ReportEvidence {
                kind: role.into(),
                source_id: id.into(),
                text,
            });
            if candidates.len() > 20 {
                candidates.pop_front();
            }
        }
        self.feedback_ui.candidates = candidates.into_iter().collect();
        self.feedback_ui.mode = FeedbackMode::Evidence;
        self.feedback_ui.cursor = 0;
        self.feedback_ui.scroll = 0;
        Ok(())
    }

    pub(super) async fn handle_bug_report(&mut self, args: &str) -> Result<()> {
        self.feedback_ui.error = None;
        self.feedback_ui.quick_send = false;
        let (action, text) = args
            .trim()
            .split_once(char::is_whitespace)
            .unwrap_or((args.trim(), ""));
        if action.is_empty() || action == "queue" {
            return self.feedback_queue();
        }
        if action == "compose" {
            self.feedback_ui.draft = None;
            self.feedback_ui.path = None;
            self.feedback_ui.editor.clear();
            self.feedback_ui.editor_cursor = 0;
            self.feedback_ui.mode = FeedbackMode::Edit("new");
            self.active_modal = ActiveModal::Feedback;
            return Ok(());
        }
        let mut saved = if let Some(path) = &self.feedback_ui.path {
            let id = self.feedback_ui.draft.as_ref().map(|d| d.id.as_str());
            bug_report::load_all(path)?
                .into_iter()
                .find(|d| Some(d.id.as_str()) == id)
        } else {
            bug_report::load(self.session_manager.current_session_path().as_deref())?
        };
        let mut path = self
            .feedback_ui
            .path
            .clone()
            .or_else(|| self.session_manager.current_session_path());
        match action {
            "draft" | "new" => {
                if let Some(draft) = saved.as_mut().filter(|d| {
                    action == "draft"
                        && !matches!(d.status, DraftStatus::Sent { .. } | DraftStatus::Dismissed)
                }) {
                    draft.edit(Some(text), None, None)?;
                } else {
                    saved = Some(BugReport::new(text)?);
                    self.ensure_session_started()?;
                    path = self.session_manager.current_session_path();
                }
            }
            "expected" => {
                saved
                    .as_mut()
                    .context("Create a draft first.")?
                    .edit(None, Some(text), None)?;
            }
            "repro" => saved
                .as_mut()
                .context("Create a draft first.")?
                .set_reproduction(text)?,
            "diagnostics" => {
                let enabled = match text.trim() {
                    "on" => true,
                    "off" => false,
                    _ => bail!("Use /bug diagnostics on|off."),
                };
                saved
                    .as_mut()
                    .context("Create a draft first.")?
                    .edit(None, None, Some(enabled))?;
            }
            "dismiss" | "discard" | "hide" => {
                let draft = saved.as_mut().context("No bug report draft to dismiss.")?;
                ensure!(
                    !matches!(
                        draft.status,
                        DraftStatus::Sent { .. } | DraftStatus::Sending
                    ),
                    "An accepted or uncertain submission cannot be discarded. Retry with /bug send."
                );
                if action == "hide" {
                    draft.hidden = true;
                } else {
                    draft.status = DraftStatus::Dismissed;
                }
                self.active_modal = ActiveModal::None;
                self.state.add_system_message(
                    if action == "hide" {
                        self.state
                            .locale
                            .translate("Feedback card hidden. The draft is in /feedback.")
                    } else {
                        self.state.locale.translate("Bug report draft dismissed.")
                    }
                    .into(),
                );
            }
            "review" => {
                let draft = saved
                    .as_mut()
                    .context("Create a draft with /bug <description> first.")?;
                ensure!(
                    !matches!(
                        draft.status,
                        DraftStatus::Sent { .. } | DraftStatus::Dismissed
                    ),
                    "Create a new report."
                );
                if draft.status != DraftStatus::Sending {
                    match FeedbackClient::resolve() {
                        Ok(client) => {
                            draft.destination = Some(client.destination);
                            draft.status = DraftStatus::Reviewed;
                        }
                        Err(error) => {
                            self.feedback_ui.error = Some(self.state.locale.format(
                                "{0} You can export this report locally with a.",
                                &[(error).to_string()],
                            ));
                        }
                    }
                }
                self.feedback_ui.mode = FeedbackMode::Review;
                self.feedback_ui.scroll = 0;
                self.active_modal = ActiveModal::Feedback;
                self.state.add_system_message(draft.preview());
            }
            "send" => {
                let draft = saved.as_mut().context("Create a draft first.")?;
                if let DraftStatus::Sent { reference } = &draft.status {
                    self.state.add_system_message(self.state.locale.format(
                        "Bug report already submitted: {0}",
                        std::slice::from_ref(reference),
                    ));
                    return Ok(());
                }
                ensure!(
                    self.feedback_ui.send.is_none(),
                    "A submission is already in progress."
                );
                let client = FeedbackClient::resolve()?;
                draft.prepare_send(&client.destination)?;
                let path = path
                    .as_ref()
                    .context("Drafts require session persistence.")?;
                self.feedback_save(path, draft)?;
                let draft = draft.clone();
                let path = path.clone();
                let (tx, rx) = tokio::sync::oneshot::channel();
                tokio::spawn(async move {
                    let result = client.send(&draft).await;
                    let _ = tx.send((path, draft, result));
                });
                self.feedback_ui.send = Some(rx);
                self.feedback_ui.error =
                    Some(self.state.locale.translate("Sending report…").into());
            }
            "export" => {
                let draft = saved.as_ref().context("Create a draft first.")?;
                let directory = self
                    .session_manager
                    .sessions_dir()
                    .parent()
                    .context("No session root")?
                    .join("feedback-bundles");
                let exported = draft.export(&directory)?;
                self.state.add_system_message(self.state.locale.format(
                    "Feedback saved to {0}. Nothing was sent.",
                    &[(exported.display()).to_string()],
                ));
                self.feedback_ui.error = Some(
                    self.state
                        .locale
                        .format("Saved {0}", &[(exported.display()).to_string()]),
                );
            }
            _ => {
                // Claude-compatible free-form /bug and /feedback descriptions.
                let mut draft = BugReport::new(args.trim())?;
                draft.context.model = self.state.model.clone().unwrap_or_default();
                match FeedbackClient::resolve() {
                    Ok(client) => {
                        draft.destination = Some(client.destination);
                        draft.status = DraftStatus::Reviewed;
                    }
                    Err(error) => {
                        self.feedback_ui.error = Some(
                            self.state
                                .locale
                                .format("{0} Press a to export locally.", &[(error).to_string()]),
                        );
                    }
                }
                saved = Some(draft);
                self.ensure_session_started()?;
                path = self.session_manager.current_session_path();
                self.feedback_ui.mode = FeedbackMode::Review;
                self.active_modal = ActiveModal::Feedback;
            }
        }
        let mut draft = saved.context("No draft to save.")?;
        if draft.context.model.is_empty() && matches!(draft.status, DraftStatus::Draft) {
            draft.context.model = self.state.model.clone().unwrap_or_default();
        }
        let path = path.context("Bug reports require session persistence.")?;
        if action != "send" {
            self.feedback_save(&path, &draft)?;
        }
        self.feedback_ui.draft = Some(draft);
        self.feedback_ui.path = Some(path);
        Ok(())
    }

    pub(super) fn poll_feedback_send(&mut self) {
        let current = self.session_manager.current_session_path();
        if current != self.feedback_ui.observed_session {
            self.feedback_ui.observed_session = current.clone();
            self.feedback_ui.path = current.clone();
            self.feedback_ui.draft = bug_report::load(current.as_deref()).ok().flatten();
            self.feedback_ui.cards_shown = 0;
        }
        let Some(rx) = self.feedback_ui.send.as_mut() else {
            return;
        };
        let completed = match rx.try_recv() {
            Ok(value) => value,
            Err(tokio::sync::oneshot::error::TryRecvError::Empty) => return,
            Err(_) => {
                self.feedback_ui.send = None;
                self.feedback_ui.error = Some(
                    self.state.locale.translate("Submission could not be confirmed. /bug send retries the saved report.").into(),
                );
                return;
            }
        };
        self.feedback_ui.send = None;
        let (path, mut draft, result) = completed;
        match result {
            Ok(reference) => {
                draft.status = DraftStatus::Sent {
                    reference: reference.clone(),
                };
                match self.feedback_save(&path,&draft) {
                    Ok(()) => self.state.add_system_message(self.state.locale.format("Bug report submitted: {0}", std::slice::from_ref(&(reference)))),
                    Err(error) => self.state.add_system_message(self.state.locale.format("Report submitted: {0}, but the local receipt could not be saved: {1}. Retry uses the same report ID.", &[(reference).clone(), (error).to_string()])),
                }
                if self
                    .feedback_ui
                    .draft
                    .as_ref()
                    .is_some_and(|d| d.id == draft.id)
                {
                    self.feedback_ui.draft = Some(draft);
                    self.active_modal = ActiveModal::None;
                }
            }
            Err(error) => {
                self.feedback_ui.error = Some(error.to_string());
                self.state.add_system_message(error.to_string());
            }
        }
    }

    pub(super) fn paste_feedback(&mut self, raw: &str) {
        if matches!(self.feedback_ui.mode, FeedbackMode::Edit(_)) {
            for c in raw
                .chars()
                .filter(|c| !c.is_control() || matches!(c, '\n' | '\t'))
            {
                if self.feedback_ui.editor.len() + c.len_utf8() > 4000 {
                    break;
                }
                self.feedback_ui
                    .editor
                    .insert(self.feedback_ui.editor_cursor, c);
                self.feedback_ui.editor_cursor += c.len_utf8();
            }
        }
    }

    pub(super) async fn handle_feedback_key(&mut self, code: KeyCode) -> Result<()> {
        let result = self.feedback_key_inner(code).await;
        if let Err(error) = result {
            self.feedback_ui.error = Some(error.to_string());
        }
        Ok(())
    }

    async fn feedback_key_inner(&mut self, code: KeyCode) -> Result<()> {
        if let FeedbackMode::Edit(field) = self.feedback_ui.mode {
            match code {
                KeyCode::Esc => self.feedback_ui.mode = FeedbackMode::Review,
                KeyCode::Backspace => {
                    if let Some((index, _)) = self.feedback_ui.editor
                        [..self.feedback_ui.editor_cursor]
                        .char_indices()
                        .next_back()
                    {
                        self.feedback_ui.editor.remove(index);
                        self.feedback_ui.editor_cursor = index;
                    }
                }
                KeyCode::Delete
                    if self.feedback_ui.editor_cursor < self.feedback_ui.editor.len() =>
                {
                    self.feedback_ui
                        .editor
                        .remove(self.feedback_ui.editor_cursor);
                }
                KeyCode::Left => {
                    self.feedback_ui.editor_cursor = self.feedback_ui.editor
                        [..self.feedback_ui.editor_cursor]
                        .char_indices()
                        .next_back()
                        .map_or(0, |(index, _)| index);
                }
                KeyCode::Right => {
                    self.feedback_ui.editor_cursor += self.feedback_ui.editor
                        [self.feedback_ui.editor_cursor..]
                        .chars()
                        .next()
                        .map_or(0, char::len_utf8);
                }
                KeyCode::Home => self.feedback_ui.editor_cursor = 0,
                KeyCode::End => self.feedback_ui.editor_cursor = self.feedback_ui.editor.len(),
                KeyCode::Char(c)
                    if !c.is_control() && self.feedback_ui.editor.len() + c.len_utf8() <= 4000 =>
                {
                    self.feedback_ui
                        .editor
                        .insert(self.feedback_ui.editor_cursor, c);
                    self.feedback_ui.editor_cursor += c.len_utf8();
                }
                KeyCode::Enter => {
                    let text = self.feedback_ui.editor.clone();
                    self.handle_bug_report(&format!("{field} {text}")).await?;
                    self.handle_bug_report("review").await?;
                }
                _ => {}
            }
            return Ok(());
        }
        match self.feedback_ui.mode {
            FeedbackMode::Queue => match code {
                KeyCode::Esc => self.active_modal = ActiveModal::None,
                KeyCode::Up => self.feedback_ui.cursor = self.feedback_ui.cursor.saturating_sub(1),
                KeyCode::Down => {
                    self.feedback_ui.cursor = (self.feedback_ui.cursor + 1)
                        .min(self.feedback_ui.queue.len().saturating_sub(1));
                }
                KeyCode::Enter => {
                    if let Some((path, draft)) =
                        self.feedback_ui.queue.get(self.feedback_ui.cursor).cloned()
                    {
                        self.feedback_ui.path = Some(path);
                        self.feedback_ui.draft = Some(draft);
                        self.handle_bug_report("review").await?;
                    }
                }
                KeyCode::Char('w') => {
                    self.feedback_ui.draft = None;
                    self.feedback_ui.path = None;
                    self.feedback_ui.editor.clear();
                    self.feedback_ui.editor_cursor = 0;
                    self.feedback_ui.mode = FeedbackMode::Edit("new");
                }
                _ => {}
            },
            FeedbackMode::Evidence => match code {
                KeyCode::Esc => {
                    self.handle_bug_report("review").await?;
                }
                KeyCode::Up => {
                    self.feedback_ui.cursor = self.feedback_ui.cursor.saturating_sub(1);
                    self.feedback_ui.scroll = self.feedback_ui.scroll.saturating_sub(3);
                }
                KeyCode::Down => {
                    self.feedback_ui.cursor = (self.feedback_ui.cursor + 1)
                        .min(self.feedback_ui.candidates.len().saturating_sub(1));
                    self.feedback_ui.scroll = self.feedback_ui.scroll.saturating_add(3);
                }
                KeyCode::Char(' ') => {
                    if let Some(item) = self
                        .feedback_ui
                        .candidates
                        .get(self.feedback_ui.cursor)
                        .cloned()
                    {
                        let mut draft = self.feedback_ui.draft.clone().context("No draft")?;
                        let mut items = draft.context.evidence.clone();
                        if items
                            .iter()
                            .any(|e| e.source_id == item.source_id && e.kind == item.kind)
                        {
                            items.retain(|e| e.source_id != item.source_id || e.kind != item.kind);
                        } else {
                            items.push(item);
                        }
                        draft.set_evidence(items)?;
                        let path = self.feedback_ui.path.clone().context("No session")?;
                        self.feedback_save(&path, &draft)?;
                        self.feedback_ui.draft = Some(draft);
                    }
                }
                _ => {}
            },
            FeedbackMode::Review => match code {
                KeyCode::Esc => self.feedback_queue()?,
                KeyCode::Up => self.feedback_ui.scroll = self.feedback_ui.scroll.saturating_sub(3),
                KeyCode::Down => {
                    self.feedback_ui.scroll = self.feedback_ui.scroll.saturating_add(3);
                }
                KeyCode::Char('e' | 'x' | 'r') => {
                    let field = match code {
                        KeyCode::Char('x') => "expected",
                        KeyCode::Char('r') => "repro",
                        _ => "draft",
                    };
                    let draft = self.feedback_ui.draft.as_ref().context("No draft")?;
                    self.feedback_ui.editor = match field {
                        "expected" => draft.expected_behavior.clone(),
                        "repro" => draft.context.reproduction_steps.clone(),
                        _ => draft.description.clone(),
                    };
                    self.feedback_ui.editor_cursor = self.feedback_ui.editor.len();
                    self.feedback_ui.mode = FeedbackMode::Edit(field);
                    self.feedback_ui.scroll = 0;
                }
                KeyCode::Char('v') => self.feedback_candidates()?,
                KeyCode::Char('d') => {
                    let enabled = self
                        .feedback_ui
                        .draft
                        .as_ref()
                        .is_some_and(|d| d.include_diagnostics);
                    self.handle_bug_report(if enabled {
                        self.state.locale.translate("diagnostics off")
                    } else {
                        self.state.locale.translate("diagnostics on")
                    })
                    .await?;
                    self.handle_bug_report("review").await?;
                }
                KeyCode::Char('s') => self.handle_bug_report("send").await?,
                KeyCode::Char('2') if self.feedback_ui.quick_send => {
                    self.handle_bug_report("send").await?;
                }
                KeyCode::Char('a') => self.handle_bug_report("export").await?,
                KeyCode::Char('0') => self.handle_bug_report("discard").await?,
                _ => {}
            },
            FeedbackMode::Edit(_) => unreachable!(),
        }
        Ok(())
    }
}

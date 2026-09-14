use super::*;

fn model_palette_resource(
    model: &crate::model_catalog::ModelInfo,
    current_route: Option<&str>,
    locale: crate::localization::Locale,
) -> PaletteResource {
    let route = crate::model_catalog::model_route(model);
    let mut resource = PaletteResource::from(model).description(
        locale.format(
            "{0} · {1}k context · {2}",
            &[
                (model.provider).clone(),
                (model.capabilities.context_tokens / 1000).to_string(),
                locale
                    .translate(match model.verification.state {
                        crate::model_catalog::VerificationState::Catalog => "Catalog",
                        crate::model_catalog::VerificationState::Verified => "Verified",
                        crate::model_catalog::VerificationState::Unavailable => "Unavailable",
                        crate::model_catalog::VerificationState::Unknown => "Unknown",
                    })
                    .to_owned(),
            ],
        ),
    );
    if current_route == Some(route.as_str()) {
        resource = resource.status(locale.translate("current"));
    }
    resource
}

impl App {
    /// Handle a key press
    pub(super) async fn handle_key(
        &mut self,
        code: KeyCode,
        modifiers: CrosstermModifiers,
    ) -> Result<()> {
        if code != KeyCode::Esc
            || self.active_modal != ActiveModal::None
            || self.history_search.is_some()
        {
            self.last_esc_at = None;
        }
        if self.active_modal != ActiveModal::ThemeSelector {
            self.cancel_theme_preview();
        }
        let ctrl = modifiers.contains(CrosstermModifiers::CONTROL);
        let alt = modifiers.contains(CrosstermModifiers::ALT);
        let shift = modifiers.contains(CrosstermModifiers::SHIFT);

        if self.active_modal == ActiveModal::None && self.history_search.is_some() {
            self.handle_history_search_key(code, modifiers);
            return Ok(());
        }

        if self.active_modal == ActiveModal::None
            && self.state.input().is_empty()
            && !ctrl
            && !alt
            && self.feedback_ui.card_visible()
        {
            match code {
                KeyCode::Char('1') => return self.handle_bug_report("review").await,
                KeyCode::Char('2') => {
                    self.handle_bug_report("review").await?;
                    self.feedback_ui.quick_send = true;
                    return Ok(());
                }
                KeyCode::Char('0') => return self.handle_bug_report("hide").await,
                _ => {}
            }
        }
        // Suggestions only fill an empty composer; a separate Enter still submits.
        if self.active_modal == ActiveModal::None
            && self.state.input().is_empty()
            && matches!(code, KeyCode::Right | KeyCode::Tab)
            && !ctrl
            && !alt
        {
            let candidate = self.dex_next_prompt();
            if let Some(prompt) = self.dex_delight.suggestion.take(candidate) {
                self.state.set_input(prompt);
                self.update_slash_state();
                return Ok(());
            }
        }
        if self.active_modal == ActiveModal::None && matches!(code, KeyCode::Char(_) | KeyCode::Esc)
        {
            self.dex_delight.suggestion.dismiss();
            self.dex_delight.notice = None;
        }
        // Handle modal-specific input first
        match self.active_modal {
            ActiveModal::DexAppearance => return self.handle_dex_appearance_key(code),
            ActiveModal::FileSearch => return self.handle_file_search_key(code, ctrl).await,
            ActiveModal::SessionSwitcher => {
                return self.handle_session_switcher_key(code, ctrl).await;
            }
            ActiveModal::Operations => return self.handle_operations_key(code),
            ActiveModal::McpManager => return self.handle_mcp_manager_key(code).await,
            ActiveModal::CommandPalette => {
                return self.handle_command_palette_key(code, ctrl).await;
            }
            ActiveModal::Approval => return self.handle_approval_key(code, modifiers).await,
            ActiveModal::ModelSelector => return self.handle_model_selector_key(code, ctrl).await,
            ActiveModal::ThemeSelector => return self.handle_theme_selector_key(code, ctrl).await,
            ActiveModal::Setup => return self.handle_setup_modal_key(code, ctrl).await,
            ActiveModal::ShortcutsHelp => return self.handle_shortcuts_help_key(code).await,
            ActiveModal::SelectiveSummary => return self.handle_selective_summary_key(code).await,
            ActiveModal::RewindPicker => return self.handle_rewind_picker_key(code),
            ActiveModal::DetailView => return self.handle_detail_view_key(code),
            ActiveModal::Feedback => {
                if ctrl && code == KeyCode::Char('c') {
                    self.active_modal = ActiveModal::None;
                    return Ok(());
                }
                if shift && code == KeyCode::Enter {
                    self.paste_feedback("\n");
                    return Ok(());
                }
                return self.handle_feedback_key(code).await;
            }
            ActiveModal::None => {}
        }

        if self
            .try_restore_last_queued_follow_up(code, modifiers)
            .await?
        {
            return Ok(());
        }

        if code == KeyCode::Char('b') && ctrl && !alt {
            self.handle_command_action(CommandAction::Boost).await;
            return Ok(());
        }

        // Apply through the same runtime/session path as /thinking. Modal focus
        // navigation is handled above; drafts and in-flight turns stay intact.
        if (code == KeyCode::BackTab
            && (modifiers.is_empty() || modifiers == CrosstermModifiers::SHIFT))
            || (code == KeyCode::Tab && modifiers == CrosstermModifiers::SHIFT)
        {
            let level = crate::model_dynamics::next_thinking_level(
                &self.current_model,
                self.current_thinking_level,
            );
            self.handle_command_action(CommandAction::SetThinkingLevel(
                level.label().to_ascii_lowercase(),
            ))
            .await;
            return Ok(());
        }

        if self.matches_binding(self.cycle_model_binding, code, modifiers) {
            self.cycle_model(false);
            return Ok(());
        }
        if self.matches_binding(self.cycle_model_backward_binding, code, modifiers) {
            self.cycle_model(true);
            return Ok(());
        }

        if self.matches_binding(self.command_palette_binding, code, modifiers) {
            self.show_command_palette();
            return Ok(());
        }
        if self.matches_binding(self.file_search_binding, code, modifiers) {
            self.file_search.show();
            self.active_modal = ActiveModal::FileSearch;
            return Ok(());
        }
        if modifiers == CrosstermModifiers::CONTROL
            && !self.matches_binding(self.toggle_tool_outputs_binding, code, modifiers)
            && !self.matches_binding(self.queued_follow_up_edit_binding, code, modifiers)
        {
            match code {
                KeyCode::Char('s') => {
                    self.swap_draft_stash();
                    return Ok(());
                }
                KeyCode::Char('r') => {
                    self.open_history_search();
                    return Ok(());
                }
                _ => {}
            }
        }

        if is_focus_view_binding(code, modifiers) {
            let enabled = self.state.toggle_focus_view();
            self.state.status = Some(if enabled {
                self.state.locale.translate("Focus view on").to_string()
            } else {
                self.state.locale.translate("Focus view off").to_string()
            });
            return Ok(());
        }
        if self.matches_binding(self.toggle_tool_outputs_binding, code, modifiers) {
            if self.state.focus_view {
                self.state.toggle_latest_focus_turn();
                return Ok(());
            }
            if !self.state.busy {
                self.toggle_last_tool_call();
                return Ok(());
            }
        }
        // Ctrl+E: open the full-output detail view for the most recent
        // expandable transcript item (Droid parity: Ctrl+O shows full
        // accumulated output; Ctrl+O is file search in maestro's map).
        if ctrl && !alt && matches!(code, KeyCode::Char('e')) {
            self.open_detail_view();
            return Ok(());
        }

        match code {
            // Quit
            KeyCode::Char('c') if ctrl => {
                if self.state.busy {
                    // Drop in-flight guardian reviews: a verdict arriving
                    // after the interrupt must neither relay an approval nor
                    // pop the approval modal.
                    self.cancel_pending_guardian_reviews();

                    let has_queued_steering = self.state.queued_steering_count > 0;
                    self.submit_queued_steering_after_interrupt = has_queued_steering;
                    self.restore_queued_prompts_after_interrupt =
                        !has_queued_steering && self.state.queued_prompt_count > 0;

                    if let Some(agent) = &self.native_agent {
                        if self.state.queued_prompt_count > 0 {
                            agent.cancel_keep_queue();
                        } else {
                            agent.cancel();
                        }
                    } else {
                        self.state.busy = false;
                        self.queued_prompt_inflight = None;
                        self.queued_prompt_active = None;
                        if self.submit_queued_steering_after_interrupt {
                            self.submit_queued_steering_after_interrupt = false;
                            let batch = self.drain_queued_steering_batch_for_interrupt();
                            self.restore_queued_prompts_to_input(batch);
                        } else if self.restore_queued_prompts_after_interrupt {
                            self.restore_queued_prompts_after_interrupt = false;
                            let batch = self.drain_queued_prompts_for_restore();
                            self.restore_queued_prompts_to_input(batch);
                        } else {
                            self.sync_queue_prompt_count();
                        }
                    }
                    // Make cancel vs quit explicit: first Ctrl+C cancels the
                    // turn; a second Ctrl+C while idle quits (handled below).
                    self.state.status = Some(
                        self.state
                            .locale
                            .translate("Cancelled. Ctrl+C again to quit.")
                            .to_string(),
                    );
                } else {
                    self.should_quit = true;
                }
            }
            KeyCode::Char('d') if ctrl => {
                self.should_quit = true;
            }

            // Open modals
            KeyCode::Char('r') if ctrl && alt => {
                // Session switcher
                self.session_switcher.show();
                self.active_modal = ActiveModal::SessionSwitcher;
            }
            KeyCode::F(1) => {
                // Keyboard shortcuts help
                self.shortcuts_help.show();
                self.active_modal = ActiveModal::ShortcutsHelp;
            }
            KeyCode::Char('?') if self.state.input().is_empty() && !ctrl && !alt => {
                // Match the discoverable command entry point used by other
                // agent CLIs while preserving '?' as ordinary prompt text.
                self.show_command_palette();
            }

            // @ trigger for file search
            KeyCode::Char('@') if !self.state.busy => {
                self.state.insert_char('@');
                self.file_search.show();
                self.active_modal = ActiveModal::FileSearch;
            }

            // / trigger for slash commands
            KeyCode::Char('/') if self.state.input().is_empty() => {
                self.state.insert_char('/');
                self.slash_state.set_query("", &self.slash_matcher);
            }
            // Swallow extra leading slashes while the menu is open so the input
            // never becomes `//` / `///` from double-tapping `/` (// menu bug).
            KeyCode::Char('/')
                if !ctrl
                    && self.state.input().starts_with('/')
                    && self.state.input().chars().all(|c| c == '/') =>
            {
                self.slash_state.set_query("", &self.slash_matcher);
            }

            // Tab for slash command completion
            KeyCode::Tab if self.state.input().starts_with('/') => {
                self.handle_slash_tab();
            }
            KeyCode::Tab if self.should_queue_follow_up_on_tab() => {
                let input = self.state.input().to_string();
                let ok = self.handle_follow_up_submit(input).await?;
                if ok {
                    self.editing_queued_follow_up = None;
                    self.state.set_input("");
                }
            }
            KeyCode::Tab if self.should_submit_on_tab() => {
                let input = self.state.take_input();
                self.submit_prompt(input).await?;
            }

            // Navigation
            KeyCode::Up => {
                if self.state.input().starts_with('/') && self.slash_state.has_completions() {
                    self.slash_state.cycle_prev();
                    self.apply_slash_completion();
                } else if !self.state.input().is_empty() {
                    self.state.move_up();
                } else if self.state.focus_view {
                    self.state.select_previous_focus_turn();
                } else {
                    self.state.scroll_up(1);
                }
            }
            KeyCode::Down => {
                if self.state.input().starts_with('/') && self.slash_state.has_completions() {
                    self.slash_state.cycle_next();
                    self.apply_slash_completion();
                } else if !self.state.input().is_empty() {
                    self.state.move_down();
                } else if self.state.focus_view {
                    self.state.select_next_focus_turn();
                } else {
                    self.state.scroll_down(1);
                }
            }
            // Vim-style scrolling: only when input is empty (not typing)
            KeyCode::Char('k') if ctrl => {
                if self.state.input().is_empty() {
                    self.state.scroll_up(1);
                } else {
                    self.state.delete_to_end_of_line();
                    self.update_slash_state();
                }
            }
            KeyCode::Char('j') if ctrl && self.state.input().is_empty() => {
                self.state.scroll_down(1);
            }
            KeyCode::PageUp => {
                let step = (self.capabilities.viewport_height as usize).max(5) / 2;
                self.state.scroll_up(step.max(1));
            }
            KeyCode::PageDown => {
                let step = (self.capabilities.viewport_height as usize).max(5) / 2;
                self.state.scroll_down(step.max(1));
            }
            // Jump shortcuts: only when input is empty (not typing)
            KeyCode::Char('g') if self.state.input().is_empty() && !ctrl => {
                // Jump to top (oldest messages)
                self.state.scroll_offset = usize::MAX / 2;
            }
            KeyCode::Char('G') if self.state.input().is_empty() => {
                // Jump to bottom (newest messages)
                self.state.scroll_offset = 0;
            }
            KeyCode::Tab if !self.state.busy && self.state.input().is_empty() => {
                // Tab: toggle thinking on last assistant message with thinking
                self.toggle_last_thinking();
            }

            // Input editing
            KeyCode::Char('a') if ctrl => {
                self.state.move_home_smart();
            }
            KeyCode::Char('b') if alt => {
                self.state.move_word_left();
            }
            KeyCode::Char('f') if alt => {
                self.state.move_word_right();
            }
            KeyCode::Char('w') if ctrl => {
                self.state.delete_word_backward();
                self.update_slash_state();
            }
            KeyCode::Char('y') if alt => {
                self.state.yank_kill_ring();
                self.update_slash_state();
            }
            KeyCode::Char(c) if !ctrl => {
                self.state.insert_char(c);
                self.update_slash_state();
            }
            KeyCode::Backspace => {
                if alt {
                    self.state.delete_word_backward();
                } else {
                    self.state.backspace();
                }
                self.update_slash_state();
            }
            KeyCode::Delete => {
                self.state.delete();
            }
            KeyCode::Left => {
                if ctrl || alt {
                    self.state.move_word_left();
                } else {
                    self.state.move_left();
                }
            }
            KeyCode::Right => {
                if ctrl || alt {
                    self.state.move_word_right();
                } else if self.state.cursor() == self.state.input().len()
                    && self.state.ghost_completion.is_some()
                {
                    // Accept the ghost-text completion (fish-shell style).
                    if let Some(suffix) = self.state.ghost_completion.take() {
                        self.state.insert_str(&suffix);
                        self.update_slash_state();
                    }
                } else {
                    self.state.move_right();
                }
            }
            KeyCode::Home => {
                self.state.move_home_smart();
            }
            KeyCode::End => {
                if self.state.cursor() == self.state.input().len()
                    && self.state.ghost_completion.is_some()
                {
                    // Already at end: End accepts the ghost-text completion.
                    if let Some(suffix) = self.state.ghost_completion.take() {
                        self.state.insert_str(&suffix);
                        self.update_slash_state();
                    }
                } else {
                    self.state.move_end();
                }
            }

            // Submit or newline (Shift+Enter for newline)
            KeyCode::Enter => {
                if shift {
                    // Shift+Enter: insert newline for multi-line input
                    self.state.insert_char('\n');
                } else if self.state.focus_view && self.state.input().is_empty() {
                    self.state.toggle_selected_focus_turn();
                } else if !self.state.input().is_empty() {
                    // Trim so " /attach list" still runs as a slash command and
                    // is never queued as steering with a missing leading '/'.
                    if self.state.input().trim_start().starts_with('/') {
                        self.execute_slash_command().await?;
                    } else if self.state.busy {
                        let input = self.state.input().to_string();
                        if alt && input.trim_start().starts_with('!') {
                            self.state.insert_char('\n');
                        } else {
                            let ok = if alt {
                                self.handle_follow_up_submit(input).await?
                            } else {
                                self.handle_steer_submit(input).await?
                            };
                            if ok {
                                self.editing_queued_follow_up = None;
                                self.state.set_input("");
                            }
                        }
                    } else if alt {
                        self.state.insert_char('\n');
                    } else {
                        let input = self.state.take_input();
                        self.submit_prompt(input).await?;
                    }
                }
            }

            // Delete to start of line
            KeyCode::Char('u') if ctrl => {
                self.state.delete_to_start_of_line();
                self.update_slash_state();
            }

            // Paste from clipboard (image → temp PNG attachment, else text)
            KeyCode::Char('y') if ctrl => {
                if let Ok(path) = self.clipboard.paste_image_to_temp_file() {
                    let path_str = path.display().to_string();
                    self.pending_attachments.push(path_str.clone());
                    // Surface the path in the composer so the operator sees it.
                    let mention = format!("[image:{path_str}] ");
                    self.state.insert_paste(&mention);
                    self.state.status = Some(self.state.locale.format(
                        "Pasted image → attached for next prompt: {0}",
                        std::slice::from_ref(&(path_str)),
                    ));
                    self.update_slash_state();
                } else if let Ok(text) = self.clipboard.paste() {
                    // Insert text including newlines for multi-line support;
                    // large pastes are folded into a display chip.
                    self.state.insert_paste(&text);
                    self.update_slash_state();
                }
            }

            // Clear screen
            KeyCode::Char('l') if ctrl => {
                // Clear messages
                self.state.messages.clear();
                self.state.clear_focus_turn_state();
                self.state.scroll_offset = 0;
            }

            // Escape: dismiss completions; double-Esc clears a draft input or,
            // when the input is empty, opens the rewind picker.
            KeyCode::Esc => {
                if self.slash_state.has_completions() {
                    self.slash_state.reset();
                    self.last_esc_at = None;
                } else if !self.state.input().is_empty() {
                    let now = Instant::now();
                    let double_press = self
                        .last_esc_at
                        .is_some_and(|t| now.duration_since(t) < Duration::from_millis(700));
                    if double_press {
                        self.state.set_input("");
                        self.update_slash_state();
                        self.last_esc_at = None;
                        self.state
                            .status
                            .replace(self.state.locale.translate("Input cleared").to_string());
                    } else {
                        self.last_esc_at = Some(now);
                        self.state.status.replace(
                            self.state
                                .locale
                                .translate("Press Esc again to clear input")
                                .to_string(),
                        );
                    }
                } else {
                    let now = Instant::now();
                    let double_press = self
                        .last_esc_at
                        .is_some_and(|t| now.duration_since(t) < Duration::from_millis(700));
                    if double_press {
                        self.last_esc_at = None;
                        self.open_rewind_picker();
                    } else {
                        self.last_esc_at = Some(now);
                        self.state.status.replace(
                            self.state
                                .locale
                                .translate("Press Esc again to rewind files")
                                .to_string(),
                        );
                    }
                }
            }

            _ => {}
        }

        Ok(())
    }

    pub(super) fn should_queue_follow_up_on_tab(&self) -> bool {
        self.state.can_queue_follow_up_shortcut()
    }

    pub(super) fn should_submit_on_tab(&self) -> bool {
        if self.state.busy {
            return false;
        }
        let input = self.state.input();
        let trimmed = input.trim_start();
        !input.trim().is_empty() && !trimmed.starts_with('/') && !trimmed.starts_with('!')
    }

    /// Route a bracketed paste to the open modal's text input, or to the
    /// main input when no modal is open.
    pub(super) fn handle_paste(&mut self, raw: &str) {
        self.last_esc_at = None;
        if self.active_modal == ActiveModal::None && self.history_search.is_some() {
            self.paste_history_query(raw);
            return;
        }
        // Normalize line endings like the main-input paste path does.
        let text: String = raw.chars().filter(|c| *c != '\r').collect();
        match self.active_modal {
            ActiveModal::Feedback => self.paste_feedback(&text),
            ActiveModal::FileSearch => self.file_search.insert_str(&text),
            ActiveModal::SessionSwitcher => self.session_switcher.insert_str(&text),
            ActiveModal::CommandPalette => self.command_palette.insert_str(&text),
            ActiveModal::ModelSelector => self.model_selector.insert_str(&text),
            ActiveModal::ThemeSelector => {
                let outcome = self.theme_selector.insert_str(&text);
                self.apply_theme_picker_outcome(outcome);
            }
            ActiveModal::Setup => self.setup_modal.insert_str(&text),
            ActiveModal::None => {
                self.dex_delight.suggestion.dismiss();
                self.dex_delight.notice = None;
                self.state.insert_paste(raw);
                self.update_slash_state();
            }
            // Modals without a text input have nothing to paste into.
            _ => {}
        }
    }

    /// Handle keys in file search modal
    pub(super) async fn handle_file_search_key(&mut self, code: KeyCode, ctrl: bool) -> Result<()> {
        match code {
            KeyCode::Esc => {
                self.file_search.hide();
                self.active_modal = ActiveModal::None;
            }
            KeyCode::Enter => {
                if let Some(file) = self.file_search.confirm() {
                    // Insert file path at cursor
                    for c in file.relative_path.chars() {
                        self.state.insert_char(c);
                    }
                    self.state.insert_char(' ');
                }
                self.active_modal = ActiveModal::None;
            }
            KeyCode::Up => {
                self.file_search.move_up();
            }
            KeyCode::Down => {
                self.file_search.move_down();
            }
            KeyCode::Char(c) if !ctrl => {
                self.file_search.insert_char(c);
            }
            KeyCode::Backspace => {
                self.file_search.backspace();
            }
            KeyCode::Left => {
                self.file_search.move_left();
            }
            KeyCode::Right => {
                self.file_search.move_right();
            }
            _ => {}
        }
        Ok(())
    }

    /// Handle keys in session switcher modal
    pub(super) async fn handle_session_switcher_key(
        &mut self,
        code: KeyCode,
        ctrl: bool,
    ) -> Result<()> {
        match code {
            KeyCode::Esc => {
                self.session_switcher.hide();
                self.active_modal = ActiveModal::None;
            }
            KeyCode::Enter => {
                if self.state.busy {
                    self.state.status = Some(
                        self.state
                            .locale
                            .translate(
                                "Wait for the active response to finish before switching sessions.",
                            )
                            .to_string(),
                    );
                    return Ok(());
                }
                if let Some(session_id) = self.session_switcher.confirm() {
                    // Load and restore the session
                    match self.session_manager.find_session(&session_id) {
                        Ok(session) => self.resume_session_path(&session.path, &session.id),
                        Err(e) => {
                            self.state.error = Some(
                                self.state
                                    .locale
                                    .format("Failed to load session: {0}", &[(e).to_string()]),
                            );
                        }
                    }
                }
                self.active_modal = ActiveModal::None;
            }
            KeyCode::Up => {
                self.session_switcher.move_up();
            }
            KeyCode::Down => {
                self.session_switcher.move_down();
            }
            KeyCode::Delete => {
                if let Err(e) = self.session_switcher.delete_selected() {
                    self.state.error = Some(e);
                }
            }
            KeyCode::Char(c) if !ctrl => {
                self.session_switcher.insert_char(c);
            }
            KeyCode::Backspace => {
                self.session_switcher.backspace();
            }
            _ => {}
        }
        Ok(())
    }

    fn handle_operations_key(&mut self, code: KeyCode) -> Result<()> {
        match code {
            KeyCode::Esc => {
                self.operations.hide();
                self.active_modal = ActiveModal::None;
            }
            KeyCode::Up => self.operations.move_up(),
            KeyCode::Down => self.operations.move_down(),
            KeyCode::Home => self.operations.select_first(),
            KeyCode::End => self.operations.select_last(),
            KeyCode::Left | KeyCode::BackTab => self.operations.focus_previous(),
            KeyCode::Right | KeyCode::Tab => self.operations.focus_next(),
            KeyCode::PageUp => self.operations.scroll_up(),
            KeyCode::PageDown => self.operations.scroll_down(),
            KeyCode::Char('r') => self.operations.refresh(),
            KeyCode::Char('v') => self.operations.toggle_view(),
            KeyCode::Char('a') => {
                if let Some(message_id) = self
                    .operations
                    .selected_held_control_id()
                    .map(str::to_owned)
                {
                    match self
                        .tool_executor
                        .approve_held_subagent_control(&message_id)
                    {
                        Ok(_) => {
                            self.state.status = Some(self.state.locale.format(
                                "Approved control {0}",
                                std::slice::from_ref(&(message_id)),
                            ));
                            self.operations.refresh();
                        }
                        Err(error) => self.state.error = Some(error),
                    }
                }
            }
            KeyCode::Char('c') => {
                if let Some(subagent_id) = self.operations.selected_agent_id().map(str::to_owned) {
                    match self.tool_executor.cancel_subagent_by_id(&subagent_id) {
                        Ok(()) => {
                            self.state.status = Some(self.state.locale.format(
                                "Cancelled subagent {0}",
                                std::slice::from_ref(&(subagent_id)),
                            ));
                            self.operations.refresh();
                        }
                        Err(error) => self.state.error = Some(error),
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    pub(super) async fn handle_mcp_manager_key(&mut self, code: KeyCode) -> Result<()> {
        if self.mcp_manager.in_catalog() {
            match code {
                KeyCode::Esc => self.mcp_manager.leave_catalog(),
                KeyCode::Up => self.mcp_manager.move_up(),
                KeyCode::Down => self.mcp_manager.move_down(),
                KeyCode::Backspace => self.mcp_manager.search_backspace(),
                KeyCode::Char(ch) if !ch.is_control() => self.mcp_manager.search_character(ch),
                KeyCode::Enter => {
                    if let Some(entry) = self.mcp_manager.selected_catalog() {
                        if self.mcp_manager.catalog_entry_configured(entry.id) {
                            self.state.status = Some(format!(
                                "{}: {}",
                                entry.id,
                                self.state
                                    .locale
                                    .text(crate::localization::TextKey::Configured)
                            ));
                        } else if self
                            .apply_mcp_manager_config(vec![
                                "registry".into(),
                                "add".into(),
                                entry.id.into(),
                            ])
                            .await
                        {
                            self.mcp_manager.leave_catalog();
                        }
                    }
                }
                _ => {}
            }
            return Ok(());
        }
        match code {
            KeyCode::Esc => self.active_modal = ActiveModal::None,
            KeyCode::Up => self.mcp_manager.move_up(),
            KeyCode::Down => self.mcp_manager.move_down(),
            KeyCode::Enter => self.mcp_manager.toggle_tools(),
            KeyCode::Char('r' | 'R') => {
                if let Some(name) = self
                    .mcp_manager
                    .selected()
                    .map(|status| status.name.clone())
                {
                    self.tool_executor.retry_mcp_server(&name).await;
                    self.last_mcp_status_refresh = None;
                    self.refresh_mcp_badges_with_force(true).await;
                    self.state.status = Some(
                        self.state
                            .locale
                            .format("Retrying MCP server {0}", std::slice::from_ref(&(name))),
                    );
                }
            }
            KeyCode::Char(' ') => {
                let selected = self.mcp_manager.selected().cloned();
                if let Some(status) = selected {
                    if !mcp_manager_entry_is_mutable(&status) {
                        self.state.status = Some(self.state.locale.format("MCP server {0} is controlled by {1} configuration and is read-only here", &[(status.name).clone(), (mcp_scope_arg(status.scope)).to_string()]));
                        return Ok(());
                    }
                    if let Some((tool, enabled)) = self
                        .mcp_manager
                        .selected_tool()
                        .map(|(tool, enabled)| (tool.to_string(), enabled))
                    {
                        self.apply_mcp_manager_config(vec![
                            "tool".to_string(),
                            status.name,
                            tool,
                            if enabled { "off" } else { "on" }.to_string(),
                            "--scope".to_string(),
                            mcp_scope_arg(status.scope).to_string(),
                        ])
                        .await;
                    } else {
                        self.apply_mcp_manager_config(vec![
                            if status.state == crate::tools::McpLifecycleState::Disabled {
                                "enable"
                            } else {
                                "disable"
                            }
                            .to_string(),
                            status.name,
                            "--scope".to_string(),
                            mcp_scope_arg(status.scope).to_string(),
                        ])
                        .await;
                    }
                }
            }
            KeyCode::Char('a' | 'A') => {
                self.active_modal = ActiveModal::None;
                self.state.set_input("/mcp config add ");
                self.update_slash_state();
            }
            KeyCode::Char('c' | 'C') => self.mcp_manager.enter_catalog(),
            KeyCode::Char('o' | 'O') => {
                if let Some(status) = self.mcp_manager.selected().cloned() {
                    if !mcp_manager_entry_is_mutable(&status) {
                        self.state.status = Some(self.state.locale.format("MCP server {0} is controlled by {1} configuration and is read-only here", &[(status.name).clone(), (mcp_scope_arg(status.scope)).to_string()]));
                        return Ok(());
                    }
                    self.handle_mcp_action(crate::commands::McpAction::Configure {
                        args: vec!["auth".into(), status.name],
                    })
                    .await;
                }
            }
            KeyCode::Char('x' | 'X') => {
                if let Some(status) = self.mcp_manager.selected().cloned() {
                    if !mcp_manager_entry_is_mutable(&status) {
                        self.state.status = Some(self.state.locale.format("MCP server {0} is controlled by {1} configuration and is read-only here", &[(status.name).clone(), (mcp_scope_arg(status.scope)).to_string()]));
                        return Ok(());
                    }
                    self.apply_mcp_manager_config(vec!["clear-auth".to_string(), status.name])
                        .await;
                }
            }
            KeyCode::Char('d' | 'D') => {
                if let Some(status) = self.mcp_manager.selected().cloned() {
                    if !mcp_manager_entry_is_mutable(&status) {
                        self.state.status = Some(self.state.locale.format("MCP server {0} is controlled by {1} configuration and is read-only here", &[(status.name).clone(), (mcp_scope_arg(status.scope)).to_string()]));
                        return Ok(());
                    }
                    self.apply_mcp_manager_config(vec![
                        self.state.locale.translate("remove").to_string(),
                        status.name,
                        "--scope".to_string(),
                        mcp_scope_arg(status.scope).to_string(),
                    ])
                    .await;
                }
            }
            KeyCode::Char('p' | 'P') => {
                self.active_modal = ActiveModal::None;
                self.state.set_input("/mcp config permissions list");
                self.update_slash_state();
            }
            _ => {}
        }
        Ok(())
    }

    async fn apply_mcp_manager_config(&mut self, args: Vec<String>) -> bool {
        match crate::mcp_config_cli::apply_mcp_config(&args) {
            Ok(message) => {
                self.state.status = Some(message);
                self.last_mcp_status_refresh = None;
                self.refresh_mcp_badges_with_force(true).await;
                true
            }
            Err(error) => {
                self.state.status = Some(
                    self.state
                        .locale
                        .format("MCP configuration failed: {0}", &[(error).to_string()]),
                );
                false
            }
        }
    }

    /// Handle keys in command palette modal
    pub(super) fn show_command_palette(&mut self) {
        let mut resources = Vec::new();
        let mut commands = self.command_registry.all();
        commands.sort_by(|left, right| left.name.cmp(&right.name));
        resources.extend(commands.into_iter().map(|command| {
            PaletteResource::new(
                PaletteResourceKind::Command,
                command.name.clone(),
                format!("/{}", command.name),
            )
            .localized_description(command.description.clone(), command.localized_description)
            .search_terms(command.aliases.clone())
        }));
        resources.extend(self.workspace_files.iter().map(|file| {
            PaletteResource::new(
                PaletteResourceKind::File,
                file.relative_path.clone(),
                file.relative_path.clone(),
            )
            .description(file.name.clone())
        }));
        if let Ok(sessions) = self.session_manager.list_all_sessions() {
            resources.extend(sessions.into_iter().map(|session| {
                let mut resource = PaletteResource::new(
                    PaletteResourceKind::Session,
                    session.id.clone(),
                    session.title(),
                )
                .description(format!(
                    "{} · {} messages · {}",
                    session.timestamp,
                    session.stats.total_messages(),
                    session.model
                ))
                .search_terms([session.cwd.clone(), session.model.clone()]);
                if session.is_favorite() {
                    resource = resource.status("favorite");
                }
                resource
            }));
        }
        let models = crate::model_catalog::available_models();
        let current_model_route = crate::components::model_selector::canonical_current_route(
            &self.current_model,
            &models,
        );
        resources.extend(models.iter().map(|model| {
            model_palette_resource(model, current_model_route.as_deref(), self.state.locale)
        }));
        let current_theme = crate::themes::current_theme_name();
        resources.extend(crate::themes::available_themes().into_iter().map(|theme| {
            let mut resource =
                PaletteResource::new(PaletteResourceKind::Theme, theme.clone(), theme.clone());
            if theme == current_theme {
                resource = resource.status(self.state.locale.translate("current"));
            }
            resource
        }));
        self.command_palette.set_resources(resources);
        self.command_palette.show();
        self.active_modal = ActiveModal::CommandPalette;
    }

    pub(super) async fn handle_command_palette_key(
        &mut self,
        code: KeyCode,
        ctrl: bool,
    ) -> Result<()> {
        match code {
            KeyCode::Esc => {
                self.command_palette.hide();
                self.active_modal = ActiveModal::None;
            }
            KeyCode::Enter => {
                self.active_modal = ActiveModal::None;
                if let Some(resource) = self.command_palette.confirm() {
                    match resource.kind {
                        PaletteResourceKind::Command => {
                            self.state.set_input(&format!("/{}", resource.id));
                            self.execute_slash_command().await?;
                        }
                        PaletteResourceKind::File => {
                            for c in resource.id.chars() {
                                self.state.insert_char(c);
                            }
                            self.state.insert_char(' ');
                        }
                        PaletteResourceKind::Session => {
                            if self.state.busy {
                                self.state.status = Some(
                                    self.state.locale.translate("Wait for the active response to finish before switching sessions.")
                                        .to_string(),
                                );
                                self.active_modal = ActiveModal::None;
                                return Ok(());
                            }
                            self.session_switcher.show();
                            if self.session_switcher.select_by_id(&resource.id) {
                                self.handle_session_switcher_key(KeyCode::Enter, false)
                                    .await?;
                            } else {
                                self.state.error = Some(
                                    self.state
                                        .locale
                                        .translate("Session no longer exists")
                                        .to_string(),
                                );
                            }
                        }
                        PaletteResourceKind::Model => {
                            self.handle_command_action(CommandAction::SetModel(resource.id))
                                .await;
                        }
                        PaletteResourceKind::Theme => {
                            self.handle_command_action(CommandAction::SetTheme(resource.id))
                                .await;
                        }
                    }
                }
            }
            KeyCode::Up => {
                self.command_palette.move_up();
            }
            KeyCode::Down => {
                self.command_palette.move_down();
            }
            KeyCode::Char(c) if !ctrl => {
                self.command_palette.insert_char(c);
            }
            KeyCode::Backspace => {
                self.command_palette.backspace();
            }
            KeyCode::Left => {
                self.command_palette.move_left();
            }
            KeyCode::Right => {
                self.command_palette.move_right();
            }
            _ => {}
        }
        Ok(())
    }

    /// Handle keys in rewind picker modal
    pub(super) fn handle_rewind_picker_key(&mut self, code: KeyCode) -> Result<()> {
        match code {
            KeyCode::Esc => {
                self.rewind_picker.hide();
                self.active_modal = ActiveModal::None;
            }
            KeyCode::Enter => {
                if let Some(checkpoint) = self.rewind_picker.confirm() {
                    self.restore_file_checkpoint(checkpoint);
                }
                self.active_modal = ActiveModal::None;
            }
            KeyCode::Char(mode @ ('c' | 'b')) => {
                if let Some(checkpoint) = self.rewind_picker.confirm() {
                    let total = self
                        .session_manager
                        .writer()
                        .and_then(|writer| writer.saved_user_turn_count().ok());
                    match (checkpoint.user_turn_index, total) {
                        (Some(index), Some(total)) if index < total =>
                            self.rewind_saved_turns(total - index, false, mode == 'b'),
                        _ => self.state.error = Some(
                            self.state.locale.translate("This checkpoint has no saved conversation turn. Use Enter to restore files.").into()),
                    }
                }
                self.active_modal = ActiveModal::None;
            }
            KeyCode::Up => {
                self.rewind_picker.move_up();
            }
            KeyCode::Down => {
                self.rewind_picker.move_down();
            }
            _ => {}
        }
        Ok(())
    }

    /// Handle keys in the full-output detail view overlay.
    ///
    /// Esc/q/Enter closes the overlay and restores the modal that was active
    /// when it opened (the approval modal when expanded from there).
    pub(super) fn handle_detail_view_key(&mut self, code: KeyCode) -> Result<()> {
        let viewport_height = self.capabilities.viewport_height as usize;
        let Some(detail) = &mut self.detail_view else {
            self.active_modal = ActiveModal::None;
            return Ok(());
        };
        if detail.handle_key(code, viewport_height) {
            self.detail_view = None;
            let mut return_modal = self.detail_return_modal;
            self.detail_return_modal = ActiveModal::None;
            // The approval queue may have drained while the overlay was open.
            if return_modal == ActiveModal::Approval && !self.approval_controller.is_visible() {
                return_modal = ActiveModal::None;
            }
            self.active_modal = return_modal;
        }
        Ok(())
    }

    /// Open the detail view for the most recent expandable transcript item:
    /// full untruncated tool output, full thinking, or full message text,
    /// falling back to the current error surface when the transcript is empty.
    pub(super) fn open_detail_view(&mut self) {
        match self.latest_detail_target() {
            Some((title, content, include_evidence)) => {
                let content = if !self.state.busy && include_evidence {
                    let changed_files = self.state.session_id.as_deref().and_then(|id| {
                        crate::checkpoints::CheckpointStore::new(
                            self.session_manager.sessions_dir(),
                            id,
                        )
                        .latest()
                        .map(|checkpoint| checkpoint.entries.len())
                    });
                    let evidence = crate::components::activity::evidence_summary(
                        &self.state.messages,
                        changed_files,
                    );
                    self.state.locale.format(
                        "Session evidence:\n{0}\n\n{1}",
                        &[(evidence).clone(), (content).clone()],
                    )
                } else {
                    content
                };
                self.detail_view = Some(DetailView::new(title, content));
                self.detail_return_modal = ActiveModal::None;
                self.active_modal = ActiveModal::DetailView;
            }
            None => {
                self.state
                    .status
                    .replace(self.state.locale.translate("Nothing to expand").to_string());
            }
        }
    }

    /// Pick the most recent expandable item and build its full-content view.
    fn latest_detail_target(&self) -> Option<(String, String, bool)> {
        for message in self.state.messages.iter().rev() {
            let include_evidence =
                message.role == MessageRole::Assistant && message.kind != MessageKind::System;
            if let Some(call) = message
                .tool_calls
                .iter()
                .rev()
                .find(|call| !call.output.trim().is_empty())
            {
                let args = serde_json::to_string_pretty(&call.args)
                    .unwrap_or_else(|_| call.args.to_string());
                let content = format!("Args:\n{args}\n\nOutput:\n{}", call.output);
                return Some((format!("Tool: {}", call.tool), content, include_evidence));
            }
            if !message.thinking.trim().is_empty() {
                return Some((
                    self.state.locale.translate("Thinking").to_string(),
                    message.thinking.clone(),
                    include_evidence,
                ));
            }
            if !message.content.trim().is_empty() {
                let title = match message.kind {
                    MessageKind::System => self.state.locale.translate("System message"),
                    _ if message.role == MessageRole::User => {
                        self.state.locale.translate("User message")
                    }
                    _ => self.state.locale.translate("Assistant message"),
                };
                return Some((title.to_string(), message.content.clone(), include_evidence));
            }
        }
        self.state.error.as_ref().map(|error| {
            (
                self.state.locale.translate("Error").to_string(),
                error.clone(),
                false,
            )
        })
    }

    /// Handle keys in approval modal
    pub(super) async fn handle_approval_key(
        &mut self,
        code: KeyCode,
        modifiers: CrosstermModifiers,
    ) -> Result<()> {
        // Ctrl+E: expand the command/diff being approved into the detail view
        // (Droid parity: Ctrl+O on tool confirmation prompts). The approval
        // modal clips the command to a few lines; the detail view shows the
        // full command and arguments. Esc returns to the approval modal.
        if modifiers.contains(CrosstermModifiers::CONTROL) && matches!(code, KeyCode::Char('e')) {
            if let Some(request) = self.approval_controller.selected_request() {
                self.detail_view = Some(DetailView::new(
                    self.state
                        .locale
                        .format("Approval: {0}", std::slice::from_ref(&(request.tool))),
                    approval_detail_content(request),
                ));
                self.detail_return_modal = ActiveModal::Approval;
                self.active_modal = ActiveModal::DetailView;
            }
            return Ok(());
        }
        // More than one pending approval means parallel tool calls landed in the
        // same batch: use the batch interaction so the user answers one modal.
        if approval_modal_kind(&self.approval_controller) == ApprovalModalKind::Batched {
            return self.handle_batched_approval_key(code).await;
        }
        match code {
            KeyCode::Char('s' | 'S' | 'w' | 'W')
                if self
                    .approval_controller
                    .current()
                    .is_some_and(|request| crate::mcp::McpClient::is_mcp_tool(&request.tool)) =>
            {
                let persistent = matches!(code, KeyCode::Char('w' | 'W'));
                if let Some((request, _decision)) =
                    self.approval_controller.decide(ApprovalDecision::Approve)
                {
                    if let Err(error) = self
                        .tool_executor
                        .remember_mcp_permission(&request.tool, persistent)
                    {
                        self.state.status = Some(self.state.locale.format(
                            "Approved once; could not remember MCP permission: {0}",
                            std::slice::from_ref(&(error)),
                        ));
                    }
                    self.handle_tool_approval(request.call_id, request.tool, request.args, true)
                        .await?;
                }
                if self.approval_controller.current().is_none() {
                    self.active_modal = ActiveModal::None;
                }
            }
            KeyCode::Char('y' | 'Y') | KeyCode::Enter => {
                if let Some((request, _decision)) =
                    self.approval_controller.decide(ApprovalDecision::Approve)
                {
                    // Execute the tool and send response
                    self.handle_tool_approval(request.call_id, request.tool, request.args, true)
                        .await?;
                }
                // Check if more approvals pending
                if self.approval_controller.current().is_none() {
                    self.active_modal = ActiveModal::None;
                }
            }
            KeyCode::Char('n' | 'N') | KeyCode::Esc => {
                if let Some((request, _decision)) =
                    self.approval_controller.decide(ApprovalDecision::Deny)
                {
                    // Send denial
                    self.handle_tool_approval(request.call_id, request.tool, request.args, false)
                        .await?;
                }
                // Check if more approvals pending
                if self.approval_controller.current().is_none() {
                    self.active_modal = ActiveModal::None;
                }
            }
            KeyCode::Char('a' | 'A') => {
                // Approve all
                while let Some((request, _decision)) =
                    self.approval_controller.decide(ApprovalDecision::Approve)
                {
                    self.handle_tool_approval(request.call_id, request.tool, request.args, true)
                        .await?;
                }
                self.active_modal = ActiveModal::None;
            }
            _ => {}
        }
        Ok(())
    }

    /// Handle keys in the batched approval modal (multiple pending requests).
    ///
    /// `y`/`n` decide the selected request; `a`/`d` decide the whole batch at
    /// once. Each decision flows through `handle_tool_approval`, so approval
    /// recording and agent-owned execution semantics are identical to the
    /// single-call path.
    async fn handle_batched_approval_key(&mut self, code: KeyCode) -> Result<()> {
        match code {
            KeyCode::Up => self.approval_controller.select_prev(),
            KeyCode::Down => self.approval_controller.select_next(),
            KeyCode::Char('s' | 'S' | 'w' | 'W')
                if self
                    .approval_controller
                    .selected_request()
                    .is_some_and(|request| crate::mcp::McpClient::is_mcp_tool(&request.tool)) =>
            {
                let persistent = matches!(code, KeyCode::Char('w' | 'W'));
                if let Some((request, _decision)) = self
                    .approval_controller
                    .decide_selected(ApprovalDecision::Approve)
                {
                    if let Err(error) = self
                        .tool_executor
                        .remember_mcp_permission(&request.tool, persistent)
                    {
                        self.state.status = Some(self.state.locale.format(
                            "Approved once; could not remember MCP permission: {0}",
                            std::slice::from_ref(&(error)),
                        ));
                    }
                    self.handle_tool_approval(request.call_id, request.tool, request.args, true)
                        .await?;
                }
                if self.approval_controller.current().is_none() {
                    self.active_modal = ActiveModal::None;
                }
            }
            KeyCode::Char('y' | 'Y') | KeyCode::Enter => {
                if let Some((request, _decision)) = self
                    .approval_controller
                    .decide_selected(ApprovalDecision::Approve)
                {
                    self.handle_tool_approval(request.call_id, request.tool, request.args, true)
                        .await?;
                }
                if self.approval_controller.current().is_none() {
                    self.active_modal = ActiveModal::None;
                }
            }
            KeyCode::Char('n' | 'N') | KeyCode::Esc => {
                if let Some((request, _decision)) = self
                    .approval_controller
                    .decide_selected(ApprovalDecision::Deny)
                {
                    self.handle_tool_approval(request.call_id, request.tool, request.args, false)
                        .await?;
                }
                if self.approval_controller.current().is_none() {
                    self.active_modal = ActiveModal::None;
                }
            }
            KeyCode::Char('a' | 'A') => {
                // Approve all pending calls in FIFO order
                for (request, _decision) in self
                    .approval_controller
                    .decide_all(ApprovalDecision::Approve)
                {
                    self.handle_tool_approval(request.call_id, request.tool, request.args, true)
                        .await?;
                }
                self.active_modal = ActiveModal::None;
            }
            KeyCode::Char('d' | 'D') => {
                // Deny all pending calls in FIFO order
                for (request, _decision) in
                    self.approval_controller.decide_all(ApprovalDecision::Deny)
                {
                    self.handle_tool_approval(request.call_id, request.tool, request.args, false)
                        .await?;
                }
                self.active_modal = ActiveModal::None;
            }
            _ => {}
        }
        Ok(())
    }

    /// Handle keys in model selector modal
    pub(super) async fn handle_model_selector_key(
        &mut self,
        code: KeyCode,
        ctrl: bool,
    ) -> Result<()> {
        match code {
            KeyCode::Esc => {
                self.model_selector.hide();
                self.active_modal = ActiveModal::None;
            }
            KeyCode::Enter => {
                if let Some(model_id) = self.model_selector.confirm() {
                    self.switch_model(&model_id, false);
                }
                // Confirming the "show all models" row expands the list and
                // keeps the modal open.
                if !self.model_selector.is_visible() {
                    self.active_modal = ActiveModal::None;
                }
            }
            KeyCode::Char('d') if ctrl => {
                // Ctrl+D: persist the highlighted model as the user default
                // and switch to it.
                let model_id = self.model_selector.selected_model_id();
                if let Some(model_id) = model_id {
                    self.model_selector.hide();
                    self.switch_model(&model_id, true);
                    self.active_modal = ActiveModal::None;
                }
            }
            KeyCode::Tab => {
                self.model_selector.toggle_show_all();
            }
            KeyCode::Up => {
                self.model_selector.move_up();
            }
            KeyCode::Down => {
                self.model_selector.move_down();
            }
            KeyCode::Char(c) if !ctrl => {
                self.model_selector.insert_char(c);
            }
            KeyCode::Backspace => {
                self.model_selector.backspace();
            }
            KeyCode::Left => {
                self.model_selector.move_left();
            }
            KeyCode::Right => {
                self.model_selector.move_right();
            }
            _ => {}
        }
        Ok(())
    }

    /// Handle keys in theme selector modal
    pub(super) async fn handle_theme_selector_key(
        &mut self,
        code: KeyCode,
        ctrl: bool,
    ) -> Result<()> {
        let outcome = self.theme_selector.handle_key(code, ctrl);
        self.apply_theme_picker_outcome(outcome);
        Ok(())
    }

    /// A forced modal replacement cancels an unconfirmed theme transaction.
    pub(super) fn cancel_theme_preview(&mut self) {
        if self.theme_selector.is_visible() {
            if let Some(original) = self.theme_selector.original_theme() {
                crate::themes::set_theme(original.clone());
            }
            self.theme_selector.hide();
        }
    }

    fn apply_theme_picker_outcome(&mut self, outcome: maestro_ui::PickerOutcome<String>) {
        use maestro_ui::PickerOutcome;
        match self.theme_selector.theme_for(&outcome) {
            Ok(Some(theme)) => {
                crate::themes::set_theme(theme);
                if let PickerOutcome::Selected(name) = &outcome {
                    self.state.status = Some(
                        self.state
                            .locale
                            .format("Theme: {0}", std::slice::from_ref(name)),
                    );
                }
            }
            Ok(None) => {}
            Err(error) => {
                if let Some(original) = self.theme_selector.original_theme() {
                    crate::themes::set_theme(original.clone());
                }
                self.state.error = Some(
                    self.state
                        .locale
                        .format("Could not apply theme: {0}", &[(error).to_string()]),
                );
            }
        }
        if matches!(
            outcome,
            PickerOutcome::Selected(_) | PickerOutcome::Cancelled
        ) {
            self.active_modal = ActiveModal::None;
        }
    }

    pub(super) async fn handle_setup_modal_key(&mut self, code: KeyCode, ctrl: bool) -> Result<()> {
        use crate::components::SetupPage;
        use crate::telemetry::OnboardingStage;
        let before = self.setup_modal.page();
        match code {
            KeyCode::Esc if self.setup_modal.back() => {
                self.close_onboarding(false);
            }
            KeyCode::Enter => match self.setup_modal.confirm() {
                Some(SetupAdvance::StartEvalops) => self.start_setup_evalops_login(),
                Some(SetupAdvance::SaveKey {
                    provider_id,
                    secret,
                }) => self.finish_setup_api_key(provider_id, &secret),
                Some(SetupAdvance::ProfileSaved) => {
                    self.record_onboarding(OnboardingStage::ProfileSaved);
                }
                Some(SetupAdvance::StartChecks) => self.start_onboarding_checks(),
                Some(SetupAdvance::Finish) => {
                    self.close_onboarding(true);
                }
                None => {}
            },
            KeyCode::Char('d') if ctrl => {
                self.setup_modal.toggle_share_diagnostics();
                self.ui_prefs.onboarding_share_diagnostics =
                    Some(self.setup_modal.share_diagnostics());
                if self.ui_prefs.save_default().is_err() {
                    self.setup_modal.set_status(
                        self.state
                            .locale
                            .translate("Could not save the information-sharing preference."),
                    );
                }
            }
            KeyCode::Char('f') if !ctrl && before == SetupPage::Results => {
                self.close_onboarding(false);
                self.handle_bug_report("").await?;
            }
            KeyCode::PageUp => self.setup_modal.scroll_up(),
            KeyCode::PageDown => self.setup_modal.scroll_down(),
            KeyCode::Up => self.setup_modal.move_up(),
            KeyCode::Down => self.setup_modal.move_down(),
            KeyCode::Char(c) if !ctrl => self.setup_modal.insert_char(c),
            KeyCode::Backspace => self.setup_modal.backspace(),
            _ => {}
        }
        if before == SetupPage::Welcome && code == KeyCode::Enter {
            self.ui_prefs.onboarding_share_diagnostics = Some(self.setup_modal.share_diagnostics());
            if self.ui_prefs.save_default().is_err() {
                self.setup_modal.set_status(
                    self.state
                        .locale
                        .translate("Could not save the information-sharing preference."),
                );
            }
            self.record_onboarding(OnboardingStage::Started);
        }
        Ok(())
    }

    fn start_setup_evalops_login(&mut self) {
        if self.setup_login_rx.is_some() {
            return;
        }
        let (tx, rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let result = crate::init_cli::perform_evalops_login()
                .await
                .map_err(|error| format!("{error:#}"));
            let _ = tx.send(result);
        });
        self.setup_login_rx = Some(rx);
        self.setup_modal.set_waiting_evalops();
        self.state.add_system_message(
            self.state
                .locale
                .translate("Opening EvalOps login in the browser.")
                .to_string(),
        );
    }

    fn finish_setup_api_key(&mut self, provider_id: &str, secret: &str) {
        match crate::connections_cli::save_local_api_key(provider_id, secret) {
            Ok(connection_id) => {
                let default_model = crate::model_catalog::default_model_for_provider(provider_id)
                    .unwrap_or("gpt-5.6");
                let route =
                    crate::config::compose_model_route(Some(provider_id), Some(default_model))
                        .unwrap_or_else(|| format!("{provider_id}/{default_model}"));
                if let Err(error) =
                    crate::config_cli::persist_user_model_and_provider(default_model, provider_id)
                {
                    self.setup_modal.set_status(self.state.locale.format(
                        "Saved key, but could not write config.toml: {0}",
                        &[(error).to_string()],
                    ));
                    return;
                }
                self.switch_model(&route, false);
                self.setup_modal.set_connection_ready();
                self.record_onboarding(crate::telemetry::OnboardingStage::ConnectionSaved);
                if self.native_agent.is_none() {
                    self.pending_agent_spawn = true;
                }
                self.state.add_system_message(self.state.locale.format(
                    "Saved {0} key as {1}. Model set to {2}.",
                    &[
                        (provider_id).to_string(),
                        (connection_id).clone(),
                        (route).clone(),
                    ],
                ));
            }
            Err(error) => self.setup_modal.set_status(error.to_string()),
        }
    }

    /// Handle keyboard shortcuts help key events
    pub(super) async fn handle_shortcuts_help_key(&mut self, code: KeyCode) -> Result<()> {
        match code {
            KeyCode::Esc | KeyCode::F(1) => {
                self.shortcuts_help.hide();
                self.active_modal = ActiveModal::None;
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.shortcuts_help.scroll_up(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.shortcuts_help.scroll_down(1);
            }
            KeyCode::PageUp => {
                self.shortcuts_help.scroll_up(10);
            }
            KeyCode::PageDown => {
                self.shortcuts_help.scroll_down(10);
            }
            _ => {}
        }
        Ok(())
    }

    /// Handle tool approval decision
    pub(super) async fn handle_tool_approval(
        &mut self,
        call_id: String,
        _tool: String,
        _args: serde_json::Value,
        approved: bool,
    ) -> Result<()> {
        self.tool_history.record_approval(&call_id, approved);
        if approved {
            // Relay only the decision. The native agent executes approved
            // calls in model order and emits their lifecycle events. Starting
            // execution here would let batched approvals run concurrently and
            // reorder a gated write/edit relative to later calls.
            if let Some(tx) = &self.tool_response_tx {
                let _ = tx.send((call_id, true, None, ExecutionSource::Native, None));
            }
        } else {
            self.tool_history
                .fail(&call_id, self.state.locale.translate("Denied").to_string());
            // Move the transcript row out of `Pending` on denial.
            self.state
                .fail_tool_call(&call_id, self.state.locale.translate("Denied"));
            // Send denial
            if let Some(tx) = &self.tool_response_tx {
                let _ = tx.send((call_id, false, None, ExecutionSource::Native, None));
            }
        }
        Ok(())
    }
}

fn mcp_scope_arg(scope: crate::mcp::McpConfigScope) -> &'static str {
    match scope {
        crate::mcp::McpConfigScope::User => "user",
        crate::mcp::McpConfigScope::Local => "local",
        crate::mcp::McpConfigScope::Project => "project",
        crate::mcp::McpConfigScope::Managed => "managed",
        crate::mcp::McpConfigScope::Enterprise => "enterprise",
    }
}

fn mcp_manager_entry_is_mutable(status: &crate::tools::McpServerStatus) -> bool {
    !matches!(
        status.scope,
        crate::mcp::McpConfigScope::Managed | crate::mcp::McpConfigScope::Enterprise
    ) && status.state != crate::tools::McpLifecycleState::ConfigError
}

fn is_focus_view_binding(code: KeyCode, modifiers: CrosstermModifiers) -> bool {
    modifiers.contains(CrosstermModifiers::CONTROL)
        && modifiers.contains(CrosstermModifiers::ALT)
        && matches!(code, KeyCode::Char('f' | 'F'))
}

/// Build the full-content body for expanding an approval request: the reason,
/// the complete (unclipped) command, and the pretty-printed arguments.
fn approval_detail_content(request: &ApprovalRequest) -> String {
    let mut sections = Vec::new();
    if let Some(reason) = &request.reason {
        sections.push(maestro_ui::localization::format(
            "Reason:\n{0}",
            std::slice::from_ref(reason),
        ));
    }
    sections.push(maestro_ui::localization::format(
        "Command:\n{0}",
        &[(request.display_command()).clone()],
    ));
    if let Some(source) = &request.command_source {
        sections.push(maestro_ui::localization::format(
            "Source and execution context:\n{0}",
            std::slice::from_ref(source),
        ));
    }
    sections.push(maestro_ui::localization::format(
        "Args:\n{0}",
        &[(request.display_args_pretty()).clone()],
    ));
    sections.join("\n\n")
}

#[cfg(test)]
mod focus_view_tests {
    use super::*;

    #[test]
    fn focus_view_binding_requires_control_and_alt() {
        assert!(is_focus_view_binding(
            KeyCode::Char('f'),
            CrosstermModifiers::CONTROL | CrosstermModifiers::ALT
        ));
        assert!(!is_focus_view_binding(
            KeyCode::Char('f'),
            CrosstermModifiers::CONTROL
        ));
        assert!(!is_focus_view_binding(
            KeyCode::Char('f'),
            CrosstermModifiers::ALT
        ));
    }
}

#[cfg(test)]
mod model_palette_tests {
    use super::*;

    #[test]
    fn current_status_canonicalizes_bare_and_aliased_mirrored_routes() {
        let models = crate::model_catalog::available_models();
        let google = models
            .iter()
            .find(|model| model.provider == "google" && model.id == "gemini-2.5-pro")
            .expect("Google Gemini row");
        let vertex = models
            .iter()
            .find(|model| model.provider == "vertex-ai" && model.id == "gemini-2.5-pro")
            .expect("Vertex Gemini row");

        for (current, expected_provider) in [
            ("gemini-2.5-pro", "google"),
            ("gemini/gemini-2.5-pro", "google"),
            ("vertex/gemini-2.5-pro", "vertex-ai"),
        ] {
            let current_route =
                crate::components::model_selector::canonical_current_route(current, &models);
            let google_resource = model_palette_resource(
                google,
                current_route.as_deref(),
                crate::localization::Locale::English,
            );
            let vertex_resource = model_palette_resource(
                vertex,
                current_route.as_deref(),
                crate::localization::Locale::English,
            );

            assert_eq!(
                google_resource.status.as_deref(),
                (expected_provider == "google").then_some("current"),
                "current route {current}"
            );
            assert_eq!(
                vertex_resource.status.as_deref(),
                (expected_provider == "vertex-ai").then_some("current"),
                "current route {current}"
            );
        }
    }
    #[test]
    fn model_metadata_uses_selected_locale_and_preserves_route() {
        let mut model = crate::model_catalog::available_models()[0].clone();
        model.verification.state = crate::model_catalog::VerificationState::Catalog;
        let route = crate::model_catalog::model_route(&model);
        for locale in crate::localization::Locale::ALL {
            let resource =
                crate::localization::with_locale(crate::localization::Locale::English, || {
                    model_palette_resource(&model, Some(&route), locale)
                });
            assert_eq!(resource.id, route);
            assert_eq!(
                resource.status.as_deref(),
                Some(locale.translate("current"))
            );
            let description = resource.description.unwrap();
            assert!(description.contains(&model.provider));
            assert!(description.contains(locale.translate("Catalog")));
            if locale != crate::localization::Locale::English {
                assert!(!description.contains("k context"));
            }
        }
    }
}

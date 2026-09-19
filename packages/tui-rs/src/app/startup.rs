//! Keep network verification and extension discovery off the terminal thread.
//! The normal shell and model picker remain usable while preparation is pending,
//! but no dispatcher, agent, tool executor, or MCP connection can run before
//! policy is resolved.

use super::*;
use std::sync::mpsc::{Receiver, TryRecvError};

pub(super) struct PreparedStartup {
    pub local_discovery: Option<(
        crate::local_models::LocalDiscoveryHandle,
        Receiver<crate::local_models::LocalDiscoveryBatch>,
    )>,
    pub config: crate::config::ComposerConfig,
    pub plugin_registry: PluginRegistry,
    pub loaded_skills: Vec<LoadedSkill>,
    pub skill_load_errors: Vec<SkillLoadError>,
    pub custom_prompts: Vec<PromptDefinition>,
    pub exec_commands: Vec<crate::exec_commands::ExecCommand>,
    pub managed_setup: crate::managed_setup::ManagedSetupClient,
    pub managed_setup_identity_scope: Option<crate::telemetry::TelemetryIdentityScope>,
}

impl PreparedStartup {
    pub(super) fn load(session: PlatformSessionResolution) -> Self {
        Self::load_for_model(session, &crate::codex_auth::resolve_default_model())
    }

    fn load_for_model(session: PlatformSessionResolution, model: &str) -> Self {
        let workspace = std::env::current_dir().unwrap_or_else(|_| ".".into());
        let config = crate::config::load_config(&workspace, None);
        let plugin_registry = PluginRegistry::discover();
        let (loaded_skills, skill_load_errors) =
            SkillLoader::with_plugins(&plugin_registry).load_all_with_paths();
        let dirs = plugin_registry.command_paths();
        let custom_prompts = crate::prompts::load_prompts_with_plugin_dirs(&workspace, &dirs);
        let exec_commands = crate::exec_commands::discover_with_plugin_dirs(&workspace, &dirs);
        let (managed_setup, managed_setup_identity_scope) = match session {
            PlatformSessionResolution::Detect => resolve_startup_managed_setup(
                model,
                crate::credential_mode::current_verified_identity_session,
                || match crate::credential_mode::detect() {
                    Ok(crate::credential_mode::DetectedMode::Platform(session)) => Some(session),
                    _ => None,
                },
            ),
            #[cfg(test)]
            PlatformSessionResolution::UseNoPlatformSession => {
                (crate::managed_setup::ManagedSetupClient::unmanaged(), None)
            }
        };
        Self {
            local_discovery: None,
            config,
            plugin_registry,
            loaded_skills,
            skill_load_errors,
            custom_prompts,
            exec_commands,
            managed_setup,
            managed_setup_identity_scope,
        }
    }
}

pub(super) struct StartupHandoff {
    pub prepared: PreparedStartup,
    pub textarea: crate::components::textarea::TextArea,
    pub model: String,
    pub model_changed: bool,
    pub resume_session: Option<crate::session::SessionInfo>,
    pub local_discovery_handle: crate::local_models::LocalDiscoveryHandle,
    pub local_discovery_rx: Receiver<crate::local_models::LocalDiscoveryBatch>,
    pub local_discovery_batch: Option<crate::local_models::LocalDiscoveryBatch>,
}

#[derive(Debug)]
struct StartupModelChoice {
    model: String,
    persist_default: bool,
}

fn resolve_startup_managed_setup(
    model: &str,
    verified: impl FnOnce() -> anyhow::Result<crate::credential_mode::PlatformSession>,
    unverified: impl FnOnce() -> Option<crate::credential_mode::PlatformSession>,
) -> (
    crate::managed_setup::ManagedSetupClient,
    Option<crate::telemetry::TelemetryIdentityScope>,
) {
    if maestro_local_host::safety::vendor_network_disabled()
        || crate::local_models::is_local_model_route(model)
    {
        let stored = unverified();
        return (
            crate::managed_setup::ManagedSetupClient::offline_local(stored.as_ref()),
            None,
        );
    }
    resolve_verified_managed_setup(verified(), unverified)
}

fn preparation<T: Send + 'static>(
    load: impl FnOnce() -> T + Send + 'static,
) -> Result<Receiver<T>> {
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("maestro-startup".into())
        .spawn(move || {
            // A closed receiver means the user cancelled. This worker never owns
            // the terminal and cannot re-enable raw mode after restoration.
            let _ = tx.send(load());
        })?;
    Ok(rx)
}

pub(super) fn prepare_with_composer(
    terminal: &mut terminal::Terminal,
    capabilities: &mut TerminalCapabilities,
    events: &mut Option<TerminalEventReader>,
) -> Result<StartupHandoff> {
    let initial_model = crate::codex_auth::resolve_default_model();
    let mut selected_model = initial_model.clone();
    let mut rx = start_preparation(&selected_model)?;
    let (local_discovery, local_discovery_rx) = crate::local_models::spawn_local_model_discovery();
    local_discovery.refresh();
    let mut latest_local_discovery = None;
    let mut resume_session = None;
    let mut pending_session: Option<Receiver<Result<crate::session::SessionInfo>>> = None;
    let mut sessions = SessionSwitcher::new(std::env::current_dir()?.to_string_lossy().as_ref());
    let terminal_info = crate::terminal_info::TerminalInfo::get();
    let model_binding =
        load_rust_tui_keybindings(&terminal_info.name, std::env::var_os("TMUX").is_some())
            .cycle_model;
    let mut state = AppState::new();
    state.locale = crate::ui_prefs::UiPrefs::load_default().locale();
    state.model = Some(selected_model.clone());
    let mut model_selector = ModelSelector::new();
    model_selector.set_current_model(Some(selected_model.clone()));
    update_startup_status(&mut state, None, &selected_model, &model_binding.display());
    render_startup_shell(
        terminal,
        capabilities,
        &state,
        &mut model_selector,
        &mut sessions,
    )?;

    loop {
        if let Some(pending) = &pending_session {
            match pending.try_recv() {
                Ok(Ok(session)) => {
                    selected_model = session.model.clone();
                    state.model = Some(selected_model.clone());
                    model_selector.set_current_model(Some(selected_model.clone()));
                    rx = start_preparation(&selected_model)?;
                    resume_session = Some(session);
                    pending_session = None;
                }
                Ok(Err(error)) => {
                    sessions.show_error(
                        state
                            .locale
                            .format("Failed to load session: {0}", &[error.to_string()]),
                    );
                    pending_session = None;
                    render_startup_shell(
                        terminal,
                        capabilities,
                        &state,
                        &mut model_selector,
                        &mut sessions,
                    )?;
                }
                Err(TryRecvError::Disconnected) => {
                    sessions.show_error(
                        state
                            .locale
                            .translate("Session loading stopped. Try again.")
                            .to_owned(),
                    );
                    pending_session = None;
                    render_startup_shell(
                        terminal,
                        capabilities,
                        &state,
                        &mut model_selector,
                        &mut sessions,
                    )?;
                }
                Err(TryRecvError::Empty) => {}
            }
        }
        if sessions.poll_refresh() {
            render_startup_shell(
                terminal,
                capabilities,
                &state,
                &mut model_selector,
                &mut sessions,
            )?;
        }
        while let Ok(batch) = local_discovery_rx.try_recv() {
            if model_selector.apply_discovery(&batch) {
                latest_local_discovery = Some(batch);
                update_startup_status(
                    &mut state,
                    latest_local_discovery.as_ref(),
                    &selected_model,
                    &model_binding.display(),
                );
                render_startup_shell(
                    terminal,
                    capabilities,
                    &state,
                    &mut model_selector,
                    &mut sessions,
                )?;
            }
        }

        if !model_selector.is_visible() && !sessions.is_visible() && pending_session.is_none() {
            match rx.try_recv() {
                Ok(prepared) => {
                    return Ok(StartupHandoff {
                        prepared,
                        textarea: state.textarea,
                        model: selected_model.clone(),
                        model_changed: selected_model != initial_model,
                        resume_session,
                        local_discovery_handle: local_discovery,
                        local_discovery_rx,
                        local_discovery_batch: latest_local_discovery,
                    });
                }
                Err(TryRecvError::Disconnected) => bail!("Startup preparation failed"),
                Err(TryRecvError::Empty) => {}
            }
        }

        let next = if let Some(reader) = events {
            reader
                .poll(Duration::from_millis(16))
                .map_err(anyhow::Error::from)?
        } else if event::poll(Duration::from_millis(16))? {
            AppTerminalEvent::from_crossterm(event::read()?)
        } else {
            None
        };
        let Some(event) = next else {
            continue;
        };
        if !model_selector.is_visible()
            && handle_startup_sessions(&mut state, &mut sessions, &event)?
        {
            render_startup_shell(
                terminal,
                capabilities,
                &state,
                &mut model_selector,
                &mut sessions,
            )?;
            continue;
        }
        if sessions.is_visible() {
            // Enter selects metadata only. The full app reopens the transcript
            // under its existing lock and admission path before resuming it.
            if let AppTerminalEvent::Key(key) = &event {
                if should_handle_key_event(key.kind) && key.code == KeyCode::Enter {
                    if let Some(session) = sessions.selected_session().cloned() {
                        pending_session = Some(preparation(move || load_startup_session(session))?);
                        sessions.hide();
                    }
                }
            }
            render_startup_shell(
                terminal,
                capabilities,
                &state,
                &mut model_selector,
                &mut sessions,
            )?;
            continue;
        }
        if matches!(&event, AppTerminalEvent::Key(key) if should_handle_key_event(key.kind) && key.code == KeyCode::Char('r') && key.modifiers.contains(CrosstermModifiers::CONTROL))
        {
            local_discovery.refresh();
            model_selector.mark_local_refreshing();
        }
        if let Some(choice) =
            edit_startup_shell(&mut state, &mut model_selector, model_binding, event)?
        {
            if choice.persist_default {
                if let Err(error) = crate::config_cli::persist_user_model_default(&choice.model) {
                    state.status = Some(
                        state
                            .locale
                            .format("Failed to save default model: {0}", &[(error).to_string()]),
                    );
                    render_startup_shell(
                        terminal,
                        capabilities,
                        &state,
                        &mut model_selector,
                        &mut sessions,
                    )?;
                    continue;
                }
            }
            if choice.model != selected_model {
                // Replacing the receiver closes the old preparation channel.
                // A late cloud result cannot overwrite the visible selection.
                resume_session = None;
                pending_session = None;
                selected_model = choice.model;
                state.model = Some(selected_model.clone());
                state.thinking_level = crate::model_dynamics::normalize_thinking(
                    &selected_model,
                    state.thinking_level,
                );
                model_selector.set_current_model(Some(selected_model.clone()));
                rx = start_preparation(&selected_model)?;
            }
            update_startup_status(
                &mut state,
                latest_local_discovery.as_ref(),
                &selected_model,
                &model_binding.display(),
            );
        }
        render_startup_shell(
            terminal,
            capabilities,
            &state,
            &mut model_selector,
            &mut sessions,
        )?;
    }
}

fn load_startup_session(
    mut session: crate::session::SessionInfo,
) -> Result<crate::session::SessionInfo> {
    // The index deliberately omits model metadata. Read the selected header off
    // the input thread; resume_session_path later rereads under the writer lock.
    // This value selects a preparation route, never grants authority.
    let (header, _, _) = crate::session::SessionReader::read_header(&session.path)?;
    session.model = header.model;
    session.thinking_level = header.thinking_level;
    Ok(session)
}

fn start_preparation(model: &str) -> Result<Receiver<PreparedStartup>> {
    let model = model.to_owned();
    preparation(move || PreparedStartup::load_for_model(PlatformSessionResolution::Detect, &model))
}

fn update_startup_status(
    state: &mut AppState,
    discovery: Option<&crate::local_models::LocalDiscoveryBatch>,
    selected_model: &str,
    model_binding: &str,
) {
    let local_status = match discovery {
        None => state
            .locale
            .translate("checking local runtimes")
            .to_string(),
        Some(batch) if batch.models.is_empty() => state
            .locale
            .translate("no local models detected")
            .to_string(),
        Some(batch) => state.locale.format(
            "{0} local models available",
            std::slice::from_ref(&batch.models.len().to_string()),
        ),
    };
    let route_status = if crate::local_models::is_local_model_route(selected_model) {
        state
            .locale
            .translate("preparing a local session")
            .to_string()
    } else {
        state
            .locale
            .translate("preparing secure access")
            .to_string()
    };
    state.status = Some(state.locale.format(
        "Starting… {0} · {1} · {2} or /model to choose",
        &[route_status, local_status, model_binding.to_string()],
    ));
}

fn render_startup_shell(
    terminal: &mut terminal::Terminal,
    capabilities: &mut TerminalCapabilities,
    state: &AppState,
    model_selector: &mut ModelSelector,
    sessions: &mut SessionSwitcher,
) -> Result<()> {
    let size = terminal.size()?;
    let (top, height) = terminal::calculate_viewport(size.height);
    if capabilities.viewport_top != top || capabilities.viewport_height != height {
        *terminal = terminal::recreate_with_viewport(height)?;
        capabilities.viewport_top = top;
        capabilities.viewport_height = height;
    }
    terminal.draw(|frame| {
        crate::localization::with_locale(state.locale, || {
            let area = frame.area();
            frame.render_widget(
                ChatView::new(state).with_footer_style(FooterStyle::Rich),
                area,
            );
            let status_height = u16::from(!state.zen_mode);
            let input_height = calculate_input_height(state, area);
            let input_area = Rect {
                x: area.x,
                y: area
                    .y
                    .saturating_add(area.height.saturating_sub(status_height + input_height)),
                width: area.width,
                height: input_height,
            };
            if input_area.y > area.y {
                let notice_area = Rect::new(
                    area.x.saturating_add(1),
                    input_area.y - 1,
                    area.width.saturating_sub(2),
                    1,
                );
                frame.render_widget(ratatui::widgets::Clear, notice_area);
                frame.render_widget(
                    ratatui::widgets::Paragraph::new(state.status.as_deref().unwrap_or_default())
                        .style(crate::themes::current_ui_theme().muted_style()),
                    notice_area,
                );
            }
            if sessions.is_visible() {
                sessions.render(frame, area);
            } else if model_selector.is_visible() {
                model_selector.render(frame, area);
            } else {
                let input = ChatInputWidget::new(
                    &state.textarea,
                    ChatInputWidgetOptions {
                        busy: false,
                        pending_input_preview: None,
                        ghost_text: None,
                    },
                );
                if let Some(cursor) = input.cursor_pos(input_area) {
                    frame.set_cursor_position(cursor);
                }
            }
        });
    })?;
    Ok(())
}

/// Returns true when a history navigation event was consumed. Enter is left
/// to the caller so selection never creates an executable dispatcher here.
fn handle_startup_sessions(
    state: &mut AppState,
    sessions: &mut SessionSwitcher,
    event: &AppTerminalEvent,
) -> Result<bool> {
    if let AppTerminalEvent::Resize { width, .. } = event {
        state.set_input_width(crate::components::composer_editor_width(*width));
    }
    if let AppTerminalEvent::Key(key) = event {
        if !should_handle_key_event(key.kind) {
            return Ok(sessions.is_visible());
        }
        let ctrl = key.modifiers.contains(CrosstermModifiers::CONTROL);
        let alt = key.modifiers.contains(CrosstermModifiers::ALT);
        if ctrl && key.code == KeyCode::Char('c') {
            bail!("Startup cancelled");
        }
        if !sessions.is_visible() {
            if (ctrl && alt && key.code == KeyCode::Char('r'))
                || (key.code == KeyCode::Enter
                    && key.modifiers.is_empty()
                    && matches!(state.input().trim(), "/sessions" | "/resume"))
            {
                if key.code == KeyCode::Enter {
                    state.set_input("");
                }
                sessions.show();
                return Ok(true);
            }
            return Ok(false);
        }
        match key.code {
            KeyCode::Enter => return Ok(false),
            KeyCode::Esc => sessions.hide(),
            KeyCode::Up => sessions.move_up(),
            KeyCode::Down => sessions.move_down(),
            KeyCode::Backspace => sessions.backspace(),
            KeyCode::Char('r') if ctrl => sessions.refresh_async(),
            KeyCode::Delete => {
                if let Err(error) = sessions.delete_selected() {
                    state.status = Some(error);
                }
            }
            KeyCode::Char(c) if !ctrl && !alt => sessions.insert_char(c),
            _ => {}
        }
        return Ok(true);
    }
    if let AppTerminalEvent::Paste(text) = event {
        if sessions.is_visible() {
            sessions.insert_str(text);
            return Ok(true);
        }
    }
    Ok(sessions.is_visible())
}

#[cfg(test)]
fn wait_with_composer<T>(
    rx: &Receiver<T>,
    state: &mut AppState,
    mut render: impl FnMut(&AppState) -> Result<()>,
    mut next_event: impl FnMut() -> Result<Option<AppTerminalEvent>>,
) -> Result<T> {
    render(state)?;
    loop {
        match rx.try_recv() {
            Ok(prepared) => return Ok(prepared),
            Err(TryRecvError::Disconnected) => bail!("Startup preparation failed"),
            Err(TryRecvError::Empty) => {}
        }
        if let Some(event) = next_event()? {
            edit_draft(state, event)?;
            render(state)?;
        }
    }
}

fn edit_startup_shell(
    state: &mut AppState,
    model_selector: &mut ModelSelector,
    model_binding: crate::key_hints::KeyBinding,
    event: AppTerminalEvent,
) -> Result<Option<StartupModelChoice>> {
    // Cancellation and resize apply even when a modal owns keyboard focus.
    if let AppTerminalEvent::Key(key) = &event {
        if should_handle_key_event(key.kind)
            && key.code == KeyCode::Char('c')
            && key.modifiers.contains(CrosstermModifiers::CONTROL)
        {
            bail!("Startup cancelled");
        }
    }
    if let AppTerminalEvent::Resize { width, .. } = &event {
        state.set_input_width(crate::components::composer_editor_width(*width));
    }
    if model_selector.is_visible() {
        match event {
            AppTerminalEvent::Paste(text) => model_selector.insert_str(&text),
            AppTerminalEvent::Key(key) if should_handle_key_event(key.kind) => {
                let ctrl = key.modifiers.contains(CrosstermModifiers::CONTROL);
                match key.code {
                    KeyCode::Esc => model_selector.hide(),
                    KeyCode::Enter => {
                        return Ok(model_selector.confirm().map(|model| StartupModelChoice {
                            model,
                            persist_default: false,
                        }));
                    }
                    KeyCode::Char('d') if ctrl => {
                        return Ok(model_selector.confirm().map(|model| StartupModelChoice {
                            model,
                            persist_default: true,
                        }));
                    }
                    KeyCode::Tab => model_selector.toggle_show_all(),
                    KeyCode::Up => model_selector.move_up(),
                    KeyCode::Down => model_selector.move_down(),
                    KeyCode::Left => model_selector.move_left(),
                    KeyCode::Right => model_selector.move_right(),
                    KeyCode::Backspace => model_selector.backspace(),
                    KeyCode::Char(c) if !ctrl => model_selector.insert_char(c),
                    _ => {}
                }
            }
            _ => {}
        }
        return Ok(None);
    }

    match event {
        AppTerminalEvent::Paste(text) => state.insert_paste(&text),
        AppTerminalEvent::Resize { width, .. } => {
            state.set_input_width(crate::components::composer_editor_width(width));
        }
        AppTerminalEvent::Key(key) if should_handle_key_event(key.kind) => {
            if model_binding.matches(&key) {
                model_selector.show();
                return Ok(None);
            }
            match (key.code, key.modifiers) {
                (KeyCode::Char('c' | 'd'), m) if m.contains(CrosstermModifiers::CONTROL) => {
                    bail!("Startup cancelled");
                }
                (KeyCode::Char(c), m)
                    if !m.intersects(CrosstermModifiers::CONTROL | CrosstermModifiers::ALT) =>
                {
                    state.insert_char(c);
                }
                (KeyCode::Backspace, _) => state.backspace(),
                (KeyCode::Delete, _) => state.delete(),
                (KeyCode::Left, _) => state.move_left(),
                (KeyCode::Right, _) => state.move_right(),
                (KeyCode::Up, _) => state.move_up(),
                (KeyCode::Down, _) => state.move_down(),
                (KeyCode::Home, _) => state.move_home(),
                (KeyCode::End, _) => state.move_end(),
                (KeyCode::Enter, m) if m.contains(CrosstermModifiers::SHIFT) => {
                    state.insert_char('\n');
                }
                (KeyCode::Enter, _) if state.input().trim() == "/model" => {
                    state.set_input("");
                    model_selector.show();
                }
                (KeyCode::Enter, _) => {
                    state.status = Some(
                        state
                            .locale
                            .translate(
                                "Still starting. Your draft is here; press Enter when ready.",
                            )
                            .into(),
                    );
                }
                _ => {}
            }
        }
        _ => {}
    }
    Ok(None)
}

#[cfg(test)]
fn edit_draft(state: &mut AppState, event: AppTerminalEvent) -> Result<()> {
    let mut selector = ModelSelector::new();
    edit_startup_shell(
        state,
        &mut selector,
        crate::key_hints::ctrl(KeyCode::Char('p')),
        event,
    )
    .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn local_model_startup_does_not_resolve_live_identity_or_bind_tenant_scope() {
        for model in ["llamacpp/qwen3", "lmstudio/gemma", "ollama/llama3.2"] {
            let verified_calls = Cell::new(0);
            let unverified_calls = Cell::new(0);
            let (managed_setup, identity_scope) = resolve_startup_managed_setup(
                model,
                || {
                    verified_calls.set(verified_calls.get() + 1);
                    panic!("local startup must not contact Identity")
                },
                || {
                    unverified_calls.set(unverified_calls.get() + 1);
                    Some(crate::credential_mode::PlatformSession {
                        access_token: "stale-token".to_owned(),
                        organization_id: "org-test".to_owned(),
                        workspace_id: Some("workspace-test".to_owned()),
                        provider_ref: serde_json::json!({}),
                        email: None,
                        user_id: None,
                    })
                },
            );
            assert_eq!(
                managed_setup.origin(),
                crate::managed_setup::ManagedSetupOrigin::FailedClosed
            );
            assert!(identity_scope.is_none());
            assert_eq!(verified_calls.get(), 0);
            assert_eq!(unverified_calls.get(), 1);
        }
    }

    #[test]
    fn startup_renders_and_edits_before_preparation_is_released() {
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let rx = preparation(move || {
            release_rx.recv().unwrap();
            42
        })
        .unwrap();
        let mut state = AppState::new();
        let mut events = VecDeque::from([
            AppTerminalEvent::Paste("draft é\r\nsecond line".into()),
            AppTerminalEvent::Key(event::KeyEvent::new(
                KeyCode::Enter,
                CrosstermModifiers::NONE,
            )),
        ]);
        let mut frames = Vec::new();
        let result = wait_with_composer(
            &rx,
            &mut state,
            |state| {
                frames.push(state.input().to_owned());
                Ok(())
            },
            || {
                if let Some(event) = events.pop_front() {
                    return Ok(Some(event));
                }
                let _ = release_tx.send(());
                std::thread::yield_now();
                Ok(None)
            },
        )
        .unwrap();
        assert_eq!(result, 42);
        assert_eq!(frames[0], "");
        assert_eq!(state.input(), "draft é\nsecond line");
        assert!(frames.iter().any(|frame| frame == "draft é\nsecond line"));
        assert!(state.status.as_deref().unwrap().contains("press Enter"));
    }

    #[test]
    fn startup_model_picker_preserves_unicode_draft_and_cursor() {
        let binding = crate::key_hints::ctrl(KeyCode::Char('p'));
        let mut state = AppState::new();
        state.set_input("draft é");
        state.move_left();
        let original_cursor = state.cursor();
        let mut selector = ModelSelector::new();
        selector.set_current_model(Some("ollama/qwen3".to_owned()));

        let opened = edit_startup_shell(
            &mut state,
            &mut selector,
            binding,
            AppTerminalEvent::Key(event::KeyEvent::new(
                KeyCode::Char('p'),
                CrosstermModifiers::CONTROL,
            )),
        )
        .unwrap();
        assert!(opened.is_none());
        assert!(selector.is_visible());
        assert_eq!(state.input(), "draft é");
        assert_eq!(state.cursor(), original_cursor);

        let choice = edit_startup_shell(
            &mut state,
            &mut selector,
            binding,
            AppTerminalEvent::Key(event::KeyEvent::new(
                KeyCode::Enter,
                CrosstermModifiers::NONE,
            )),
        )
        .unwrap()
        .expect("active model should be selectable");
        assert_eq!(choice.model, "ollama/qwen3");
        assert!(!choice.persist_default);
        assert_eq!(state.input(), "draft é");
        assert_eq!(state.cursor(), original_cursor);
    }

    #[test]
    fn startup_history_shortcut_preserves_the_draft_and_escape_restores_focus() {
        let mut state = AppState::new();
        state.set_input("draft 東京");
        state.move_left();
        let cursor = state.cursor();
        let mut sessions = SessionSwitcher::new("/tmp");
        let open = AppTerminalEvent::Key(event::KeyEvent::new(
            KeyCode::Char('r'),
            CrosstermModifiers::CONTROL | CrosstermModifiers::ALT,
        ));
        assert!(handle_startup_sessions(&mut state, &mut sessions, &open).unwrap());
        assert!(sessions.is_visible());
        assert_eq!(state.input(), "draft 東京");
        assert_eq!(state.cursor(), cursor);
        let close =
            AppTerminalEvent::Key(event::KeyEvent::new(KeyCode::Esc, CrosstermModifiers::NONE));
        assert!(handle_startup_sessions(&mut state, &mut sessions, &close).unwrap());
        assert!(!sessions.is_visible());
        assert_eq!(state.cursor(), cursor);
    }

    #[test]
    fn startup_cancel_works_while_model_picker_has_focus() {
        let mut state = AppState::new();
        let mut selector = ModelSelector::new();
        selector.show();
        let error = edit_startup_shell(
            &mut state,
            &mut selector,
            crate::key_hints::ctrl(KeyCode::Char('p')),
            AppTerminalEvent::Key(event::KeyEvent::new(
                KeyCode::Char('c'),
                CrosstermModifiers::CONTROL,
            )),
        )
        .unwrap_err();
        assert!(error.to_string().contains("Startup cancelled"));
    }

    #[test]
    fn startup_model_command_opens_picker_without_submitting() {
        let mut state = AppState::new();
        state.set_input("/model");
        let mut selector = ModelSelector::new();
        let choice = edit_startup_shell(
            &mut state,
            &mut selector,
            crate::key_hints::ctrl(KeyCode::Char('p')),
            AppTerminalEvent::Key(event::KeyEvent::new(
                KeyCode::Enter,
                CrosstermModifiers::NONE,
            )),
        )
        .unwrap();
        assert!(choice.is_none());
        assert!(selector.is_visible());
        assert!(state.input().is_empty());
    }

    #[test]
    fn startup_cancellation_does_not_wait_for_preparation() {
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let rx = preparation(move || {
            release_rx.recv().unwrap();
        })
        .unwrap();
        let result = wait_with_composer(
            &rx,
            &mut AppState::new(),
            |_| Ok(()),
            || {
                Ok(Some(AppTerminalEvent::Key(event::KeyEvent::new(
                    KeyCode::Char('c'),
                    CrosstermModifiers::CONTROL,
                ))))
            },
        );
        assert!(result.unwrap_err().to_string().contains("cancelled"));
        release_tx.send(()).unwrap();
    }

    #[test]
    fn startup_worker_failure_is_not_readiness() {
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        drop(tx);
        assert!(
            wait_with_composer(
                &rx,
                &mut AppState::new(),
                |_| Ok(()),
                || panic!("must fail closed")
            )
            .is_err()
        );
    }
}

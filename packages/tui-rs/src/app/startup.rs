//! Keep network verification and extension discovery off the terminal thread.
//! Only the composer exists while preparation is pending: no command dispatcher,
//! agent, tool executor, or MCP connection can run before policy is resolved.

use super::*;
use std::sync::mpsc::{Receiver, TryRecvError};

pub(super) struct PreparedStartup {
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
        let workspace = std::env::current_dir().unwrap_or_else(|_| ".".into());
        let config = crate::config::load_config(&workspace, None);
        let plugin_registry = PluginRegistry::discover();
        let (loaded_skills, skill_load_errors) =
            SkillLoader::with_plugins(&plugin_registry).load_all_with_paths();
        let dirs = plugin_registry.command_paths();
        let custom_prompts = crate::prompts::load_prompts_with_plugin_dirs(&workspace, &dirs);
        let exec_commands = crate::exec_commands::discover_with_plugin_dirs(&workspace, &dirs);
        let (managed_setup, managed_setup_identity_scope) = match session {
            PlatformSessionResolution::Detect => resolve_verified_managed_setup(
                crate::credential_mode::current_verified_identity_session(),
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
) -> Result<(PreparedStartup, crate::components::textarea::TextArea)> {
    let rx = preparation(|| PreparedStartup::load(PlatformSessionResolution::Detect))?;
    let mut state = AppState::new();
    state.locale = crate::ui_prefs::UiPrefs::load_default().locale();
    state.status = Some(
        state
            .locale
            .translate("Starting… You can type while setup finishes.")
            .into(),
    );
    wait_with_composer(
        &rx,
        &mut state,
        |state| {
            let size = terminal.size()?;
            let (top, height) = terminal::calculate_viewport(size.height);
            if capabilities.viewport_top != top || capabilities.viewport_height != height {
                *terminal = terminal::recreate_with_viewport(height)?;
                capabilities.viewport_top = top;
                capabilities.viewport_height = height;
            }
            terminal.draw(|frame| {
                let area = frame.area();
                let input_height =
                    calculate_input_height(state, area).min(area.height.saturating_sub(1));
                let input_area = Rect {
                    y: area
                        .y
                        .saturating_add(area.height.saturating_sub(1 + input_height)),
                    height: input_height,
                    ..area
                };
                let input = ChatInputWidget::new(
                    &state.textarea,
                    ChatInputWidgetOptions {
                        busy: false,
                        pending_input_preview: None,
                        ghost_text: None,
                    },
                );
                let cursor = input.cursor_pos(input_area);
                frame.render_widget(input, input_area);
                if input_area.y > area.y {
                    frame.render_widget(
                        ratatui::widgets::Paragraph::new(
                            state.status.as_deref().unwrap_or_default(),
                        ),
                        Rect::new(area.x, input_area.y - 1, area.width, 1),
                    );
                }
                if let Some(cursor) = cursor {
                    frame.set_cursor_position(cursor);
                }
            })?;
            Ok(())
        },
        || {
            if let Some(reader) = events {
                return reader.poll(Duration::from_millis(16)).map_err(Into::into);
            }
            if event::poll(Duration::from_millis(16))? {
                Ok(AppTerminalEvent::from_crossterm(event::read()?))
            } else {
                Ok(None)
            }
        },
    )
    .map(|prepared| (prepared, state.textarea))
}

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

fn edit_draft(state: &mut AppState, event: AppTerminalEvent) -> Result<()> {
    match event {
        AppTerminalEvent::Paste(text) => state.insert_paste(&text),
        AppTerminalEvent::Resize { width, .. } => {
            state.set_input_width(crate::components::composer_editor_width(width));
        }
        AppTerminalEvent::Key(key) if should_handle_key_event(key.kind) => {
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
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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

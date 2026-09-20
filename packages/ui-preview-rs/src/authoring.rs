//! Small, portable interaction receipts for deterministic UI stories.
use crate::{Scene, registry::Story};
use crossterm::event::KeyCode;
use maestro_ui::{ActionPicker, Menu, PickerOptions, UiTheme};
use ratatui::{Terminal, backend::TestBackend, widgets::ListItem};
use serde::{Deserialize, Serialize};
use std::{fs::OpenOptions, io::Write, path::Path};

pub const REPLAY_VERSION: u8 = 1;
pub const MAX_INPUTS: usize = 64;
pub const MAX_TEXT_BYTES: usize = 4_096;
pub const REPLAY_STEP_MS: u64 = 80;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum StoryKey {
    Up,
    Down,
    Left,
    Right,
    Enter,
    Escape,
    Backspace,
}
impl StoryKey {
    #[must_use]
    pub fn code(self) -> KeyCode {
        match self {
            Self::Up => KeyCode::Up,
            Self::Down => KeyCode::Down,
            Self::Left => KeyCode::Left,
            Self::Right => KeyCode::Right,
            Self::Enter => KeyCode::Enter,
            Self::Escape => KeyCode::Esc,
            Self::Backspace => KeyCode::Backspace,
        }
    }
}

/// Inputs describe user intent. The product controller remains the authority
/// for what each input does, and hosts explicitly own effects such as retry.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "kebab-case", deny_unknown_fields)]
pub enum StoryInput {
    Key {
        key: StoryKey,
        #[serde(default)]
        ctrl: bool,
    },
    Text {
        text: String,
    },
    Resize {
        width: u16,
        height: u16,
    },
    Retry,
}

/// A replay is both a fixture recipe and a portable, reviewable interaction receipt.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct StorySequence {
    pub version: u8,
    pub id: String,
    pub label: String,
    pub width: u16,
    pub height: u16,
    pub inputs: Vec<StoryInput>,
}
impl StorySequence {
    #[must_use]
    pub fn new(id: &str, label: &str, width: u16, height: u16) -> Self {
        Self {
            version: REPLAY_VERSION,
            id: id.into(),
            label: label.into(),
            width,
            height,
            inputs: Vec::new(),
        }
    }
    #[must_use]
    pub fn inputs(mut self, inputs: impl IntoIterator<Item = StoryInput>) -> Self {
        self.inputs = inputs.into_iter().collect();
        self
    }
    pub fn validate(&self) -> Result<(), String> {
        if self.version != REPLAY_VERSION {
            return Err(format!(
                "unsupported story sequence version: {}",
                self.version
            ));
        }
        validate_id(&self.id)?;
        if self.label.is_empty() || self.label.len() > 160 {
            return Err("story sequence label must be 1..160 bytes".into());
        }
        validate_dimensions(self.width, self.height)?;
        if self.inputs.len() > MAX_INPUTS {
            return Err(format!("story sequence exceeds {MAX_INPUTS} inputs"));
        }
        let mut text_bytes = 0usize;
        for input in &self.inputs {
            match input {
                StoryInput::Text { text } => {
                    if text.chars().any(char::is_control) {
                        return Err("story text cannot contain control characters".into());
                    }
                    text_bytes = text_bytes.saturating_add(text.len());
                }
                StoryInput::Resize { width, height } => {
                    validate_dimensions(*width, *height)?;
                }
                StoryInput::Key { .. } | StoryInput::Retry => {}
            }
        }
        if text_bytes > MAX_TEXT_BYTES {
            return Err(format!(
                "story sequence text exceeds {MAX_TEXT_BYTES} bytes"
            ));
        }
        Ok(())
    }
    /// One case before input plus one after each input. The step index is a
    /// deterministic 80 ms authoring clock, while resize inputs update later frames.
    pub fn cases(&self) -> Result<Vec<(u16, u16, u64)>, String> {
        self.validate()?;
        let (mut width, mut height) = (self.width, self.height);
        let mut cases = vec![(width, height, 0)];
        for (index, input) in self.inputs.iter().enumerate() {
            if let StoryInput::Resize {
                width: next_width,
                height: next_height,
            } = input
            {
                width = *next_width;
                height = *next_height;
            }
            cases.push((width, height, (index + 1) as u64 * REPLAY_STEP_MS));
        }
        Ok(cases)
    }
}

pub(crate) fn validate_dimensions(width: u16, height: u16) -> Result<(), String> {
    if !(8..=240).contains(&width) || !(3..=100).contains(&height) {
        return Err("width must be 8..240 and height 3..100".into());
    }
    Ok(())
}

pub(crate) fn validate_id(id: &str) -> Result<(), String> {
    if id.is_empty()
        || id.len() > 64
        || !id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    {
        return Err("story ID must be 1..64 lowercase letters, digits or hyphens".into());
    }
    Ok(())
}

/// Source for a Cargo-discovered example. `create_new` is enforced by the CLI.
pub fn scaffold_source(id: &str) -> Result<String, String> {
    validate_id(id)?;
    Ok(include_str!("../examples/menu_story.rs").replace("menu-story", id))
}

pub fn write_scaffold(id: &str, output: &Path) -> Result<(), String> {
    let source = scaffold_source(id)?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(output)
        .map_err(|error| format!("refusing to overwrite {}: {error}", output.display()))?;
    file.write_all(source.as_bytes()).map_err(|e| e.to_string())
}

/// The small default recipe: caller-owned labels, real shared Menu/ActionPicker,
/// and deterministic replay. Use `Story::replay` directly for product adapters.
pub fn menu_story(
    sequence: StorySequence,
    source: &str,
    title: &'static str,
    items: Vec<String>,
    placeholder: &'static str,
    empty: &'static str,
) -> Result<Story, String> {
    Story::replay(
        sequence,
        source,
        move |scene: &Scene, inputs: &[StoryInput]| {
            let mut picker = ActionPicker::new(items.clone())
                .identified_by(String::as_str)
                .map_err(|error| error.to_string())?
                .searchable(String::as_str);
            picker.open();
            for input in inputs {
                match input {
                    StoryInput::Key { key, ctrl } => {
                        picker.handle_key(key.code(), *ctrl);
                    }
                    StoryInput::Text { text } => {
                        picker.insert_str(text);
                    }
                    StoryInput::Resize { .. } | StoryInput::Retry => {}
                }
            }
            let mut terminal = Terminal::new(TestBackend::new(scene.width, scene.height))
                .map_err(|error| error.to_string())?;
            terminal
                .draw(|frame| {
                    Menu::new(title, &mut picker)
                        .options(PickerOptions {
                            placeholder,
                            empty,
                            ..PickerOptions::default()
                        })
                        .render_items(frame, frame.area(), UiTheme::default(), |item| {
                            ListItem::new(item.as_str())
                        });
                })
                .map_err(|error| error.to_string())?;
            Ok(terminal.backend().buffer().clone())
        },
    )
}

pub fn export_story(story: Story) -> Result<String, String> {
    let mut registry = crate::registry::Registry::default();
    registry.add(story)?;
    crate::review::html(&registry.captures()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static NEXT_TEMP_FILE: AtomicUsize = AtomicUsize::new(0);

    #[test]
    fn sequence_cases_follow_resizes_and_keep_every_input_step() {
        let sequence = StorySequence::new("menu-journey", "Menu journey", 60, 20).inputs([
            StoryInput::Text { text: "夜".into() },
            StoryInput::Resize {
                width: 32,
                height: 12,
            },
            StoryInput::Key {
                key: StoryKey::Escape,
                ctrl: false,
            },
        ]);
        assert_eq!(
            sequence.cases().unwrap(),
            vec![(60, 20, 0), (60, 20, 80), (32, 12, 160), (32, 12, 240)]
        );
        assert_eq!(
            serde_json::from_str::<StorySequence>(&serde_json::to_string(&sequence).unwrap())
                .unwrap(),
            sequence
        );
    }

    #[test]
    fn sequence_rejects_unbounded_and_ambiguous_inputs() {
        let mut sequence = StorySequence::new("UPPER", "label", 60, 20);
        assert!(sequence.validate().is_err());
        sequence.id = "valid".into();
        sequence.inputs = vec![StoryInput::Text {
            text: "x".repeat(MAX_TEXT_BYTES + 1),
        }];
        assert!(sequence.validate().is_err());
        sequence.inputs = vec![StoryInput::Resize {
            width: 241,
            height: 20,
        }];
        assert!(sequence.validate().is_err());
    }

    #[test]
    fn scaffold_is_the_compiled_example_and_never_overwrites() {
        assert_eq!(
            scaffold_source("menu-story").unwrap(),
            include_str!("../examples/menu_story.rs")
        );
        let output = std::env::temp_dir().join(format!(
            "maestro-ui-story-{}-{}.rs",
            std::process::id(),
            NEXT_TEMP_FILE.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_file(&output);
        write_scaffold("generated-menu", &output).unwrap();
        let before = std::fs::read_to_string(&output).unwrap();
        assert!(before.contains("generated-menu"));
        assert!(write_scaffold("replacement", &output).is_err());
        assert_eq!(std::fs::read_to_string(&output).unwrap(), before);
        std::fs::remove_file(output).unwrap();
    }
}

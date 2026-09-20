//! Small, portable interaction receipts for deterministic UI stories.
use crate::{
    Scene,
    registry::{Registry, Story},
};
use crossterm::event::KeyCode;
use maestro_ui::{ActionPicker, Menu, PickerOptions, PickerOutcome, PickerStatus, UiTheme};
use ratatui::{Terminal, backend::TestBackend, widgets::ListItem};
use serde::{Deserialize, Serialize};
use std::{collections::HashSet, fs::OpenOptions, io::Write, path::Path};
use unicode_width::UnicodeWidthStr;

pub const REPLAY_VERSION: u8 = 1;
pub const MAX_INPUTS: usize = 64;
pub const MAX_TEXT_BYTES: usize = 4_096;
pub const REPLAY_STEP_MS: u64 = 80;
pub const MENU_RECIPE_VERSION: u8 = 1;
pub const MAX_MENU_ITEMS: usize = 64;

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

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum MenuRecipeState {
    Ready,
    Empty,
    Loading,
    Error,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MenuRecipeItem {
    pub id: String,
    pub label: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MenuRecipe {
    pub version: u8,
    pub id: String,
    pub label: String,
    pub title: String,
    pub placeholder: String,
    pub empty: String,
    pub items: Vec<MenuRecipeItem>,
    pub state: MenuRecipeState,
    pub status_message: String,
    pub width: u16,
    pub height: u16,
    pub inputs: Vec<StoryInput>,
}

impl MenuRecipe {
    #[must_use]
    pub fn starter() -> Self {
        Self {
            version: MENU_RECIPE_VERSION,
            id: "new-menu".into(),
            label: "New menu".into(),
            title: "Choose a workspace".into(),
            placeholder: "Filter workspaces".into(),
            empty: "No matching workspaces".into(),
            items: [
                ("application", "Application"),
                ("documentation", "Documentation"),
                ("docs-site", "docs-site"),
            ]
            .map(|(id, label)| MenuRecipeItem {
                id: id.into(),
                label: label.into(),
            })
            .into(),
            state: MenuRecipeState::Ready,
            status_message: String::new(),
            width: 60,
            height: 20,
            inputs: Vec::new(),
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.version != MENU_RECIPE_VERSION {
            return Err(format!("unsupported menu recipe version: {}", self.version));
        }
        validate_id(&self.id)?;
        validate_text("label", &self.label, 160, false)?;
        validate_text("title", &self.title, 160, false)?;
        validate_text("placeholder", &self.placeholder, 160, false)?;
        validate_text("empty message", &self.empty, 240, false)?;
        validate_text("status message", &self.status_message, 240, true)?;
        validate_dimensions(self.width, self.height)?;
        if self.items.len() > MAX_MENU_ITEMS {
            return Err(format!("menu recipe exceeds {MAX_MENU_ITEMS} items"));
        }
        let mut total = 0usize;
        let mut unique = HashSet::new();
        for item in &self.items {
            validate_id(&item.id)?;
            validate_text("menu item label", &item.label, 160, false)?;
            total = total.saturating_add(item.id.len() + item.label.len());
            if !unique.insert(&item.id) {
                return Err("menu recipe item IDs must be unique".into());
            }
        }
        if total > MAX_TEXT_BYTES {
            return Err(format!("menu recipe items exceed {MAX_TEXT_BYTES} bytes"));
        }
        if matches!(
            self.state,
            MenuRecipeState::Loading | MenuRecipeState::Error
        ) && self.status_message.is_empty()
        {
            return Err("loading and error recipes need a status message".into());
        }
        self.sequence().validate()
    }

    #[must_use]
    pub fn sequence(&self) -> StorySequence {
        StorySequence {
            version: REPLAY_VERSION,
            id: self.id.clone(),
            label: self.label.clone(),
            width: self.width,
            height: self.height,
            inputs: self.inputs.clone(),
        }
    }

    pub fn story(&self, source: &str) -> Result<Story, String> {
        self.validate()?;
        let sequence = self.sequence();
        let recipe = self.clone();
        Story::replay(sequence, source, move |scene, inputs| {
            render_menu_recipe(&recipe, scene, inputs).map(|rendered| rendered.buffer)
        })
    }

    pub fn fixture_source(&self) -> Result<String, String> {
        self.fixture_source_for_adapter("shared-menu")
    }

    pub fn fixture_source_for_adapter(&self, adapter: &str) -> Result<String, String> {
        self.validate()?;
        validate_id(adapter)?;
        let state = match self.state {
            MenuRecipeState::Ready => "Ready",
            MenuRecipeState::Empty => "Empty",
            MenuRecipeState::Loading => "Loading",
            MenuRecipeState::Error => "Error",
        };
        let items = self
            .items
            .iter()
            .map(|item| {
                format!(
                    "        maestro_ui_preview::authoring::MenuRecipeItem {{ id: {:?}.into(), label: {:?}.into() }},",
                    item.id, item.label
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        let inputs = self
            .inputs
            .iter()
            .map(story_input_source)
            .map(|input| format!("        {input},"))
            .collect::<Vec<_>>()
            .join("\n");
        Ok(format!(
            r#"//! Generated review fixture using the production Menu and ActionPicker widgets.
//! The caller still owns every effect returned by the controller.
use maestro_ui_preview::authoring::{{
    MenuRecipe, MenuRecipeState, export_story,
}};
use maestro_ui_preview::registry::Registry;

/// Declarative fixture data; product code still owns any resulting effects.
pub fn recipe() -> MenuRecipe {{
    MenuRecipe {{
        version: {version},
        id: {id:?}.into(),
        label: {label:?}.into(),
        title: {title:?}.into(),
        placeholder: {placeholder:?}.into(),
        empty: {empty:?}.into(),
        items: vec![
{items}
        ],
        state: MenuRecipeState::{state},
        status_message: {status_message:?}.into(),
        width: {width},
        height: {height},
        inputs: vec![
{inputs}
        ],
    }}
}}

/// Add this recipe to the ordinary capture and PR-evidence registry.
pub fn register(registry: &mut Registry) -> Result<(), String> {{
    registry.add(recipe().story(file!())?.adapter({adapter:?}))
}}

pub fn main() -> Result<(), String> {{
    let recipe = recipe();
    println!("{{}}", export_story(recipe.story(file!())?)?);
    Ok(())
}}
"#,
            version = self.version,
            id = self.id,
            label = self.label,
            title = self.title,
            placeholder = self.placeholder,
            empty = self.empty,
            status_message = self.status_message,
            width = self.width,
            height = self.height,
            adapter = adapter,
        ))
    }
}

fn validate_text(label: &str, value: &str, maximum: usize, empty: bool) -> Result<(), String> {
    if (!empty && value.is_empty()) || value.len() > maximum || value.chars().any(char::is_control)
    {
        return Err(format!(
            "{label} must be {}..{maximum} bytes without control characters",
            usize::from(!empty)
        ));
    }
    Ok(())
}

fn story_input_source(input: &StoryInput) -> String {
    match input {
        StoryInput::Key { key, ctrl } => format!(
            "maestro_ui_preview::authoring::StoryInput::Key {{ key: maestro_ui_preview::authoring::StoryKey::{key:?}, ctrl: {ctrl} }}"
        ),
        StoryInput::Text { text } => {
            format!("maestro_ui_preview::authoring::StoryInput::Text {{ text: {text:?}.into() }}")
        }
        StoryInput::Resize { width, height } => {
            format!(
                "maestro_ui_preview::authoring::StoryInput::Resize {{ width: {width}, height: {height} }}"
            )
        }
        StoryInput::Retry => "maestro_ui_preview::authoring::StoryInput::Retry".into(),
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum MenuSimulatedEffect {
    Focused { id: String, label: String },
    Selected { id: String, label: String },
    Cancelled,
    RetryRequested,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum CoverageAvailability {
    Available,
    Missing,
    NotApplicable,
}

#[derive(Debug, Serialize)]
pub struct CoverageEntry {
    pub variant: &'static str,
    pub availability: CoverageAvailability,
    pub exercised: bool,
    pub automated_check: Option<bool>,
    pub note: &'static str,
}

#[derive(Debug, Serialize)]
pub struct MenuStudioResponse {
    pub capture: crate::review::Capture,
    pub transcript: String,
    pub effects: Vec<MenuSimulatedEffect>,
    pub recipe: MenuRecipe,
    pub fixture_source: String,
    pub coverage: Vec<CoverageEntry>,
    pub effect_policy: &'static str,
}

struct MenuRender {
    buffer: ratatui::buffer::Buffer,
    effects: Vec<MenuSimulatedEffect>,
}

fn record_menu_outcome(
    outcome: PickerOutcome<MenuRecipeItem>,
    effects: &mut Vec<MenuSimulatedEffect>,
) {
    match outcome {
        PickerOutcome::Changed(Some(item)) => effects.push(MenuSimulatedEffect::Focused {
            id: item.id,
            label: item.label,
        }),
        PickerOutcome::Selected(item) => effects.push(MenuSimulatedEffect::Selected {
            id: item.id,
            label: item.label,
        }),
        PickerOutcome::Cancelled => effects.push(MenuSimulatedEffect::Cancelled),
        PickerOutcome::Pending | PickerOutcome::Changed(None) => {}
    }
}

fn render_menu_recipe(
    recipe: &MenuRecipe,
    scene: &Scene,
    inputs: &[StoryInput],
) -> Result<MenuRender, String> {
    let initial_items = if recipe.state == MenuRecipeState::Empty {
        Vec::new()
    } else {
        recipe.items.clone()
    };
    let mut picker = ActionPicker::new(initial_items)
        .identified_by(|item| item.id.as_str())
        .map_err(|error| error.to_string())?
        .searchable(|item| item.label.as_str());
    picker.open();
    picker.set_status(match recipe.state {
        MenuRecipeState::Ready | MenuRecipeState::Empty => PickerStatus::Ready,
        MenuRecipeState::Loading => PickerStatus::Loading(recipe.status_message.clone()),
        MenuRecipeState::Error => PickerStatus::Error(recipe.status_message.clone()),
    });
    let mut effects = Vec::new();
    for input in inputs {
        match input {
            StoryInput::Key { key, ctrl } => {
                record_menu_outcome(picker.handle_key(key.code(), *ctrl), &mut effects);
            }
            StoryInput::Text { text } => {
                record_menu_outcome(picker.insert_str(text), &mut effects);
            }
            StoryInput::Retry if recipe.state == MenuRecipeState::Error => {
                effects.push(MenuSimulatedEffect::RetryRequested);
                picker.set_status(PickerStatus::Ready);
            }
            StoryInput::Resize { .. } | StoryInput::Retry => {}
        }
    }
    let mut terminal = Terminal::new(TestBackend::new(scene.width, scene.height))
        .map_err(|error| error.to_string())?;
    terminal
        .draw(|frame| {
            Menu::new(recipe.title.as_str(), &mut picker)
                .options(PickerOptions {
                    placeholder: recipe.placeholder.as_str(),
                    empty: recipe.empty.as_str(),
                    ..PickerOptions::default()
                })
                .render_items(frame, frame.area(), UiTheme::default(), |item| {
                    ListItem::new(item.label.as_str())
                });
        })
        .map_err(|error| error.to_string())?;
    Ok(MenuRender {
        buffer: terminal.backend().buffer().clone(),
        effects,
    })
}

pub fn menu_studio(recipe: MenuRecipe) -> Result<MenuStudioResponse, String> {
    recipe.validate()?;
    let sequence = recipe.sequence();
    let &(width, height, time_ms) = sequence
        .cases()?
        .last()
        .ok_or("menu recipe has no capture case")?;
    let scene = Scene {
        id: recipe.id.clone(),
        label: recipe.label.clone(),
        width,
        height,
        time_ms,
    };
    let rendered = render_menu_recipe(&recipe, &scene, &recipe.inputs)?;
    let mut capture = crate::review::from_buffer(scene, &rendered.buffer)?;
    capture.source = "products/maestro/packages/ui-preview-rs/src/authoring.rs".into();
    let transcript = crate::review::transcript(&capture);
    let fixture_source = recipe.fixture_source()?;
    let state = recipe.state;
    let final_width = capture.scene.width;
    let coverage = vec![
        coverage(
            "ready",
            state == MenuRecipeState::Ready,
            "Explicit ready recipe state",
        ),
        coverage(
            "loading",
            state == MenuRecipeState::Loading,
            "Explicit loading recipe state",
        ),
        coverage(
            "empty",
            state == MenuRecipeState::Empty,
            "Explicit empty recipe state",
        ),
        coverage(
            "error",
            state == MenuRecipeState::Error,
            "Explicit error recipe state",
        ),
        coverage(
            "long content",
            state == MenuRecipeState::Ready
                && recipe
                    .items
                    .iter()
                    .any(|item| item.label.as_str().width() > 32),
            "Available through caller-owned item text",
        ),
        coverage(
            "Unicode",
            state == MenuRecipeState::Ready
                && recipe.items.iter().any(|item| !item.label.is_ascii()),
            "Available through caller-owned item text",
        ),
        coverage(
            "narrow",
            final_width < 48,
            "Available through bounded terminal width",
        ),
        CoverageEntry {
            variant: "reduced motion",
            availability: CoverageAvailability::NotApplicable,
            exercised: false,
            automated_check: None,
            note: "This static menu recipe has no motion path",
        },
        coverage(
            "interaction",
            !recipe.inputs.is_empty(),
            "Keyboard and text inputs replay through ActionPicker",
        ),
    ];
    Ok(MenuStudioResponse {
        capture,
        transcript,
        effects: rendered.effects,
        recipe,
        fixture_source,
        coverage,
        effect_policy: "interactions are simulated; external actions and settings are untouched",
    })
}

fn coverage(variant: &'static str, exercised: bool, note: &'static str) -> CoverageEntry {
    CoverageEntry {
        variant,
        availability: CoverageAvailability::Available,
        exercised,
        automated_check: None,
        note,
    }
}

#[must_use]
pub fn menu_recipe_fixtures() -> Vec<MenuRecipe> {
    let starter = MenuRecipe::starter();
    let fixture = |id: &str, label: &str, state, status_message: &str| MenuRecipe {
        id: id.into(),
        label: label.into(),
        state,
        status_message: status_message.into(),
        ..starter.clone()
    };
    let mut long = fixture(
        "menu-recipe-long",
        "Menu recipe / long content",
        MenuRecipeState::Ready,
        "",
    );
    long.items = (1..=24)
        .map(|index| MenuRecipeItem {
            id: format!("workspace-{index:02}"),
            label: format!("workspace-with-a-reviewably-long-name-{index:02}"),
        })
        .collect();
    let mut unicode = fixture(
        "menu-recipe-unicode",
        "Menu recipe / Unicode",
        MenuRecipeState::Ready,
        "",
    );
    unicode.items = [
        ("cafe", "Café crème"),
        ("dawn", "夜明け"),
        ("greeting", "Καλημέρα"),
        ("docs", "Docs 🌙"),
    ]
    .map(|(id, label)| MenuRecipeItem {
        id: id.into(),
        label: label.into(),
    })
    .into();
    let mut narrow = fixture(
        "menu-recipe-narrow",
        "Menu recipe / narrow",
        MenuRecipeState::Ready,
        "",
    );
    narrow.width = 30;
    narrow.height = 14;
    vec![
        fixture(
            "menu-recipe-ready",
            "Menu recipe / ready",
            MenuRecipeState::Ready,
            "",
        ),
        fixture(
            "menu-recipe-loading",
            "Menu recipe / loading",
            MenuRecipeState::Loading,
            "Loading workspaces…",
        ),
        fixture(
            "menu-recipe-empty",
            "Menu recipe / empty",
            MenuRecipeState::Empty,
            "",
        ),
        fixture(
            "menu-recipe-error",
            "Menu recipe / error",
            MenuRecipeState::Error,
            "Could not load workspaces. Retry.",
        ),
        long,
        unicode,
        narrow,
    ]
}

fn menu_recipe_interactions() -> Vec<MenuRecipe> {
    let mut select = MenuRecipe::starter();
    select.id = "menu-recipe-select".into();
    select.label = "Menu recipe / navigate and select".into();
    select.inputs = vec![
        StoryInput::Key {
            key: StoryKey::Down,
            ctrl: false,
        },
        StoryInput::Key {
            key: StoryKey::Enter,
            ctrl: false,
        },
    ];

    let mut filter_cancel = MenuRecipe::starter();
    filter_cancel.id = "menu-recipe-filter-cancel".into();
    filter_cancel.label = "Menu recipe / filter and cancel".into();
    filter_cancel.inputs = vec![
        StoryInput::Text { text: "doc".into() },
        StoryInput::Key {
            key: StoryKey::Escape,
            ctrl: false,
        },
    ];

    let mut retry = menu_recipe_fixtures()
        .into_iter()
        .find(|recipe| recipe.state == MenuRecipeState::Error)
        .expect("error fixture");
    retry.id = "menu-recipe-retry".into();
    retry.label = "Menu recipe / retry and select".into();
    retry.inputs = vec![
        StoryInput::Retry,
        StoryInput::Text { text: "doc".into() },
        StoryInput::Key {
            key: StoryKey::Enter,
            ctrl: false,
        },
    ];

    let mut unicode_resize = menu_recipe_fixtures()
        .into_iter()
        .find(|recipe| recipe.id == "menu-recipe-unicode")
        .expect("Unicode fixture");
    unicode_resize.id = "menu-recipe-unicode-resize".into();
    unicode_resize.label = "Menu recipe / Unicode, resize, and cancel".into();
    unicode_resize.inputs = vec![
        StoryInput::Text { text: "夜".into() },
        StoryInput::Resize {
            width: 30,
            height: 14,
        },
        StoryInput::Key {
            key: StoryKey::Escape,
            ctrl: false,
        },
    ];
    vec![select, filter_cancel, retry, unicode_resize]
}

/// Register recipe-owned states and interaction prefixes in the ordinary
/// capture catalog consumed by local review and PR evidence.
pub fn register_menu_recipes(registry: &mut Registry) -> Result<(), String> {
    for recipe in menu_recipe_fixtures()
        .into_iter()
        .chain(menu_recipe_interactions())
    {
        registry.add(
            recipe
                .story("products/maestro/packages/ui-preview-rs/src/authoring.rs")?
                .adapter("shared-menu"),
        )?;
    }
    Ok(())
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
    MenuRecipe {
        version: MENU_RECIPE_VERSION,
        id: sequence.id.clone(),
        label: sequence.label.clone(),
        title: title.into(),
        placeholder: placeholder.into(),
        empty: empty.into(),
        items: items
            .into_iter()
            .enumerate()
            .map(|(index, label)| MenuRecipeItem {
                id: format!("item-{}", index + 1),
                label,
            })
            .collect(),
        state: MenuRecipeState::Ready,
        status_message: String::new(),
        width: sequence.width,
        height: sequence.height,
        inputs: sequence.inputs.clone(),
    }
    .story(source)
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

    #[test]
    fn menu_recipe_replays_stable_ids_and_round_trips_exact_capture() {
        let mut recipe = MenuRecipe::starter();
        recipe.title = "Choose \"docs\" \\ 夜".into();
        recipe.items = vec![
            MenuRecipeItem {
                id: "overview".into(),
                label: "Overview".into(),
            },
            MenuRecipeItem {
                id: "docs".into(),
                label: "Docs \\ 夜".into(),
            },
        ];
        recipe.inputs = vec![
            StoryInput::Text {
                text: "Docs".into(),
            },
            StoryInput::Key {
                key: StoryKey::Enter,
                ctrl: false,
            },
        ];
        let first = menu_studio(recipe.clone()).unwrap();
        assert!(matches!(
            first.effects.last(),
            Some(MenuSimulatedEffect::Selected { id, .. }) if id == "docs"
        ));
        let imported: MenuRecipe =
            serde_json::from_str(&serde_json::to_string(&recipe).unwrap()).unwrap();
        let replayed = menu_studio(imported).unwrap();
        assert_eq!(
            crate::review::json(std::slice::from_ref(&first.capture)).unwrap(),
            crate::review::json(std::slice::from_ref(&replayed.capture)).unwrap()
        );
        assert!(first.fixture_source.contains(r#"Choose \"docs\" \\ 夜"#));
        assert!(first.fixture_source.contains("id: \"docs\".into()"));
        assert!(
            first
                .fixture_source
                .contains("pub fn register(registry: &mut Registry)")
        );
        assert!(first.fixture_source.contains(".adapter(\"shared-menu\")"));
    }

    #[test]
    fn menu_recipe_reports_error_retry_narrow_and_truthful_coverage() {
        let mut recipe = menu_recipe_fixtures()
            .into_iter()
            .find(|recipe| recipe.state == MenuRecipeState::Error)
            .unwrap();
        recipe.inputs.push(StoryInput::Retry);
        recipe.inputs.push(StoryInput::Resize {
            width: 30,
            height: 14,
        });
        let response = menu_studio(recipe).unwrap();
        assert_eq!(
            (response.capture.scene.width, response.capture.scene.height),
            (30, 14)
        );
        assert!(
            response
                .effects
                .contains(&MenuSimulatedEffect::RetryRequested)
        );
        let coverage = |name| {
            response
                .coverage
                .iter()
                .find(|row| row.variant == name)
                .unwrap()
        };
        assert!(coverage("error").exercised);
        assert!(coverage("narrow").exercised);
        assert_eq!(
            coverage("reduced motion").availability,
            CoverageAvailability::NotApplicable
        );
        assert!(
            response
                .coverage
                .iter()
                .all(|row| row.automated_check.is_none())
        );
    }

    #[test]
    fn menu_recipe_rejects_duplicate_or_unbounded_caller_content() {
        let mut recipe = MenuRecipe::starter();
        recipe.items.push(recipe.items[0].clone());
        assert!(recipe.validate().is_err());
        recipe = MenuRecipe::starter();
        recipe.items[0].label = "x".repeat(161);
        assert!(recipe.validate().is_err());
        recipe = MenuRecipe::starter();
        recipe.items[0].id = "Display text is not identity".into();
        assert!(recipe.validate().is_err());
    }

    #[test]
    fn menu_recipes_register_states_and_every_interaction_prefix() {
        let mut registry = Registry::default();
        register_menu_recipes(&mut registry).unwrap();
        let captures = registry.captures().unwrap();
        assert_eq!(captures.len(), 21);
        assert!(captures.iter().any(|capture| {
            capture.scene.id == "menu-recipe-unicode-resize"
                && capture.scene.width == 30
                && capture.scene.time_ms == 240
        }));
        assert!(captures.iter().all(|capture| {
            capture.source == "products/maestro/packages/ui-preview-rs/src/authoring.rs"
        }));
    }
}

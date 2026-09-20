//! Effect-free host for the production ThemeSelector controller and renderer.
use maestro_tui::components::ThemeSelector;
use maestro_ui::{PickerOutcome, PickerStatus};
use maestro_ui_preview::{
    Scene,
    authoring::{REPLAY_VERSION, StoryInput, StoryKey, StorySequence},
    review::{self, Capture},
};
use serde::{Deserialize, Serialize};

const SOURCE: &str = "products/maestro/packages/tui-rs/examples/support/theme_selector_story.rs";

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ThemeFixture {
    Ready,
    Loading,
    Empty,
    Error,
    Long,
    Unicode,
    Narrow,
}
impl ThemeFixture {
    fn id(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::Loading => "loading",
            Self::Empty => "empty",
            Self::Error => "error",
            Self::Long => "long",
            Self::Unicode => "unicode",
            Self::Narrow => "narrow",
        }
    }
    fn label(self) -> &'static str {
        match self {
            Self::Ready => "Theme selector / ready",
            Self::Loading => "Theme selector / loading",
            Self::Empty => "Theme selector / empty",
            Self::Error => "Theme selector / error",
            Self::Long => "Theme selector / long names",
            Self::Unicode => "Theme selector / Unicode",
            Self::Narrow => "Theme selector / narrow",
        }
    }
    fn dimensions(self) -> (u16, u16) {
        if self == Self::Narrow {
            (28, 14)
        } else {
            (72, 22)
        }
    }
}

const FIXTURES: [ThemeFixture; 7] = [
    ThemeFixture::Ready,
    ThemeFixture::Loading,
    ThemeFixture::Empty,
    ThemeFixture::Error,
    ThemeFixture::Long,
    ThemeFixture::Unicode,
    ThemeFixture::Narrow,
];

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "type", content = "theme", rename_all = "kebab-case")]
pub enum SimulatedEffect {
    PreviewTheme(String),
    CommitTheme(String),
    RestoreOpeningTheme,
    RetryThemes,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ThemeReplayRequest {
    pub version: u8,
    pub fixture: ThemeFixture,
    pub width: u16,
    pub height: u16,
    pub inputs: Vec<StoryInput>,
}
impl ThemeReplayRequest {
    fn sequence(&self, id: &str, label: &str) -> StorySequence {
        StorySequence {
            version: self.version,
            id: id.into(),
            label: label.into(),
            width: self.width,
            height: self.height,
            inputs: self.inputs.clone(),
        }
    }
    pub fn validate(&self) -> Result<(), String> {
        self.sequence("theme-selector-live", "Theme selector / live")
            .validate()
    }
}

#[derive(Debug, Serialize)]
pub struct ThemeReplayResponse {
    pub capture: Capture,
    pub effects: Vec<SimulatedEffect>,
    pub sequence: ThemeReplayRequest,
    pub effect_policy: &'static str,
}

struct ThemeStory {
    selector: ThemeSelector,
    fixture: ThemeFixture,
    effects: Vec<SimulatedEffect>,
    width: u16,
    height: u16,
}
impl ThemeStory {
    fn new(fixture: ThemeFixture, width: u16, height: u16) -> Result<Self, String> {
        let mut selector =
            ThemeSelector::with_themes(items(fixture)).map_err(|error| error.to_string())?;
        selector.show();
        selector.set_status(match fixture {
            ThemeFixture::Loading => PickerStatus::Loading("Loading themes…".into()),
            ThemeFixture::Error => {
                PickerStatus::Error("Themes could not be loaded. Retry to continue.".into())
            }
            _ => PickerStatus::Ready,
        });
        Ok(Self {
            selector,
            fixture,
            effects: Vec::new(),
            width,
            height,
        })
    }
    fn apply(&mut self, input: &StoryInput) -> Result<(), String> {
        match input {
            StoryInput::Key { key, ctrl } => {
                let outcome = self.selector.handle_key(key.code(), *ctrl);
                self.record(outcome);
            }
            StoryInput::Text { text } => {
                let outcome = self.selector.insert_str(text);
                self.record(outcome);
            }
            StoryInput::Resize { width, height } => {
                self.width = *width;
                self.height = *height;
            }
            StoryInput::Retry => {
                if self.fixture == ThemeFixture::Error {
                    self.effects.push(SimulatedEffect::RetryThemes);
                    self.selector
                        .replace_themes(items(ThemeFixture::Ready))
                        .map_err(|error| error.to_string())?;
                    self.selector.set_status(PickerStatus::Ready);
                    self.fixture = ThemeFixture::Ready;
                }
            }
        }
        Ok(())
    }
    fn record(&mut self, outcome: PickerOutcome<String>) {
        match outcome {
            PickerOutcome::Changed(Some(theme)) => {
                self.effects.push(SimulatedEffect::PreviewTheme(theme));
            }
            PickerOutcome::Changed(None) | PickerOutcome::Cancelled => {
                self.effects.push(SimulatedEffect::RestoreOpeningTheme);
            }
            PickerOutcome::Selected(theme) => {
                self.effects.push(SimulatedEffect::CommitTheme(theme));
            }
            PickerOutcome::Pending => {}
        }
    }
    fn capture(mut self, id: &str, label: &str, step: usize) -> Result<Capture, String> {
        let scene = Scene {
            id: id.into(),
            label: label.into(),
            width: self.width,
            height: self.height,
            time_ms: step as u64 * maestro_ui_preview::authoring::REPLAY_STEP_MS,
        };
        let mut capture = review::capture(scene, |frame| {
            self.selector.render(frame, frame.area());
        })?;
        capture.source = SOURCE.into();
        Ok(capture)
    }
}

fn items(fixture: ThemeFixture) -> Vec<String> {
    match fixture {
        ThemeFixture::Loading | ThemeFixture::Empty | ThemeFixture::Error => Vec::new(),
        ThemeFixture::Long => (1..=24)
            .map(|index| format!("workspace-contrast-theme-with-a-long-name-{index:02}"))
            .collect(),
        ThemeFixture::Unicode => ["Café crème", "夜明け", "Καλημέρα", "Nord 🌙"]
            .into_iter()
            .map(str::to_owned)
            .collect(),
        ThemeFixture::Ready | ThemeFixture::Narrow => [
            "auto",
            "dark",
            "light",
            "green",
            "pink",
            "blue",
            "green-dark",
            "pink-dark",
            "blue-dark",
            "high-contrast",
            "vscode-monokai",
            "vscode-light-modern",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect(),
    }
}

pub fn replay(request: ThemeReplayRequest) -> Result<ThemeReplayResponse, String> {
    request.validate()?;
    let mut story = ThemeStory::new(request.fixture, request.width, request.height)?;
    for input in &request.inputs {
        story.apply(input)?;
    }
    let effects = story.effects.clone();
    let capture = story.capture(
        "theme-selector-live",
        "Theme selector / interactive replay",
        request.inputs.len(),
    )?;
    Ok(ThemeReplayResponse {
        capture,
        effects,
        sequence: request,
        effect_policy: "simulated only; no theme, sign-in, tool, or setting effect was applied",
    })
}

pub fn presets() -> Vec<ThemeReplayRequest> {
    vec![
        ThemeReplayRequest {
            version: REPLAY_VERSION,
            fixture: ThemeFixture::Ready,
            width: 72,
            height: 22,
            inputs: vec![
                StoryInput::Key {
                    key: StoryKey::Down,
                    ctrl: false,
                },
                StoryInput::Key {
                    key: StoryKey::Down,
                    ctrl: false,
                },
                StoryInput::Key {
                    key: StoryKey::Enter,
                    ctrl: false,
                },
            ],
        },
        ThemeReplayRequest {
            version: REPLAY_VERSION,
            fixture: ThemeFixture::Ready,
            width: 72,
            height: 22,
            inputs: vec![
                StoryInput::Text {
                    text: "vscode".into(),
                },
                StoryInput::Key {
                    key: StoryKey::Down,
                    ctrl: false,
                },
                StoryInput::Key {
                    key: StoryKey::Escape,
                    ctrl: false,
                },
            ],
        },
        ThemeReplayRequest {
            version: REPLAY_VERSION,
            fixture: ThemeFixture::Unicode,
            width: 72,
            height: 22,
            inputs: vec![
                StoryInput::Resize {
                    width: 30,
                    height: 14,
                },
                StoryInput::Text { text: "夜".into() },
            ],
        },
        ThemeReplayRequest {
            version: REPLAY_VERSION,
            fixture: ThemeFixture::Error,
            width: 72,
            height: 22,
            inputs: vec![
                StoryInput::Retry,
                StoryInput::Text {
                    text: "vscode".into(),
                },
                StoryInput::Key {
                    key: StoryKey::Enter,
                    ctrl: false,
                },
            ],
        },
    ]
}

pub fn fixture_requests() -> Vec<ThemeReplayRequest> {
    FIXTURES
        .into_iter()
        .map(|fixture| {
            let (width, height) = fixture.dimensions();
            ThemeReplayRequest {
                version: REPLAY_VERSION,
                fixture,
                width,
                height,
                inputs: Vec::new(),
            }
        })
        .collect()
}

pub fn captures() -> Result<Vec<Capture>, String> {
    let mut captures = Vec::new();
    for fixture in FIXTURES {
        let (width, height) = fixture.dimensions();
        captures.push(ThemeStory::new(fixture, width, height)?.capture(
            &format!("theme-selector-{}", fixture.id()),
            fixture.label(),
            0,
        )?);
    }
    for (index, request) in presets().into_iter().enumerate() {
        let id = format!("theme-selector-interaction-{}", index + 1);
        let label = match index {
            0 => "Theme selector interaction / keyboard commit",
            1 => "Theme selector interaction / type and cancel",
            2 => "Theme selector interaction / Unicode and resize",
            _ => "Theme selector interaction / error and retry",
        };
        for step in 0..=request.inputs.len() {
            let mut story = ThemeStory::new(request.fixture, request.width, request.height)?;
            for input in &request.inputs[..step] {
                story.apply(input)?;
            }
            captures.push(story.capture(&id, label, step)?);
        }
    }
    Ok(captures)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixtures_and_interactions_are_deterministic_and_bounded() {
        let captures = captures().unwrap();
        assert_eq!(captures.len(), 22);
        assert!(captures.iter().all(|capture| capture.scene.width <= 72));
        assert_eq!(
            review::json(&captures).unwrap(),
            review::json(&super::captures().unwrap()).unwrap()
        );
    }

    #[test]
    fn retry_cancel_and_commit_are_receipts_without_runtime_effects() {
        let retry = replay(presets().pop().unwrap()).unwrap();
        assert_eq!(retry.effects[0], SimulatedEffect::RetryThemes);
        assert!(matches!(
            retry.effects.last(),
            Some(SimulatedEffect::CommitTheme(_))
        ));
        assert!(retry.effect_policy.starts_with("simulated only"));

        let cancel = replay(presets()[1].clone()).unwrap();
        assert!(matches!(
            cancel.effects.last(),
            Some(SimulatedEffect::RestoreOpeningTheme)
        ));
    }

    #[test]
    fn unicode_and_narrow_render_use_production_clipping_and_focus_styles() {
        let response = replay(presets()[2].clone()).unwrap();
        assert_eq!(
            (response.capture.scene.width, response.capture.scene.height),
            (30, 14)
        );
        let text = response
            .capture
            .cells
            .iter()
            .map(|cell| cell.text.as_str())
            .collect::<String>();
        assert!(text.contains('夜'));
        assert!(text.contains("Esc cancel"));
        assert!(
            response
                .capture
                .cells
                .iter()
                .any(|cell| cell.modifiers != 0)
        );
    }

    #[test]
    fn request_bounds_are_fail_closed() {
        let mut request = presets()[0].clone();
        request.width = 500;
        assert!(replay(request).is_err());
        let malformed =
            r#"{"version":1,"fixture":"ready","width":60,"height":20,"inputs":[],"path":"/tmp"}"#;
        assert!(serde_json::from_str::<ThemeReplayRequest>(malformed).is_err());
    }

    #[test]
    fn fixture_items_are_fixed_and_do_not_scan_user_theme_directories() {
        let ready = items(ThemeFixture::Ready);
        assert_eq!(ready.len(), 12);
        assert_eq!(ready[0], "auto");
        assert_eq!(ready.last().unwrap(), "vscode-light-modern");
    }
}

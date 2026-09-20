//! Full-TUI-linked fixtures; never starts App, authentication or checks.
//! COLORTERM=truecolor cargo run --locked -p maestro-tui --example onboarding-preview -- --html > review.html
mod support;

use maestro_tui::{
    components::{SetupModal, SetupPage, SetupPresentation, startup::render_startup},
    doctor::CheckStatus,
    onboarding_checks::{OnboardingCheck, OnboardingReadiness},
    telemetry::OnboardingCheckId,
};
use maestro_ui_preview::{
    Scene,
    authoring::{MenuRecipe, menu_recipe_fixtures, menu_studio},
    review::{self, Capture},
    schema::{
        RECIPE_SCHEMA, RECIPE_SCHEMA_VERSION, THEME_REPLAY_SCHEMA, THEME_REPLAY_SCHEMA_VERSION,
        WireEnvelope, migrate_recipe, migrate_theme_replay,
    },
};

const STATES: &[(&str, SetupPage)] = &[
    ("welcome", SetupPage::Welcome),
    ("role", SetupPage::Role),
    ("use-case", SetupPage::UseCase),
    ("workflow", SetupPage::Workflow),
    ("connect", SetupPage::Mode),
    ("verify", SetupPage::Verify),
    ("checking", SetupPage::Checking),
    ("ready", SetupPage::Results),
    ("failed", SetupPage::Results),
    ("waiting", SetupPage::WaitingEvalops),
    ("provider", SetupPage::Provider),
    ("key", SetupPage::Key),
];
fn report(ready: bool) -> OnboardingReadiness {
    let checks = [
        OnboardingCheckId::Config,
        OnboardingCheckId::Identity,
        OnboardingCheckId::Provider,
        OnboardingCheckId::Model,
        OnboardingCheckId::ManagedSetup,
        OnboardingCheckId::Workspace,
        OnboardingCheckId::ModelProbe,
        OnboardingCheckId::ToolProbe,
    ]
    .into_iter()
    .map(|id| {
        let fail = !ready && id == OnboardingCheckId::ModelProbe;
        OnboardingCheck {
            id,
            status: if fail {
                CheckStatus::Fail
            } else {
                CheckStatus::Pass
            },
            summary: if fail {
                "Model request failed"
            } else {
                "Fixture check passed"
            }
            .into(),
            repair: fail.then(|| "Check the selected model and retry.".into()),
        }
    })
    .collect();
    OnboardingReadiness {
        checks,
        ready,
        elapsed_ms: 320,
    }
}
fn fixture(id: &str, expected: SetupPage) -> SetupModal {
    let mut modal = SetupModal::new();
    modal.show();
    match id {
        "welcome" => {}
        "role" | "use-case" | "workflow" | "connect" => {
            // Bounded transitions make a changed product flow fail, never hang.
            for _ in 0..4 {
                if modal.page() == expected {
                    break;
                }
                let _ = modal.confirm();
            }
        }
        "verify" => modal.set_connection_ready(),
        "checking" => modal.set_checking(),
        "ready" | "failed" => modal.set_check_results(report(id == "ready")),
        "waiting" => modal.set_waiting_evalops(),
        "provider" | "key" => {
            for _ in 0..4 {
                let _ = modal.confirm();
            }
            assert_eq!(modal.page(), SetupPage::Mode);
            modal.move_down();
            // Inspect this intent; never dispatch it to an authentication runtime.
            assert!(matches!(
                modal.confirm(),
                Some(maestro_tui::components::SetupAdvance::StartEvalops)
            ));
            assert!(modal.continue_to_byok_after_identity());
            if id == "key" {
                let _ = modal.confirm();
            }
        }
        _ => panic!("unknown fixture: {id}"),
    }
    assert_eq!(modal.page(), expected, "fixture {id}");
    modal
}
fn captures() -> Result<Vec<Capture>, String> {
    let mut captures = Vec::new();
    for (width, height) in [(40, 24), (60, 32), (100, 40)] {
        for motion in [true, false] {
            for tick in 0..if motion { 36 } else { 1 } {
                let scene = Scene {
                    id: if motion {
                        "first-boot"
                    } else {
                        "first-boot-motion-off"
                    }
                    .into(),
                    label: if motion {
                        "First launch"
                    } else {
                        "First launch · motion off"
                    }
                    .into(),
                    width,
                    height,
                    time_ms: tick * 80,
                };
                captures.push(review::capture(scene, |f| {
                    render_startup(
                        f,
                        f.area(),
                        maestro_tui::themes::current_ui_theme(),
                        motion.then_some(tick),
                    );
                })?);
            }
        }
        for &(id, page) in STATES {
            let times: &[u64] = if id == "welcome" {
                &[0, 640, 1280, 1920]
            } else {
                &[0]
            };
            for &time_ms in times {
                let mut modal = fixture(id, page);
                let scene = Scene {
                    id: format!("onboarding-{id}"),
                    label: format!("Onboarding / {id}"),
                    width,
                    height,
                    time_ms,
                };
                captures.push(review::capture(scene, |f| {
                    modal.render_with_dex(
                        f,
                        f.area(),
                        SetupPresentation {
                            animations: true,
                            animation_frame: time_ms / 80,
                            ..SetupPresentation::default()
                        },
                    );
                })?);
            }
        }
    }
    for capture in &mut captures {
        capture.source = "products/maestro/packages/tui-rs/examples/onboarding-preview.rs".into();
    }
    captures.extend(support::ui_stories::captures()?);
    captures.extend(maestro_ui_preview::registry()?.captures()?);
    Ok(captures)
}
fn profiled_captures(profile_id: &str) -> Result<Vec<Capture>, String> {
    let profile = maestro_ui_preview::schema::builtin_profile(profile_id)?;
    let mut selected = captures()?;
    selected.retain(|capture| {
        profile
            .geometries
            .contains(&(capture.scene.width, capture.scene.height))
            && (profile.terminal.motion || capture.semantic.is_some() || capture.scene.time_ms == 0)
    });
    if selected.is_empty() {
        return Err(format!(
            "coverage profile {profile_id} selected no captures"
        ));
    }
    for capture in &mut selected {
        capture.metadata = maestro_ui_preview::schema::CaptureMetadata::for_profile(&profile);
    }
    Ok(selected)
}
fn run() -> Result<(), String> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args == ["--sequences"] {
        println!(
            "{}",
            serde_json::to_string(&serde_json::json!({
                "fixtures": support::theme_selector_story::fixture_requests()
                    .into_iter()
                    .map(|request| WireEnvelope::new(THEME_REPLAY_SCHEMA, THEME_REPLAY_SCHEMA_VERSION, request))
                    .collect::<Vec<_>>(),
                "menu_starter": WireEnvelope::new(RECIPE_SCHEMA, RECIPE_SCHEMA_VERSION, MenuRecipe::starter()),
                "menu_fixtures": menu_recipe_fixtures()
                    .into_iter()
                    .map(|recipe| WireEnvelope::new(RECIPE_SCHEMA, RECIPE_SCHEMA_VERSION, recipe))
                    .collect::<Vec<_>>(),
                "presets": support::theme_selector_story::presets()
                    .into_iter()
                    .zip([
                        "Navigate and select",
                        "Filter and cancel",
                        "Unicode and resize",
                        "Retry and select",
                    ])
                    .map(|(request, label)| serde_json::json!({
                        "label": label,
                        "request": WireEnvelope::new(THEME_REPLAY_SCHEMA, THEME_REPLAY_SCHEMA_VERSION, request),
                    }))
                    .collect::<Vec<_>>(),
            }))
            .map_err(|error| error.to_string())?
        );
        return Ok(());
    }
    if args == ["--studio-stdin"] {
        use std::io::Read;
        let mut body = String::new();
        std::io::stdin()
            .take(16_385)
            .read_to_string(&mut body)
            .map_err(|error| error.to_string())?;
        if body.len() > 16_384 {
            return Err("menu recipe exceeds 16384 bytes".into());
        }
        let value = serde_json::from_str(&body).map_err(|error| error.to_string())?;
        let recipe = migrate_recipe(value)
            .map_err(|error| error.to_string())?
            .value;
        println!(
            "{}",
            serde_json::to_string(&menu_studio(recipe)?).map_err(|error| error.to_string())?
        );
        return Ok(());
    }
    if args == ["--replay-stdin"] {
        use std::io::Read;
        let mut body = String::new();
        std::io::stdin()
            .take(16_385)
            .read_to_string(&mut body)
            .map_err(|error| error.to_string())?;
        if body.len() > 16_384 {
            return Err("replay request exceeds 16384 bytes".into());
        }
        let value = serde_json::from_str(&body).map_err(|error| error.to_string())?;
        let request =
            migrate_theme_replay::<support::theme_selector_story::ThemeReplayRequest>(value)
                .map_err(|error| error.to_string())?
                .value;
        let response = support::theme_selector_story::replay(request)?;
        println!(
            "{}",
            serde_json::to_string(&response).map_err(|error| error.to_string())?
        );
        return Ok(());
    }
    let mut format = None;
    let mut profile = None;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--html" | "--json" if format.is_none() => format = Some(args[index].as_str()),
            "--profile" if profile.is_none() => {
                index += 1;
                profile = Some(
                    args.get(index)
                        .ok_or("missing value for --profile")?
                        .as_str(),
                );
            }
            _ => {
                return Err("usage: onboarding-preview [--html|--json] [--profile pr-v1|scheduled-v1] | [--sequences|--replay-stdin|--studio-stdin]".into());
            }
        }
        index += 1;
    }
    let captures = profile.map_or_else(captures, profiled_captures)?;
    let output = if format == Some("--html") {
        review::html(&captures)?
    } else {
        review::json(&captures)?
    };
    println!("{output}");
    Ok(())
}
fn main() {
    if let Err(error) = run() {
        eprintln!("{error}");
        std::process::exit(2);
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn every_named_fixture_reaches_its_declared_page() {
        for &(id, page) in STATES {
            assert_eq!(fixture(id, page).page(), page);
        }
    }
    #[test]
    fn all_captures_are_deterministic_and_failed_is_distinct_from_ready() {
        let a = captures().unwrap();
        assert_eq!(
            review::json(&a).unwrap(),
            review::json(&captures().unwrap()).unwrap()
        );
        let text = |id: &str| {
            a.iter()
                .find(|c| c.scene.id == id && c.scene.width == 100)
                .unwrap()
                .cells
                .iter()
                .map(|c| c.text.as_str())
                .collect::<String>()
        };
        assert_ne!(text("onboarding-ready"), text("onboarding-failed"));
        assert!(text("onboarding-failed").contains("Model request failed"));
    }

    #[test]
    fn menu_authoring_fixtures_render_through_the_shared_recipe() {
        for recipe in menu_recipe_fixtures() {
            let response = menu_studio(recipe).unwrap();
            assert!(!response.transcript.is_empty());
            assert!(
                response
                    .effect_policy
                    .contains("external actions and settings are untouched")
            );
        }
    }

    #[test]
    fn retry_and_cancel_presets_have_passing_behavior_contracts() {
        let presets = support::theme_selector_story::presets();
        for index in [1, 3] {
            let response = support::theme_selector_story::replay(presets[index].clone()).unwrap();
            assert!(
                response.semantic.passed(),
                "{:?}",
                response.semantic.failures()
            );
        }
        let cancel = support::theme_selector_story::replay(presets[1].clone()).unwrap();
        let observed = cancel
            .semantic
            .observations
            .iter()
            .map(|observation| (observation.name(), observation.display_value()))
            .collect::<std::collections::BTreeMap<_, _>>();
        assert_eq!(observed.get("state").map(String::as_str), Some("ready"));
        assert_eq!(
            observed.get("focus-region").map(String::as_str),
            Some("closed")
        );
        let response = support::theme_selector_story::replay(presets[3].clone()).unwrap();
        let failed = maestro_ui_preview::contract::evaluate_step(
            response.semantic.step,
            response.semantic.observations,
            response.semantic.effects,
            &[maestro_ui_preview::contract::StoryExpectation::effect_count("retry-themes", 2)],
        )
        .unwrap();
        assert_eq!(
            failed.failures(),
            ["expected retry-themes exactly 2 times; observed 1"]
        );
    }

    #[test]
    fn named_profiles_select_bounded_captures_and_stamp_identity() {
        let pr = profiled_captures("pr-v1").unwrap();
        let scheduled = profiled_captures("scheduled-v1").unwrap();
        assert!(!pr.is_empty());
        assert!(pr.len() < scheduled.len());
        for (id, captures) in [("pr-v1", pr), ("scheduled-v1", scheduled)] {
            assert!(
                captures
                    .iter()
                    .all(|capture| capture.metadata.profile.id == id)
            );
            assert!(
                captures
                    .iter()
                    .all(|capture| capture.metadata.profile.version == 1)
            );
            for (story_id, width) in [
                ("theme-selector-interaction-3", 30),
                ("theme-selector-narrow", 28),
            ] {
                let final_capture = captures
                    .iter()
                    .filter(|capture| capture.scene.id == story_id && capture.scene.width == width)
                    .max_by_key(|capture| capture.scene.time_ms)
                    .unwrap_or_else(|| panic!("{id} omitted {story_id} at {width} columns"));
                let semantic = final_capture
                    .semantic
                    .as_ref()
                    .unwrap_or_else(|| panic!("{id} omitted final semantics for {story_id}"));
                assert!(semantic.passed(), "{id} failed {story_id} semantics");
                assert!(!semantic.expectations.is_empty());
            }
        }
        assert!(profiled_captures("unknown").is_err());
    }
}

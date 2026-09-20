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
    review::{self, Capture},
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
    captures.extend(support::theme_selector_story::captures()?);
    captures.extend(maestro_ui_preview::registry()?.captures()?);
    Ok(captures)
}
fn run() -> Result<(), String> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args == ["--sequences"] {
        println!(
            "{}",
            serde_json::to_string(&serde_json::json!({
                "fixtures": support::theme_selector_story::fixture_requests(),
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
                        "request": request,
                    }))
                    .collect::<Vec<_>>(),
            }))
            .map_err(|error| error.to_string())?
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
        let request = serde_json::from_str(&body).map_err(|error| error.to_string())?;
        let response = support::theme_selector_story::replay(request)?;
        println!(
            "{}",
            serde_json::to_string(&response).map_err(|error| error.to_string())?
        );
        return Ok(());
    }
    if args.len() > 1 || args.first().is_some_and(|a| a != "--html" && a != "--json") {
        return Err("usage: onboarding-preview [--html|--json|--sequences|--replay-stdin]".into());
    }
    let captures = captures()?;
    let output = if args.first().is_some_and(|a| a == "--html") {
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
}

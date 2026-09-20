//! Headless component rendering; no terminal session or agent runtime.
use maestro_ui_preview::{
    Scene, StoryFilter,
    adapters::{AdapterRegistry, AdapterVerifier, PromotionPlan, create_story, promote_recipe},
    ansi, catalog, render,
};
use std::{
    path::{Path, PathBuf},
    process::Command,
};

fn repo_root() -> Result<PathBuf, String> {
    let current = std::env::current_dir().map_err(|error| error.to_string())?;
    for directory in current.ancestors() {
        if directory.join("products/maestro").is_dir() {
            return Ok(directory.to_path_buf());
        }
    }
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    manifest
        .ancestors()
        .find(|directory| directory.join("products/maestro").is_dir())
        .map(Path::to_path_buf)
        .ok_or("run Studio inside the Mono repository".into())
}

fn verify_adapter(
    root: &Path,
    verifier: &AdapterVerifier,
    plan: Option<&PromotionPlan>,
) -> Result<(), String> {
    if let Some(plan) = plan {
        let status = Command::new("rustfmt")
            .current_dir(root)
            .args(["--edition", "2024"])
            .arg(root.join(&plan.fixture))
            .status()
            .map_err(|error| format!("start rustfmt: {error}"))?;
        if !status.success() {
            return Err("generated fixture formatting failed".into());
        }
    }
    let maestro = root.join("products/maestro");
    let mut cargo = Command::new("cargo");
    cargo.current_dir(maestro).args(["test", "--locked"]);
    match verifier {
        AdapterVerifier::UiPreview => {
            cargo.args(["-p", "maestro-ui-preview"]);
        }
        AdapterVerifier::OnboardingPreview => {
            cargo.args(["-p", "maestro-tui", "--example", "onboarding-preview"]);
        }
    }
    let status = cargo
        .status()
        .map_err(|error| format!("start focused verifier: {error}"))?;
    if status.success() {
        Ok(())
    } else {
        Err("focused behavior verification failed".into())
    }
}

fn onboarding_story(root: &Path, story_id: &str) -> Result<Option<Vec<serde_json::Value>>, String> {
    let output = Command::new("cargo")
        .current_dir(root.join("products/maestro"))
        .args([
            "run",
            "--quiet",
            "--locked",
            "-p",
            "maestro-tui",
            "--example",
            "onboarding-preview",
            "--",
            "--json",
        ])
        .output()
        .map_err(|error| format!("start onboarding verifier: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "onboarding verifier failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    let captures: Vec<serde_json::Value> =
        serde_json::from_slice(&output.stdout).map_err(|error| error.to_string())?;
    validate_onboarding_captures(captures, story_id)
}

fn validate_onboarding_captures(
    captures: Vec<serde_json::Value>,
    story_id: &str,
) -> Result<Option<Vec<serde_json::Value>>, String> {
    let matching: Vec<_> = captures
        .into_iter()
        .filter(|capture| {
            capture
                .pointer("/scene/id")
                .and_then(serde_json::Value::as_str)
                == Some(story_id)
        })
        .collect();
    if matching.is_empty() {
        return Ok(None);
    }
    let failures: Vec<_> = matching
        .iter()
        .flat_map(|capture| {
            capture
                .pointer("/semantic/expectations")
                .and_then(serde_json::Value::as_array)
                .into_iter()
                .flatten()
        })
        .filter(|expectation| {
            expectation
                .get("passed")
                .and_then(serde_json::Value::as_bool)
                == Some(false)
        })
        .filter_map(|expectation| {
            expectation
                .get("message")
                .and_then(serde_json::Value::as_str)
        })
        .collect();
    if !failures.is_empty() {
        return Err(format!(
            "story expectations failed: {}",
            failures.join("; ")
        ));
    }
    Ok(Some(matching))
}

fn studio(args: &[String]) -> Result<(), String> {
    let (command, tail) = args
        .split_first()
        .ok_or("usage: studio <new|verify|promote>")?;
    if command == "manifests" {
        if !tail.is_empty() {
            return Err("usage: studio manifests".into());
        }
        println!(
            "{}",
            serde_json::to_string_pretty(&AdapterRegistry::builtins()?.manifests())
                .map_err(|error| error.to_string())?
        );
        return Ok(());
    }
    if command == "coverage" {
        if !tail.is_empty() {
            return Err("usage: studio coverage".into());
        }
        let registry = maestro_ui_preview::registry()?;
        let adapters = AdapterRegistry::builtins()?;
        let profiles = maestro_ui_preview::schema::builtin_profiles();
        let coverage = profiles
            .iter()
            .map(|profile| registry.coverage_for_profile(&adapters, profile))
            .collect::<Result<Vec<_>, _>>()?;
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "coverage": coverage,
                "contributions": registry.contribution_diagnostics(&adapters)?,
            }))
            .map_err(|error| error.to_string())?
        );
        return Ok(());
    }
    if !matches!(command.as_str(), "new" | "verify" | "promote") {
        return Err(format!("unknown studio command: {command}"));
    }
    if command == "verify" {
        if tail.len() != 1 {
            return Err("usage: studio verify <story-id>".into());
        }
        let story_id = &tail[0];
        if story_id.starts_with('-') {
            return Err(format!("unknown argument: {story_id}"));
        }
        let captures: Vec<_> = maestro_ui_preview::registry()?
            .results()?
            .into_iter()
            .filter(|capture| capture.scene.id == *story_id)
            .collect();
        if captures.is_empty() {
            let root = repo_root()?;
            if let Some(capture) = onboarding_story(&root, story_id)? {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&capture).map_err(|error| error.to_string())?
                );
                return Ok(());
            }
            return Err(format!("unknown story: {story_id}"));
        }
        let failures: Vec<_> = captures
            .iter()
            .filter_map(|capture| capture.semantic.as_ref())
            .flat_map(|result| result.failures())
            .collect();
        if !failures.is_empty() {
            return Err(format!(
                "story expectations failed: {}",
                failures.join("; ")
            ));
        }
        println!(
            "{}",
            serde_json::to_string_pretty(&captures).map_err(|error| error.to_string())?
        );
        return Ok(());
    }
    let mut adapter_id = None;
    let mut positional = None;
    let mut check = false;
    let mut index = 0;
    while index < tail.len() {
        match tail[index].as_str() {
            "--adapter" => {
                if adapter_id.is_some() {
                    return Err("--adapter may be specified once".into());
                }
                index += 1;
                adapter_id = Some(
                    tail.get(index)
                        .ok_or("missing value for --adapter")?
                        .clone(),
                );
            }
            "--check" => check = true,
            arg if arg.starts_with('-') => return Err(format!("unknown argument: {arg}")),
            arg if positional.is_none() => positional = Some(arg.to_owned()),
            _ => return Err("studio accepts exactly one story or recipe path".into()),
        }
        index += 1;
    }
    let adapter_id = adapter_id.ok_or("studio command requires --adapter")?;
    let value =
        positional.ok_or_else(|| format!("studio {command} requires a story or recipe path"))?;
    let adapters = AdapterRegistry::builtins()?;
    let adapter = adapters.get(&adapter_id)?;
    let root = repo_root()?;
    let plan = if command == "new" {
        create_story(&root, adapter, &value, check)?
    } else {
        promote_recipe(&root, adapter, Path::new(&value), check, |plan| {
            verify_adapter(&root, &adapter.verifier, Some(plan))
        })?
    };
    println!(
        "{}",
        serde_json::to_string_pretty(&plan).map_err(|error| error.to_string())?
    );
    Ok(())
}
fn run() -> Result<(), String> {
    let raw: Vec<_> = std::env::args().skip(1).collect();
    if raw.first().is_some_and(|arg| arg == "studio") {
        return studio(&raw[1..]);
    }
    let mut args = raw.into_iter();
    let mut scene = Scene {
        id: "startup".into(),
        label: String::new(),
        width: 100,
        height: 10,
        time_ms: 0,
    };
    let mut list = false;
    let mut selectors = false;
    let mut format = String::from("ansi");
    let mut identity = false;
    let mut scaffold = None;
    let mut output = None;
    let mut filter = None;
    let mut profile_id = None;
    let mut sequences = false;
    while let Some(arg) = args.next() {
        if arg == "--html" || arg == "--json" {
            format = arg.trim_start_matches("--").into();
            continue;
        }
        if arg == "--identity" {
            identity = true;
            continue;
        }
        if arg == "--list" {
            list = true;
            continue;
        }
        if arg == "--sequences" {
            sequences = true;
            continue;
        }
        if arg == "--story" || arg == "--adapter" {
            if filter.is_some() {
                return Err("only one --story or --adapter filter is allowed".into());
            }
            let value = args
                .next()
                .ok_or_else(|| format!("missing value for {arg}"))?;
            filter = Some(if arg == "--story" {
                StoryFilter::Story(value)
            } else {
                StoryFilter::Adapter(value)
            });
            continue;
        }
        if arg == "--profile" {
            if profile_id.is_some() {
                return Err("--profile may be specified once".into());
            }
            profile_id = Some(args.next().ok_or("missing value for --profile")?);
            continue;
        }
        if arg == "--scaffold" {
            scaffold = Some(args.next().ok_or("missing value for --scaffold")?);
            continue;
        }
        if arg == "--output" {
            output = Some(PathBuf::from(
                args.next().ok_or("missing value for --output")?,
            ));
            continue;
        }
        selectors = true;
        let value = args
            .next()
            .ok_or_else(|| format!("missing value for {arg}"))?;
        match arg.as_str() {
            "--scene" => scene.id = value,
            "--width" => scene.width = value.parse().map_err(|_| "invalid width")?,
            "--height" => scene.height = value.parse().map_err(|_| "invalid height")?,
            "--time-ms" => scene.time_ms = value.parse().map_err(|_| "invalid time-ms")?,
            _ => return Err(format!("unknown argument: {arg}")),
        }
    }
    if let Some(scaffold) = scaffold {
        if selectors || list || sequences || identity || filter.is_some() || format != "ansi" {
            return Err("--scaffold cannot be combined with rendering options".into());
        }
        let output = output.ok_or("--scaffold requires --output")?;
        maestro_ui_preview::authoring::write_scaffold(&scaffold, &output)?;
        println!("{}", output.display());
        return Ok(());
    }
    if output.is_some() {
        return Err("--output requires --scaffold".into());
    }
    if profile_id.is_some()
        && (format == "ansi" || list || sequences || identity || filter.is_some())
    {
        return Err("--profile requires an unfiltered --html or --json export".into());
    }
    if format != "ansi" && (selectors || list || sequences || identity) {
        return Err("--html and --json export the full catalog; use them without scene selectors, --list, or --identity".into());
    }
    if filter.is_some() && format == "ansi" && !list && !sequences {
        return Err("--story and --adapter require --html, --json, --list, or --sequences".into());
    }
    let registry = maestro_ui_preview::registry()?;
    if identity {
        if filter.is_some() {
            return Err("filters cannot be combined with --identity".into());
        }
        println!("{}", env!("MAESTRO_PREVIEW_SOURCE_DIGEST"));
    } else if list {
        if let Some(filter) = filter {
            let selected = registry.filtered(filter.clone())?;
            println!(
                "{}",
                serde_json::to_string_pretty(
                    &serde_json::json!({"filter": filter, "scenes": selected.scenes()})
                )
                .map_err(|e| e.to_string())?
            );
        } else {
            println!(
                "{}",
                serde_json::to_string_pretty(&catalog()).map_err(|e| e.to_string())?
            );
        }
    } else if sequences {
        if let Some(filter) = filter {
            let selected = registry.filtered(filter.clone())?;
            println!(
                "{}",
                serde_json::to_string_pretty(
                    &serde_json::json!({"filter": filter, "sequences": selected.sequences()})
                )
                .map_err(|e| e.to_string())?
            );
        } else {
            println!(
                "{}",
                serde_json::to_string_pretty(&registry.sequences()).map_err(|e| e.to_string())?
            );
        }
    } else if format != "ansi" {
        let adapters = AdapterRegistry::builtins()?;
        let profile = profile_id
            .as_deref()
            .map(maestro_ui_preview::schema::builtin_profile)
            .transpose()?;
        let captures = if let Some(profile) = &profile {
            registry.results_for_profile(&adapters, profile)?
        } else if let Some(filter) = filter {
            registry.filtered(filter)?.captures()?
        } else {
            registry.captures()?
        };
        let output = if format == "html" {
            let profile = profile
                .unwrap_or_else(|| maestro_ui_preview::schema::builtin_profiles()[0].clone());
            let coverage = registry.coverage_for_profile(&adapters, &profile)?;
            maestro_ui_preview::review::html_with_coverage(&captures, &coverage)?
        } else {
            maestro_ui_preview::review::json(&captures)?
        };
        println!("{output}");
    } else {
        print!("{}", ansi(&render(&scene)?));
    }
    Ok(())
}
fn main() {
    if let Err(error) = run() {
        eprintln!("{error}");
        std::process::exit(2);
    }
}

#[cfg(test)]
mod studio_tests {
    use super::*;

    #[test]
    fn controller_capture_verification_rejects_failed_expectations() {
        let captures = vec![serde_json::json!({
            "scene": {"id": "theme-selector-interaction"},
            "semantic": {"expectations": [{"passed": false, "message": "retry missing"}]}
        })];
        assert_eq!(
            validate_onboarding_captures(captures, "theme-selector-interaction").unwrap_err(),
            "story expectations failed: retry missing"
        );
    }
}

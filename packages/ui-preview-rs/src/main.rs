//! Headless component rendering; no terminal session or agent runtime.
use maestro_ui_preview::{Scene, ansi, catalog, render};
use std::path::PathBuf;
fn run() -> Result<(), String> {
    let mut args = std::env::args().skip(1);
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
        if selectors || list || identity || format != "ansi" {
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
    if format != "ansi" && (selectors || list || identity) {
        return Err("--html and --json export the full catalog; use them without scene selectors, --list, or --identity".into());
    }
    if identity {
        println!("{}", env!("MAESTRO_PREVIEW_SOURCE_DIGEST"));
    } else if list {
        println!(
            "{}",
            serde_json::to_string_pretty(&catalog()).map_err(|e| e.to_string())?
        );
    } else if format != "ansi" {
        let captures = maestro_ui_preview::registry()?.captures()?;
        let output = if format == "html" {
            maestro_ui_preview::review::html(&captures)?
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

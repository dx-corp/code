//! Native `maestro painter` inline-image command.

use std::fs;
use std::io::{self, IsTerminal as _, Write as _};
use std::path::{Component, Path, PathBuf};

use anyhow::Result;
use base64::Engine as _;

const KITTY_CHUNK_SIZE: usize = 4096;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TerminalImageSupport {
    Iterm,
    Kitty,
    Sixel,
    None,
}

pub fn run_painter(args: &[String]) -> Result<i32> {
    match args.first().map(String::as_str) {
        Some("show") => show_image(args.get(1).map(String::as_str)),
        _ => {
            println!("{}", painter_help());
            Ok(0)
        }
    }
}

fn painter_help() -> &'static str {
    crate::localization::cli_locale().translate("Usage:\n  maestro painter show <path>   Render an image inline in a capable terminal\n\nRequires iTerm2, WezTerm, or kitty. Run from a plain shell, not inside\nthe full-screen TUI.")
}

fn show_image(path: Option<&str>) -> Result<i32> {
    let Some(path) = path else {
        eprintln!(
            "{}",
            crate::localization::cli_locale()
                .format("maestro painter show requires an image path.", &[])
        );
        return Ok(1);
    };
    let resolved = resolve_path(path)?;
    let Ok(bytes) = fs::read(&resolved) else {
        eprintln!(
            "{}",
            crate::localization::cli_locale().format(
                "Could not read image: {0}",
                &[(resolved.display()).to_string()]
            )
        );
        return Ok(1);
    };
    let support = detect_terminal_image_support(
        std::env::var("TERM_PROGRAM").ok().as_deref(),
        std::env::var("TERM").ok().as_deref(),
    );
    let display = resolve_inline_display(
        &bytes,
        resolved.file_name().and_then(|name| name.to_str()),
        io::stdout().is_terminal(),
        support,
    );
    match display {
        Ok(escape) => {
            io::stdout().write_all(escape.as_bytes())?;
            Ok(0)
        }
        Err(reason) => {
            eprintln!("{reason}");
            Ok(1)
        }
    }
}

fn resolve_path(value: &str) -> Result<PathBuf> {
    let path = Path::new(value);
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut resolved = PathBuf::new();
    for component in joined.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                resolved.pop();
            }
            value => resolved.push(value.as_os_str()),
        }
    }
    Ok(resolved)
}

fn detect_terminal_image_support(
    term_program: Option<&str>,
    term: Option<&str>,
) -> TerminalImageSupport {
    let term_program = term_program.unwrap_or_default();
    let term = term.unwrap_or_default();
    if matches!(term_program, "iTerm.app" | "WezTerm") {
        TerminalImageSupport::Iterm
    } else if term == "xterm-kitty" || term_program == "kitty" {
        TerminalImageSupport::Kitty
    } else if term.to_ascii_lowercase().contains("sixel") {
        TerminalImageSupport::Sixel
    } else {
        TerminalImageSupport::None
    }
}

fn resolve_inline_display(
    bytes: &[u8],
    name: Option<&str>,
    is_tty: bool,
    support: TerminalImageSupport,
) -> Result<String, String> {
    if !is_tty {
        return Err("stdout is not a TTY; inline image display requires a terminal.".into());
    }
    match support {
        TerminalImageSupport::Iterm => Ok(encode_iterm_inline(bytes, name)),
        TerminalImageSupport::Kitty => Ok(encode_kitty_inline(bytes)),
        TerminalImageSupport::Sixel | TerminalImageSupport::None => Err(format!(
            "Inline image display is not supported in this terminal (detected: {}). Use iTerm2, WezTerm, or kitty.",
            support_name(support)
        )),
    }
}

fn support_name(support: TerminalImageSupport) -> &'static str {
    match support {
        TerminalImageSupport::Iterm => "iterm",
        TerminalImageSupport::Kitty => "kitty",
        TerminalImageSupport::Sixel => "sixel",
        TerminalImageSupport::None => "none",
    }
}

fn encode_iterm_inline(bytes: &[u8], name: Option<&str>) -> String {
    let engine = base64::engine::general_purpose::STANDARD;
    let mut arguments = vec![
        "inline=1".to_string(),
        "width=auto".to_string(),
        "height=auto".to_string(),
        "preserveAspectRatio=0".to_string(),
    ];
    if let Some(name) = name {
        arguments.push(format!("name={}", engine.encode(name.as_bytes())));
    }
    format!(
        "\u{1b}]1337;File={}:{}\u{7}",
        arguments.join(";"),
        engine.encode(bytes)
    )
}

fn encode_kitty_inline(bytes: &[u8]) -> String {
    let payload = base64::engine::general_purpose::STANDARD.encode(bytes);
    if payload.is_empty() {
        return "\u{1b}_Ga=T,f=100,t=f;\u{1b}\\".into();
    }
    payload
        .as_bytes()
        .chunks(KITTY_CHUNK_SIZE)
        .enumerate()
        .map(|(index, chunk)| {
            let chunk = String::from_utf8_lossy(chunk);
            if index == 0 {
                format!("\u{1b}_Ga=T,f=100,t=f;{chunk}\u{1b}\\")
            } else {
                format!("\u{1b}_Gm=1;{chunk}\u{1b}\\")
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn help_documents_supported_terminals() {
        assert!(painter_help().contains("maestro painter show"));
        assert!(painter_help().contains("iTerm2"));
        assert!(painter_help().contains("kitty"));
    }

    #[test]
    fn terminal_detection_matches_legacy_contract() {
        assert_eq!(
            detect_terminal_image_support(Some("iTerm.app"), None),
            TerminalImageSupport::Iterm
        );
        assert_eq!(
            detect_terminal_image_support(Some("WezTerm"), None),
            TerminalImageSupport::Iterm
        );
        assert_eq!(
            detect_terminal_image_support(None, Some("xterm-kitty")),
            TerminalImageSupport::Kitty
        );
        assert_eq!(
            detect_terminal_image_support(None, Some("xterm-sixel")),
            TerminalImageSupport::Sixel
        );
    }

    #[test]
    fn iterm_encoding_preserves_filename_and_payload() {
        let encoded = encode_iterm_inline(&[0x89, 0x50, 0x4e, 0x47], Some("img.png"));
        assert!(encoded.starts_with("\u{1b}]1337;File="));
        assert!(encoded.contains("name=aW1nLnBuZw=="));
        assert!(encoded.ends_with("iVBORw==\u{7}"));
    }

    #[test]
    fn kitty_encoding_chunks_large_payloads() {
        let encoded = encode_kitty_inline(&vec![7; 4_000]);
        assert!(encoded.starts_with("\u{1b}_Ga=T,f=100,t=f;"));
        assert!(encoded.contains("\u{1b}\\\u{1b}_Gm=1;"));
    }

    #[test]
    fn inline_display_rejects_non_tty_and_unsupported_terminals() {
        let bytes = [0x89, 0x50, 0x4e, 0x47];
        assert!(
            resolve_inline_display(&bytes, None, false, TerminalImageSupport::Iterm)
                .unwrap_err()
                .contains("not a TTY")
        );
        assert!(
            resolve_inline_display(&bytes, None, true, TerminalImageSupport::None)
                .unwrap_err()
                .contains("detected: none")
        );
        assert!(resolve_inline_display(&bytes, None, true, TerminalImageSupport::Sixel).is_err());
    }
}

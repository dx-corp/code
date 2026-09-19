//! Terminal Information and Detection Utilities
//!
//! Provides comprehensive terminal detection that works across local, SSH, and WSL sessions:
//!
//! - **Terminal Emulator Detection**: Identify kitty, iTerm2, Alacritty, WezTerm, etc.
//! - **SSH Detection**: Check if running in an SSH session
//! - **WSL Detection**: Check if running in Windows Subsystem for Linux
//! - **Path Conversion**: Convert Windows paths to WSL paths
//! - **User-Agent Generation**: For API requests
//!
//! Ported from OpenAI Codex CLI (MIT licensed).

use std::path::PathBuf;
use std::sync::OnceLock;

// ─────────────────────────────────────────────────────────────────────────────
// TERMINAL EMULATOR DETECTION
// ─────────────────────────────────────────────────────────────────────────────

static TERMINAL_INFO: OnceLock<TerminalInfo> = OnceLock::new();

/// Information about the detected terminal emulator.
#[derive(Debug, Clone)]
pub struct TerminalInfo {
    /// Terminal name (e.g., "kitty", "iTerm2", "Alacritty")
    pub name: String,
    /// Optional version string
    pub version: Option<String>,
    /// Whether this is running in an SSH session
    pub is_ssh: bool,
    /// Whether this is running in WSL
    pub is_wsl: bool,
    /// Whether the terminal supports true color (24-bit)
    pub supports_true_color: bool,
    /// Whether the terminal supports 256 colors
    pub supports_256_color: bool,
    /// Whether the terminal supports keyboard enhancement
    pub supports_keyboard_enhancement: bool,
}

impl TerminalInfo {
    /// Get the cached terminal info, detecting on first call.
    pub fn get() -> &'static TerminalInfo {
        TERMINAL_INFO.get_or_init(detect_terminal_info)
    }

    /// Format as a user-agent string for API requests.
    #[must_use]
    pub fn user_agent(&self) -> String {
        let name = sanitize_header_value(&self.name);
        match &self.version {
            Some(v) if !v.is_empty() => format!("{}/{}", name, sanitize_header_value(v)),
            _ => name,
        }
    }
}

/// Detect terminal information from environment.
fn detect_terminal_info() -> TerminalInfo {
    let (name, version) = detect_terminal_name_version();
    let is_ssh = is_ssh_session();
    let is_wsl = is_wsl();

    TerminalInfo {
        name,
        version,
        is_ssh,
        is_wsl,
        supports_true_color: crate::color_utils::has_true_color_support(),
        supports_256_color: crate::color_utils::has_256_color_support(),
        supports_keyboard_enhancement: false, // Set at runtime by TUI
    }
}

/// Detect terminal name and version from environment variables.
fn detect_terminal_name_version() -> (String, Option<String>) {
    // TERM_PROGRAM is the most reliable when set
    if let Ok(tp) = std::env::var("TERM_PROGRAM") {
        if !tp.trim().is_empty() {
            let ver = std::env::var("TERM_PROGRAM_VERSION").ok();
            return (tp, ver);
        }
    }

    // WezTerm
    if let Ok(v) = std::env::var("WEZTERM_VERSION") {
        if !v.trim().is_empty() {
            return ("WezTerm".to_string(), Some(v));
        }
        return ("WezTerm".to_string(), None);
    }

    // Kitty
    if std::env::var("KITTY_WINDOW_ID").is_ok()
        || std::env::var("TERM")
            .map(|t| t.contains("kitty"))
            .unwrap_or(false)
    {
        return ("kitty".to_string(), None);
    }

    // Alacritty
    if std::env::var("ALACRITTY_SOCKET").is_ok()
        || std::env::var("ALACRITTY_LOG").is_ok()
        || std::env::var("TERM")
            .map(|t| t == "alacritty")
            .unwrap_or(false)
    {
        return ("Alacritty".to_string(), None);
    }

    // Ghostty
    if std::env::var("GHOSTTY_RESOURCES_DIR").is_ok() {
        return ("Ghostty".to_string(), None);
    }

    // Konsole
    if let Ok(v) = std::env::var("KONSOLE_VERSION") {
        if !v.trim().is_empty() {
            return ("Konsole".to_string(), Some(v));
        }
        return ("Konsole".to_string(), None);
    }

    // GNOME Terminal
    if std::env::var("GNOME_TERMINAL_SCREEN").is_ok() {
        return ("gnome-terminal".to_string(), None);
    }

    // VTE-based terminals
    if let Ok(v) = std::env::var("VTE_VERSION") {
        if !v.trim().is_empty() {
            return ("VTE".to_string(), Some(v));
        }
        return ("VTE".to_string(), None);
    }

    // Windows Terminal
    if std::env::var("WT_SESSION").is_ok() {
        return ("WindowsTerminal".to_string(), None);
    }

    // Fallback to TERM
    let term = fallback_terminal_name(std::env::var("TERM").ok());
    (term, None)
}

fn fallback_terminal_name(term: Option<String>) -> String {
    term.filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "unknown".to_string())
}

/// Sanitize a string for use in HTTP header values.
fn sanitize_header_value(value: &str) -> String {
    value
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' || c == '/' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

// ─────────────────────────────────────────────────────────────────────────────
// SSH DETECTION
// ─────────────────────────────────────────────────────────────────────────────

/// Check if the current session is an SSH session.
///
/// Checks multiple signals:
/// - `SSH_CLIENT` environment variable
/// - `SSH_TTY` environment variable
/// - `SSH_CONNECTION` environment variable
#[must_use]
pub fn is_ssh_session() -> bool {
    std::env::var("SSH_CLIENT").is_ok()
        || std::env::var("SSH_TTY").is_ok()
        || std::env::var("SSH_CONNECTION").is_ok()
}

/// Get SSH connection info if available.
///
/// Returns (`client_ip`, `client_port`, `server_port`) if `SSH_CONNECTION` is set.
#[must_use]
pub fn ssh_connection_info() -> Option<(String, String, String)> {
    let conn = std::env::var("SSH_CONNECTION").ok()?;
    let parts: Vec<&str> = conn.split_whitespace().collect();
    if parts.len() >= 3 {
        Some((
            parts[0].to_string(),
            parts[1].to_string(),
            parts[2].to_string(),
        ))
    } else {
        None
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// WSL DETECTION AND PATH CONVERSION
// ─────────────────────────────────────────────────────────────────────────────

/// Check if running in Windows Subsystem for Linux.
///
/// Uses multiple detection methods:
/// 1. Check /proc/version for "microsoft" or "WSL" (most reliable)
/// 2. Check WSL-specific environment variables (handles custom kernels)
#[cfg(target_os = "linux")]
pub fn is_wsl() -> bool {
    // Primary: Check /proc/version for "microsoft" or "WSL"
    if let Ok(version) = std::fs::read_to_string("/proc/version") {
        let version_lower = version.to_lowercase();
        if version_lower.contains("microsoft") || version_lower.contains("wsl") {
            return true;
        }
    }

    // Fallback: Check WSL environment variables
    // This handles edge cases like custom Linux kernels installed in WSL
    std::env::var_os("WSL_DISTRO_NAME").is_some() || std::env::var_os("WSL_INTEROP").is_some()
}

#[cfg(not(target_os = "linux"))]
#[must_use]
pub fn is_wsl() -> bool {
    false
}

/// Convert a Windows path to a WSL path.
///
/// Converts paths like `C:\Users\Alice\file.txt` to `/mnt/c/Users/Alice/file.txt`.
///
/// Returns `None` for:
/// - UNC paths (\\\\server\\share)
/// - Invalid Windows paths
/// - Non-Linux systems
///
/// # Example
///
/// ```rust,ignore
/// use maestro_local_host::terminal_info::convert_windows_path_to_wsl;
///
/// let wsl_path = convert_windows_path_to_wsl(r"C:\Users\Alice\file.txt");
/// assert_eq!(wsl_path, Some(PathBuf::from("/mnt/c/Users/Alice/file.txt")));
/// ```
#[cfg(target_os = "linux")]
pub fn convert_windows_path_to_wsl(input: &str) -> Option<PathBuf> {
    // Don't convert UNC paths
    if input.starts_with("\\\\") {
        return None;
    }

    // Must start with drive letter
    let drive_letter = input.chars().next()?.to_ascii_lowercase();
    if !drive_letter.is_ascii_lowercase() {
        return None;
    }

    // Must have colon after drive letter
    if input.get(1..2) != Some(":") {
        return None;
    }

    // Build WSL path: /mnt/{drive_letter}/{rest}
    let mut result = PathBuf::from(format!("/mnt/{drive_letter}"));
    for component in input
        .get(2..)?
        .trim_start_matches(['\\', '/'])
        .split(['\\', '/'])
        .filter(|component| !component.is_empty())
    {
        result.push(component);
    }

    Some(result)
}

#[cfg(not(target_os = "linux"))]
#[must_use]
pub fn convert_windows_path_to_wsl(_input: &str) -> Option<PathBuf> {
    None
}

/// Normalize a pasted path that may be a Windows path, file:// URL, or shell-escaped.
///
/// Handles:
/// - `file://` URLs
/// - Windows drive paths (C:\...)
/// - UNC paths (\\\\server\\share)
/// - Shell-escaped paths
///
/// On WSL, Windows paths are automatically converted to WSL paths.
pub fn normalize_pasted_path(pasted: &str) -> Option<PathBuf> {
    let pasted = pasted.trim();

    // Handle file:// URLs
    if let Ok(url) = url::Url::parse(pasted) {
        if url.scheme() == "file" {
            return url.to_file_path().ok();
        }
    }

    // Detect Windows paths (drive letter or UNC)
    let looks_like_windows_path = {
        // Drive letter path: C:\ or C:/
        let drive = pasted
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic())
            && pasted.get(1..2) == Some(":")
            && pasted.get(2..3).is_some_and(|s| s == "\\" || s == "/");
        // UNC path: \\server\share
        let unc = pasted.starts_with("\\\\");
        drive || unc
    };

    if looks_like_windows_path {
        #[cfg(target_os = "linux")]
        {
            if is_wsl() {
                if let Some(converted) = convert_windows_path_to_wsl(pasted) {
                    return Some(converted);
                }
            }
        }
        return Some(PathBuf::from(pasted));
    }

    // Try shell unescaping for single paths
    let parts: Vec<String> = shlex::Shlex::new(pasted).collect();
    if parts.len() == 1 {
        return parts.into_iter().next().map(PathBuf::from);
    }

    None
}

// ─────────────────────────────────────────────────────────────────────────────
// TERMINAL CAPABILITY QUERIES
// ─────────────────────────────────────────────────────────────────────────────

/// Check if stdin is a terminal (TTY).
#[must_use]
pub fn is_stdin_tty() -> bool {
    use std::io::IsTerminal;
    std::io::stdin().is_terminal()
}

/// Check if stdout is a terminal (TTY).
#[must_use]
pub fn is_stdout_tty() -> bool {
    use std::io::IsTerminal;
    std::io::stdout().is_terminal()
}

/// Check if stderr is a terminal (TTY).
#[must_use]
pub fn is_stderr_tty() -> bool {
    use std::io::IsTerminal;
    std::io::stderr().is_terminal()
}

/// Check if running in a fully interactive terminal session.
///
/// Returns true if both stdin and stdout are TTYs.
#[must_use]
pub fn is_interactive() -> bool {
    is_stdin_tty() && is_stdout_tty()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sanitize_header_value() {
        assert_eq!(sanitize_header_value("iTerm2"), "iTerm2");
        assert_eq!(
            sanitize_header_value("WezTerm/20240101"),
            "WezTerm/20240101"
        );
        assert_eq!(
            sanitize_header_value("term with spaces"),
            "term_with_spaces"
        );
        assert_eq!(sanitize_header_value("term@special!"), "term_special_");
    }

    #[test]
    fn test_terminal_info_get() {
        // Just ensure it doesn't panic
        let info = TerminalInfo::get();
        assert!(!info.name.is_empty());
    }

    #[test]
    fn empty_term_falls_back_to_unknown() {
        assert_eq!(fallback_terminal_name(None), "unknown");
        assert_eq!(fallback_terminal_name(Some(String::new())), "unknown");
        assert_eq!(fallback_terminal_name(Some("  ".into())), "unknown");
        assert_eq!(
            fallback_terminal_name(Some("xterm-256color".into())),
            "xterm-256color"
        );
    }

    #[test]
    fn test_user_agent_format() {
        let info = TerminalInfo {
            name: "TestTerm".to_string(),
            version: Some("1.0.0".to_string()),
            is_ssh: false,
            is_wsl: false,
            supports_true_color: true,
            supports_256_color: true,
            supports_keyboard_enhancement: false,
        };
        assert_eq!(info.user_agent(), "TestTerm/1.0.0");
    }

    #[test]
    fn test_user_agent_no_version() {
        let info = TerminalInfo {
            name: "TestTerm".to_string(),
            version: None,
            is_ssh: false,
            is_wsl: false,
            supports_true_color: true,
            supports_256_color: true,
            supports_keyboard_enhancement: false,
        };
        assert_eq!(info.user_agent(), "TestTerm");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_convert_windows_path_to_wsl() {
        let result = convert_windows_path_to_wsl(r"C:\Users\Alice\file.txt");
        assert_eq!(result, Some(PathBuf::from("/mnt/c/Users/Alice/file.txt")));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_convert_windows_path_with_forward_slashes() {
        let result = convert_windows_path_to_wsl("C:/Users/Alice/file.txt");
        assert_eq!(result, Some(PathBuf::from("/mnt/c/Users/Alice/file.txt")));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_convert_unc_path_returns_none() {
        let result = convert_windows_path_to_wsl(r"\\server\share\file.txt");
        assert!(result.is_none());
    }

    #[test]
    fn test_normalize_file_url() {
        #[cfg(not(windows))]
        {
            let result = normalize_pasted_path("file:///tmp/test.txt");
            assert_eq!(result, Some(PathBuf::from("/tmp/test.txt")));
        }
    }

    #[test]
    fn test_normalize_shell_escaped() {
        let result = normalize_pasted_path("/path/with\\ space/file.txt");
        assert_eq!(result, Some(PathBuf::from("/path/with space/file.txt")));
    }

    #[test]
    fn test_normalize_quoted_path() {
        let result = normalize_pasted_path("'/path/with space/file.txt'");
        assert_eq!(result, Some(PathBuf::from("/path/with space/file.txt")));
    }

    #[test]
    fn test_normalize_multiple_paths_returns_none() {
        let result = normalize_pasted_path("/path/a.txt /path/b.txt");
        assert!(result.is_none());
    }
}

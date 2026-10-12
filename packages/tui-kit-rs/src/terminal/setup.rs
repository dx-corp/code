//! Terminal setup and teardown
//!
//! This module handles low-level terminal initialization and cleanup, including
//! raw mode configuration, keyboard enhancement flags, and panic hook installation.
//!
//! # Platform-Specific Design
//!
//! We use a dedicated terminal device for output instead of stdin/stdout:
//!
//! - **Unix:** `/dev/tty`
//! - **Windows:** `CONOUT$`
//!
//! This allows the application to reserve stdin/stdout for IPC communication with
//! the TypeScript backend while maintaining terminal control across platforms.
//!
//! # Raw Mode Configuration
//!
//! The terminal is configured with crossterm's raw mode, which provides:
//!
//! - No line buffering (characters available immediately)
//! - No echo (application controls output)
//! - No canonical mode processing
//! - Direct access to all keyboard events
//!
//! # Keyboard Enhancement
//!
//! On terminals that support it (detected via crossterm's `supports_keyboard_enhancement()`),
//! we enable enhanced keyboard protocol flags:
//!
//! - `DISAMBIGUATE_ESCAPE_CODES`: Distinguish Escape key from Alt+key sequences
//! - `REPORT_EVENT_TYPES`: Differentiate press, release, and repeat events
//! - `REPORT_ALTERNATE_KEYS`: Provide base layout keys alongside modified ones
//!
//! These enhancements improve the reliability of keyboard shortcuts, especially in
//! SSH sessions and modern terminals like iTerm2, `WezTerm`, and Kitty.
//!
//! # Inline Viewport Mode
//!
//! This terminal uses ratatui's inline viewport mode, which:
//!
//! - Reserves a fixed number of rows at the bottom of the terminal screen
//! - Allows content above the viewport to scroll into native terminal scrollback
//! - Maintains compatibility with SSH, tmux, and screen scrollback buffers
//! - Preserves the user's existing terminal content above the TUI
//!
//! The viewport height is calculated as `terminal_height - 2` to leave room for
//! context lines while maximizing usable space.
//!
//! # Panic Hook
//!
//! A custom panic hook is installed that ensures the terminal is properly restored
//! even if the application crashes. This prevents leaving the terminal in a broken
//! state (raw mode, hidden cursor, etc.) which would require manual reset.
//!
//! # Crash Handler
//!
//! In addition to the panic hook (which only fires for Rust panics),
//! [`crate::crash_handler`] installs an async-signal-safe SIGSEGV/SIGBUS
//! handler that records the crash under `~/.composer/crash/` and restores the
//! terminal from signal-handler context. The release profile uses
//! `panic = "abort"`, so panics run the hook and then die via SIGABRT; the
//! signal handler covers the hard-crash paths the hook cannot reach.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::panic;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use crossterm::{
    cursor,
    event::{
        DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, EnableBracketedPaste,
        EnableFocusChange, EnableMouseCapture, KeyboardEnhancementFlags,
        PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, supports_keyboard_enhancement},
};
use ratatui::backend::CrosstermBackend;
use ratatui::{TerminalOptions, Viewport};
use uncurses::ansi::color::REQUEST_BACKGROUND_COLOR;
use uncurses::ansi::mode::{self, Mode};
use uncurses::ansi::status::REQUEST_LIGHT_DARK_REPORT;

use crate::sync_output::EndSynchronizedUpdate;

/// Global terminal device handle for terminal output.
///
/// This static mutex stores the terminal device handle after initialization
/// (e.g. `/dev/tty` or `CONOUT$`), making it available for cleanup in the
/// panic hook and restore functions.
///
/// We use `Lazy` from `once_cell` to ensure thread-safe lazy initialization,
/// and `Mutex` to provide interior mutability for the restore operation.
static TTY: std::sync::LazyLock<Mutex<Option<File>>> =
    std::sync::LazyLock::new(|| Mutex::new(None));
/// True only when Maestro changed DEC mode 2031 from reset to set.
static THEME_REPORTING_ENABLED_BY_MAESTRO: AtomicBool = AtomicBool::new(false);

#[cfg(windows)]
const TERMINAL_DEVICE: &str = "CONOUT$";
#[cfg(not(windows))]
const TERMINAL_DEVICE: &str = "/dev/tty";

/// Type alias for our terminal backend.
///
/// Uses `CrosstermBackend<File>` instead of the typical `CrosstermBackend<Stdout>`
/// because we write to the terminal device rather than stdout. This allows
/// stdin/stdout to be used for IPC communication with the TypeScript backend.
pub type Terminal = ratatui::Terminal<CrosstermBackend<File>>;

/// Terminal capabilities detected during initialization.
///
/// This struct captures feature detection results and viewport configuration,
/// allowing the application to adapt its behavior based on terminal capabilities.
///
/// # Fields
///
/// - `enhanced_keys`: Whether the terminal supports the enhanced keyboard protocol,
///   which provides better modifier key disambiguation and event type reporting.
///
/// - `viewport_top`: The 1-indexed row number where the inline viewport begins.
///   Used for ANSI scroll region operations (DECSTBM) to push content into scrollback.
///
/// - `viewport_height`: The number of rows allocated to the inline viewport.
///   Typically `terminal_height - 2` to maximize space while leaving context.
#[derive(Debug, Clone, Copy)]
pub struct TerminalCapabilities {
    /// Whether the terminal supports enhanced keyboard (modifier disambiguation)
    pub enhanced_keys: bool,
    /// The row where the viewport starts (1-indexed for ANSI)
    pub viewport_top: u16,
    /// Height of the viewport
    pub viewport_height: u16,
}

/// Check if a terminal device is available.
///
/// Returns `true` if the application can open the terminal device for read/write,
/// indicating that we're running in a terminal environment. Returns `false`
/// if running in a non-interactive context (e.g., piped input, systemd service).
///
/// This is a quick availability check that discards error details. For detailed
/// error reporting, use [`check_tty()`] instead.
#[must_use]
pub fn is_tty_available() -> bool {
    OpenOptions::new()
        .read(true)
        .write(true)
        .open(TERMINAL_DEVICE)
        .is_ok()
}

/// Check if a terminal device is available, returning detailed errors.
///
/// This function attempts to open the terminal device for read/write access and returns
/// an `io::Result` that can be used to diagnose why TTY access failed.
///
/// # Errors
///
/// Returns an error if:
/// - terminal device doesn't exist (not a Unix system)
/// - No controlling terminal (running as a daemon, via SSH without TTY allocation)
/// - Permission denied (rare, but possible in restricted environments)
///
/// # Example
///
/// ```no_run
/// # use maestro_tui_kit::terminal::check_tty;
/// if let Err(e) = check_tty() {
///     eprintln!("Cannot access terminal: {}", e);
///     std::process::exit(1);
/// }
/// ```
pub fn check_tty() -> io::Result<()> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .open(TERMINAL_DEVICE)
        .map(|_| ())
}

/// Initialize the terminal for TUI rendering.
///
/// This sets up:
/// - Raw mode (no line buffering, no echo)
/// - Inline viewport mode (content scrolls into native scrollback)
/// - Bracketed paste mode
/// - Keyboard enhancement flags (if supported)
/// - Focus change events
/// - Panic hook to restore terminal on crash
/// - SIGSEGV/SIGBUS crash handler (records the crash, restores the terminal),
///   plus a one-line notice if the previous run crashed
///
/// Uses a platform-specific terminal device (`/dev/tty` on Unix, `CONOUT$` on Windows)
/// so that stdin/stdout can be used for IPC.
pub fn init() -> io::Result<(Terminal, TerminalCapabilities)> {
    // Surface a crash from the previous run before taking over the terminal.
    // This must run before `crash_handler::install` below, which truncates
    // the crash blob this check reads.
    if let Some(crash_dir) = crate::crash_handler::default_crash_dir() {
        if let Some(report) = crate::crash_handler::check_previous_crash(&crash_dir) {
            eprintln!("{}", crate::crash_handler::crash_notice(&report));
        }
    }

    // Open terminal device for I/O
    // This allows us to use stdin/stdout for IPC with TypeScript
    let mut tty = OpenOptions::new()
        .read(true)
        .write(true)
        .open(TERMINAL_DEVICE)
        .map_err(|e| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("Cannot open {TERMINAL_DEVICE}: {e}"),
            )
        })?;

    // Save the pre-raw-mode termios so the crash handler can restore the
    // terminal from async-signal-safe context.
    crate::crash_handler::save_terminal_state(&tty);

    // Get terminal size
    let (_width, height) = crossterm::terminal::size()?;

    // Reserve some rows for terminal scrollback history
    // Use most of the screen but leave a few lines at the top for context
    let viewport_height = height.saturating_sub(2).max(10);
    let viewport_top = height.saturating_sub(viewport_height) + 1; // 1-indexed for ANSI

    // Check capabilities before entering raw mode
    let enhanced_keys = supports_keyboard_enhancement().unwrap_or(false);

    // Enable raw mode
    enable_raw_mode()?;

    // Enable bracketed paste
    execute!(tty, EnableBracketedPaste)?;

    // Try to enable keyboard enhancement (may fail on some terminals)
    if enhanced_keys {
        let _ = execute!(
            tty,
            PushKeyboardEnhancementFlags(
                KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                    | KeyboardEnhancementFlags::REPORT_EVENT_TYPES
                    | KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS
            )
        );
    }

    // Enable focus change events
    let _ = execute!(tty, EnableFocusChange);

    // Enable mouse capture for scroll wheel support
    let _ = execute!(tty, EnableMouseCapture);

    // Move cursor to bottom of screen and print enough newlines to create
    // space for the inline viewport. This ensures the viewport starts at
    // the correct position for history push.
    write!(tty, "\x1b[{height};1H")?; // Move to last row
    for _ in 0..viewport_height {
        writeln!(tty)?;
    }
    tty.flush()?;

    // Set up panic hook to restore terminal
    let original_hook = panic::take_hook();
    panic::set_hook(Box::new(move |panic_info| {
        // Attempt to restore terminal before printing panic
        let _ = restore_impl();
        original_hook(panic_info);
    }));

    // Store the TTY handle globally for restore
    // Use unwrap_or_else to recover from poisoned locks
    *TTY.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(tty.try_clone()?);

    // Create the terminal with inline viewport mode
    let backend = CrosstermBackend::new(tty);
    let terminal = Terminal::with_options(
        backend,
        TerminalOptions {
            viewport: Viewport::Inline(viewport_height),
        },
    )?;

    let capabilities = TerminalCapabilities {
        enhanced_keys,
        viewport_top,
        viewport_height,
    };

    // Install the hard-crash (SIGSEGV/SIGBUS) handler now that the terminal
    // is configured. The panic hook above covers Rust panics; this covers
    // crashes the hook cannot intercept.
    if let Some(crash_dir) = crate::crash_handler::default_crash_dir() {
        let _ = crate::crash_handler::install(&crash_dir, env!("CARGO_PKG_VERSION"));
    }

    Ok((terminal, capabilities))
}

/// Initialize a fallback terminal for non-interactive contexts.
///
/// This avoids raw mode and `/dev/tty` usage, falling back to a null sink.
pub fn init_fallback() -> io::Result<(Terminal, TerminalCapabilities)> {
    let fallback_path = if cfg!(windows) { "NUL" } else { "/dev/null" };
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(fallback_path)?;
    let (_width, height) = crossterm::terminal::size().unwrap_or((80, 24));
    let (viewport_top, viewport_height) = calculate_viewport(height);

    let original_hook = panic::take_hook();
    panic::set_hook(Box::new(move |panic_info| {
        let _ = restore_impl();
        original_hook(panic_info);
    }));

    *TTY.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(file.try_clone()?);

    let backend = CrosstermBackend::new(file);
    let terminal = Terminal::with_options(
        backend,
        TerminalOptions {
            viewport: Viewport::Inline(viewport_height),
        },
    )?;

    let capabilities = TerminalCapabilities {
        enhanced_keys: false,
        viewport_top,
        viewport_height,
    };

    Ok((terminal, capabilities))
}

/// Restore the terminal to its original state.
pub fn restore() -> io::Result<()> {
    restore_impl()
}

/// Write raw output (e.g. OSC escape sequences) to the terminal device.
///
/// Uses the terminal handle stored during [`init()`]; when the terminal was
/// never initialized (headless/fallback contexts, unit tests) this is a
/// no-op so callers never write stray sequences to stdout/stderr.
pub fn write_raw(data: &str) -> io::Result<()> {
    let mut guard = TTY
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(ref mut tty) = *guard {
        tty.write_all(data.as_bytes())?;
        tty.flush()?;
    }
    Ok(())
}

/// Discover the existing light/dark reporting mode and current terminal theme.
pub fn initialize_theme_reporting() -> io::Result<()> {
    write_raw_bytes(&theme_reporting_initialization_sequence()?)
}

/// Enable live light/dark reports after the terminal reports the mode as reset.
pub fn enable_theme_reporting() -> io::Result<()> {
    write_raw_bytes(&theme_reporting_enable_sequence()?)?;
    THEME_REPORTING_ENABLED_BY_MAESTRO.store(true, Ordering::Release);
    Ok(())
}

/// Request the terminal's current light/dark preference and background color.
pub fn query_theme() -> io::Result<()> {
    write_raw_bytes(&theme_query_sequence())
}

/// Stop live light/dark reports.
pub fn disable_theme_reporting() -> io::Result<()> {
    if !THEME_REPORTING_ENABLED_BY_MAESTRO.swap(false, Ordering::AcqRel) {
        return Ok(());
    }
    if let Err(error) = write_raw_bytes(&theme_reporting_disable_sequence()?) {
        THEME_REPORTING_ENABLED_BY_MAESTRO.store(true, Ordering::Release);
        return Err(error);
    }
    Ok(())
}

fn theme_reporting_initialization_sequence() -> io::Result<Vec<u8>> {
    let mut data = Vec::new();
    mode::write_request_mode(&mut data, Mode::LIGHT_DARK)?;
    data.extend_from_slice(&theme_query_sequence());
    Ok(data)
}

fn theme_reporting_enable_sequence() -> io::Result<Vec<u8>> {
    let mut data = Vec::new();
    mode::write_set_mode(&mut data, &[Mode::LIGHT_DARK])?;
    Ok(data)
}

fn theme_query_sequence() -> Vec<u8> {
    let mut data =
        Vec::with_capacity(REQUEST_LIGHT_DARK_REPORT.len() + REQUEST_BACKGROUND_COLOR.len());
    data.extend_from_slice(REQUEST_LIGHT_DARK_REPORT);
    data.extend_from_slice(REQUEST_BACKGROUND_COLOR);
    data
}

fn theme_reporting_disable_sequence() -> io::Result<Vec<u8>> {
    let mut data = Vec::new();
    mode::write_reset_mode(&mut data, &[Mode::LIGHT_DARK])?;
    Ok(data)
}

fn write_raw_bytes(data: &[u8]) -> io::Result<()> {
    let mut guard = TTY
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(ref mut tty) = *guard {
        tty.write_all(data)?;
        tty.flush()?;
    }
    Ok(())
}

fn restore_impl() -> io::Result<()> {
    // Get the TTY handle - recover from poisoned lock to ensure terminal cleanup
    let mut guard = TTY
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);

    if let Some(ref mut tty) = *guard {
        // Never leave a terminal holding a synchronized frame. Reset live
        // light/dark reporting only if Maestro enabled it; a parent process
        // may have entered with mode 2031 already set.
        let _ = execute!(tty, EndSynchronizedUpdate);
        if THEME_REPORTING_ENABLED_BY_MAESTRO.swap(false, Ordering::AcqRel) {
            let _ = mode::write_reset_mode(tty, &[Mode::LIGHT_DARK]);
        }

        // Pop keyboard enhancement flags
        let _ = execute!(tty, PopKeyboardEnhancementFlags);

        // Disable bracketed paste
        let _ = execute!(tty, DisableBracketedPaste);

        // Disable focus change
        let _ = execute!(tty, DisableFocusChange);

        // Disable mouse capture
        let _ = execute!(tty, DisableMouseCapture);

        // Show cursor
        let _ = execute!(tty, cursor::Show);
    }

    // Disable raw mode
    disable_raw_mode()?;

    Ok(())
}

/// Get the current terminal size.
///
/// Returns a tuple of `(width, height)` in columns and rows. This uses crossterm's
/// `terminal::size()` which queries the terminal via ioctl on Unix systems.
///
/// # Errors
///
/// Returns an error if the terminal size cannot be determined (e.g., not running
/// in a terminal, or the terminal driver doesn't support size queries).
///
/// # Example
///
/// ```no_run
/// # use maestro_tui_kit::terminal::size;
/// let (width, height) = size()?;
/// println!("Terminal is {}x{} characters", width, height);
/// # Ok::<(), std::io::Error>(())
/// ```
pub fn size() -> io::Result<(u16, u16)> {
    crossterm::terminal::size()
}

/// Calculate viewport dimensions after a terminal resize.
///
/// This function computes the optimal viewport height and starting row based on
/// the new terminal height, maintaining the inline viewport layout strategy.
///
/// # Arguments
///
/// - `height`: The new terminal height in rows
///
/// # Returns
///
/// A tuple of `(viewport_top, viewport_height)` where:
/// - `viewport_top` is the 1-indexed row where the viewport starts
/// - `viewport_height` is the number of rows allocated to the viewport
///
/// The viewport height is calculated as `height - 2`, with a minimum of 10 rows
/// to ensure usability even in very small terminals.
#[must_use]
pub fn calculate_viewport(height: u16) -> (u16, u16) {
    let viewport_height = height.saturating_sub(2).max(10);
    let viewport_top = height.saturating_sub(viewport_height) + 1;
    (viewport_top, viewport_height)
}

/// Recreate the terminal with a new inline viewport height.
///
/// ratatui's `Viewport::Inline(height)` fixes the height at construction, so
/// the only way to grow or shrink the viewport after a terminal resize is to
/// rebuild the `Terminal` around the same terminal device handle.
pub fn recreate_with_viewport(viewport_height: u16) -> io::Result<Terminal> {
    let tty = TTY
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .as_ref()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "terminal not initialized"))?
        .try_clone()?;
    let backend = CrosstermBackend::new(tty);
    Terminal::with_options(
        backend,
        TerminalOptions {
            viewport: Viewport::Inline(viewport_height),
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn theme_reporting_discovers_before_enabling_and_queries_both_signals() {
        assert_eq!(
            theme_reporting_initialization_sequence().unwrap(),
            b"\x1b[?2031$p\x1b[?996n\x1b]11;?\x07"
        );
        assert_eq!(theme_reporting_enable_sequence().unwrap(), b"\x1b[?2031h");
        assert_eq!(theme_query_sequence(), b"\x1b[?996n\x1b]11;?\x07");
        assert_eq!(theme_reporting_disable_sequence().unwrap(), b"\x1b[?2031l");
    }
}

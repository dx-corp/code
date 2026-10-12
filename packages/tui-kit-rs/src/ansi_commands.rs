//! ANSI Terminal Commands for Scroll Regions and Terminal Control
//!
//! This module provides crossterm-compatible Commands for advanced terminal
//! operations that are essential for proper scrolling over SSH:
//!
//! - **Scroll Regions (DECSTBM)**: Limit scrolling to a subset of the terminal
//! - **Reverse Index (RI)**: Scroll content down within a region
//! - **Alternate Screen Scroll**: Enable mouse wheel translation in alt screen
//! - **Desktop Notifications (OSC 9)**: Send notifications when unfocused
//!
//! These commands work over SSH because they use standard ANSI escape sequences
//! supported by virtually all terminal emulators.
//!
//! Ported from OpenAI Codex CLI (MIT licensed).

use std::fmt;
use std::io;
use std::ops::Range;

use crossterm::Command;

// ─────────────────────────────────────────────────────────────────────────────
// SCROLL REGION COMMANDS (DECSTBM)
// ─────────────────────────────────────────────────────────────────────────────

/// Set the scrolling region (DECSTBM - DEC Set Top and Bottom Margins).
///
/// This limits scrolling to the specified range of rows. When content is added
/// within this region, only the region scrolls, not the entire terminal.
///
/// The range uses 1-based row numbers (as per ANSI standard).
///
/// # Example
///
/// ```rust,ignore
/// use crossterm::execute;
/// use maestro_tui_kit::ansi_commands::SetScrollRegion;
///
/// // Set scroll region to rows 5-20 (1-based)
/// execute!(stdout(), SetScrollRegion(5..20))?;
/// ```
///
/// # ANSI Sequence
///
/// `ESC [ <top> ; <bottom> r` - CSI Pt ; Pb r
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetScrollRegion(pub Range<u16>);

impl Command for SetScrollRegion {
    fn write_ansi(&self, f: &mut impl fmt::Write) -> fmt::Result {
        // DECSTBM: ESC [ Pt ; Pb r
        // Pt = top row (1-based), Pb = bottom row (1-based)
        write!(f, "\x1b[{};{}r", self.0.start, self.0.end)
    }

    #[cfg(windows)]
    fn execute_winapi(&self) -> io::Result<()> {
        Err(io::Error::other(
            "SetScrollRegion requires ANSI support; WinAPI not supported",
        ))
    }

    #[cfg(windows)]
    fn is_ansi_code_supported(&self) -> bool {
        true
    }
}

/// Reset the scrolling region to the full terminal height.
///
/// This undoes any previous `SetScrollRegion` command, restoring normal
/// full-screen scrolling behavior.
///
/// # ANSI Sequence
///
/// `ESC [ r` - CSI r (no parameters = reset to full screen)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResetScrollRegion;

impl Command for ResetScrollRegion {
    fn write_ansi(&self, f: &mut impl fmt::Write) -> fmt::Result {
        write!(f, "\x1b[r")
    }

    #[cfg(windows)]
    fn execute_winapi(&self) -> io::Result<()> {
        Err(io::Error::other(
            "ResetScrollRegion requires ANSI support; WinAPI not supported",
        ))
    }

    #[cfg(windows)]
    fn is_ansi_code_supported(&self) -> bool {
        true
    }
}

/// Reverse Index (RI) - Move cursor up one line, scrolling if at top of region.
///
/// When the cursor is at the top margin of the scroll region, this command
/// scrolls the content down by one line (inserting a blank line at the top).
///
/// This is used to insert history lines above the viewport without disturbing
/// the content below.
///
/// # ANSI Sequence
///
/// `ESC M` - Reverse Index
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReverseIndex;

impl Command for ReverseIndex {
    fn write_ansi(&self, f: &mut impl fmt::Write) -> fmt::Result {
        write!(f, "\x1bM")
    }

    #[cfg(windows)]
    fn execute_winapi(&self) -> io::Result<()> {
        Err(io::Error::other(
            "ReverseIndex requires ANSI support; WinAPI not supported",
        ))
    }

    #[cfg(windows)]
    fn is_ansi_code_supported(&self) -> bool {
        true
    }
}

/// Index (IND) - Move cursor down one line, scrolling if at bottom of region.
///
/// When the cursor is at the bottom margin of the scroll region, this command
/// scrolls the content up by one line (inserting a blank line at the bottom).
///
/// # ANSI Sequence
///
/// `ESC D` - Index
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Index;

impl Command for Index {
    fn write_ansi(&self, f: &mut impl fmt::Write) -> fmt::Result {
        write!(f, "\x1bD")
    }

    #[cfg(windows)]
    fn execute_winapi(&self) -> io::Result<()> {
        Err(io::Error::other(
            "Index requires ANSI support; WinAPI not supported",
        ))
    }

    #[cfg(windows)]
    fn is_ansi_code_supported(&self) -> bool {
        true
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// ALTERNATE SCREEN SCROLL MODE (DECSET/DECRST 1007)
// ─────────────────────────────────────────────────────────────────────────────

/// Enable alternate screen scroll mode.
///
/// When enabled, mouse wheel events in the alternate screen are translated
/// to arrow key sequences, allowing scrolling in TUI applications that don't
/// handle mouse events directly.
///
/// This is important for SSH sessions where mouse reporting may not work.
///
/// # ANSI Sequence
///
/// `ESC [ ? 1007 h` - DECSET 1007
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EnableAlternateScroll;

impl Command for EnableAlternateScroll {
    fn write_ansi(&self, f: &mut impl fmt::Write) -> fmt::Result {
        write!(f, "\x1b[?1007h")
    }

    #[cfg(windows)]
    fn execute_winapi(&self) -> io::Result<()> {
        Err(io::Error::other(
            "EnableAlternateScroll requires ANSI support; WinAPI not supported",
        ))
    }

    #[cfg(windows)]
    fn is_ansi_code_supported(&self) -> bool {
        true
    }
}

/// Disable alternate screen scroll mode.
///
/// # ANSI Sequence
///
/// `ESC [ ? 1007 l` - DECRST 1007
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DisableAlternateScroll;

impl Command for DisableAlternateScroll {
    fn write_ansi(&self, f: &mut impl fmt::Write) -> fmt::Result {
        write!(f, "\x1b[?1007l")
    }

    #[cfg(windows)]
    fn execute_winapi(&self) -> io::Result<()> {
        Err(io::Error::other(
            "DisableAlternateScroll requires ANSI support; WinAPI not supported",
        ))
    }

    #[cfg(windows)]
    fn is_ansi_code_supported(&self) -> bool {
        true
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// DESKTOP NOTIFICATIONS (OSC 9)
// ─────────────────────────────────────────────────────────────────────────────

/// Send a desktop notification via OSC 9.
///
/// This is supported by many terminal emulators including iTerm2, Kitty,
/// and Windows Terminal. When the terminal is unfocused, this can trigger
/// a system notification.
///
/// Note: Support varies by terminal. Some terminals ignore this sequence.
///
/// # ANSI Sequence
///
/// `ESC ] 9 ; <message> BEL` - OSC 9 notification
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PostNotification(pub String);

impl Command for PostNotification {
    fn write_ansi(&self, f: &mut impl fmt::Write) -> fmt::Result {
        // OSC 9 ; message BEL
        //
        // `self.0` is sanitized here (not at construction) because this is
        // the actual write boundary: unlike `osc_set_title`, which sanitizes
        // before building its `String`, `PostNotification` stores the raw
        // message and only escapes it when it is actually written to the
        // terminal, so a caller cannot bypass sanitization by writing the
        // ANSI sequence some other way.
        write!(
            f,
            "\x1b]9;{}\x07",
            crate::notifications::sanitize_osc_text(&self.0)
        )
    }

    #[cfg(windows)]
    fn execute_winapi(&self) -> io::Result<()> {
        Err(io::Error::other(
            "PostNotification requires ANSI support; WinAPI not supported",
        ))
    }

    #[cfg(windows)]
    fn is_ansi_code_supported(&self) -> bool {
        true
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// CURSOR SAVE/RESTORE (DECSC/DECRC)
// ─────────────────────────────────────────────────────────────────────────────

/// Save cursor position and attributes (DECSC).
///
/// Saves the current cursor position and attributes so they can be
/// restored later with `RestoreCursor`.
///
/// # ANSI Sequence
///
/// `ESC 7` - DECSC (Save Cursor)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SaveCursor;

impl Command for SaveCursor {
    fn write_ansi(&self, f: &mut impl fmt::Write) -> fmt::Result {
        write!(f, "\x1b7")
    }

    #[cfg(windows)]
    fn execute_winapi(&self) -> io::Result<()> {
        Err(io::Error::other(
            "SaveCursor requires ANSI support; WinAPI not supported",
        ))
    }

    #[cfg(windows)]
    fn is_ansi_code_supported(&self) -> bool {
        true
    }
}

/// Restore cursor position and attributes (DECRC).
///
/// Restores the cursor position and attributes previously saved with `SaveCursor`.
///
/// # ANSI Sequence
///
/// `ESC 8` - DECRC (Restore Cursor)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RestoreCursor;

impl Command for RestoreCursor {
    fn write_ansi(&self, f: &mut impl fmt::Write) -> fmt::Result {
        write!(f, "\x1b8")
    }

    #[cfg(windows)]
    fn execute_winapi(&self) -> io::Result<()> {
        Err(io::Error::other(
            "RestoreCursor requires ANSI support; WinAPI not supported",
        ))
    }

    #[cfg(windows)]
    fn is_ansi_code_supported(&self) -> bool {
        true
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// SCROLL UP/DOWN (CSI S / CSI T)
// ─────────────────────────────────────────────────────────────────────────────

/// Scroll the screen up by N lines (CSI S).
///
/// Moves all content up, inserting blank lines at the bottom.
/// Uses the current scroll region if one is set.
///
/// # ANSI Sequence
///
/// `ESC [ <n> S` - Scroll Up
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScrollUp(pub u16);

impl Command for ScrollUp {
    fn write_ansi(&self, f: &mut impl fmt::Write) -> fmt::Result {
        write!(f, "\x1b[{}S", self.0)
    }

    #[cfg(windows)]
    fn execute_winapi(&self) -> io::Result<()> {
        Err(io::Error::other(
            "ScrollUp requires ANSI support; WinAPI not supported",
        ))
    }

    #[cfg(windows)]
    fn is_ansi_code_supported(&self) -> bool {
        true
    }
}

/// Scroll the screen down by N lines (CSI T).
///
/// Moves all content down, inserting blank lines at the top.
/// Uses the current scroll region if one is set.
///
/// # ANSI Sequence
///
/// `ESC [ <n> T` - Scroll Down
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScrollDown(pub u16);

impl Command for ScrollDown {
    fn write_ansi(&self, f: &mut impl fmt::Write) -> fmt::Result {
        write!(f, "\x1b[{}T", self.0)
    }

    #[cfg(windows)]
    fn execute_winapi(&self) -> io::Result<()> {
        Err(io::Error::other(
            "ScrollDown requires ANSI support; WinAPI not supported",
        ))
    }

    #[cfg(windows)]
    fn is_ansi_code_supported(&self) -> bool {
        true
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// UTILITY FUNCTIONS
// ─────────────────────────────────────────────────────────────────────────────

/// Scroll content within a region down by `count` lines using Reverse Index.
///
/// This is useful for inserting history lines above a viewport:
/// 1. Set scroll region to include area above viewport
/// 2. Move cursor to top of region
/// 3. Call this function to scroll down
/// 4. Reset scroll region
///
/// # Arguments
///
/// * `writer` - The output writer (e.g., stdout)
/// * `count` - Number of lines to scroll down
///
/// # Example
///
/// ```rust,ignore
/// use maestro_tui_kit::ansi_commands::{SetScrollRegion, ResetScrollRegion, scroll_region_down};
/// use crossterm::{execute, cursor::MoveTo};
///
/// let mut stdout = std::io::stdout();
/// execute!(stdout, SetScrollRegion(1..10))?;
/// execute!(stdout, MoveTo(0, 0))?;
/// scroll_region_down(&mut stdout, 3)?;
/// execute!(stdout, ResetScrollRegion)?;
/// ```
pub fn scroll_region_down<W: io::Write>(writer: &mut W, count: u16) -> io::Result<()> {
    use crossterm::queue;
    for _ in 0..count {
        queue!(writer, ReverseIndex)?;
    }
    Ok(())
}

/// Scroll content within a region up by `count` lines using Index.
pub fn scroll_region_up<W: io::Write>(writer: &mut W, count: u16) -> io::Result<()> {
    use crossterm::queue;
    for _ in 0..count {
        queue!(writer, Index)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn command_to_string<C: Command>(cmd: C) -> String {
        let mut s = String::new();
        cmd.write_ansi(&mut s).unwrap();
        s
    }

    #[test]
    fn test_set_scroll_region() {
        let cmd = SetScrollRegion(5..20);
        assert_eq!(command_to_string(cmd), "\x1b[5;20r");
    }

    #[test]
    fn test_reset_scroll_region() {
        assert_eq!(command_to_string(ResetScrollRegion), "\x1b[r");
    }

    #[test]
    fn test_reverse_index() {
        assert_eq!(command_to_string(ReverseIndex), "\x1bM");
    }

    #[test]
    fn test_index() {
        assert_eq!(command_to_string(Index), "\x1bD");
    }

    #[test]
    fn test_enable_alternate_scroll() {
        assert_eq!(command_to_string(EnableAlternateScroll), "\x1b[?1007h");
    }

    #[test]
    fn test_disable_alternate_scroll() {
        assert_eq!(command_to_string(DisableAlternateScroll), "\x1b[?1007l");
    }

    #[test]
    fn test_post_notification() {
        let cmd = PostNotification("Hello world".to_string());
        assert_eq!(command_to_string(cmd), "\x1b]9;Hello world\x07");
    }

    #[test]
    fn test_post_notification_strips_embedded_escape() {
        // A minimal OSC-0 (set title) sequence embedded in the message.
        let cmd = PostNotification("hello\x1b]0;pwned\x07world".to_string());
        let out = command_to_string(cmd);
        assert_eq!(out, "\x1b]9;hello]0;pwnedworld\x07");
        assert_eq!(out.matches('\x1b').count(), 1);
        assert_eq!(out.matches('\x07').count(), 1);
    }

    #[test]
    fn test_save_restore_cursor() {
        assert_eq!(command_to_string(SaveCursor), "\x1b7");
        assert_eq!(command_to_string(RestoreCursor), "\x1b8");
    }

    #[test]
    fn test_scroll_up_down() {
        assert_eq!(command_to_string(ScrollUp(5)), "\x1b[5S");
        assert_eq!(command_to_string(ScrollDown(3)), "\x1b[3T");
    }
}

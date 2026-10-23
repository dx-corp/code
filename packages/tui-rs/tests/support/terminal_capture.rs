//! Screen reconstruction for PTY assertions. Raw escape-stripped output loses
//! cursor-only spaces and characters reused by differential terminal paints.

use std::collections::VecDeque;

const SCROLLBACK_ROWS: usize = 1024;
const SNAPSHOT_LIMIT: usize = 256;

pub(super) struct TerminalCapture {
    parser: vt100::Parser<ScreenHistory>,
    raw: Option<Vec<u8>>,
}

impl TerminalCapture {
    pub(super) fn new(rows: u16, cols: u16) -> Self {
        Self {
            raw: None,
            parser: vt100::Parser::new_with_callbacks(
                rows,
                cols,
                SCROLLBACK_ROWS,
                ScreenHistory::default(),
            ),
        }
    }

    pub(super) fn process(&mut self, bytes: &[u8]) {
        if let Some(raw) = &mut self.raw {
            const LIMIT: usize = 4 * 1024 * 1024;
            if bytes.len() >= LIMIT {
                raw.clear();
                raw.extend_from_slice(&bytes[bytes.len() - LIMIT..]);
            } else {
                let excess = raw.len().saturating_add(bytes.len()).saturating_sub(LIMIT);
                raw.drain(..excess);
                raw.extend_from_slice(bytes);
            }
        }
        // Keep the parser across reads: PTYs may split escape sequences and
        // UTF-8 characters at any byte, and later paints reuse existing cells.
        self.parser.process(bytes);
        let snapshot = screen_rows(self.parser.screen()).join("\n");
        self.parser.callbacks_mut().record(snapshot);
    }

    /// Keep the terminal emulator in sync with its PTY before handling resize queries.
    pub(super) fn resize(&mut self, rows: u16, columns: u16) {
        self.parser.screen_mut().set_size(rows, columns);
    }

    pub(super) fn cursor_report(&self) -> String {
        let (row, column) = self.parser.screen().cursor_position();
        format!("\x1b[{};{}R", row + 1, column + 1)
    }

    /// Opt-in graphics evidence: the VT text emulator intentionally discards APC image bytes.
    pub(super) fn capture_raw(&mut self) {
        self.raw = Some(Vec::new());
    }

    pub(super) fn raw_bytes(&self) -> &[u8] {
        self.raw.as_deref().unwrap_or_default()
    }

    /// Current visible screen only, for controls that revisit an earlier value.
    pub(super) fn current_text(&self) -> String {
        screen_rows(self.parser.screen()).join("\n")
    }

    /// Defer decisions while a synchronized paint is split across PTY reads.
    pub(super) fn completed_current_text(&self) -> Option<String> {
        (!self.parser.callbacks().sync_in_progress).then(|| self.current_text())
    }

    /// Formatted current frame for image evidence, preserving actual cell colors.
    pub(super) fn current_formatted(&self) -> Vec<u8> {
        self.parser.screen().contents_formatted()
    }

    pub(super) fn text(&self) -> String {
        let mut text = self
            .parser
            .callbacks()
            .snapshots
            .iter()
            .cloned()
            .collect::<Vec<_>>();
        // Read the emulator's real scrollback as well: several screens of
        // output can scroll past in a single PTY read before a snapshot.
        let mut screen = self.parser.screen().clone();
        screen.set_scrollback(SCROLLBACK_ROWS);
        let rows = usize::from(screen.size().0);
        while screen.scrollback() > 0 {
            let offset = screen.scrollback();
            text.extend(screen_rows(&screen).into_iter().take(offset.min(rows)));
            screen.set_scrollback(offset.saturating_sub(rows));
        }
        text.extend(screen_rows(&screen));
        text.join("\n")
    }
}

#[derive(Default)]
struct ScreenHistory {
    snapshots: VecDeque<String>,
    sync_in_progress: bool,
}

impl ScreenHistory {
    fn record(&mut self, snapshot: String) {
        if self.snapshots.back() == Some(&snapshot) {
            return;
        }
        if self.snapshots.len() == SNAPSHOT_LIMIT {
            self.snapshots.pop_front();
        }
        self.snapshots.push_back(snapshot);
    }
}

impl vt100::Callbacks for ScreenHistory {
    fn unhandled_csi(
        &mut self,
        screen: &mut vt100::Screen,
        first: Option<u8>,
        _second: Option<u8>,
        params: &[&[u16]],
        command: char,
    ) {
        // Preserve completed synchronized frames even when a subsequent
        // redraw or screen clear arrives in the same read. vt100 delegates
        // the unsupported DEC synchronized-output mode to this callback.
        if first == Some(b'?') && params.contains(&&[2026][..]) {
            match command {
                'h' => self.sync_in_progress = true,
                'l' => {
                    self.sync_in_progress = false;
                    self.record(screen_rows(screen).join("\n"));
                }
                _ => {}
            }
        }
    }
}

fn screen_rows(screen: &vt100::Screen) -> Vec<String> {
    let cols = screen.size().1;
    screen
        .rows(0, cols)
        .map(|mut row| {
            // vt100 trims trailing blank cells; keep them so a prompt prefix
            // ending in a space remains observable, just as on the terminal.
            let width = unicode_width::UnicodeWidthStr::width(row.as_str());
            row.extend(std::iter::repeat_n(
                ' ',
                usize::from(cols).saturating_sub(width),
            ));
            row
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_report_tracks_resize_and_later_cursor_moves() {
        let mut capture = TerminalCapture::new(36, 120);
        capture.process(b"\x1b[36;120H");
        assert_eq!(capture.cursor_report(), "\x1b[36;120R");
        capture.resize(20, 40);
        assert_eq!(capture.cursor_report(), "\x1b[20;40R");
        capture.process(b"\x1b[7;5H\x1b[6n");
        assert_eq!(capture.cursor_report(), "\x1b[7;5R");
    }

    #[test]
    fn reconstructs_cursor_only_spaces_and_reused_cells() {
        let mut capture = TerminalCapture::new(4, 40);
        capture.process(b"\x1b[2;3Hprintf\x1b[1Cpty-e2e-ran");
        assert!(capture.text().contains("printf pty-e2e-ran"));
        capture.process(b"\x1b[2;10HPTY");
        assert!(capture.text().contains("printf PTY-e2e-ran"));
        assert!(!capture.text().contains("printfpty-e2e-ran"));
    }

    #[test]
    fn reconstructs_split_escape_sequences_and_utf8() {
        let mut capture = TerminalCapture::new(2, 40);
        for byte in "\x1b[2;3H✓\x1b[1Cdone".as_bytes() {
            capture.process(&[*byte]);
        }
        assert!(capture.text().contains("✓ done"));
    }

    #[test]
    fn retains_scrollback_from_a_single_read() {
        let mut capture = TerminalCapture::new(2, 20);
        capture.process(b"first line\r\nsecond line\r\nthird line\r\nlast line");
        let text = capture.text();
        for expected in ["first line", "second line", "third line", "last line"] {
            assert!(text.contains(expected), "missing {expected}: {text}");
        }
    }

    #[test]
    fn retains_completed_frames_cleared_within_the_same_read() {
        let mut capture = TerminalCapture::new(2, 40);
        capture.process(
            b"\x1b[?2026hApproval Required\x1b[?2026l\x1b[?2026h\x1b[2J\x1b[Hdone\x1b[?2026l",
        );
        assert!(capture.text().contains("Approval Required"));
        assert!(capture.text().contains("done"));
        assert!(!capture.parser.screen().contents().contains("Approval"));
    }

    #[test]
    fn defers_current_frame_decisions_until_split_synchronized_paint_completes() {
        let mut capture = TerminalCapture::new(2, 40);
        capture.process(b"\x1b[?2026h> guarded prompt\x1b[?2026l");
        assert!(
            capture
                .completed_current_text()
                .unwrap()
                .contains("> guarded prompt")
        );
        capture.process(b"\x1b[?2026h\x1b[2J\x1b[H");
        assert!(!capture.current_text().contains("> guarded prompt"));
        assert_eq!(capture.completed_current_text(), None);
        capture.process(b"> guarded prompt\x1b[?2026l");
        assert!(
            capture
                .completed_current_text()
                .unwrap()
                .contains("> guarded prompt")
        );
    }

    #[test]
    fn retains_trailing_blank_cells_for_input_prefixes() {
        let mut capture = TerminalCapture::new(2, 40);
        capture.process(b"/mcp config add");
        assert!(capture.text().contains("/mcp config add "));
    }

    #[test]
    fn bounds_snapshot_history_and_deduplicates_unchanged_paints() {
        let mut capture = TerminalCapture::new(2, 40);
        for index in 0..(SNAPSHOT_LIMIT + 10) {
            capture.process(format!("\r\x1b[2Kframe {index}").as_bytes());
        }
        assert_eq!(capture.parser.callbacks().snapshots.len(), SNAPSHOT_LIMIT);
        capture.process(b"\x1b[0m");
        assert_eq!(capture.parser.callbacks().snapshots.len(), SNAPSHOT_LIMIT);
        assert!(!capture.parser.callbacks().snapshots[0].contains("frame 0 "));
    }
}

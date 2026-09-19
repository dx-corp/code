//! Screen reconstruction for PTY assertions. Raw escape-stripped output loses
//! cursor-only spaces and characters reused by differential terminal paints.

use std::collections::VecDeque;

const SCROLLBACK_ROWS: usize = 1024;
const SNAPSHOT_LIMIT: usize = 256;

pub(super) struct TerminalCapture {
    parser: vt100::Parser<ScreenHistory>,
}

impl TerminalCapture {
    pub(super) fn new(rows: u16, cols: u16) -> Self {
        Self {
            parser: vt100::Parser::new_with_callbacks(
                rows,
                cols,
                SCROLLBACK_ROWS,
                ScreenHistory::default(),
            ),
        }
    }

    pub(super) fn process(&mut self, bytes: &[u8]) {
        // Keep the parser across reads: PTYs may split escape sequences and
        // UTF-8 characters at any byte, and later paints reuse existing cells.
        self.parser.process(bytes);
        let snapshot = screen_rows(self.parser.screen()).join("\n");
        self.parser.callbacks_mut().record(snapshot);
    }

    /// Current visible screen only, for controls that revisit an earlier value.
    pub(super) fn current_text(&self) -> String {
        screen_rows(self.parser.screen()).join("\n")
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
        if first == Some(b'?') && command == 'l' && params.contains(&&[2026][..]) {
            self.record(screen_rows(screen).join("\n"));
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

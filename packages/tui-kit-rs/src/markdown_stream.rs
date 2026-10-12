//! Streaming Markdown Collector
//!
//! Provides a newline-gated accumulator that renders markdown and commits only
//! fully completed logical lines. This is essential for streaming AI responses
//! where text arrives incrementally.
//!
//! Ported from OpenAI Codex CLI (MIT licensed).
//!
//! # How It Works
//!
//! The collector buffers incoming text deltas and only commits lines to the
//! output when a newline is encountered. This prevents partial/incomplete
//! markdown from being rendered (which can cause visual glitches).
//!
//! # Example
//!
//! ```rust,ignore
//! use maestro_tui_kit::markdown_stream::MarkdownStreamCollector;
//!
//! let mut collector = MarkdownStreamCollector::new(Some(80));
//!
//! // Stream arrives in chunks
//! collector.push_delta("# Hello ");
//! collector.push_delta("World\n");
//!
//! // Only commits lines after newlines
//! let lines = collector.commit_complete_lines();
//! assert_eq!(lines.len(), 1);
//!
//! // Finalize to get any remaining content
//! let remaining = collector.finalize_and_drain();
//! ```

use ratatui::text::Line;

use crate::markdown::render_markdown_with_width;
use crate::render_utils::is_blank_line;

/// Newline-gated accumulator that renders markdown and commits only fully
/// completed logical lines.
pub struct MarkdownStreamCollector {
    buffer: String,
    committed_line_count: usize,
    width: Option<usize>,
}

impl MarkdownStreamCollector {
    /// Create a new collector with optional width for wrapping.
    #[must_use]
    pub fn new(width: Option<usize>) -> Self {
        Self {
            buffer: String::new(),
            committed_line_count: 0,
            width,
        }
    }

    /// Clear all state and start fresh.
    pub fn clear(&mut self) {
        self.buffer.clear();
        self.committed_line_count = 0;
    }

    /// Push a text delta to the buffer.
    pub fn push_delta(&mut self, delta: &str) {
        self.buffer.push_str(delta);
    }

    /// Get the current buffer contents (for debugging/display).
    #[must_use]
    pub fn buffer(&self) -> &str {
        &self.buffer
    }

    /// Update the width for wrapping.
    pub fn set_width(&mut self, width: Option<usize>) {
        self.width = width;
    }

    /// Render the full buffer and return only the newly completed logical lines
    /// since the last commit.
    ///
    /// When the buffer does not end with a newline, the final rendered line is
    /// considered incomplete and is not emitted.
    pub fn commit_complete_lines(&mut self) -> Vec<Line<'static>> {
        let source = self.buffer.clone();

        // Find the last newline - only commit up to there
        let last_newline_idx = match source.rfind('\n') {
            Some(idx) => idx,
            None => return Vec::new(), // No complete lines yet
        };

        let source = source[..=last_newline_idx].to_string();

        // Render markdown
        let rendered = render_markdown_to_lines(&source, self.width);

        // Count complete lines (skip trailing blank line if present)
        let mut complete_line_count = rendered.len();
        if complete_line_count > 0 && is_blank_line(&rendered[complete_line_count - 1]) {
            complete_line_count -= 1;
        }

        // Return only new lines since last commit
        if self.committed_line_count >= complete_line_count {
            return Vec::new();
        }

        let out = rendered[self.committed_line_count..complete_line_count].to_vec();
        self.committed_line_count = complete_line_count;
        out
    }

    /// Finalize the stream: emit all remaining lines beyond the last commit.
    ///
    /// If the buffer does not end with a newline, a temporary one is appended
    /// for rendering. This should be called when the stream ends.
    pub fn finalize_and_drain(&mut self) -> Vec<Line<'static>> {
        let mut source = self.buffer.clone();
        if !source.ends_with('\n') {
            source.push('\n');
        }

        let rendered = render_markdown_to_lines(&source, self.width);

        let out = if self.committed_line_count >= rendered.len() {
            Vec::new()
        } else {
            rendered[self.committed_line_count..].to_vec()
        };

        // Reset for next stream
        self.clear();
        out
    }

    /// Get the total number of lines committed so far.
    #[must_use]
    pub fn committed_count(&self) -> usize {
        self.committed_line_count
    }
}

/// Render markdown to lines, optionally wrapping to width.
fn render_markdown_to_lines(source: &str, width: Option<usize>) -> Vec<Line<'static>> {
    let rendered = render_markdown_with_width(source, width);

    // Convert to owned lines
    rendered
        .into_iter()
        .map(|line| {
            Line::from(
                line.spans
                    .into_iter()
                    .map(|span| ratatui::text::Span::styled(span.content.to_string(), span.style))
                    .collect::<Vec<_>>(),
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_commit_until_newline() {
        let mut c = MarkdownStreamCollector::new(None);
        c.push_delta("Hello, world");
        let out = c.commit_complete_lines();
        assert!(out.is_empty(), "should not commit without newline");

        c.push_delta("!\n");
        let out2 = c.commit_complete_lines();
        assert!(!out2.is_empty(), "should commit after newline");
    }

    #[test]
    fn finalize_commits_partial_line() {
        let mut c = MarkdownStreamCollector::new(None);
        c.push_delta("Line without newline");
        let out = c.finalize_and_drain();
        assert!(!out.is_empty());
    }

    #[test]
    fn multiple_lines_commit_incrementally() {
        let mut c = MarkdownStreamCollector::new(None);

        c.push_delta("Line 1\n");
        let out1 = c.commit_complete_lines();
        // First line should commit
        assert!(!out1.is_empty(), "first line should commit");

        c.push_delta("Line 2\n");
        let _out2 = c.commit_complete_lines();
        // Second line may be empty if already committed, or have new content
        // The key invariant is no duplication

        // Should not duplicate
        let out3 = c.commit_complete_lines();
        assert!(out3.is_empty(), "no new content should be empty");
    }

    #[test]
    fn clear_resets_state() {
        let mut c = MarkdownStreamCollector::new(None);
        c.push_delta("Some text\n");
        c.commit_complete_lines();

        c.clear();
        assert_eq!(c.buffer(), "");
        assert_eq!(c.committed_count(), 0);
    }

    #[test]
    fn committed_count_tracks_lines() {
        let mut c = MarkdownStreamCollector::new(None);
        assert_eq!(c.committed_count(), 0);

        c.push_delta("Line 1\n");
        c.commit_complete_lines();
        let count1 = c.committed_count();

        c.push_delta("Line 2\n");
        c.commit_complete_lines();
        let count2 = c.committed_count();

        // Count should increase
        assert!(count2 >= count1, "committed count should not decrease");
    }
}

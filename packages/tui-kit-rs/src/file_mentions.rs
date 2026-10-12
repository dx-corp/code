//! Inline `@file` mention expansion for the composer.
//!
//! Tokens of the form `@path`, `@path:LINE`, or `@path:START-END` in a
//! submitted prompt are expanded with the referenced file contents before the
//! prompt is sent to the agent. The visible user message keeps the original
//! text; only the agent-bound copy carries the inlined blocks.
//!
//! # Grammar
//!
//! - `@src/main.rs` — whole file (size-capped)
//! - `@src/main.rs:42` — single line
//! - `@src/main.rs:10-50` — 1-based inclusive line range
//!
//! A token is left untouched (and nothing is inlined) when the path does not
//! resolve to a readable text file. This keeps `@`-mentions that are not file
//! references (e.g. social handles) literal.

use std::path::{Path, PathBuf};

/// Maximum file size considered for inlining (1 MiB).
const MAX_FILE_BYTES: u64 = 1 << 20;
/// Maximum number of lines inlined per mention.
const MAX_LINES: usize = 2_000;

struct Mention {
    /// The token as written (including the leading `@`), used for matching.
    path: String,
    start: Option<usize>,
    end: Option<usize>,
}

/// Parse a single token into a mention, or `None` when the token is not an
/// `@`-reference. The path part may not contain whitespace (callers split on
/// whitespace first).
fn parse_mention(token: &str) -> Option<Mention> {
    let body = token.strip_prefix('@')?;
    if body.is_empty() {
        return None;
    }

    // Split off an optional trailing :LINE or :START-END range. The range must
    // be entirely numeric; a colon anywhere else is part of the path.
    if let Some((path_part, range_part)) = body.rsplit_once(':') {
        let parse_range = || -> Option<(Option<usize>, Option<usize>)> {
            if let Some((start, end)) = range_part.split_once('-') {
                let start = start.parse::<usize>().ok()?;
                let end = end.parse::<usize>().ok()?;
                Some((Some(start), Some(end)))
            } else {
                let line = range_part.parse::<usize>().ok()?;
                Some((Some(line), Some(line)))
            }
        };
        if let Some((start, end)) = parse_range() {
            if !path_part.is_empty() {
                return Some(Mention {
                    path: path_part.to_string(),
                    start,
                    end,
                });
            }
        }
    }

    Some(Mention {
        path: body.to_string(),
        start: None,
        end: None,
    })
}

/// Resolve the mention path against `cwd` and read the selected lines.
/// Returns `None` for missing, non-file, binary, or oversized targets.
fn read_mention_lines(mention: &Mention, cwd: &Path) -> Option<(PathBuf, Vec<String>)> {
    let raw = Path::new(&mention.path);
    let full = if raw.is_absolute() {
        raw.to_path_buf()
    } else {
        cwd.join(raw)
    };

    let metadata = std::fs::metadata(&full).ok()?;
    if !metadata.is_file() || metadata.len() > MAX_FILE_BYTES {
        return None;
    }

    let bytes = std::fs::read(&full).ok()?;
    if bytes.contains(&0) {
        return None; // binary
    }
    let text = String::from_utf8_lossy(&bytes);
    let lines: Vec<String> = text.lines().map(str::to_string).collect();

    let selected = match (mention.start, mention.end) {
        (Some(start), Some(end)) => {
            if start == 0 || start > end || start > lines.len() {
                return None;
            }
            let end = end.min(lines.len());
            lines[start - 1..end].to_vec()
        }
        _ => lines,
    };

    let selected: Vec<String> = selected.into_iter().take(MAX_LINES).collect();
    Some((full, selected))
}

/// Expand `@file` mentions in `input` by appending fenced content blocks.
///
/// The mention tokens themselves are preserved in the text; referenced content
/// is appended as `<file path="..." lines="...">` blocks the model can read
/// directly. Unresolvable mentions leave the input unchanged apart from any
/// other successful expansions.
#[must_use]
pub fn expand_file_mentions(input: &str, cwd: &Path) -> String {
    let mut blocks = Vec::new();

    for token in input.split_whitespace() {
        let Some(mention) = parse_mention(token) else {
            continue;
        };
        let Some((path, lines)) = read_mention_lines(&mention, cwd) else {
            continue;
        };

        let range_label = match (mention.start, mention.end) {
            (Some(start), Some(end)) if start == end => format!(" line {start}"),
            (Some(start), Some(end)) => format!(" lines {start}-{end}"),
            _ => String::new(),
        };
        let mut block = format!("\n\n<file path=\"{}\"{}>\n", path.display(), range_label);
        block.push_str(&lines.join("\n"));
        block.push_str("\n</file>");
        blocks.push(block);
    }

    if blocks.is_empty() {
        return input.to_string();
    }

    let mut expanded = input.to_string();
    for block in blocks {
        expanded.push_str(&block);
    }
    expanded
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn temp_dir_with_file(name: &str, contents: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("mkdir");
        }
        let mut file = std::fs::File::create(&path).expect("create");
        file.write_all(contents.as_bytes()).expect("write");
        let cwd = dir.path().to_path_buf();
        (dir, cwd)
    }

    #[test]
    fn expands_whole_file() {
        let (_dir, cwd) = temp_dir_with_file("a.txt", "one\ntwo\nthree\n");
        let expanded = expand_file_mentions("look at @a.txt please", &cwd);
        assert!(expanded.contains("<file path="));
        assert!(expanded.contains("one\ntwo\nthree"));
        assert!(expanded.ends_with("</file>"));
        // Token preserved inline.
        assert!(expanded.contains("@a.txt"));
    }

    #[test]
    fn expands_line_range() {
        let (_dir, cwd) = temp_dir_with_file("a.txt", "one\ntwo\nthree\nfour\n");
        let expanded = expand_file_mentions("@a.txt:2-3", &cwd);
        assert!(expanded.contains("lines 2-3"));
        assert!(expanded.contains("two\nthree"));
        assert!(!expanded.contains("four\n</file>"));
    }

    #[test]
    fn expands_single_line() {
        let (_dir, cwd) = temp_dir_with_file("a.txt", "one\ntwo\nthree\n");
        let expanded = expand_file_mentions("@a.txt:2", &cwd);
        assert!(expanded.contains(" line 2"));
        assert!(expanded.contains(">two\n") || expanded.contains("\ntwo\n"));
        assert!(!expanded.contains("three"));
    }

    #[test]
    fn clamps_range_to_file_length() {
        let (_dir, cwd) = temp_dir_with_file("a.txt", "one\ntwo\n");
        let expanded = expand_file_mentions("@a.txt:1-99", &cwd);
        assert!(expanded.contains("one\ntwo"));
    }

    #[test]
    fn leaves_missing_file_literal() {
        let (_dir, cwd) = temp_dir_with_file("a.txt", "one\n");
        let expanded = expand_file_mentions("ping @nope.txt", &cwd);
        assert_eq!(expanded, "ping @nope.txt");
    }

    #[test]
    fn leaves_non_file_tokens_literal() {
        let dir = tempfile::tempdir().expect("tempdir");
        let expanded = expand_file_mentions("thanks @someone", dir.path());
        assert_eq!(expanded, "thanks @someone");
    }

    #[test]
    fn skips_binary_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("bin.dat"), b"a\0b").expect("write");
        let expanded = expand_file_mentions("@bin.dat", dir.path());
        assert_eq!(expanded, "@bin.dat");
    }

    #[test]
    fn range_out_of_bounds_is_literal() {
        let (_dir, cwd) = temp_dir_with_file("a.txt", "one\n");
        let expanded = expand_file_mentions("@a.txt:5-9", &cwd);
        assert_eq!(expanded, "@a.txt:5-9");
    }
}

//! Incremental release policy for assistant text on the headless protocol.
//! The bytes in `Release::text` are always a prefix of the bytes pushed so far.

const DEFAULT_MAX_BYTES: usize = 4_096;
const DEFAULT_MAX_HOLD_MS: u64 = 1_500;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlushReason {
    Tool,
    TurnEnd,
    Error,
    Cancellation,
    Finalization,
    SizeLimit,
    LatencyLimit,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    pub text: String,
    pub held_ms: u64,
    pub forced: Option<FlushReason>,
    pub standalone_title: bool,
    pub semantic_unit: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Following {
    None,
    Awaiting,
    TableHeader,
    TableDelimiter,
    Fence { marker: char, width: usize },
}

#[derive(Debug, Clone)]
pub struct SemanticTextRelease {
    pending: String,
    line: String,
    line_non_title: bool,
    following: Following,
    held_since_ms: Option<u64>,
    max_bytes: usize,
    max_hold_ms: u64,
    titles_seen: u64,
    peak_held_bytes: usize,
}

impl Default for SemanticTextRelease {
    fn default() -> Self {
        Self::new(DEFAULT_MAX_BYTES, DEFAULT_MAX_HOLD_MS)
    }
}

impl SemanticTextRelease {
    pub fn new(max_bytes: usize, max_hold_ms: u64) -> Self {
        Self {
            pending: String::new(),
            line: String::new(),
            line_non_title: false,
            following: Following::None,
            held_since_ms: None,
            max_bytes: max_bytes.max(4),
            max_hold_ms,
            titles_seen: 0,
            peak_held_bytes: 0,
        }
    }

    pub fn held_bytes(&self) -> usize {
        self.pending.len()
    }

    pub fn titles_seen(&self) -> u64 {
        self.titles_seen
    }

    pub fn peak_held_bytes(&self) -> usize {
        self.peak_held_bytes
    }

    pub fn push(&mut self, delta: &str, now_ms: u64) -> Vec<Release> {
        let mut releases = Vec::new();
        for ch in delta.chars() {
            if self.pending.len() + ch.len_utf8() > self.max_bytes {
                let forced = (!self.line_non_title || self.following != Following::None)
                    .then_some(FlushReason::SizeLimit);
                self.release(now_ms, forced, &mut releases);
                self.line.clear();
                self.line_non_title = true;
            }
            if self.pending.is_empty() {
                self.held_since_ms = Some(now_ms);
            }
            self.pending.push(ch);
            self.peak_held_bytes = self.peak_held_bytes.max(self.pending.len());
            if !self.line_non_title {
                self.line.push(ch);
            }
            if ch == '\n' {
                if self.line_non_title {
                    self.release(now_ms, None, &mut releases);
                } else {
                    self.finish_line(now_ms, &mut releases);
                }
                self.line.clear();
                self.line_non_title = false;
            } else if self.following == Following::None
                && !self.line_non_title
                && !could_be_title(&self.line)
            {
                // Ordinary prose need not wait for a line or a transport timer.
                self.line.clear();
                self.line_non_title = true;
            }
            if self.pending.len() >= self.max_bytes {
                let forced = (!self.line_non_title || self.following != Following::None)
                    .then_some(FlushReason::SizeLimit);
                self.release(now_ms, forced, &mut releases);
                self.line.clear();
                self.line_non_title = true;
            }
        }
        if self.line_non_title && self.following == Following::None {
            self.release(now_ms, None, &mut releases);
        }
        if self
            .held_since_ms
            .is_some_and(|start| now_ms.saturating_sub(start) >= self.max_hold_ms)
        {
            self.release(now_ms, Some(FlushReason::LatencyLimit), &mut releases);
            if !self.line.is_empty() {
                self.line.clear();
                self.line_non_title = true;
            }
        }
        // Ordinary prose can pass through immediately, but a provider token
        // is never a reason to publish one event per character.
        let mut compact: Vec<Release> = Vec::new();
        for release in releases {
            if !release.semantic_unit && release.forced.is_none() {
                if let Some(last) = compact.last_mut() {
                    if !last.semantic_unit
                        && last.forced.is_none()
                        && last.text.len() + release.text.len() <= self.max_bytes
                    {
                        last.text.push_str(&release.text);
                        continue;
                    }
                }
            }
            compact.push(release);
        }
        compact
    }

    pub fn tick(&mut self, now_ms: u64) -> Vec<Release> {
        let mut releases = Vec::new();
        if self
            .held_since_ms
            .is_some_and(|start| now_ms.saturating_sub(start) >= self.max_hold_ms)
        {
            self.release(now_ms, Some(FlushReason::LatencyLimit), &mut releases);
            if !self.line.is_empty() {
                self.line.clear();
                self.line_non_title = true;
            }
        }
        releases
    }

    pub fn flush(&mut self, reason: FlushReason, now_ms: u64) -> Vec<Release> {
        let mut releases = Vec::new();
        self.release(now_ms, Some(reason), &mut releases);
        self.following = Following::None;
        self.line.clear();
        self.line_non_title = false;
        releases
    }

    fn finish_line(&mut self, now_ms: u64, releases: &mut Vec<Release>) {
        let line = self.line.trim_end_matches(['\r', '\n']);
        match self.following {
            Following::None if standalone_title(line) => {
                self.following = Following::Awaiting;
                self.titles_seen += 1;
            }
            Following::None => self.release(now_ms, None, releases),
            Following::Awaiting if line.trim().is_empty() => {}
            Following::Awaiting if standalone_title(line) => {}
            Following::Awaiting => {
                if let Some((marker, width)) = opening_fence(line) {
                    self.following = Following::Fence { marker, width };
                } else if table_row(line) {
                    self.following = Following::TableHeader;
                } else {
                    // A complete first paragraph line or list item is useful
                    // with the title. Later lines flow without title buffering.
                    self.release(now_ms, None, releases);
                }
            }
            Following::TableHeader if table_delimiter(line) => {
                self.following = Following::TableDelimiter;
            }
            Following::TableDelimiter if line.trim().is_empty() => {}
            Following::TableDelimiter if table_row(line) => {
                self.release(now_ms, None, releases);
            }
            Following::TableHeader | Following::TableDelimiter => {
                self.release(now_ms, None, releases);
            }
            Following::Fence { marker, width } if closing_fence(line, marker, width) => {
                self.release(now_ms, None, releases);
            }
            Following::Fence { .. } => {}
        }
    }

    fn release(&mut self, now_ms: u64, forced: Option<FlushReason>, releases: &mut Vec<Release>) {
        if self.pending.is_empty() {
            return;
        }
        let incomplete_title = self.following == Following::None && standalone_title(&self.line);
        if incomplete_title {
            self.titles_seen += 1;
        }
        let standalone_title = forced.is_some()
            && (self.following == Following::Awaiting && self.line.trim().is_empty()
                || incomplete_title);
        let semantic_unit = self.following != Following::None;
        releases.push(Release {
            text: std::mem::take(&mut self.pending),
            held_ms: now_ms.saturating_sub(self.held_since_ms.unwrap_or(now_ms)),
            forced,
            standalone_title,
            semantic_unit,
        });
        self.held_since_ms = None;
        self.following = Following::None;
    }
}

fn could_be_title(line: &str) -> bool {
    let value = line.trim_start_matches(' ');
    if value.starts_with('#') {
        let width = value.bytes().take_while(|byte| *byte == b'#').count();
        return width <= 6 && (value.len() == width || value.as_bytes().get(width) == Some(&b' '));
    }
    if value == "*" {
        return true;
    }
    if let Some(body) = value.strip_prefix("**") {
        if let Some(end) = body.find("**") {
            return body[end + 2..].trim().trim_end_matches(':').is_empty();
        }
        return true;
    }
    false
}

fn standalone_title(line: &str) -> bool {
    let value = line.trim();
    if value.starts_with('#') {
        let width = value.bytes().take_while(|byte| *byte == b'#').count();
        return (1..=6).contains(&width)
            && value.as_bytes().get(width) == Some(&b' ')
            && !value[width + 1..].trim().is_empty();
    }
    if let Some(body) = value.strip_prefix("**") {
        let body = body.strip_suffix(':').unwrap_or(body);
        if let Some(body) = body.strip_suffix("**") {
            return !body.trim().is_empty();
        }
    }
    false
}

fn opening_fence(line: &str) -> Option<(char, usize)> {
    let value = line.trim_start_matches(' ');
    let marker = value.chars().next()?;
    if !matches!(marker, '`' | '~') {
        return None;
    }
    let width = value.chars().take_while(|ch| *ch == marker).count();
    (width >= 3).then_some((marker, width))
}

fn closing_fence(line: &str, marker: char, width: usize) -> bool {
    let value = line.trim();
    let count = value.chars().take_while(|ch| *ch == marker).count();
    count >= width && value[count..].trim().is_empty()
}

fn table_row(line: &str) -> bool {
    line.trim().contains('|')
}

fn table_delimiter(line: &str) -> bool {
    let value = line.trim().trim_matches('|');
    let mut cells = 0;
    for cell in value.split('|') {
        let cell = cell.trim().trim_matches(':');
        if cell.len() < 3 || !cell.bytes().all(|byte| byte == b'-') {
            return false;
        }
        cells += 1;
    }
    cells >= 2
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(input: &str, chunk: usize) -> Vec<Release> {
        let mut policy = SemanticTextRelease::default();
        let mut out = Vec::new();
        for slice in input.as_bytes().chunks(chunk) {
            // ASCII fixtures make arbitrary byte slicing valid UTF-8 here.
            out.extend(policy.push(std::str::from_utf8(slice).unwrap(), 0));
        }
        out.extend(policy.flush(FlushReason::Finalization, 0));
        assert_eq!(
            out.iter()
                .map(|release| release.text.as_str())
                .collect::<String>(),
            input
        );
        out
    }

    #[test]
    fn titles_wait_for_the_next_useful_block() {
        let mut policy = SemanticTextRelease::default();
        assert!(policy.push("## Risk by area", 0).is_empty());
        assert!(policy.push("\n\n", 0).is_empty());
        assert_eq!(
            policy.push("The largest risk is...\n", 100)[0].text,
            "## Risk by area\n\nThe largest risk is...\n"
        );
        assert!(policy.push("**Next steps**\n", 200).is_empty());
        assert_eq!(
            policy.push("- First...\n", 300)[0].text,
            "**Next steps**\n- First...\n"
        );
        assert!(policy.push("**Decision**:\n", 400).is_empty());
        assert_eq!(
            policy.push("Proceed with care.\n", 500)[0].text,
            "**Decision**:\nProceed with care.\n"
        );
    }

    #[test]
    fn table_and_fence_wait_until_coherent() {
        let mut policy = SemanticTextRelease::default();
        assert!(
            policy
                .push("# Table\n| A | B |\n| --- | --- |\n", 0)
                .is_empty()
        );
        assert_eq!(
            policy.push("| 1 | 2 |\n", 0)[0].text,
            "# Table\n| A | B |\n| --- | --- |\n| 1 | 2 |\n"
        );
        assert!(policy.push("# Code\n```rs\nfn main() {}\n", 0).is_empty());
        assert_eq!(
            policy.push("```\n", 0)[0].text,
            "# Code\n```rs\nfn main() {}\n```\n"
        );
    }

    #[test]
    fn inline_bold_and_prior_prose_flow() {
        let out = run(
            "Before heading\n# Heading\nFollowing.\n**Note:** read this\n",
            1,
        );
        let text = out
            .iter()
            .map(|release| release.text.as_str())
            .collect::<String>();
        assert_eq!(
            text,
            "Before heading\n# Heading\nFollowing.\n**Note:** read this\n"
        );
        let heading_index = out
            .iter()
            .position(|release| release.text.contains("# Heading"))
            .unwrap();
        assert_eq!(
            out[..heading_index]
                .iter()
                .map(|release| release.text.as_str())
                .collect::<String>(),
            "Before heading\n"
        );
        assert!(out[heading_index].text.contains("Following."));
        assert!(
            out.iter()
                .any(|release| release.text.contains("**Note:**") && !release.semantic_unit)
        );
    }

    #[test]
    fn limits_and_boundaries_never_lose_text() {
        for reason in [
            FlushReason::Tool,
            FlushReason::TurnEnd,
            FlushReason::Error,
            FlushReason::Cancellation,
            FlushReason::Finalization,
        ] {
            let mut policy = SemanticTextRelease::default();
            assert!(policy.push("# Held\n", 0).is_empty());
            assert_eq!(policy.flush(reason, 5)[0].text, "# Held\n");
        }
        let mut policy = SemanticTextRelease::new(12, 10);
        assert!(policy.push("# Held\n", 0).is_empty());
        assert_eq!(policy.tick(10)[0].forced, Some(FlushReason::LatencyLimit));
        let mut incomplete = SemanticTextRelease::new(128, 10);
        assert!(incomplete.push("# Held", 0).is_empty());
        assert!(incomplete.tick(10)[0].standalone_title);
        assert_eq!(incomplete.titles_seen(), 1);
        assert!(!policy.push("# Long title\n", 20).is_empty());
        let mut unclosed = SemanticTextRelease::new(128, 10);
        assert!(unclosed.push("# Code\n```rust\nlet x = 1;\n", 0).is_empty());
        let timed = unclosed.tick(10);
        assert_eq!(timed.len(), 1);
        assert_eq!(timed[0].forced, Some(FlushReason::LatencyLimit));
        assert_eq!(timed[0].text, "# Code\n```rust\nlet x = 1;\n");
        assert_eq!(
            run(&"normal prose ".repeat(20_000), 1)
                .iter()
                .map(|r| r.text.len())
                .sum::<usize>(),
            260_000
        );
    }

    #[test]
    fn arbitrary_chunk_boundaries_preserve_semantics() {
        let input = "# A\nFirst.\n**B**\n- item\n# C\n| A | B |\n| --- | --- |\n| 1 | 2 |\n# D\n```\ncode\n```\n";
        let canonical = run(input, 1)
            .into_iter()
            .filter(|release| release.semantic_unit)
            .map(|release| release.text)
            .collect::<Vec<_>>();
        for chunk in 2..=input.len() {
            let actual = run(input, chunk)
                .into_iter()
                .filter(|release| release.semantic_unit)
                .map(|release| release.text)
                .collect::<Vec<_>>();
            assert_eq!(actual, canonical, "chunk size {chunk}");
        }
    }

    #[test]
    fn generated_unicode_streams_preserve_every_byte_at_every_character_split() {
        let atoms = ["é", "🧪", "\n", "#", "*", "|", "`", " text", "- item", ":"];
        let mut seed = 0x5eed_cafe_u64;
        for _case in 0..80 {
            let mut input = String::new();
            for _ in 0..160 {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                input.push_str(atoms[(seed as usize) % atoms.len()]);
            }
            let mut policy = SemanticTextRelease::new(128, 30);
            let mut output = String::new();
            for (index, ch) in input.chars().enumerate() {
                let now_ms = index as u64 * 100;
                for release in policy.push(ch.encode_utf8(&mut [0; 4]), now_ms) {
                    output.push_str(&release.text);
                }
                if index % 37 == 0 {
                    for release in policy.tick(now_ms + 30) {
                        output.push_str(&release.text);
                    }
                }
                if index % 53 == 0 {
                    for release in policy.flush(FlushReason::Tool, now_ms + 31) {
                        output.push_str(&release.text);
                    }
                }
                assert!(policy.held_bytes() < 128);
            }
            for release in policy.flush(FlushReason::Finalization, 500) {
                output.push_str(&release.text);
            }
            assert_eq!(output.as_bytes(), input.as_bytes());
        }
    }

    #[test]
    fn one_megabyte_stream_stays_bounded_and_reports_throughput() {
        let input = "ordinary assistant prose. ".repeat(43_692);
        let mut policy = SemanticTextRelease::default();
        let mut output = String::with_capacity(input.len());
        let started = std::time::Instant::now();
        for chunk in input.as_bytes().chunks(32) {
            for release in policy.push(std::str::from_utf8(chunk).unwrap(), 0) {
                assert!(release.text.len() <= 4_096);
                output.push_str(&release.text);
            }
            assert!(policy.held_bytes() <= 4_096);
        }
        for release in policy.flush(FlushReason::Finalization, 0) {
            output.push_str(&release.text);
        }
        assert_eq!(output.as_bytes(), input.as_bytes());
        assert!(policy.peak_held_bytes() <= 4_096);
        eprintln!(
            "semantic_text_perf bytes={} elapsed_ms={} peak_held_bytes={}",
            input.len(),
            started.elapsed().as_millis(),
            policy.peak_held_bytes()
        );
    }

    #[test]
    fn large_prose_delta_releases_promptly_without_a_forced_flush() {
        let input = "ordinary assistant prose. ".repeat(1_000);
        let mut policy = SemanticTextRelease::default();
        let releases = policy.push(&input, 42);
        assert_eq!(
            releases
                .iter()
                .map(|release| release.text.as_str())
                .collect::<String>(),
            input
        );
        assert!(releases.iter().all(|release| release.forced.is_none()));
        assert!(releases.iter().all(|release| release.text.len() <= 4_096));
        assert_eq!(policy.held_bytes(), 0);
    }
}

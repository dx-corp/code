//! Prompt history storage and search

use std::collections::VecDeque;
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::PathBuf;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use crate::config::HistoryPersistence;
use crate::safety::expand_tilde;

/// Configuration for prompt history
#[derive(Debug, Clone)]
pub struct HistoryConfig {
    /// Maximum number of entries to keep
    pub max_entries: usize,
    /// Path to history file
    pub path: PathBuf,
    /// Whether to deduplicate consecutive entries
    pub deduplicate: bool,
    /// Minimum prompt length to store
    pub min_length: usize,
    /// Maximum total bytes to keep (approximate, JSONL size)
    pub max_bytes: Option<usize>,
    /// Whether to persist history to disk
    pub persistence: HistoryPersistence,
}

impl Default for HistoryConfig {
    fn default() -> Self {
        let default_path = dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".composer")
            .join("history")
            .join("prompts.jsonl");
        let path = env::var("MAESTRO_PROMPT_HISTORY_FILE")
            .ok()
            .and_then(|raw| {
                let trimmed = raw.trim();
                if trimmed.is_empty() {
                    None
                } else {
                    Some(PathBuf::from(trimmed))
                }
            })
            .map(|raw_path| expand_tilde(&raw_path).unwrap_or(raw_path))
            .unwrap_or(default_path);

        Self {
            max_entries: 10000,
            path,
            deduplicate: true,
            min_length: 2,
            max_bytes: None,
            persistence: HistoryPersistence::SaveAll,
        }
    }
}

impl HistoryConfig {
    /// Create config with custom path
    pub fn with_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.path = path.into();
        self
    }

    /// Set max entries
    #[must_use]
    pub fn with_max_entries(mut self, max: usize) -> Self {
        self.max_entries = max;
        self
    }

    /// Set max bytes
    #[must_use]
    pub fn with_max_bytes(mut self, max: usize) -> Self {
        self.max_bytes = Some(max);
        self
    }

    /// Set persistence mode
    #[must_use]
    pub fn with_persistence(mut self, persistence: HistoryPersistence) -> Self {
        self.persistence = persistence;
        self
    }
}

/// A single history entry
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryEntry {
    /// The prompt text
    pub prompt: String,
    /// When the prompt was entered
    pub timestamp: SystemTime,
    /// Optional session ID
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Optional tags
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
}

impl HistoryEntry {
    /// Create a new history entry
    pub fn new(prompt: impl Into<String>) -> Self {
        Self {
            prompt: prompt.into(),
            timestamp: SystemTime::now(),
            session_id: None,
            tags: Vec::new(),
        }
    }

    /// Set session ID
    pub fn with_session(mut self, session_id: impl Into<String>) -> Self {
        self.session_id = Some(session_id.into());
        self
    }

    /// Add a tag
    pub fn with_tag(mut self, tag: impl Into<String>) -> Self {
        self.tags.push(tag.into());
        self
    }
}

/// Result of a history search
#[derive(Debug, Clone)]
pub struct SearchResult {
    /// Matching entries
    pub matches: Vec<SearchMatch>,
    /// Total searched
    pub total_searched: usize,
}

/// A single search match
#[derive(Debug, Clone)]
pub struct SearchMatch {
    /// The matching entry
    pub entry: HistoryEntry,
    /// Index in history (0 = most recent)
    pub index: usize,
    /// Match score (higher = better match)
    pub score: f64,
}

/// Prompt history with persistence and navigation
#[derive(Debug)]
pub struct PromptHistory {
    /// In-memory entries (most recent last)
    entries: VecDeque<HistoryEntry>,
    /// Current navigation position (None = not navigating)
    position: Option<usize>,
    /// Working buffer for current input
    working_buffer: String,
    /// Configuration
    config: HistoryConfig,
    /// Whether history has been modified
    dirty: bool,
}

impl PromptHistory {
    /// Create a new empty history
    #[must_use]
    pub fn new(config: HistoryConfig) -> Self {
        Self {
            entries: VecDeque::new(),
            position: None,
            working_buffer: String::new(),
            config,
            dirty: false,
        }
    }

    /// Load history from default location or create new
    pub fn load_or_create() -> std::io::Result<Self> {
        Self::load_with_config(HistoryConfig::default())
    }

    /// Load history with custom config
    pub fn load_with_config(config: HistoryConfig) -> std::io::Result<Self> {
        let mut history = Self::new(config);
        history.load()?;
        Ok(history)
    }

    fn should_persist(&self) -> bool {
        self.config.persistence != HistoryPersistence::None
    }

    /// Load entries from disk
    pub fn load(&mut self) -> std::io::Result<()> {
        if !self.should_persist() {
            return Ok(());
        }
        if !self.config.path.exists() {
            return Ok(());
        }

        let file = File::open(&self.config.path)?;
        let reader = BufReader::new(file);

        for line in reader.lines() {
            let line = line?;
            if line.is_empty() {
                continue;
            }

            if let Ok(entry) = serde_json::from_str::<HistoryEntry>(&line) {
                self.entries.push_back(entry);
            }
        }

        let trimmed = self.trim_if_needed();
        if trimmed && self.should_persist() {
            let _ = self.save();
        }

        Ok(())
    }

    /// Save history to disk
    pub fn save(&mut self) -> std::io::Result<()> {
        if !self.should_persist() {
            self.dirty = false;
            return Ok(());
        }
        if !self.dirty {
            return Ok(());
        }

        // Ensure directory exists
        if let Some(parent) = self.config.path.parent() {
            fs::create_dir_all(parent)?;
        }

        let file = File::create(&self.config.path)?;
        let mut writer = BufWriter::new(file);

        for entry in &self.entries {
            if let Ok(line) = serde_json::to_string(entry) {
                writeln!(writer, "{line}")?;
            }
        }

        writer.flush()?;
        self.dirty = false;
        Ok(())
    }

    /// Append a single entry (efficient incremental save)
    fn append_to_file(&self, entry: &HistoryEntry) -> std::io::Result<()> {
        if !self.should_persist() {
            return Ok(());
        }
        if let Some(parent) = self.config.path.parent() {
            fs::create_dir_all(parent)?;
        }

        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.config.path)?;
        let mut writer = BufWriter::new(file);

        if let Ok(line) = serde_json::to_string(entry) {
            writeln!(writer, "{line}")?;
        }

        writer.flush()
    }

    fn add_entry(&mut self, entry: HistoryEntry) {
        self.entries.push_back(entry);

        self.trim_if_needed();

        if self.should_persist() {
            if self.dirty {
                let _ = self.save();
            } else if let Some(last) = self.entries.back() {
                if self.append_to_file(last).is_err() {
                    self.dirty = true;
                }
            }
        }

        self.position = None;
    }

    fn trim_if_needed(&mut self) -> bool {
        let mut trimmed = false;

        while self.entries.len() > self.config.max_entries {
            self.entries.pop_front();
            trimmed = true;
        }

        if let Some(max_bytes) = self.config.max_bytes {
            if max_bytes > 0 {
                trimmed |= self.trim_to_max_bytes(max_bytes);
            }
        }

        if trimmed {
            self.dirty = true;
        }

        trimmed
    }

    fn trim_to_max_bytes(&mut self, max_bytes: usize) -> bool {
        if self.entries.is_empty() {
            return false;
        }

        let mut total = 0;
        let mut selected = Vec::new();

        for entry in self.entries.iter().rev() {
            let size = Self::entry_byte_size(entry);
            if size > max_bytes {
                continue;
            }
            if total + size > max_bytes {
                break;
            }
            selected.push(entry.clone());
            total += size;
        }

        selected.reverse();
        if selected.is_empty() && !self.entries.is_empty() {
            if let Some(last) = self.entries.back().cloned() {
                selected.push(last);
            }
        }

        if selected.len() != self.entries.len() {
            self.entries = VecDeque::from(selected);
            return true;
        }

        false
    }

    fn entry_byte_size(entry: &HistoryEntry) -> usize {
        serde_json::to_vec(entry)
            .map(|data| data.len() + 1)
            .unwrap_or(0)
    }

    /// Add a prompt to history
    pub fn add(&mut self, prompt: impl Into<String>) {
        let prompt = prompt.into();

        // Skip if too short
        if prompt.len() < self.config.min_length {
            return;
        }

        // Skip if duplicate of last entry
        if self.config.deduplicate {
            if let Some(last) = self.entries.back() {
                if last.prompt == prompt {
                    return;
                }
            }
        }

        let entry = HistoryEntry::new(&prompt);
        self.add_entry(entry);
    }

    /// Add a prompt with session context
    pub fn add_with_session(&mut self, prompt: impl Into<String>, session_id: impl Into<String>) {
        let prompt = prompt.into();

        if prompt.len() < self.config.min_length {
            return;
        }

        if self.config.deduplicate {
            if let Some(last) = self.entries.back() {
                if last.prompt == prompt {
                    return;
                }
            }
        }

        let entry = HistoryEntry::new(&prompt).with_session(session_id);
        self.add_entry(entry);
    }

    /// Start navigation with current input
    pub fn start_navigation(&mut self, current_input: &str) {
        self.working_buffer = current_input.to_string();
        self.position = None;
    }

    /// Get the previous entry (up arrow)
    pub fn previous(&mut self) -> Option<&str> {
        if self.entries.is_empty() {
            return None;
        }

        let new_pos = match self.position {
            None => self.entries.len().saturating_sub(1),
            Some(0) => 0,
            Some(pos) => pos - 1,
        };

        self.position = Some(new_pos);
        self.entries.get(new_pos).map(|e| e.prompt.as_str())
    }

    /// Get the next entry (down arrow)
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> Option<&str> {
        match self.position {
            None => None,
            Some(pos) => {
                if pos + 1 >= self.entries.len() {
                    // Return to working buffer
                    self.position = None;
                    Some(self.working_buffer.as_str())
                } else {
                    self.position = Some(pos + 1);
                    self.entries.get(pos + 1).map(|e| e.prompt.as_str())
                }
            }
        }
    }

    /// Reset navigation position
    pub fn reset_navigation(&mut self) {
        self.position = None;
        self.working_buffer.clear();
    }

    /// Get the current entry being viewed (if navigating)
    #[must_use]
    pub fn current(&self) -> Option<&str> {
        match self.position {
            None => None,
            Some(pos) => self.entries.get(pos).map(|e| e.prompt.as_str()),
        }
    }

    /// Check if currently navigating
    #[must_use]
    pub fn is_navigating(&self) -> bool {
        self.position.is_some()
    }

    /// Search history with fuzzy matching
    #[must_use]
    pub fn search(&self, query: &str) -> SearchResult {
        let query_lower = query.to_lowercase();
        let mut matches = Vec::new();

        for (idx, entry) in self.entries.iter().enumerate().rev() {
            let prompt_lower = entry.prompt.to_lowercase();

            // Calculate match score
            let score = if entry.prompt == query {
                1.0 // Exact match
            } else if prompt_lower == query_lower {
                0.95 // Case-insensitive exact match
            } else if prompt_lower.starts_with(&query_lower) {
                0.9 // Prefix match
            } else if prompt_lower.contains(&query_lower) {
                0.7 // Contains match
            } else if fuzzy_match(&prompt_lower, &query_lower) {
                0.5 // Fuzzy match
            } else {
                continue;
            };

            // Recency boost: more recent entries get higher scores
            let recency_boost = (idx as f64 / self.entries.len() as f64) * 0.1;

            matches.push(SearchMatch {
                entry: entry.clone(),
                index: self.entries.len() - 1 - idx,
                score: score + recency_boost,
            });
        }

        // Sort by score descending (handle NaN safely)
        matches.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        SearchResult {
            matches,
            total_searched: self.entries.len(),
        }
    }

    /// Get recent entries
    #[must_use]
    pub fn recent(&self, count: usize) -> Vec<&HistoryEntry> {
        self.entries.iter().rev().take(count).collect()
    }

    /// Get all entries
    pub fn all(&self) -> impl Iterator<Item = &HistoryEntry> {
        self.entries.iter()
    }

    /// Get entry count
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Check if empty
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Clear all history
    pub fn clear(&mut self) {
        self.entries.clear();
        self.position = None;
        self.dirty = true;
        if self.should_persist() {
            let _ = self.save();
        } else {
            let _ = self.delete_file();
        }
    }

    /// Delete history file
    pub fn delete_file(&self) -> std::io::Result<()> {
        if self.config.path.exists() {
            fs::remove_file(&self.config.path)
        } else {
            Ok(())
        }
    }

    /// Get entries for a specific session
    #[must_use]
    pub fn for_session(&self, session_id: &str) -> Vec<&HistoryEntry> {
        self.entries
            .iter()
            .filter(|e| e.session_id.as_deref() == Some(session_id))
            .collect()
    }
}

impl Default for PromptHistory {
    fn default() -> Self {
        Self::new(HistoryConfig::default())
    }
}

/// Simple fuzzy matching (checks if all query chars appear in order)
fn fuzzy_match(haystack: &str, needle: &str) -> bool {
    let mut haystack_chars = haystack.chars().peekable();

    for needle_char in needle.chars() {
        loop {
            match haystack_chars.next() {
                Some(h_char) if h_char == needle_char => break,
                Some(_) => continue,
                None => return false,
            }
        }
    }

    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn test_config(dir: &TempDir) -> HistoryConfig {
        HistoryConfig::default()
            .with_path(dir.path().join("test_history.jsonl"))
            .with_max_entries(100)
    }

    #[test]
    fn test_add_and_retrieve() {
        let dir = TempDir::new().unwrap();
        let mut history = PromptHistory::new(test_config(&dir));

        history.add("first");
        history.add("second");
        history.add("third");

        assert_eq!(history.len(), 3);
    }

    #[test]
    fn test_deduplication() {
        let dir = TempDir::new().unwrap();
        let mut history = PromptHistory::new(test_config(&dir));

        history.add("same");
        history.add("same"); // Should be skipped
        history.add("different");
        history.add("same"); // Should be added (not consecutive)

        assert_eq!(history.len(), 3);
    }

    #[test]
    fn test_navigation() {
        let dir = TempDir::new().unwrap();
        let mut history = PromptHistory::new(test_config(&dir));

        history.add("first");
        history.add("second");
        history.add("third");

        history.start_navigation("current");

        assert_eq!(history.previous(), Some("third"));
        assert_eq!(history.previous(), Some("second"));
        assert_eq!(history.previous(), Some("first"));
        assert_eq!(history.previous(), Some("first")); // Stays at first

        assert_eq!(history.next(), Some("second"));
        assert_eq!(history.next(), Some("third"));
        assert_eq!(history.next(), Some("current")); // Back to working buffer
    }

    #[test]
    fn test_search_exact() {
        let dir = TempDir::new().unwrap();
        let mut history = PromptHistory::new(test_config(&dir));

        history.add("git status");
        history.add("git commit -m 'test'");
        history.add("npm install");

        let results = history.search("git");
        assert_eq!(results.matches.len(), 2);
        assert!(results.matches[0].entry.prompt.contains("git"));
    }

    #[test]
    fn test_search_fuzzy() {
        let dir = TempDir::new().unwrap();
        let mut history = PromptHistory::new(test_config(&dir));

        history.add("git status");
        history.add("commit message");

        // "gs" should fuzzy match "git status"
        let results = history.search("gs");
        assert!(!results.matches.is_empty());
    }

    #[test]
    fn test_persistence() {
        let dir = TempDir::new().unwrap();
        let config = test_config(&dir);

        // Write history
        {
            let mut history = PromptHistory::new(config.clone());
            history.add("persisted");
            history.save().unwrap();
        }

        // Read it back
        {
            let history = PromptHistory::load_with_config(config).unwrap();
            assert_eq!(history.len(), 1);
            assert_eq!(history.entries[0].prompt, "persisted");
        }
    }

    #[test]
    fn test_max_entries() {
        let dir = TempDir::new().unwrap();
        let config = test_config(&dir).with_max_entries(3);
        let mut history = PromptHistory::new(config);

        history.add("one");
        history.add("two");
        history.add("three");
        history.add("four"); // Should evict "one"

        assert_eq!(history.len(), 3);
        assert_eq!(history.entries[0].prompt, "two");
    }

    #[test]
    fn test_max_bytes_trims_recent_entries() {
        let dir = TempDir::new().unwrap();
        let mut history = PromptHistory::new(test_config(&dir));

        history.add("one");
        history.add("two");
        history.add("three");

        let last = history.entries.back().unwrap().clone();
        let budget = PromptHistory::entry_byte_size(&last) + 1;
        history.config.max_bytes = Some(budget);
        history.trim_if_needed();

        assert_eq!(history.len(), 1);
        assert_eq!(history.entries.back().unwrap().prompt, "three");
    }

    #[test]
    fn test_max_bytes_keeps_last_when_budget_too_small() {
        let dir = TempDir::new().unwrap();
        let mut history = PromptHistory::new(test_config(&dir));

        history.add("first");
        history.add("second");

        history.config.max_bytes = Some(1);
        history.trim_if_needed();

        assert_eq!(history.len(), 1);
        assert_eq!(history.entries.back().unwrap().prompt, "second");
    }

    #[test]
    fn test_min_length() {
        let dir = TempDir::new().unwrap();
        let mut history = PromptHistory::new(test_config(&dir));

        history.add("x"); // Too short
        history.add("ok"); // Just right

        assert_eq!(history.len(), 1);
    }

    #[test]
    fn test_session_filter() {
        let dir = TempDir::new().unwrap();
        let mut history = PromptHistory::new(test_config(&dir));

        history.add_with_session("session1 prompt", "sess-1");
        history.add_with_session("session2 prompt", "sess-2");
        history.add("no session");

        let sess1 = history.for_session("sess-1");
        assert_eq!(sess1.len(), 1);
        assert_eq!(sess1[0].prompt, "session1 prompt");
    }

    #[test]
    fn test_persistence_none_skips_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("no_persist.jsonl");
        let config = HistoryConfig::default()
            .with_path(path.clone())
            .with_persistence(HistoryPersistence::None);
        let mut history = PromptHistory::new(config);

        history.add("hello");
        assert!(!path.exists());
    }

    #[test]
    fn test_fuzzy_match_function() {
        assert!(fuzzy_match("git status", "gs"));
        assert!(fuzzy_match("hello world", "hlo"));
        assert!(!fuzzy_match("abc", "xyz"));
        assert!(fuzzy_match("abc", "ac"));
    }

    #[test]
    fn test_recent() {
        let dir = TempDir::new().unwrap();
        let mut history = PromptHistory::new(test_config(&dir));

        history.add("old");
        history.add("middle");
        history.add("recent");

        let recent = history.recent(2);
        assert_eq!(recent.len(), 2);
        assert_eq!(recent[0].prompt, "recent");
        assert_eq!(recent[1].prompt, "middle");
    }
}

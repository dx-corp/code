//! Prompt History Module
//!
//! Provides persistent prompt history with fuzzy search and recall.
//!
//! # Features
//!
//! - **Persistence**: History saved to `~/.composer/history/prompts.jsonl`
//! - **Fuzzy search**: Find prompts by partial match
//! - **Deduplication**: Consecutive duplicates are not stored
//! - **Size limits**: Configurable max entries to prevent unbounded growth
//! - **Navigation**: Arrow key navigation through history
//!
//! # Example
//!
//! ```rust,ignore
//! use maestro_tui_kit::history::PromptHistory;
//!
//! let mut history = PromptHistory::load_or_create()?;
//! history.add("git status");
//! history.add("How do I fix this error?");
//!
//! // Navigate
//! let prev = history.previous(); // "How do I fix..."
//! let prev = history.previous(); // "git status"
//!
//! // Search
//! let matches = history.search("git");
//! ```

mod store;

pub use store::{HistoryConfig, HistoryEntry, PromptHistory, SearchMatch, SearchResult};

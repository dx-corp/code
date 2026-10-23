//! Session File Reader
//!
//! This module provides functions for reading and parsing session files stored in JSONL
//! (JSON Lines) format. It implements two reading strategies optimized for different
//! use cases: full session loading and fast header extraction.
//!
//! # File I/O Architecture
//!
//! The reader uses Rust's standard library I/O abstractions for efficient file access:
//!
//! ## Buffered Reading with `BufReader`
//!
//! Files are read using [`BufReader`] which wraps a [`File`] handle and provides
//! efficient buffering. This reduces system calls by reading larger chunks of data
//! and serving line-by-line requests from an in-memory buffer:
//!
//! ```rust,ignore
//! use std::fs::File;
//! use std::io::BufReader;
//!
//! let file = File::open(path)?;  // Open file handle (unbuffered)
//! let reader = BufReader::new(file);  // Wrap with 8KB buffer
//!
//! // BufReader::lines() returns an iterator over Result<String>
//! for line in reader.lines() {
//!     let line = line?;  // Propagate I/O errors with ?
//!     // Process line...
//! }
//! ```
//!
//! ## Line-by-Line Iterator Pattern
//!
//! The `BufReader::lines()` method returns an iterator that:
//! - Yields `Result<String, io::Error>` for each line
//! - Automatically strips line endings (\n, \r\n)
//! - Handles UTF-8 decoding
//! - Enables memory-efficient streaming (doesn't load entire file)
//!
//! Example with error handling:
//! ```rust,ignore
//! for (line_num, line_result) in reader.lines().enumerate() {
//!     let line = line_result?;  // Propagate I/O error
//!     if line.trim().is_empty() {
//!         continue;  // Skip blank lines
//!     }
//!     // Parse JSON...
//! }
//! ```
//!
//! # Reading Strategies
//!
//! ## Full Session Read
//!
//! The [`SessionReader::read_file`] function loads the complete session into memory,
//! including all messages and computed statistics. This is used when the full
//! conversation history is needed.
//!
//! **Performance**: O(n) where n is the number of entries. Memory usage is proportional
//! to session size.
//!
//! ```rust,ignore
//! let session = SessionReader::read_file("/path/to/session.jsonl")?;
//! println!("Loaded {} messages", session.messages.len());
//! println!("Total cost: ${:.2}", session.stats.total_cost);
//! ```
//!
//! ## Header-Only Read
//!
//! The [`SessionReader::read_header`] function is optimized for listing sessions
//! without loading all messages. It scans the file for:
//! 1. The session header (first line)
//! 2. Message counts (by counting role fields)
//! 3. Session metadata (optional)
//!
//! **Performance**: O(n) scan but with minimal parsing. Approximately 10x faster than
//! full read for large sessions.
//!
//! ```rust,ignore
//! let (header, stats, meta) = SessionReader::read_header(path)?;
//! println!("Session {} has {} messages", header.id, stats.user_messages);
//! ```
//!
//! ### Fast Parsing Optimization
//!
//! Header reading deserializes a borrowed discriminator and message role before
//! attempting full JSON parsing. This accepts all JSON whitespace while avoiding
//! allocations for message bodies.
//!
//! # Error Handling
//!
//! ## Custom Error Type
//!
//! The [`SessionReadError`] enum provides structured error reporting:
//!
//! ```rust,ignore
//! pub enum SessionReadError {
//!     IoError(std::io::Error),      // File system errors
//!     ParseError(String),            // JSON parsing errors
//!     InvalidFormat(String),         // Logical validation errors
//! }
//! ```
//!
//! ## From Trait for Error Conversion
//!
//! The `From<std::io::Error>` implementation enables automatic conversion of I/O errors,
//! making the `?` operator work seamlessly:
//!
//! ```rust,ignore
//! impl From<std::io::Error> for SessionReadError {
//!     fn from(e: std::io::Error) -> Self {
//!         SessionReadError::IoError(e)
//!     }
//! }
//!
//! // Now File::open can use ? operator:
//! let file = File::open(path)?;  // Auto-converts io::Error -> SessionReadError
//! ```
//!
//! ## Error Context
//!
//! Parse errors include line numbers for debugging:
//! ```rust,ignore
//! let entry: SessionEntry = serde_json::from_str(&line).map_err(|e| {
//!     SessionReadError::ParseError(format!("Line {}: {}", line_num + 1, e))
//! })?;
//! ```
//!
//! # Serde JSON Deserialization
//!
//! The module uses `serde_json::from_str` for parsing:
//!
//! ```rust,ignore
//! use serde_json;
//!
//! let entry: SessionEntry = serde_json::from_str(&line)?;
//! ```
//!
//! Serde automatically:
//! - Validates JSON syntax
//! - Checks for required fields
//! - Applies default values for missing optional fields
//! - Converts field names (camelCase <-> `snake_case`)
//! - Handles type discriminators for enums
//!
//! # Statistics Computation
//!
//! During full reads, statistics are computed incrementally as messages are parsed:
//!
//! ```rust,ignore
//! match &m.message {
//!     AppMessage::User { .. } => stats.user_messages += 1,
//!     AppMessage::Assistant { usage, content, .. } => {
//!         stats.assistant_messages += 1;
//!         if let Some(u) = usage {
//!             stats.total_input_tokens += u.input;
//!             stats.total_output_tokens += u.output;
//!             stats.total_cost += u.total_cost();
//!         }
//!         // Count tool calls in content blocks
//!         for block in content {
//!             if matches!(block, ContentBlock::ToolCall { .. }) {
//!                 stats.tool_calls += 1;
//!             }
//!         }
//!     }
//!     AppMessage::ToolResult { .. } => stats.tool_results += 1,
//! }
//! ```
//!
//! # Path Handling
//!
//! Functions accept `impl AsRef<Path>` for maximum flexibility:
//!
//! ```rust,ignore
//! pub fn read_file(path: impl AsRef<Path>) -> Result<ParsedSession, SessionReadError>
//! ```
//!
//! This trait bound allows calling with:
//! - `&Path`: Borrowed path reference
//! - `PathBuf`: Owned path
//! - `&str`: String slice (auto-converted)
//! - `String`: Owned string (auto-converted)
//!
//! Inside the function, convert to `&Path`:
//! ```rust,ignore
//! let path = path.as_ref();  // Convert to &Path
//! let file = File::open(path)?;
//! ```
//!
//! # Validation Rules
//!
//! The reader enforces session file integrity:
//!
//! 1. **Unique session header**: Only one `SessionEntry::Session` allowed
//!    ```rust,ignore
//!    if header.is_some() {
//!        return Err(SessionReadError::InvalidFormat("Multiple session headers"));
//!    }
//!    ```
//!
//! 2. **Required header**: File must start with a session header
//!    ```rust,ignore
//!    let header = header.ok_or_else(|| {
//!        SessionReadError::InvalidFormat("Missing session header")
//!    })?;
//!    ```
//!
//! 3. **Graceful degradation**: Invalid entries are skipped rather than failing the
//!    entire read (for header-only reads)
//!
//! # Memory Efficiency
//!
//! For header-only reads, messages are counted without allocating or storing them:
//! - String contains checks avoid JSON parsing overhead
//! - Only header and metadata are fully parsed
//! - Message bodies are never allocated
//!
//! For a 10,000 message session:
//! - Full read: ~50MB memory
//! - Header read: ~1KB memory
//!
//! # Rust Concepts Demonstrated
//!
//! ## Result Type and Error Propagation
//! All fallible operations return `Result<T, E>` and use `?` for error propagation.
//!
//! ## Option Type
//! Optional fields like `meta` are represented with `Option<T>` rather than null pointers.
//!
//! ## Pattern Matching
//! Entry type dispatch uses exhaustive pattern matching for type safety.
//!
//! ## Trait Bounds
//! Generic path parameters use `AsRef<Path>` for flexible input types.
//!
//! ## Iterator Adapters
//! `lines().enumerate()` demonstrates iterator composition for line numbers.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::Path;

use maestro_context::{CONTEXT_BUDGET_CUSTOM_TYPE, ContextBudgetSnapshot};
use maestro_runtime_contracts::{SESSION_EVENT_CUSTOM_TYPE, SessionEvent};
use serde::Deserialize;

use super::entries::{
    AppMessage, BranchSummaryEntry, CompactionEntry, CustomMessageEntry, MessageContent,
    ModelChange, PlanReviewEntry, SessionEntry, SessionHeader, SessionMeta, SessionStats,
    SideQuestionEntry, ThinkingLevel, ThinkingLevelChange, TokenUsage,
};
use super::wire_format_generated::is_compaction_context_excluded_message_role;

const BRANCH_SUMMARY_PREFIX: &str =
    "The following is a summary of a branch that this conversation came back from:\n\n<summary>\n";
const BRANCH_SUMMARY_SUFFIX: &str = "\n</summary>";

fn apply_attachment_extracts(
    message: AppMessage,
    extracted_by_id: &HashMap<String, String>,
) -> AppMessage {
    let AppMessage::User {
        content,
        attachments,
        timestamp,
    } = message
    else {
        return message;
    };

    let Some(attachments) = attachments else {
        return AppMessage::User {
            content,
            attachments: None,
            timestamp,
        };
    };

    let mut changed = false;
    let next_attachments = attachments
        .into_iter()
        .map(|mut attachment| {
            if let Some(extracted) = extracted_by_id.get(&attachment.id) {
                if attachment.extracted_text.as_deref() != Some(extracted) {
                    attachment.extracted_text = Some(extracted.clone());
                    changed = true;
                }
            }
            attachment
        })
        .collect::<Vec<_>>();

    if !changed {
        return AppMessage::User {
            content,
            attachments: Some(next_attachments),
            timestamp,
        };
    }

    AppMessage::User {
        content,
        attachments: Some(next_attachments),
        timestamp,
    }
}

fn timestamp_millis(timestamp: &str) -> u64 {
    chrono::DateTime::parse_from_rfc3339(timestamp)
        .map(|datetime| datetime.timestamp_millis().max(0) as u64)
        .unwrap_or(0)
}

fn custom_message_to_user_message(entry: &CustomMessageEntry) -> Option<AppMessage> {
    if !entry.display {
        return None;
    }

    Some(AppMessage::User {
        content: entry.content.clone(),
        attachments: None,
        timestamp: timestamp_millis(&entry.timestamp),
    })
}

fn branch_summary_to_user_message(entry: &BranchSummaryEntry) -> AppMessage {
    AppMessage::User {
        content: MessageContent::Text(format!(
            "{BRANCH_SUMMARY_PREFIX}{}{BRANCH_SUMMARY_SUFFIX}",
            entry.summary
        )),
        attachments: None,
        timestamp: timestamp_millis(&entry.timestamp),
    }
}

struct CompactionContextEntry {
    id: Option<String>,
    visible_index: usize,
}

struct PendingLifecycleNotification {
    entry: LifecycleNotificationEntry,
    /// Number of compaction-context entries preceding this notice.
    context_position: usize,
}

/// Host-generated subagent completion notice retained for TUI restoration.
///
/// The entry stays outside `messages`, so it never enters resumed model
/// history. `visible_index` places it between conversational messages after
/// all recorded compactions have been applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LifecycleNotificationEntry {
    pub id: String,
    pub content: String,
    pub timestamp: String,
    pub visible_index: usize,
}

/// A lifecycle note not yet proven consumed by a durable assistant turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LifecycleAgentNoteEntry {
    pub application_id: String,
    pub content: String,
}

/// Errors that can occur during session file reading.
///
/// This error type distinguishes between different failure modes to enable
/// appropriate error handling and user feedback.
///
/// # Error Categories
///
/// - **`IoError`**: File system problems (missing file, permissions, disk full)
/// - **`ParseError`**: Invalid JSON syntax or schema violations
/// - **`InvalidFormat`**: Logical validation failures (missing header, duplicate entries)
///
/// # From Trait
///
/// Implements `From<std::io::Error>` to enable automatic conversion with the `?` operator:
/// ```rust,ignore
/// let file = File::open(path)?;  // io::Error automatically converts to SessionReadError
/// ```
#[derive(Debug)]
pub enum SessionReadError {
    /// File system or I/O error.
    ///
    /// Wraps `std::io::Error` from file operations.
    IoError(std::io::Error),

    /// JSON parsing error.
    ///
    /// Includes line number and serde error message for debugging.
    ParseError(String),

    /// Logical format validation error.
    ///
    /// Used for constraints like "session header required" or "no duplicate headers".
    InvalidFormat(String),
}

impl std::fmt::Display for SessionReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SessionReadError::IoError(e) => write!(f, "IO error: {e}"),
            SessionReadError::ParseError(msg) => write!(f, "Parse error: {msg}"),
            SessionReadError::InvalidFormat(msg) => write!(f, "Invalid format: {msg}"),
        }
    }
}

impl std::error::Error for SessionReadError {}

impl From<std::io::Error> for SessionReadError {
    fn from(e: std::io::Error) -> Self {
        SessionReadError::IoError(e)
    }
}

/// Complete session loaded from a JSONL file.
///
/// Contains the full conversation history along with computed statistics and metadata.
/// This struct is returned by [`SessionReader::read_file`] after parsing the entire
/// session file.
///
/// # Memory Usage
///
/// This struct stores all messages in memory, so large sessions (thousands of messages)
/// may consume significant RAM. For listing sessions without loading content, use
/// [`SessionReader::read_header`] instead.
///
/// # Fields
///
/// - `header`: Session initialization parameters (ID, model, working directory)
/// - `messages`: Full conversation history (user, assistant, tool results)
/// - `meta`: Optional user-provided metadata (title, tags, favorite status)
/// - `stats`: Computed aggregates (message counts, token usage, cost)
/// - `file_path`: Path to the source JSONL file
#[derive(Debug, Clone)]
pub struct ParsedSession {
    /// Session initialization parameters from the header entry, with the
    /// effective final model and thinking level applied from recorded changes.
    /// The original changes remain available in `model_changes` and
    /// `thinking_level_changes`.
    pub header: SessionHeader,

    /// Latest supported session preferences; absent in legacy transcripts.
    pub settings: Option<super::SessionSettings>,

    /// Historical request provenance, never applied as current instructions.
    pub system_prompt_parts: Vec<maestro_ai::ContentBlock>,

    /// Complete conversation history in chronological order.
    pub messages: Vec<AppMessage>,

    /// Exact provider history, display projection length, and the number of
    /// compactions already reflected in that checkpoint.
    pub selective_summary_context: Option<(Vec<maestro_ai::Message>, usize, usize)>,

    /// User-provided session metadata (title, summary, tags).
    ///
    /// None if no metadata entry exists in the file.
    pub meta: Option<SessionMeta>,

    /// Aggregated statistics computed during file reading.
    pub stats: SessionStats,

    /// Recorded thinking level changes.
    pub thinking_level_changes: Vec<ThinkingLevelChange>,

    /// Recorded model changes.
    pub model_changes: Vec<ModelChange>,

    /// Recorded compaction events.
    pub compactions: Vec<CompactionEntry>,

    /// Typed session lifecycle telemetry decoded from recognized custom entries.
    pub session_events: Vec<SessionEvent>,

    /// Typed context budget snapshots decoded from recognized custom entries.
    pub context_budget_snapshots: Vec<ContextBudgetSnapshot>,

    /// TUI-only subagent completion notices in final transcript positions.
    pub lifecycle_notifications: Vec<LifecycleNotificationEntry>,

    /// Lifecycle notes awaiting proof that a later model turn consumed them.
    pub pending_lifecycle_agent_notes: Vec<LifecycleAgentNoteEntry>,

    /// Tool-free side questions, excluded from `messages` and model history.
    pub side_questions: Vec<SideQuestionEntry>,

    /// Append-only structured plan review events.
    pub plan_review_events: Vec<PlanReviewEntry>,

    /// Usage entries extracted from assistant messages (for fast usage hydration).
    pub usage_entries: Vec<UsageEntry>,

    /// Absolute path to the source session file.
    pub file_path: String,
}

/// Compact usage record extracted from assistant messages.
#[derive(Debug, Clone)]
pub struct UsageEntry {
    pub model: String,
    pub usage: TokenUsage,
}

impl ParsedSession {
    /// Get the session ID
    pub fn id(&self) -> &str {
        &self.header.id
    }

    /// Get the first user message (for preview)
    pub fn first_user_message(&self) -> Option<String> {
        for msg in &self.messages {
            if let AppMessage::User { .. } = msg {
                return Some(msg.text_content());
            }
        }
        None
    }

    /// Get the title (from meta or first message)
    pub fn title(&self) -> String {
        if let Some(ref meta) = self.meta {
            if let Some(ref title) = meta.title {
                return title.clone();
            }
        }

        // Fall back to first user message preview
        self.first_user_message()
            .map(|s| {
                let s = s.trim();
                let chars: Vec<char> = s.chars().collect();
                if chars.len() > 50 {
                    format!("{}...", chars[..47].iter().collect::<String>())
                } else {
                    s.to_string()
                }
            })
            .unwrap_or_else(|| "Untitled session".to_string())
    }

    /// Check if this session is a favorite
    pub fn is_favorite(&self) -> bool {
        self.meta.as_ref().is_some_and(|m| m.favorite)
    }
}

/// Zero-sized unit struct providing session file parsing functions.
///
/// This is a namespace for stateless functions rather than a stateful reader.
/// All methods are associated functions (no `&self` parameter) and can be called
/// directly on the type:
///
/// ```rust,ignore
/// let session = SessionReader::read_file(path)?;
/// let (header, stats, meta) = SessionReader::read_header(path)?;
/// ```
///
/// # Design Rationale
///
/// Using a unit struct rather than free functions provides:
/// - Clear namespace organization (`SessionReader::method` vs `reader_method`)
/// - Consistent API with other reader types in the ecosystem
/// - Easier discoverability through IDE autocomplete
pub struct SessionReader;

/// Borrowed fields used to identify entries while scanning a session header.
///
/// Deserializing this small shape accepts all JSON whitespace while skipping
/// message bodies without allocating them. Matching the serialized bytes
/// directly would reject otherwise valid JSONL produced by tools such as
/// Python's default `json.dumps` formatting.
#[derive(Debug, Deserialize)]
struct HeaderScanEntry<'a> {
    #[serde(borrow, rename = "type")]
    entry_type: Option<Cow<'a, str>>,
    #[serde(borrow)]
    message: Option<HeaderScanMessage<'a>>,
}

#[derive(Debug, Deserialize)]
struct HeaderScanMessage<'a> {
    #[serde(borrow)]
    role: Option<Cow<'a, str>>,
}

/// Count user boundaries without deserializing cumulative checkpoint payloads.
pub(super) fn is_user_message_line(line: &str) -> std::io::Result<bool> {
    let invalid = |error| std::io::Error::new(std::io::ErrorKind::InvalidData, error);
    let entry: HeaderScanEntry<'_> = serde_json::from_str(line.trim_end()).map_err(invalid)?;
    if entry.entry_type.as_deref() != Some("message")
        || entry.message.and_then(|message| message.role).as_deref() != Some("user")
    {
        return Ok(false);
    }
    // A damaged record must not shift file checkpoint coordinates relative to
    // SessionReader, which validates the persisted message schema.
    // Validate only user candidates; compaction records remain projection-only.
    let _: SessionEntry = serde_json::from_str(line.trim_end()).map_err(invalid)?;
    Ok(true)
}

fn apply_effective_preferences(
    header: &mut SessionHeader,
    model: Option<&str>,
    thinking_level: Option<ThinkingLevel>,
) {
    if let Some(model) = model {
        header.model = model.to_string();
    }
    if let Some(thinking_level) = thinking_level {
        header.thinking_level = thinking_level;
    }
}

impl SessionReader {
    /// Read a session from a file
    pub fn read_file(path: impl AsRef<Path>) -> Result<ParsedSession, SessionReadError> {
        let path = path.as_ref();
        let mut file = File::open(path)?;

        // A crash mid-`writeln!` leaves a torn final line, recognizable by
        // the missing trailing newline. Only that case is tolerated below;
        // a newline-terminated corrupt line still fails the load.
        let has_torn_tail = file
            .seek(SeekFrom::End(-1))
            .and_then(|_| {
                let mut byte = [0u8; 1];
                file.read_exact(&mut byte).map(|()| byte[0] != b'\n')
            })
            .unwrap_or(false);
        file.seek(SeekFrom::Start(0))?;
        let reader = BufReader::new(file);

        let mut header: Option<SessionHeader> = None;
        let mut messages: Vec<AppMessage> = Vec::new();
        let mut selective_summary_context = None;
        let mut pending_runtime_checkpoint =
            super::runtime_checkpoint::PendingCheckpoint::default();
        let mut settings: Option<super::SessionSettings> = None;
        let mut system_prompt_parts = Vec::new();
        let mut effective_model: Option<String> = None;
        let mut effective_thinking = None;
        let mut meta: Option<SessionMeta> = None;
        let mut stats = SessionStats::default();
        let mut extracted_by_id: HashMap<String, String> = HashMap::new();
        let mut thinking_level_changes: Vec<ThinkingLevelChange> = Vec::new();
        let mut model_changes: Vec<ModelChange> = Vec::new();
        let mut compactions: Vec<CompactionEntry> = Vec::new();
        let mut session_events: Vec<SessionEvent> = Vec::new();
        let mut context_budget_snapshots: Vec<ContextBudgetSnapshot> = Vec::new();
        let mut lifecycle_notifications: Vec<PendingLifecycleNotification> = Vec::new();
        let mut lifecycle_agent_notes: Vec<LifecycleAgentNoteEntry> = Vec::new();
        let mut consumed_lifecycle_agent_notes: HashSet<String> = HashSet::new();
        let mut side_questions: Vec<SideQuestionEntry> = Vec::new();
        let mut plan_review_events: Vec<PlanReviewEntry> = Vec::new();
        let mut usage_entries: Vec<UsageEntry> = Vec::new();
        let mut compaction_context_entries: Vec<CompactionContextEntry> = Vec::new();
        let mut visible_context_len = 0usize;

        // Tolerate a single unparseable line only when it is the torn final
        // one; a corrupt line anywhere else means real data damage and fails.
        let mut trailing_parse_error: Option<String> = None;

        for (line_num, line) in reader.lines().enumerate() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }

            let entry: SessionEntry = match serde_json::from_str(&line) {
                Ok(entry) => {
                    if let Some(err) = trailing_parse_error.take() {
                        return Err(SessionReadError::ParseError(err));
                    }
                    entry
                }
                Err(e) => {
                    let err = format!("Line {}: {}", line_num + 1, e);
                    if !has_torn_tail || trailing_parse_error.replace(err.clone()).is_some() {
                        return Err(SessionReadError::ParseError(err));
                    }
                    continue;
                }
            };

            let Some(entry) = pending_runtime_checkpoint.consume(entry) else {
                continue;
            };
            match entry {
                SessionEntry::Session(h) => {
                    if header.is_some() {
                        return Err(SessionReadError::InvalidFormat(
                            "Multiple session headers".to_string(),
                        ));
                    }
                    header = Some(h);
                }
                SessionEntry::Message(m) => {
                    compaction_context_entries.push(CompactionContextEntry {
                        id: m.id.clone(),
                        visible_index: visible_context_len,
                    });
                    if !is_compaction_context_excluded_message_role(m.message.role()) {
                        visible_context_len += 1;
                    }
                    // Update stats
                    match &m.message {
                        AppMessage::User { .. } => stats.user_messages += 1,
                        AppMessage::Assistant {
                            usage,
                            content,
                            model,
                            ..
                        } => {
                            stats.assistant_messages += 1;
                            if let Some(u) = usage {
                                stats.total_input_tokens += u.input;
                                stats.total_output_tokens += u.output;
                                stats.total_cost += u.total_cost();

                                let model_id = model
                                    .clone()
                                    .or_else(|| header.as_ref().map(|h| h.model.clone()))
                                    .unwrap_or_else(|| "unknown".to_string());
                                usage_entries.push(UsageEntry {
                                    model: model_id,
                                    usage: u.clone(),
                                });
                            }
                            // Count tool calls
                            for block in content {
                                if matches!(block, super::entries::ContentBlock::ToolCall { .. }) {
                                    stats.tool_calls += 1;
                                }
                            }
                        }
                        AppMessage::ToolResult { .. } => stats.tool_results += 1,
                    }
                    messages.push(m.message);
                }
                SessionEntry::AttachmentExtract(extract) => {
                    if !extract.attachment_id.is_empty() && !extract.extracted_text.is_empty() {
                        extracted_by_id.insert(extract.attachment_id, extract.extracted_text);
                    }
                }
                SessionEntry::SessionMeta(m) => {
                    meta = Some(m);
                }
                SessionEntry::ThinkingLevelChange(change) => {
                    effective_thinking = Some(change.thinking_level);
                    if let Some(settings) = &mut settings {
                        settings.thinking_level = change.thinking_level;
                    }
                    thinking_level_changes.push(change);
                }
                SessionEntry::ModelChange(change) => {
                    effective_model = Some(change.model.clone());
                    if let Some(settings) = &mut settings {
                        settings.model = change.model.clone();
                    }
                    model_changes.push(change);
                }
                SessionEntry::Compaction(mut entry) => {
                    let resolved_cut = entry
                        .first_kept_entry_id
                        .as_deref()
                        .and_then(|first_kept_entry_id| {
                            compaction_context_entries
                                .iter()
                                .position(|entry| entry.id.as_deref() == Some(first_kept_entry_id))
                                .map(|position| {
                                    (position, compaction_context_entries[position].visible_index)
                                })
                        })
                        .or_else(|| {
                            entry.first_kept_entry_index.map(|index| {
                                let position = compaction_context_entries
                                    .iter()
                                    .position(|entry| entry.visible_index >= index)
                                    .unwrap_or(compaction_context_entries.len());
                                (position, index)
                            })
                        });

                    if let Some((position, visible_index)) = resolved_cut {
                        entry.first_kept_entry_index = Some(visible_index);
                        lifecycle_notifications
                            .retain(|notification| notification.context_position > position);
                        for notification in &mut lifecycle_notifications {
                            notification.entry.visible_index = notification
                                .entry
                                .visible_index
                                .saturating_sub(visible_index);
                            notification.context_position =
                                notification.context_position.saturating_sub(position);
                        }
                        let keep_from = position.min(compaction_context_entries.len());
                        compaction_context_entries.drain(..keep_from);
                        for context_entry in &mut compaction_context_entries {
                            context_entry.visible_index =
                                context_entry.visible_index.saturating_sub(visible_index);
                        }
                        visible_context_len = visible_context_len.saturating_sub(visible_index);
                    }
                    compactions.push(entry);
                }
                SessionEntry::BranchSummary(entry) => {
                    compaction_context_entries.push(CompactionContextEntry {
                        id: entry.id.clone(),
                        visible_index: visible_context_len,
                    });
                    visible_context_len += 1;
                    messages.push(branch_summary_to_user_message(&entry));
                }
                SessionEntry::CustomMessage(entry) => {
                    compaction_context_entries.push(CompactionContextEntry {
                        id: entry.id.clone(),
                        visible_index: visible_context_len,
                    });
                    if let Some(message) = custom_message_to_user_message(&entry) {
                        visible_context_len += 1;
                        messages.push(message);
                    }
                }
                SessionEntry::SideQuestion(entry) => side_questions.push(entry),
                SessionEntry::PlanReview(entry) => plan_review_events.push(entry),
                SessionEntry::Custom(entry) => {
                    if entry.custom_type == super::checkpoint_updates::SYSTEM_PARTS_TYPE {
                        system_prompt_parts = super::checkpoint_updates::decode_parts(entry.data)
                            .map_err(SessionReadError::InvalidFormat)?;
                    } else if entry.custom_type == super::SESSION_SETTINGS_CUSTOM_TYPE {
                        let checkpoint = super::settings::decode_settings(entry.data)
                            .map_err(SessionReadError::InvalidFormat)?;
                        effective_model = Some(checkpoint.model.clone());
                        effective_thinking = Some(checkpoint.thinking_level);
                        settings = Some(checkpoint);
                    } else if entry.custom_type == super::selective_summary::CONTEXT_TYPE
                        || entry.custom_type == super::selective_summary::RUNTIME_CONTEXT_TYPE
                    {
                        let checkpoint = super::selective_summary::decode_context(entry.data)
                            .map_err(SessionReadError::InvalidFormat)?;
                        let mut projected =
                            super::selective_summary::display_messages(&checkpoint.messages);
                        super::selective_summary::restore_display_timestamps(
                            &mut projected,
                            &messages,
                            &checkpoint.messages,
                            timestamp_millis(&entry.timestamp),
                        )
                        .map_err(|error| SessionReadError::InvalidFormat(error.to_string()))?;
                        messages = projected;
                        if entry.custom_type == super::selective_summary::CONTEXT_TYPE {
                            compactions.clear();
                            lifecycle_notifications.clear();
                            lifecycle_agent_notes.clear();
                            consumed_lifecycle_agent_notes.clear();
                        }
                        compaction_context_entries.clear();
                        visible_context_len = 0;
                        for message in &checkpoint.messages {
                            compaction_context_entries.push(CompactionContextEntry {
                                id: None,
                                visible_index: visible_context_len,
                            });
                            if super::selective_summary::context_message_visible(message) {
                                visible_context_len += 1;
                            }
                        }
                        // Runtime snapshots describe the current provider history, not
                        // whether lifecycle notes have been delivered to its runner.
                        // Keep those independent delivery receipts and remap display
                        // notification boundaries to the checkpoint's entry positions.
                        for notification in &mut lifecycle_notifications {
                            notification.context_position = compaction_context_entries
                                .partition_point(|entry| {
                                    entry.visible_index < notification.entry.visible_index
                                });
                        }
                        selective_summary_context =
                            Some((checkpoint.messages, messages.len(), compactions.len()));
                    } else if entry.custom_type == super::selective_summary::USAGE_TYPE {
                        let usage = super::selective_summary::decode_usage(entry.data)
                            .map_err(SessionReadError::InvalidFormat)?;
                        let usage_tokens = TokenUsage {
                            input: usage.usage.input_tokens,
                            output: usage.usage.output_tokens,
                            cache_read: usage.usage.cache_read_tokens,
                            cache_write: usage.usage.cache_write_tokens,
                            cost: usage.usage.cost.map(|total| super::TokenCost {
                                total,
                                ..Default::default()
                            }),
                        };
                        stats.total_input_tokens += usage_tokens.input;
                        stats.total_output_tokens += usage_tokens.output;
                        stats.total_cost += usage_tokens.total_cost();
                        usage_entries.push(UsageEntry {
                            model: usage.model,
                            usage: usage_tokens,
                        });
                    } else if entry.custom_type == SESSION_EVENT_CUSTOM_TYPE {
                        if let Some(data) = entry.data {
                            if let Ok(event) = serde_json::from_value::<SessionEvent>(data) {
                                session_events.push(event);
                            }
                        }
                    } else if entry.custom_type == CONTEXT_BUDGET_CUSTOM_TYPE {
                        if let Some(data) = entry.data {
                            if let Ok(snapshot) =
                                serde_json::from_value::<ContextBudgetSnapshot>(data)
                            {
                                context_budget_snapshots.push(snapshot);
                            }
                        }
                    } else if entry.custom_type == "subagent_lifecycle_applied" {
                        if let Some((id, data)) = entry.id.zip(entry.data) {
                            if let Some(content) = data.get("content").and_then(|v| v.as_str()) {
                                lifecycle_notifications.push(PendingLifecycleNotification {
                                    entry: LifecycleNotificationEntry {
                                        id: id.clone(),
                                        content: content.to_string(),
                                        timestamp: entry.timestamp,
                                        visible_index: visible_context_len,
                                    },
                                    context_position: compaction_context_entries.len(),
                                });
                            }
                            if let Some(content) = data.get("agentNote").and_then(|v| v.as_str()) {
                                lifecycle_agent_notes.push(LifecycleAgentNoteEntry {
                                    application_id: id,
                                    content: content.to_string(),
                                });
                            }
                        }
                    } else if entry.custom_type == "subagent_lifecycle_agent_note_consumed" {
                        if let Some(application_id) = entry
                            .data
                            .as_ref()
                            .and_then(|data| data.get("applicationId"))
                            .and_then(|value| value.as_str())
                        {
                            consumed_lifecycle_agent_notes.insert(application_id.to_string());
                        }
                    }
                }
                SessionEntry::Label(_) => {}
            }
        }

        if let Some(err) = trailing_parse_error {
            eprintln!(
                "Skipping unparseable trailing line in session file {}: {err}",
                path.display()
            );
        }

        if !extracted_by_id.is_empty() {
            messages = messages
                .into_iter()
                .map(|message| apply_attachment_extracts(message, &extracted_by_id))
                .collect();
        }

        let mut header = header
            .ok_or_else(|| SessionReadError::InvalidFormat("Missing session header".to_string()))?;

        apply_effective_preferences(&mut header, effective_model.as_deref(), effective_thinking);

        lifecycle_agent_notes
            .retain(|note| !consumed_lifecycle_agent_notes.contains(note.application_id.as_str()));

        Ok(ParsedSession {
            header,
            settings,
            system_prompt_parts,
            messages,
            selective_summary_context,
            meta,
            stats,
            thinking_level_changes,
            model_changes,
            compactions,
            session_events,
            context_budget_snapshots,
            lifecycle_notifications: lifecycle_notifications
                .into_iter()
                .map(|notification| notification.entry)
                .collect(),
            pending_lifecycle_agent_notes: lifecycle_agent_notes,
            side_questions,
            plan_review_events,
            usage_entries,
            file_path: path.to_string_lossy().to_string(),
        })
    }

    /// Read just the header and stats from a session file (faster than full read)
    pub fn read_header(
        path: impl AsRef<Path>,
    ) -> Result<(SessionHeader, SessionStats, Option<SessionMeta>), SessionReadError> {
        let path = path.as_ref();
        let file = File::open(path)?;
        let mut reader = BufReader::new(file);

        let mut header: Option<SessionHeader> = None;
        let mut meta: Option<SessionMeta> = None;
        let mut stats = SessionStats::default();
        let mut last_model_change: Option<String> = None;
        let mut last_thinking_level_change: Option<ThinkingLevel> = None;
        let mut line = String::new();

        loop {
            line.clear();
            if reader.read_line(&mut line)? == 0 {
                break;
            }
            if line.trim().is_empty() {
                continue;
            }

            // Parse only the discriminator and message role. This is still much
            // cheaper than deserializing each message body, while accepting all
            // whitespace permitted by JSON.
            let Ok(scan) = serde_json::from_str::<HeaderScanEntry<'_>>(&line) else {
                continue;
            };

            match scan.entry_type.as_deref() {
                Some("session") if header.is_none() => {
                    if let Ok(SessionEntry::Session(h)) =
                        serde_json::from_str::<SessionEntry>(&line)
                    {
                        header = Some(h);
                    }
                }
                Some("message") => match scan.message.and_then(|message| message.role).as_deref() {
                    Some("user") => stats.user_messages += 1,
                    Some("assistant") => stats.assistant_messages += 1,
                    Some("toolResult") => stats.tool_results += 1,
                    _ => {}
                },
                Some("session_meta") => {
                    if let Ok(SessionEntry::SessionMeta(m)) =
                        serde_json::from_str::<SessionEntry>(&line)
                    {
                        meta = Some(m);
                    }
                }
                Some("custom") => {
                    if let Ok(SessionEntry::Custom(entry)) =
                        serde_json::from_str::<SessionEntry>(&line)
                    {
                        if entry.custom_type == super::SESSION_SETTINGS_CUSTOM_TYPE {
                            let settings = super::settings::decode_settings(entry.data)
                                .map_err(SessionReadError::InvalidFormat)?;
                            last_model_change = Some(settings.model);
                            last_thinking_level_change = Some(settings.thinking_level);
                        }
                    }
                }
                Some("model_change") => {
                    if let Ok(SessionEntry::ModelChange(change)) =
                        serde_json::from_str::<SessionEntry>(&line)
                    {
                        last_model_change = Some(change.model);
                    }
                }
                Some("thinking_level_change") => {
                    if let Ok(SessionEntry::ThinkingLevelChange(change)) =
                        serde_json::from_str::<SessionEntry>(&line)
                    {
                        last_thinking_level_change = Some(change.thinking_level);
                    }
                }
                _ => {}
            }
        }

        let mut header = header
            .ok_or_else(|| SessionReadError::InvalidFormat("Missing session header".to_string()))?;

        apply_effective_preferences(
            &mut header,
            last_model_change.as_deref(),
            last_thinking_level_change,
        );

        Ok((header, stats, meta))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    fn create_test_session() -> NamedTempFile {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(file, r#"{{"type":"session","id":"test123","timestamp":"2024-01-15T10:30:00Z","cwd":"/tmp","model":"anthropic/claude-3","thinking_level":"medium"}}"#).unwrap();
        writeln!(file, r#"{{"type":"message","timestamp":"2024-01-15T10:30:00Z","message":{{"role":"user","content":"Hello","timestamp":0}}}}"#).unwrap();
        writeln!(file, r#"{{"type":"message","timestamp":"2024-01-15T10:30:01Z","message":{{"role":"assistant","content":[{{"type":"text","text":"Hi there!"}}],"timestamp":1}}}}"#).unwrap();
        file
    }

    #[test]
    fn read_session_file() {
        let file = create_test_session();
        let session = SessionReader::read_file(file.path()).unwrap();

        assert_eq!(session.id(), "test123");
        assert_eq!(session.messages.len(), 2);
        assert_eq!(session.stats.user_messages, 1);
        assert_eq!(session.stats.assistant_messages, 1);
    }

    #[test]
    fn decodes_typed_telemetry_custom_entries() {
        let mut file = create_test_session();
        writeln!(
            file,
            "{}",
            serde_json::json!({
                "type": "custom",
                "id": "event-entry",
                "timestamp": "2024-01-15T10:30:02Z",
                "customType": SESSION_EVENT_CUSTOM_TYPE,
                "data": {
                    "schemaVersion": "evalops.maestro.session-event.v1",
                    "eventId": "event-1",
                    "timestamp": "2024-01-15T10:30:02Z",
                    "sessionId": "test123",
                    "lane": "tools",
                    "kind": "tool.started",
                    "phase": "started",
                    "correlationId": "call-1"
                }
            })
        )
        .unwrap();
        writeln!(
            file,
            "{}",
            serde_json::json!({
                "type": "custom",
                "id": "budget-entry",
                "timestamp": "2024-01-15T10:30:03Z",
                "customType": CONTEXT_BUDGET_CUSTOM_TYPE,
                "data": {
                    "schemaVersion": "evalops.maestro.context-budget.v1",
                    "snapshotId": "snapshot-1",
                    "timestamp": "2024-01-15T10:30:03Z",
                    "model": "openai/gpt-5.6",
                    "phase": "prepared_request",
                    "contextWindow": 100,
                    "systemPrompt": 10,
                    "toolSchemas": 10,
                    "toolResults": 10,
                    "conversation": 10,
                    "other": 10,
                    "responseReserve": 20,
                    "safetyMargin": 5,
                    "confidence": "measured"
                }
            })
        )
        .unwrap();
        writeln!(
            file,
            "{}",
            serde_json::json!({
                "type": "custom",
                "timestamp": "2024-01-15T10:30:04Z",
                "customType": "selective_summary_context_v1",
                "data": {
                    "messages": [
                        {"role": "user", "content": "Hello"}
                    ]
                }
            })
        )
        .unwrap();
        file.flush().unwrap();

        let session = SessionReader::read_file(file.path()).unwrap();

        assert_eq!(session.session_events.len(), 1);
        assert_eq!(session.session_events[0].event_id, "event-1");
        assert_eq!(
            session.session_events[0].correlation_id.as_deref(),
            Some("call-1")
        );
        assert_eq!(session.context_budget_snapshots.len(), 1);
        assert_eq!(
            session.context_budget_snapshots[0].snapshot_id,
            "snapshot-1"
        );
        assert_eq!(
            session.context_budget_snapshots[0].remaining_headroom(),
            Some(25)
        );
        assert!(session.selective_summary_context.is_some());
    }

    #[test]
    fn malformed_typed_telemetry_is_ignored() {
        let mut file = create_test_session();
        writeln!(
            file,
            "{}",
            serde_json::json!({
                "type": "custom",
                "timestamp": "2024-01-15T10:30:02Z",
                "customType": SESSION_EVENT_CUSTOM_TYPE,
                "data": {"eventId": "missing-required-fields"}
            })
        )
        .unwrap();
        writeln!(
            file,
            "{}",
            serde_json::json!({
                "type": "custom",
                "timestamp": "2024-01-15T10:30:03Z",
                "customType": CONTEXT_BUDGET_CUSTOM_TYPE,
                "data": "not-a-snapshot"
            })
        )
        .unwrap();
        file.flush().unwrap();

        let session = SessionReader::read_file(file.path()).unwrap();

        assert_eq!(session.messages.len(), 2);
        assert!(session.session_events.is_empty());
        assert!(session.context_budget_snapshots.is_empty());
    }

    #[test]
    fn read_header_only() {
        let file = create_test_session();
        let (header, stats, _meta) = SessionReader::read_header(file.path()).unwrap();

        assert_eq!(header.id, "test123");
        assert_eq!(stats.user_messages, 1);
    }

    #[test]
    fn effective_preferences_follow_interleaved_model_and_thinking_changes() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(
            file,
            "{}",
            serde_json::json!({
                "type": "session",
                "id": "effective-preferences",
                "timestamp": "2024-01-15T10:30:00Z",
                "cwd": "/tmp",
                "model": "openai/gpt-6-astra",
                "thinkingLevel": "off"
            })
        )
        .unwrap();
        writeln!(
            file,
            "{}",
            serde_json::json!({
                "type": "thinking_level_change",
                "timestamp": "2024-01-15T10:30:01Z",
                "thinkingLevel": "low"
            })
        )
        .unwrap();
        writeln!(
            file,
            "{}",
            serde_json::json!({
                "type": "model_change",
                "timestamp": "2024-01-15T10:30:02Z",
                "model": "openai-codex/gpt-5.6"
            })
        )
        .unwrap();
        writeln!(
            file,
            "{}",
            serde_json::json!({
                "type": "thinking_level_change",
                "timestamp": "2024-01-15T10:30:03Z",
                "thinkingLevel": "high"
            })
        )
        .unwrap();
        file.flush().unwrap();

        let session = SessionReader::read_file(file.path()).unwrap();
        assert_eq!(session.header.model, "openai-codex/gpt-5.6");
        assert_eq!(session.header.thinking_level, ThinkingLevel::High);
        assert_eq!(session.model_changes.len(), 1);
        assert_eq!(session.thinking_level_changes.len(), 2);

        let (header, _stats, _meta) = SessionReader::read_header(file.path()).unwrap();
        assert_eq!(header.model, "openai-codex/gpt-5.6");
        assert_eq!(header.thinking_level, ThinkingLevel::High);
    }

    #[test]
    fn header_scan_ignores_one_malformed_torn_preference_record_like_full_read() {
        for malformed in [
            serde_json::json!({
                "type": "model_change",
                "model": "anthropic/claude-sonnet-4.5"
            }),
            serde_json::json!({
                "type": "thinking_level_change",
                "thinkingLevel": "max"
            }),
        ] {
            let mut file = NamedTempFile::new().unwrap();
            for entry in [
                serde_json::json!({
                    "type": "session",
                    "id": "effective-preferences",
                    "timestamp": "2024-01-15T10:30:00Z",
                    "cwd": "/tmp",
                    "model": "openai/gpt-6-astra",
                    "thinkingLevel": "off"
                }),
                serde_json::json!({
                    "type": "model_change",
                    "timestamp": "2024-01-15T10:30:01Z",
                    "model": "openai-codex/gpt-5.6"
                }),
                serde_json::json!({
                    "type": "thinking_level_change",
                    "timestamp": "2024-01-15T10:30:02Z",
                    "thinkingLevel": "high"
                }),
            ] {
                writeln!(file, "{entry}").unwrap();
            }
            // A single malformed final line is the session reader's tolerated
            // crash-torn tail; it must not be treated as a real preference.
            write!(file, "{malformed}").unwrap();
            file.flush().unwrap();

            let full = SessionReader::read_file(file.path()).unwrap();
            let (header, _stats, _meta) = SessionReader::read_header(file.path()).unwrap();
            assert_eq!(full.header.model, header.model);
            assert_eq!(full.header.thinking_level, header.thinking_level);
            assert_eq!(header.model, "openai-codex/gpt-5.6");
            assert_eq!(header.thinking_level, ThinkingLevel::High);
        }
    }

    #[test]
    fn read_header_accepts_json_whitespace_and_escaped_strings() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(file, r#"{{"type": "sess\u0069on", "id": "test123", "timestamp": "2024-01-15T10:30:00Z", "cwd": "/tmp", "model": "anthropic/claude-3", "thinking_level": "medium"}}"#).unwrap();
        writeln!(file, r#"{{"type": "message", "timestamp": "2024-01-15T10:30:00Z", "message": {{"role": "user", "content": "Hello", "timestamp": 0}}}}"#).unwrap();
        writeln!(file, r#"{{"type": "message", "timestamp": "2024-01-15T10:30:01Z", "message": {{"role": "assistant", "content": [{{"type": "text", "text": "Hi there!"}}], "timestamp": 1}}}}"#).unwrap();
        writeln!(file, r#"{{"type": "message", "timestamp": "2024-01-15T10:30:02Z", "message": {{"role": "toolResult", "toolCallId": "call-1", "toolName": "read", "content": "ok", "isError": false, "timestamp": 2}}}}"#).unwrap();
        writeln!(file, r#"{{"type": "session_meta", "timestamp": "2024-01-15T10:30:03Z", "title": "Spaced metadata", "favorite": true}}"#).unwrap();

        let (header, stats, meta) = SessionReader::read_header(file.path()).unwrap();

        assert_eq!(header.id, "test123");
        assert_eq!(stats.user_messages, 1);
        assert_eq!(stats.assistant_messages, 1);
        assert_eq!(stats.tool_results, 1);
        assert_eq!(
            meta.and_then(|entry| entry.title),
            Some("Spaced metadata".to_string())
        );
    }

    #[test]
    fn read_header_scans_varying_line_lengths_and_trailing_metadata() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(file, r#"{{"type":"session","id":"test123","timestamp":"2024-01-15T10:30:00Z","cwd":"/tmp","model":"anthropic/claude-3","thinking_level":"medium"}}"#).unwrap();
        writeln!(file, r#"{{"type":"message","timestamp":"2024-01-15T10:30:01Z","message":{{"role":"assistant","content":[{{"type":"text","text":"{}"}}],"timestamp":1}}}}"#, "long response ".repeat(200)).unwrap();
        writeln!(file, r#"{{"type":"message","timestamp":"2024-01-15T10:30:02Z","message":{{"role":"toolResult","toolCallId":"call-1","toolName":"read","content":"ok","isError":false,"timestamp":2}}}}"#).unwrap();
        writeln!(file, r#"{{"type":"session_meta","timestamp":"2024-01-15T10:30:03Z","title":"Trailing metadata","favorite":true}}"#).unwrap();

        let (header, stats, meta) = SessionReader::read_header(file.path()).unwrap();

        assert_eq!(header.id, "test123");
        assert_eq!(stats.assistant_messages, 1);
        assert_eq!(stats.tool_results, 1);
        assert_eq!(
            meta.and_then(|entry| entry.title),
            Some("Trailing metadata".to_string())
        );
    }

    #[test]
    fn session_title_from_first_message() {
        let file = create_test_session();
        let session = SessionReader::read_file(file.path()).unwrap();

        // Should use first user message as title
        assert!(session.title().contains("Hello"));
    }

    #[test]
    fn read_typescript_authored_tool_session() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(file, r#"{{"type":"session","id":"test123","timestamp":"2024-01-15T10:30:00Z","cwd":"/tmp","model":"openai/gpt-5.2","thinkingLevel":"medium"}}"#).unwrap();
        writeln!(file, r#"{{"type":"message","timestamp":"2024-01-15T10:30:00Z","message":{{"role":"user","content":"Read README","timestamp":0}}}}"#).unwrap();
        writeln!(file, r#"{{"type":"message","timestamp":"2024-01-15T10:30:01Z","message":{{"role":"assistant","api":"openai-responses","provider":"openai","model":"gpt-5.2","usage":{{"input":10,"output":4,"cacheRead":2,"cacheWrite":1,"cost":{{"input":0.1,"output":0.2,"cacheRead":0.01,"cacheWrite":0.02,"total":0.33}}}},"stopReason":"toolUse","timestamp":1,"content":[{{"type":"toolCall","id":"call_1","name":"read","arguments":{{"path":"README.md"}}}}]}}}}"#).unwrap();
        writeln!(file, r#"{{"type":"message","timestamp":"2024-01-15T10:30:02Z","message":{{"role":"toolResult","toolCallId":"call_1","toolName":"read","content":[{{"type":"text","text":"file contents"}}],"isError":false,"timestamp":2}}}}"#).unwrap();

        let session = SessionReader::read_file(file.path()).unwrap();

        assert_eq!(session.messages.len(), 3);
        assert_eq!(session.stats.user_messages, 1);
        assert_eq!(session.stats.assistant_messages, 1);
        assert_eq!(session.stats.tool_calls, 1);
        assert_eq!(session.stats.tool_results, 1);
        assert_eq!(session.stats.total_input_tokens, 10);
        assert_eq!(session.stats.total_output_tokens, 4);
        assert_eq!(session.usage_entries.len(), 1);
        assert_eq!(session.usage_entries[0].model, "gpt-5.2");
        assert_eq!(session.usage_entries[0].usage.cache_read, 2);
    }

    #[test]
    fn read_typescript_compaction_cut_point_by_entry_id() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(file, r#"{{"type":"session","id":"test123","timestamp":"2024-01-15T10:30:00Z","cwd":"/tmp","model":"openai/gpt-5.2","thinkingLevel":"medium"}}"#).unwrap();
        writeln!(file, r#"{{"type":"message","id":"user-1","parentId":null,"timestamp":"2024-01-15T10:30:00Z","message":{{"role":"user","content":"Read README","timestamp":0}}}}"#).unwrap();
        writeln!(file, r#"{{"type":"message","id":"assistant-1","parentId":"user-1","timestamp":"2024-01-15T10:30:01Z","message":{{"role":"assistant","content":[{{"type":"toolCall","id":"call-1","name":"read","arguments":{{"path":"README.md"}}}}],"timestamp":1}}}}"#).unwrap();
        writeln!(file, r#"{{"type":"message","id":"tool-1","parentId":"assistant-1","timestamp":"2024-01-15T10:30:02Z","message":{{"role":"toolResult","toolCallId":"call-1","toolName":"read","content":"file contents","isError":false,"timestamp":2}}}}"#).unwrap();
        writeln!(file, r#"{{"type":"message","id":"assistant-2","parentId":"tool-1","timestamp":"2024-01-15T10:30:03Z","message":{{"role":"assistant","content":[{{"type":"text","text":"Done"}}],"timestamp":3}}}}"#).unwrap();
        writeln!(file, r#"{{"type":"compaction","id":"compact-1","parentId":"assistant-2","timestamp":"2024-01-15T10:30:04Z","summary":"Kept assistant result","firstKeptEntryId":"assistant-2","tokensBefore":1234,"auto":true}}"#).unwrap();
        writeln!(file, r#"{{"type":"message","id":"user-2","parentId":"compact-1","timestamp":"2024-01-15T10:30:05Z","message":{{"role":"user","content":"Summarize","timestamp":4}}}}"#).unwrap();
        writeln!(file, r#"{{"type":"message","id":"assistant-3","parentId":"user-2","timestamp":"2024-01-15T10:30:06Z","message":{{"role":"assistant","content":[{{"type":"text","text":"Summary"}}],"timestamp":5}}}}"#).unwrap();
        writeln!(file, r#"{{"type":"compaction","id":"compact-2","parentId":"assistant-3","timestamp":"2024-01-15T10:30:07Z","summary":"Kept summary","firstKeptEntryId":"assistant-3","tokensBefore":2345,"auto":true}}"#).unwrap();

        let session = SessionReader::read_file(file.path()).unwrap();

        assert_eq!(session.compactions.len(), 2);
        assert_eq!(
            session.compactions[0].first_kept_entry_id.as_deref(),
            Some("assistant-2")
        );
        assert_eq!(session.compactions[0].first_kept_entry_index, Some(2));
        assert_eq!(
            session.compactions[1].first_kept_entry_id.as_deref(),
            Some("assistant-3")
        );
        assert_eq!(session.compactions[1].first_kept_entry_index, Some(2));
    }

    #[test]
    fn lifecycle_notifications_follow_compaction_positions() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(file, r#"{{"type":"session","id":"test123","timestamp":"2024-01-15T10:30:00Z","cwd":"/tmp","model":"openai/gpt-5.2","thinkingLevel":"medium"}}"#).unwrap();
        writeln!(file, r#"{{"type":"message","id":"user-1","timestamp":"2024-01-15T10:30:00Z","message":{{"role":"user","content":"First","timestamp":0}}}}"#).unwrap();
        writeln!(file, r#"{{"type":"custom","id":"notice-removed","timestamp":"2024-01-15T10:30:01Z","customType":"subagent_lifecycle_applied","data":{{"content":"Removed notice"}}}}"#).unwrap();
        writeln!(file, r#"{{"type":"message","id":"assistant-1","timestamp":"2024-01-15T10:30:02Z","message":{{"role":"assistant","content":[{{"type":"text","text":"First reply"}}],"timestamp":1}}}}"#).unwrap();
        writeln!(file, r#"{{"type":"custom","id":"notice-at-cut","timestamp":"2024-01-15T10:30:02Z","customType":"subagent_lifecycle_applied","data":{{"content":"Notice at cut"}}}}"#).unwrap();
        writeln!(file, r#"{{"type":"message","id":"user-2","timestamp":"2024-01-15T10:30:03Z","message":{{"role":"user","content":"Second","timestamp":2}}}}"#).unwrap();
        writeln!(file, r#"{{"type":"custom","id":"notice-a","timestamp":"2024-01-15T10:30:04Z","customType":"subagent_lifecycle_applied","data":{{"content":"Notice A"}}}}"#).unwrap();
        writeln!(file, r#"{{"type":"custom","id":"notice-b","timestamp":"2024-01-15T10:30:05Z","customType":"subagent_lifecycle_applied","data":{{"content":"Notice B"}}}}"#).unwrap();
        writeln!(file, r#"{{"type":"compaction","id":"compact-1","timestamp":"2024-01-15T10:30:06Z","summary":"Kept second turn","firstKeptEntryId":"user-2","tokensBefore":1234,"auto":true}}"#).unwrap();
        writeln!(file, r#"{{"type":"message","id":"assistant-2","timestamp":"2024-01-15T10:30:07Z","message":{{"role":"assistant","content":[{{"type":"text","text":"Second reply"}}],"timestamp":3}}}}"#).unwrap();
        writeln!(file, r#"{{"type":"custom","id":"notice-after","timestamp":"2024-01-15T10:30:08Z","customType":"subagent_lifecycle_applied","data":{{"content":"Notice after"}}}}"#).unwrap();

        let session = SessionReader::read_file(file.path()).unwrap();

        assert_eq!(
            session
                .lifecycle_notifications
                .iter()
                .map(|entry| (entry.id.as_str(), entry.visible_index))
                .collect::<Vec<_>>(),
            vec![("notice-a", 1), ("notice-b", 1), ("notice-after", 2)]
        );
        assert!(
            !session
                .lifecycle_notifications
                .iter()
                .any(|entry| matches!(entry.id.as_str(), "notice-removed" | "notice-at-cut"))
        );
    }

    #[test]
    fn lifecycle_agent_notes_remain_replayable_until_runner_consumption_is_recorded() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(file, r#"{{"type":"session","id":"test123","timestamp":"2024-01-15T10:30:00Z","cwd":"/tmp","model":"openai/gpt-5.2","thinkingLevel":"medium"}}"#).unwrap();
        writeln!(file, r#"{{"type":"custom","id":"applied-a","timestamp":"2024-01-15T10:30:01Z","customType":"subagent_lifecycle_applied","data":{{"content":"Notice A","agentNote":"Agent note A"}}}}"#).unwrap();
        writeln!(file, r#"{{"type":"custom","id":"applied-a:agent-note-delivered","timestamp":"2024-01-15T10:30:02Z","customType":"subagent_lifecycle_agent_note_delivered","data":{{"applicationId":"applied-a"}}}}"#).unwrap();
        writeln!(file, r#"{{"type":"custom","id":"applied-a:agent-note-consumed","timestamp":"2024-01-15T10:30:03Z","customType":"subagent_lifecycle_agent_note_consumed","data":{{"applicationId":"applied-a"}}}}"#).unwrap();
        writeln!(file, r#"{{"type":"custom","id":"applied-b","timestamp":"2024-01-15T10:30:04Z","customType":"subagent_lifecycle_applied","data":{{"content":"Notice B","agentNote":"Agent note B"}}}}"#).unwrap();
        writeln!(file, r#"{{"type":"custom","id":"applied-b:agent-note-delivered","timestamp":"2024-01-15T10:30:05Z","customType":"subagent_lifecycle_agent_note_delivered","data":{{"applicationId":"applied-b"}}}}"#).unwrap();
        writeln!(file, r#"{{"type":"custom","id":"applied-c","timestamp":"2024-01-15T10:30:06Z","customType":"subagent_lifecycle_applied","data":{{"content":"Notice C","agentNote":"Agent note C"}}}}"#).unwrap();

        let session = SessionReader::read_file(file.path()).unwrap();

        assert_eq!(
            session.pending_lifecycle_agent_notes,
            vec![
                LifecycleAgentNoteEntry {
                    application_id: "applied-b".to_string(),
                    content: "Agent note B".to_string(),
                },
                LifecycleAgentNoteEntry {
                    application_id: "applied-c".to_string(),
                    content: "Agent note C".to_string(),
                },
            ]
        );
    }

    #[test]
    fn read_typescript_compaction_cut_point_by_tool_result_entry_id() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(file, r#"{{"type":"session","id":"test123","timestamp":"2024-01-15T10:30:00Z","cwd":"/tmp","model":"openai/gpt-5.2","thinkingLevel":"medium"}}"#).unwrap();
        writeln!(file, r#"{{"type":"message","id":"user-1","parentId":null,"timestamp":"2024-01-15T10:30:00Z","message":{{"role":"user","content":"Read README","timestamp":0}}}}"#).unwrap();
        writeln!(file, r#"{{"type":"message","id":"assistant-1","parentId":"user-1","timestamp":"2024-01-15T10:30:01Z","message":{{"role":"assistant","content":[{{"type":"toolCall","id":"call-1","name":"read","arguments":{{"path":"README.md"}}}}],"timestamp":1}}}}"#).unwrap();
        writeln!(file, r#"{{"type":"message","id":"tool-1","parentId":"assistant-1","timestamp":"2024-01-15T10:30:02Z","message":{{"role":"toolResult","toolCallId":"call-1","toolName":"read","content":"file contents","isError":false,"timestamp":2}}}}"#).unwrap();
        writeln!(file, r#"{{"type":"message","id":"assistant-2","parentId":"tool-1","timestamp":"2024-01-15T10:30:03Z","message":{{"role":"assistant","content":[{{"type":"text","text":"Done"}}],"timestamp":3}}}}"#).unwrap();
        writeln!(file, r#"{{"type":"compaction","id":"compact-1","parentId":"assistant-2","timestamp":"2024-01-15T10:30:04Z","summary":"Kept tool result boundary","firstKeptEntryId":"tool-1","tokensBefore":1234,"auto":true}}"#).unwrap();

        let session = SessionReader::read_file(file.path()).unwrap();

        assert_eq!(session.compactions.len(), 1);
        assert_eq!(
            session.compactions[0].first_kept_entry_id.as_deref(),
            Some("tool-1")
        );
        assert_eq!(session.compactions[0].first_kept_entry_index, Some(2));
        assert_eq!(session.stats.tool_results, 1);
    }

    #[test]
    fn read_typescript_tree_entries_in_compaction_context() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(file, r#"{{"type":"session","version":2,"id":"test123","timestamp":"2024-01-15T10:30:00Z","cwd":"/tmp","subject":"Parity","model":"openai/gpt-5.2","promptMetadata":{{"name":"system","label":"System","hash":"hash-1","source":"service"}},"parentSession":"root-session","thinkingLevel":"medium"}}"#).unwrap();
        writeln!(file, r#"{{"type":"message","id":"user-1","parentId":null,"timestamp":"2024-01-15T10:30:00Z","message":{{"role":"user","content":"Start","timestamp":0}}}}"#).unwrap();
        writeln!(file, r#"{{"type":"message","id":"tool-1","parentId":"user-1","timestamp":"2024-01-15T10:30:01Z","message":{{"role":"toolResult","toolCallId":"call-1","toolName":"read","content":[{{"type":"text","text":"ignored for compaction index"}}],"isError":false,"timestamp":1}}}}"#).unwrap();
        writeln!(file, r#"{{"type":"custom_message","id":"hidden-hook-1","parentId":"tool-1","timestamp":"2024-01-15T10:30:02Z","customType":"hook","content":"Hidden hook context","display":false}}"#).unwrap();
        writeln!(file, r#"{{"type":"custom_message","id":"visible-hook-1","parentId":"hidden-hook-1","timestamp":"2024-01-15T10:30:03Z","customType":"hook","content":"Visible hook context","display":true}}"#).unwrap();
        writeln!(file, r#"{{"type":"branch_summary","id":"branch-1","parentId":"visible-hook-1","timestamp":"2024-01-15T10:30:04Z","fromId":"user-1","summary":"Branch context"}}"#).unwrap();
        writeln!(file, r#"{{"type":"compaction","id":"compact-1","parentId":"branch-1","timestamp":"2024-01-15T10:30:04Z","summary":"Kept branch","firstKeptEntryId":"branch-1","tokensBefore":1234,"auto":true}}"#).unwrap();

        let session = SessionReader::read_file(file.path()).unwrap();

        assert_eq!(session.header.version, Some(2));
        assert_eq!(session.header.subject.as_deref(), Some("Parity"));
        assert_eq!(
            session.header.parent_session.as_deref(),
            Some("root-session")
        );
        assert_eq!(
            session.header.prompt_metadata.as_ref().unwrap()["hash"],
            "hash-1"
        );
        assert_eq!(session.messages.len(), 4);
        assert!(
            !session
                .messages
                .iter()
                .any(|message| message.text_content().contains("Hidden hook context"))
        );
        assert_eq!(session.messages[2].text_content(), "Visible hook context");
        assert!(
            session.messages[3]
                .text_content()
                .contains("Branch context")
        );
        assert_eq!(session.compactions[0].first_kept_entry_index, Some(2));
    }

    #[test]
    fn resolves_hidden_custom_message_compaction_ids_without_rendering_content() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(file, r#"{{"type":"session","version":2,"id":"test123","timestamp":"2024-01-15T10:30:00Z","cwd":"/tmp","model":"openai/gpt-5.2","thinkingLevel":"medium"}}"#).unwrap();
        writeln!(file, r#"{{"type":"message","id":"user-1","parentId":null,"timestamp":"2024-01-15T10:30:00Z","message":{{"role":"user","content":"Visible before hook","timestamp":0}}}}"#).unwrap();
        writeln!(file, r#"{{"type":"custom_message","id":"hidden-hook-1","parentId":"user-1","timestamp":"2024-01-15T10:30:01Z","customType":"hook","content":"Hidden hook context","display":false}}"#).unwrap();
        writeln!(file, r#"{{"type":"message","id":"assistant-1","parentId":"hidden-hook-1","timestamp":"2024-01-15T10:30:02Z","message":{{"role":"assistant","content":[{{"type":"text","text":"Visible after hook"}}],"timestamp":2}}}}"#).unwrap();
        writeln!(file, r#"{{"type":"compaction","id":"compact-1","parentId":"assistant-1","timestamp":"2024-01-15T10:30:03Z","summary":"Kept hidden hook boundary","firstKeptEntryId":"hidden-hook-1","tokensBefore":1234,"auto":true}}"#).unwrap();

        let session = SessionReader::read_file(file.path()).unwrap();

        assert_eq!(session.messages.len(), 2);
        assert!(
            !session
                .messages
                .iter()
                .any(|message| message.text_content().contains("Hidden hook context"))
        );
        assert_eq!(session.compactions[0].first_kept_entry_index, Some(1));
    }

    #[test]
    fn side_questions_are_loaded_outside_main_message_history() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(file, r#"{{"type":"session","version":2,"id":"test123","timestamp":"2024-01-15T10:30:00Z","cwd":"/tmp","model":"openai/gpt-5.2","thinkingLevel":"medium"}}"#).unwrap();
        writeln!(file, r#"{{"type":"message","timestamp":"2024-01-15T10:30:01Z","message":{{"role":"user","content":"Main question","timestamp":1}}}}"#).unwrap();
        writeln!(file, r#"{{"type":"side_question","id":"side-1","timestamp":"2024-01-15T10:30:02Z","question":"Side question","answer":"Side answer"}}"#).unwrap();
        writeln!(file, r#"{{"type":"plan_review","timestamp":"2024-01-15T10:30:03Z","action":"comment","id":1,"start_line":2,"end_line":3,"text":"Clarify","revision":"abc","excerpt":"lines"}}"#).unwrap();

        let session = SessionReader::read_file(file.path()).unwrap();
        assert_eq!(session.messages.len(), 1);
        assert_eq!(session.side_questions.len(), 1);
        assert_eq!(session.side_questions[0].question, "Side question");
        assert_eq!(session.plan_review_events.len(), 1);
    }

    #[test]
    fn torn_trailing_line_is_skipped() {
        let mut file = create_test_session();
        // Simulate a crash mid-`writeln!`: a partial JSON line with no newline.
        write!(file, r#"{{"type":"message","timestamp":"2024-01-15T10:30"#).unwrap();
        file.flush().unwrap();

        let session = SessionReader::read_file(file.path()).unwrap();
        assert_eq!(session.messages.len(), 2);
        assert_eq!(session.stats.user_messages, 1);
    }

    #[test]
    fn corrupt_middle_line_still_fails() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(file, r#"{{"type":"session","id":"test123","timestamp":"2024-01-15T10:30:00Z","cwd":"/tmp","model":"anthropic/claude-3","thinking_level":"medium"}}"#).unwrap();
        writeln!(file, r#"{{"type":"message","timestamp":"2024-01-15T10:30"#).unwrap();
        writeln!(file, r#"{{"type":"message","timestamp":"2024-01-15T10:30:01Z","message":{{"role":"user","content":"Hello","timestamp":0}}}}"#).unwrap();

        let result = SessionReader::read_file(file.path());
        assert!(matches!(result, Err(SessionReadError::ParseError(_))));
    }

    #[test]
    fn newline_terminated_corrupt_last_line_still_fails() {
        let mut file = create_test_session();
        // Unlike a torn write, this corrupt line is newline-terminated, so it
        // is real data damage rather than a crash artifact and must fail.
        writeln!(file, "not-json").unwrap();
        file.flush().unwrap();

        let result = SessionReader::read_file(file.path());
        assert!(matches!(result, Err(SessionReadError::ParseError(_))));
    }
}

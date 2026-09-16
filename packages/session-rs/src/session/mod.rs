//! Session Management Module
//!
//! This module provides persistent session storage for the TUI application using the
//! JSONL (JSON Lines) format. Each session represents a conversation history with an
//! AI assistant, including messages, metadata, and configuration changes.
//!
//! # Architecture Overview
//!
//! Sessions are stored as append-only JSONL files where each line represents a single
//! event in the session timeline. This format enables efficient streaming writes and
//! partial reads without loading the entire session into memory.
//!
//! ## Session File Structure
//!
//! Session files are stored in `~/.composer/agent/sessions/<cwd-hash>/` with filenames
//! following the pattern: `YYYY-MM-DDTHH-MM-SS-FFFZ_<session-id>.jsonl`
//!
//! Example session file content:
//! ```jsonl
//! {"type":"session","id":"abc123","timestamp":"2024-01-15T10:30:00Z","cwd":"/tmp","model":"anthropic/claude-3","thinking_level":"medium"}
//! {"type":"message","timestamp":"2024-01-15T10:30:01Z","message":{"role":"user","content":"Hello"}}
//! {"type":"message","timestamp":"2024-01-15T10:30:02Z","message":{"role":"assistant","content":[{"type":"text","text":"Hi!"}]}}
//! {"type":"session_meta","timestamp":"2024-01-15T10:30:03Z","title":"Greeting","tags":["test"]}
//! ```
//!
//! ## JSONL Format Benefits
//!
//! - **Append-only**: New events are written as complete lines without modifying existing data
//! - **Crash-resistant**: Each line is a valid JSON object; partial writes only affect the last line
//! - **Streamable**: Files can be read line-by-line without loading everything into memory
//! - **Human-readable**: Easy to inspect and debug with standard text tools
//! - **Version-tolerant**: Unknown fields are ignored during deserialization
//!
//! # Module Organization
//!
//! - [`entries`]: Type definitions for all session entry variants (header, messages, metadata)
//! - [`reader`]: Functions for reading and parsing JSONL session files
//! - [`writer`]: Functions for creating and appending to session files
//! - [`manager`]: High-level API for listing, loading, and managing sessions
//!
//! # File I/O Patterns
//!
//! ## Writing Sessions
//!
//! Sessions use buffered writes with explicit flush control:
//! ```rust,ignore
//! let mut writer = SessionWriter::create(path, header)?;
//! writer.append_message(message)?;
//! writer.flush()?;  // Explicit flush for durability
//! ```
//!
//! ## Reading Sessions
//!
//! The reader provides two access patterns:
//!
//! 1. **Full read**: Loads entire session with statistics
//! ```rust,ignore
//! let session = SessionReader::read_file(path)?;
//! println!("Messages: {}", session.messages.len());
//! ```
//!
//! 2. **Header-only read**: Fast path for listing sessions
//! ```rust,ignore
//! let (header, stats, meta) = SessionReader::read_header(path)?;
//! println!("Session: {}", header.id);
//! ```
//!
//! # Rust Concepts Demonstrated
//!
//! ## Serde Serialization
//!
//! The module extensively uses `serde` for JSON serialization/deserialization:
//! - `#[serde(tag = "type")]`: Tagged enum representation for `SessionEntry`
//! - `#[serde(rename_all = "snake_case")]`: Automatic field name conversion
//! - `#[serde(skip_serializing_if = "Option::is_none")]`: Omit null fields from JSON
//! - `#[serde(default)]`: Use `Default::default()` for missing fields
//!
//! ## Iterator Patterns
//!
//! Reading sessions demonstrates Rust's iterator composition:
//! ```rust,ignore
//! for (line_num, line) in reader.lines().enumerate() {
//!     let line = line?;  // Propagate I/O errors
//!     let entry: SessionEntry = serde_json::from_str(&line)?;
//! }
//! ```
//!
//! ## Path Handling
//!
//! Uses `std::path::Path` and `PathBuf` for cross-platform path manipulation:
//! - `Path::extension()` to filter `.jsonl` files
//! - `PathBuf::join()` to build file paths
//! - `AsRef<Path>` trait for flexible path parameters
//!
//! ## Error Handling
//!
//! Custom error types with `From` trait implementations for ergonomic `?` operator:
//! - `SessionReadError`: Wraps I/O and parsing errors
//! - `SessionWriteError`: Wraps write-specific errors
//!
//! # Examples
//!
//! ## Creating and Writing a Session
//!
//! ```rust,ignore
//! use crate::session::{SessionManager, SessionHeader, ThinkingLevel};
//!
//! let mut manager = SessionManager::new("/home/user/project");
//! let header = SessionHeader {
//!     id: "abc123".into(),
//!     timestamp: "2024-01-15T10:30:00Z".into(),
//!     cwd: "/home/user/project".into(),
//!     model: "anthropic/claude-3".into(),
//!     thinking_level: ThinkingLevel::Medium,
//!     // ... other fields
//! };
//!
//! manager.start_session(header)?;
//! if let Some(writer) = manager.writer() {
//!     writer.append_user_message("Hello!")?;
//!     writer.flush()?;
//! }
//! ```
//!
//! ## Listing Recent Sessions
//!
//! ```rust,ignore
//! let manager = SessionManager::new("/home/user/project");
//! let recent = manager.recent_sessions(10)?;
//! for session in recent {
//!     println!("{}: {}", session.short_id(), session.title());
//! }
//! ```
//!
//! ## Loading a Session by ID
//!
//! ```rust,ignore
//! let manager = SessionManager::new("/home/user/project");
//! let session = manager.load_session("abc123")?;
//! for message in session.messages {
//!     println!("{}: {}", message.role(), message.text_content());
//! }
//! ```

mod branching;
mod entries;
mod export;
mod fork;
mod index;
mod manager;
mod model_history;
mod reader;
mod selective_summary;
pub mod tool_operations;
mod wire_format_generated;
mod writer;

pub use branching::{
    BranchId, BranchManager, BranchMetadata, BranchPoint, BranchSummary, MessageId,
};
pub use entries::*;
pub use export::{ExportFormat, ExportOptions, SessionExporter, export_session_file};
pub use fork::{
    ForkedSession, fork_session_file, fork_session_prefix, rewind_boundary,
    rewind_boundary_with_turn_count,
};
pub use index::{IndexedSession, SessionIndexEntry, collect_sessions, default_index_path};
pub use manager::{AuxiliaryCleanup, PreparedSessionAdoption, SessionInfo, SessionManager};
pub use model_history::model_history;
pub use reader::{
    LifecycleAgentNoteEntry, LifecycleNotificationEntry, ParsedSession, SessionReadError,
    SessionReader,
};
pub use selective_summary::{append_selective_summary_checkpoint, selective_summary_usage_entry};
pub use writer::SessionLock;
pub use writer::{
    SessionWriter, generate_session_filename, sanitize_path_for_dirname, sessions_dir,
};

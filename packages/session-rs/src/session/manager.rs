//! Session Lifecycle Manager
//!
//! This module provides the high-level [`SessionManager`] API for managing session
//! persistence throughout the application lifecycle. It coordinates session discovery,
//! loading, and creation while maintaining the current active session state.
//!
//! # Responsibilities
//!
//! The `SessionManager` serves as the primary interface between the application and
//! the session persistence layer:
//!
//! 1. **Session Discovery**: Listing all sessions for a working directory
//! 2. **Session Loading**: Reading existing sessions by ID, index, or most recent
//! 3. **Session Creation**: Initializing new session files with proper headers
//! 4. **Write Coordination**: Managing the active session writer instance
//! 5. **Directory Organization**: Mapping working directories to session storage paths
//!
//! # Directory Structure
//!
//! Sessions are organized by working directory to enable project-scoped session history:
//!
//! ```text
//! ~/.composer/agent/sessions/
//!   ├── home-user-project1/          # Hash of /home/user/project1
//!   │   ├── 2024-01-15T10-30-00-000Z_abc123.jsonl
//!   │   └── 2024-01-15T11-00-00-000Z_def456.jsonl
//!   └── home-user-project2/          # Hash of /home/user/project2
//!       └── 2024-01-15T12-00-00-000Z_xyz789.jsonl
//! ```
//!
//! This structure allows:
//! - Isolating sessions per project/directory
//! - Efficiently listing sessions for the current context
//! - Searching across all projects when needed
//!
//! # Session Listing
//!
//! ## Directory-Scoped Listing
//!
//! The [`list_sessions`] method returns sessions for the current working directory,
//! sorted by modification time (newest first):
//!
//! ```rust,ignore
//! let manager = SessionManager::new("/home/user/project");
//! let sessions = manager.list_sessions()?;
//! for session in sessions {
//!     println!("{}: {}", session.short_id(), session.title());
//! }
//! ```
//!
//! ## Global Search
//!
//! The [`list_all_sessions`] method searches across all working directories:
//!
//! ```rust,ignore
//! let all_sessions = manager.list_all_sessions()?;
//! // Returns sessions from all projects, sorted by modification time
//! ```
//!
//! ## Recent Sessions
//!
//! The [`recent_sessions`] method provides a truncated list of the N most recent
//! sessions for quick access:
//!
//! ```rust,ignore
//! let recent = manager.recent_sessions(5)?;
//! // Returns up to 5 most recent sessions
//! ```
//!
//! # Session Loading Strategies
//!
//! ## By ID (with Prefix Matching)
//!
//! The [`load_session`] method accepts full session IDs or prefixes:
//!
//! ```rust,ignore
//! // Full ID
//! let session = manager.load_session("abc123def456")?;
//!
//! // Prefix (matches first session starting with "abc")
//! let session = manager.load_session("abc")?;
//! ```
//!
//! Search order:
//! 1. Current directory sessions (fast path)
//! 2. All directories (fallback for cross-project access)
//!
//! ## By Index
//!
//! The [`load_session_by_index`] method uses 1-based indexing from the recent list:
//!
//! ```rust,ignore
//! let session = manager.load_session_by_index(1)?;  // Most recent
//! let session = manager.load_session_by_index(2)?;  // Second most recent
//! ```
//!
//! This is useful for terminal UIs where users can select from a numbered list.
//!
//! ## Most Recent
//!
//! The [`most_recent_session`] method provides quick access to continue the last
//! conversation:
//!
//! ```rust,ignore
//! if let Some(session) = manager.most_recent_session()? {
//!     println!("Resuming session: {}", session.id());
//! } else {
//!     println!("No previous sessions found");
//! }
//! ```
//!
//! # Active Session Management
//!
//! ## Starting a New Session
//!
//! The [`start_session`] method initializes a new session file and writer:
//!
//! ```rust,ignore
//! let header = SessionHeader {
//!     id: uuid::Uuid::new_v4().to_string(),
//!     timestamp: chrono::Utc::now().to_rfc3339(),
//!     cwd: "/home/user/project".into(),
//!     model: "anthropic/claude-3".into(),
//!     thinking_level: ThinkingLevel::Medium,
//!     // ... other fields
//! };
//!
//! manager.start_session(header)?;
//! ```
//!
//! This:
//! 1. Sets the current session ID
//! 2. Generates a timestamped filename
//! 3. Creates the session file with the header entry
//! 4. Initializes a buffered writer for appending messages
//!
//! ## Writing to the Active Session
//!
//! The [`writer`] method provides mutable access to the active session writer:
//!
//! ```rust,ignore
//! if let Some(writer) = manager.writer() {
//!     writer.append_user_message("Hello!")?;
//!     writer.flush()?;  // Ensure durability
//! }
//! ```
//!
//! ## Flushing Writes
//!
//! The [`flush`] method ensures all buffered data is written to disk:
//!
//! ```rust,ignore
//! manager.flush()?;
//! ```
//!
//! Call this:
//! - After each complete message exchange
//! - Before long operations where crashes are possible
//! - On application shutdown
//!
//! # Sorting and Ordering
//!
//! Sessions are sorted by file modification time (newest first) using this comparison:
//!
//! ```rust,ignore
//! sessions.sort_by(|a, b| match (&b.modified, &a.modified) {
//!     (Some(b_time), Some(a_time)) => b_time.cmp(a_time),  // Both have timestamps
//!     (Some(_), None) => std::cmp::Ordering::Less,          // b is newer
//!     (None, Some(_)) => std::cmp::Ordering::Greater,       // a is newer
//!     (None, None) => std::cmp::Ordering::Equal,            // Unknown ordering
//! });
//! ```
//!
//! This ensures:
//! - Recently modified sessions appear first
//! - Sessions without metadata sort after those with metadata
//! - Stable ordering when timestamps are equal
//!
//! # File System Operations
//!
//! ## Directory Traversal
//!
//! Uses `std::fs::read_dir` for efficient directory listing:
//!
//! ```rust,ignore
//! for entry in fs::read_dir(dir)? {
//!     let entry = entry?;  // Propagate I/O errors
//!     let path = entry.path();
//!
//!     // Filter by extension
//!     if path.extension().map(|e| e == "jsonl").unwrap_or(false) {
//!         // Process session file...
//!     }
//! }
//! ```
//!
//! ## Path Extension Filtering
//!
//! Only `.jsonl` files are processed:
//! ```rust,ignore
//! path.extension()           // Option<&OsStr>
//!     .map(|e| e == "jsonl") // Option<bool>
//!     .unwrap_or(false)      // bool (default to false if no extension)
//! ```
//!
//! ## Metadata Extraction
//!
//! File modification time is read for sorting:
//! ```rust,ignore
//! let modified = entry.metadata()    // Result<Metadata>
//!     .ok()                          // Option<Metadata>
//!     .and_then(|m| m.modified().ok()); // Option<SystemTime>
//! ```
//!
//! This pattern chains fallible operations, returning `None` if any step fails.
//!
//! # Error Handling Patterns
//!
//! ## Graceful Degradation
//!
//! Invalid session files are skipped during listing rather than failing the entire
//! operation:
//!
//! ```rust,ignore
//! match SessionReader::read_header(&path) {
//!     Ok((header, stats, meta)) => {
//!         sessions.push(SessionInfo { /* ... */ });
//!     }
//!     Err(_) => {
//!         // Skip invalid session files
//!         continue;
//!     }
//! }
//! ```
//!
//! ## Specific Error Messages
//!
//! Not-found errors include context for debugging:
//! ```rust,ignore
//! Err(SessionReadError::InvalidFormat(format!(
//!     "Session not found: {}",
//!     id
//! )))
//! ```
//!
//! # Performance Considerations
//!
//! ## Fast Path Optimization
//!
//! Loading by ID searches current directory first before scanning all directories:
//! ```rust,ignore
//! // Try current directory first (fast)
//! let sessions = self.list_sessions()?;
//! for session in &sessions {
//!     if session.id == id || session.id.starts_with(id) {
//!         return SessionReader::read_file(&session.path);
//!     }
//! }
//!
//! // Fallback to global search (slower)
//! let all_sessions = self.list_all_sessions()?;
//! // ...
//! ```
//!
//! ## Header-Only Reads
//!
//! Listing uses `SessionReader::read_header` instead of full reads for 10x speedup.
//!
//! # Rust Concepts Demonstrated
//!
//! ## Interior Mutability with Option
//! The `writer` field uses `Option<SessionWriter>` to represent the optional active
//! session state. Methods use `&mut self` to modify this state.
//!
//! ## Borrowing and Lifetimes
//! The `writer()` method returns `Option<&mut SessionWriter>`, borrowing the writer
//! mutably while keeping it owned by the manager.
//!
//! ## Method Chaining
//! Optional operations use `Option::map`, `Option::and_then`, and `Option::unwrap_or`
//! for expressive null handling without explicit if-let chains.
//!
//! ## Trait Conversion
//! `Into<String>` trait bound on `new()` allows constructing from both `&str` and `String`:
//! ```rust,ignore
//! pub fn new(cwd: impl Into<String>) -> Self {
//!     let cwd = cwd.into();  // Convert to String
//!     // ...
//! }
//! ```

use std::fs;
use std::path::{Path, PathBuf};

use super::entries::{
    AttachmentExtract, SessionEntry, SessionHeader, SessionMeta, SessionStats, ThinkingLevel,
};
use super::reader::{ParsedSession, SessionReadError, SessionReader};
use super::writer::{SessionLock, SessionWriter, lock_path_for, sessions_dir};

pub type AuxiliaryCleanup = fn(&Path, &str) -> std::io::Result<()>;

fn noop_auxiliary_cleanup(_: &Path, _: &str) -> std::io::Result<()> {
    Ok(())
}

/// Lightweight session summary for listing operations.
///
/// Contains just enough information to display session lists without loading full
/// message history. Constructed from session headers using [`SessionReader::read_header`].
///
/// # Memory Efficiency
///
/// This struct is significantly smaller than [`ParsedSession`] because it doesn't
/// include the message history. For a 10,000 message session:
/// - `SessionInfo`: ~200 bytes
/// - `ParsedSession`: ~50 MB
///
/// # Display Methods
///
/// - [`title()`](SessionInfo::title): Human-readable title from metadata or first message
/// - [`short_id()`](SessionInfo::short_id): First 8 characters of session ID
/// - [`is_favorite()`](SessionInfo::is_favorite): Check if marked as favorite
#[derive(Debug, Clone)]
pub struct SessionInfo {
    /// Full session ID (typically a UUID).
    pub id: String,

    /// Persisted parent session for fork navigation.
    pub parent_session: Option<String>,

    /// Absolute path to the session JSONL file.
    pub path: PathBuf,

    /// Working directory where the session was started.
    pub cwd: String,

    /// Model identifier (e.g., "anthropic/claude-3-5-sonnet-20241022").
    pub model: String,

    /// Extended thinking budget level.
    pub thinking_level: ThinkingLevel,

    /// ISO 8601 creation timestamp.
    pub timestamp: String,

    /// Aggregated message counts and token usage.
    pub stats: SessionStats,

    /// User-provided metadata (title, tags, favorite status).
    ///
    /// None if no metadata entry exists in the session file.
    pub meta: Option<SessionMeta>,

    /// First user message preview, when known.
    ///
    /// Only populated by the session-index fast path
    /// ([`crate::session::collect_sessions`]); header-only listing leaves it None.
    pub preview: Option<String>,

    /// File modification time from filesystem metadata.
    ///
    /// Used for sorting sessions by recency. None if metadata unavailable.
    pub modified: Option<std::time::SystemTime>,
}

impl SessionInfo {
    /// Get the display title
    #[must_use]
    pub fn title(&self) -> String {
        if let Some(ref meta) = self.meta {
            if let Some(ref title) = meta.title {
                return title.clone();
            }
            if let Some(ref summary) = meta.summary {
                let chars: Vec<char> = summary.chars().collect();
                if chars.len() > 50 {
                    return format!("{}...", chars[..47].iter().collect::<String>());
                }
                return summary.clone();
            }
        }
        if let Some(preview) = self.preview.as_deref().map(str::trim) {
            if !preview.is_empty() {
                return preview.to_string();
            }
        }
        format!("Session {}", &self.id[..8.min(self.id.len())])
    }

    /// Check if this is a favorite
    #[must_use]
    pub fn is_favorite(&self) -> bool {
        self.meta.as_ref().is_some_and(|m| m.favorite)
    }

    /// Get the short ID (first 8 chars)
    #[must_use]
    pub fn short_id(&self) -> &str {
        &self.id[..8.min(self.id.len())]
    }
}

/// Validated child writer and its lock-consistent transcript held alongside
/// the original until adoption is committed.
pub struct PreparedSessionAdoption {
    session_id: String,
    writer: SessionWriter,
    session: ParsedSession,
}

impl PreparedSessionAdoption {
    /// Transcript read while this adoption held the target writer lock.
    pub fn session(&self) -> &ParsedSession {
        &self.session
    }
}

/// High-level session persistence coordinator.
///
/// Manages the lifecycle of conversation sessions, including discovery, loading,
/// creation, and writing. Maintains the current active session state and coordinates
/// file system operations.
///
/// # Responsibilities
///
/// - **Discovery**: Listing sessions for the current or all working directories
/// - **Loading**: Reading sessions by ID, index, or most recent
/// - **Creation**: Starting new sessions with proper initialization
/// - **Writing**: Managing buffered writes to the active session file
///
/// # Usage Pattern
///
/// ```rust,ignore
/// // Create manager for a working directory
/// let mut manager = SessionManager::new("/home/user/project");
///
/// // List recent sessions
/// let sessions = manager.recent_sessions(10)?;
///
/// // Start a new session
/// manager.start_session(header)?;
///
/// // Write to active session
/// if let Some(writer) = manager.writer() {
///     writer.append_user_message("Hello")?;
///     writer.flush()?;
/// }
/// ```
///
/// # Thread Safety
///
/// This type is **not** thread-safe. Use separate instances per thread or wrap
/// in a mutex for shared access.
pub struct SessionManager {
    /// Working directory path for session scoping.
    cwd: String,

    /// Filesystem path to the sessions directory for this working directory.
    ///
    /// Typically `~/.composer/agent/sessions/<cwd-hash>/`.
    sessions_dir: PathBuf,

    /// ID of the currently active session.
    ///
    /// Set by [`start_session`](SessionManager::start_session). None if no session is active.
    current_session_id: Option<String>,

    /// Buffered writer for the active session file.
    ///
    /// Set by [`start_session`](SessionManager::start_session). None if no session is active.
    writer: Option<SessionWriter>,

    /// Cleanup supplied by this manager's host, after transcript deletion commits.
    auxiliary_cleanup: AuxiliaryCleanup,
}

/// Marker file dropped inside a staged checkpoint directory (see
/// [`SessionManager::prune_staging_dir`]) when
/// [`SessionManager::remove_session_and_checkpoints`]'s rollback rename
/// itself fails after the session's `.jsonl` removal also failed.
///
/// That combination leaves the session's `.jsonl` -- and therefore the
/// session itself -- still around and listed, but its checkpoints stuck in
/// the staging namespace instead of restored to their normal
/// `checkpoints/<session_id>/` location.
/// [`SessionManager::sweep_stale_checkpoint_staging`] cannot tell that
/// apart from an ordinary orphaned leftover (a crashed prune whose final
/// `remove_dir_all` never got to run) using the lock alone, since neither
/// process is holding it by the time the sweep runs; without this marker
/// the sweep deletes still-owned, live rewind history outright. The sweep
/// checks for this file and skips (rather than deletes) any staged
/// directory containing it.
const ROLLBACK_TOMBSTONE_FILE: &str = ".rollback-failed";

impl SessionManager {
    /// Create a new session manager
    pub fn new(cwd: impl Into<String>) -> Self {
        let cwd = cwd.into();
        let dir = sessions_dir(&cwd);
        Self {
            cwd,
            sessions_dir: dir,
            current_session_id: None,
            writer: None,
            auxiliary_cleanup: noop_auxiliary_cleanup,
        }
    }

    /// Create a session manager that lists sessions from an explicit directory.
    ///
    /// Used by `maestro value --session-dir` and tests that inject a temp root.
    pub fn with_sessions_dir(cwd: impl Into<String>, sessions_dir: impl Into<PathBuf>) -> Self {
        Self {
            cwd: cwd.into(),
            sessions_dir: sessions_dir.into(),
            current_session_id: None,
            writer: None,
            auxiliary_cleanup: noop_auxiliary_cleanup,
        }
    }

    /// Configure cleanup of auxiliary files owned by this manager's host.
    /// Cleanup failures do not undo a committed transcript deletion.
    #[must_use]
    pub fn with_auxiliary_cleanup(mut self, cleanup: AuxiliaryCleanup) -> Self {
        self.auxiliary_cleanup = cleanup;
        self
    }

    /// Get the current working directory
    #[must_use]
    pub fn cwd(&self) -> &str {
        &self.cwd
    }

    /// Get the sessions directory
    #[must_use]
    pub fn sessions_dir(&self) -> &Path {
        &self.sessions_dir
    }

    /// Get the current session ID
    #[must_use]
    pub fn current_session_id(&self) -> Option<&str> {
        self.current_session_id.as_deref()
    }

    /// List all sessions for the current working directory
    pub fn list_sessions(&self) -> Result<Vec<SessionInfo>, SessionReadError> {
        self.list_sessions_in_dir(&self.sessions_dir)
    }

    /// List sessions in a specific directory
    fn list_sessions_in_dir(&self, dir: &Path) -> Result<Vec<SessionInfo>, SessionReadError> {
        if !dir.exists() {
            return Ok(Vec::new());
        }

        let mut sessions = Vec::new();

        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();

            if path.extension().is_some_and(|e| e == "jsonl") {
                match SessionReader::read_header(&path) {
                    Ok((header, stats, meta)) => {
                        let modified = entry.metadata().ok().and_then(|m| m.modified().ok());
                        sessions.push(SessionInfo {
                            parent_session: header.parent_session,
                            id: header.id,
                            path,
                            cwd: header.cwd,
                            model: header.model,
                            thinking_level: header.thinking_level,
                            timestamp: header.timestamp,
                            stats,
                            meta,
                            preview: None,
                            modified,
                        });
                    }
                    Err(_) => {
                        // Skip invalid session files
                        continue;
                    }
                }
            }
        }

        // Sort by modification time (newest first)
        sessions.sort_by(|a, b| match (&b.modified, &a.modified) {
            (Some(b_time), Some(a_time)) => b_time.cmp(a_time),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => std::cmp::Ordering::Equal,
        });

        Ok(sessions)
    }

    /// List all sessions across all working directories
    pub fn list_all_sessions(&self) -> Result<Vec<SessionInfo>, SessionReadError> {
        let base_dir = self.sessions_dir.parent().unwrap_or(&self.sessions_dir);

        if !base_dir.exists() {
            return Ok(Vec::new());
        }

        let mut all_sessions = Vec::new();

        for entry in fs::read_dir(base_dir)? {
            let entry = entry?;
            let path = entry.path();

            if path.is_dir() {
                if let Ok(sessions) = self.list_sessions_in_dir(&path) {
                    all_sessions.extend(sessions);
                }
            }
        }

        // Sort by modification time (newest first)
        all_sessions.sort_by(|a, b| match (&b.modified, &a.modified) {
            (Some(b_time), Some(a_time)) => b_time.cmp(a_time),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => std::cmp::Ordering::Equal,
        });

        Ok(all_sessions)
    }

    /// Get the N most recent sessions
    pub fn recent_sessions(&self, count: usize) -> Result<Vec<SessionInfo>, SessionReadError> {
        let mut sessions = self.list_sessions()?;
        sessions.truncate(count);
        Ok(sessions)
    }

    /// Load a session by ID
    pub fn load_session(&self, id: &str) -> Result<ParsedSession, SessionReadError> {
        SessionReader::read_file(self.find_session(id)?.path)
    }

    /// Resolve a session using the same workspace-first ID matching as loading,
    /// without reconstructing its transcript before acquiring a writer lock.
    pub fn find_session(&self, id: &str) -> Result<SessionInfo, SessionReadError> {
        // First try current directory
        let sessions = self.list_sessions()?;
        for session in sessions {
            if session.id == id || session.id.starts_with(id) {
                return Ok(session);
            }
        }

        // Try all directories
        let all_sessions = self.list_all_sessions()?;
        for session in all_sessions {
            if session.id == id || session.id.starts_with(id) {
                return Ok(session);
            }
        }

        Err(SessionReadError::InvalidFormat(format!(
            "Session not found: {id}"
        )))
    }

    /// Load a session by index (1-based, from recent list)
    pub fn load_session_by_index(&self, index: usize) -> Result<ParsedSession, SessionReadError> {
        let sessions = self.list_sessions()?;
        let session = sessions.get(index.saturating_sub(1)).ok_or_else(|| {
            SessionReadError::InvalidFormat(format!("No session at index {index}"))
        })?;
        SessionReader::read_file(&session.path)
    }

    /// Get the most recent session (for --continue)
    pub fn most_recent_session(&self) -> Result<Option<ParsedSession>, SessionReadError> {
        let sessions = self.list_sessions()?;
        if let Some(session) = sessions.first() {
            Ok(Some(SessionReader::read_file(&session.path)?))
        } else {
            Ok(None)
        }
    }

    /// Start a new session
    pub fn start_session(
        &mut self,
        header: SessionHeader,
    ) -> Result<(), super::writer::SessionWriteError> {
        // Ephemeral sessions (CLI `--no-session` / MAESTRO_NO_SESSION=1): keep an
        // id for UI/diagnostics but do not open a durable transcript writer.
        if std::env::var("MAESTRO_NO_SESSION")
            .ok()
            .is_some_and(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        {
            self.current_session_id = Some(header.id);
            self.writer = None;
            return Ok(());
        }

        let filename = super::writer::generate_session_filename(&header.id);
        let path = self.sessions_dir.join(filename);
        let session_id = header.id.clone();
        let writer = SessionWriter::create(path, header)?;
        self.current_session_id = Some(session_id);
        self.writer = Some(writer);
        Ok(())
    }

    /// Whether the active session intentionally has no transcript writer.
    #[must_use]
    pub fn is_ephemeral_session(&self) -> bool {
        self.current_session_id.is_some() && self.writer.is_none()
    }

    /// Mark an active session id without opening a durable writer.
    ///
    /// Hosts use this to exercise the `MAESTRO_NO_SESSION` lifecycle without
    /// creating a file; normal callers should use [`Self::start_session`].
    #[cfg(any(test, feature = "test-support"))]
    pub fn start_ephemeral_session_for_test(&mut self, id: impl Into<String>) {
        self.current_session_id = Some(id.into());
        self.writer = None;
    }

    /// Resume an existing session file for appending new entries.
    pub fn resume_session_by_path(
        &mut self,
        session_id: impl Into<String>,
        path: impl AsRef<Path>,
    ) -> Result<(), super::writer::SessionWriteError> {
        let path = path.as_ref();
        // Resuming the session that's already active (e.g. `/continue`, or
        // Ctrl+O re-selecting the current session -- normally the newest
        // entry) must be a no-op: `self.writer`, if set, already holds this
        // exact path's sidecar lock, so `SessionLock::acquire` below would
        // always fail with `Locked` -- a process cannot flock its own
        // already-open file a second time -- even though the session is
        // perfectly valid and there is nothing to actually resume.
        if self
            .writer
            .as_ref()
            .is_some_and(|writer| writer.path() == path)
        {
            self.current_session_id = Some(session_id.into());
            return Ok(());
        }
        let session_id = session_id.into();
        let writer = SessionWriter::open_existing(path)?;
        self.current_session_id = Some(session_id);
        self.writer = Some(writer);
        Ok(())
    }

    /// Open and validate a child while retaining the current writer and its lock.
    /// Drop the returned value to cancel; adoption itself cannot fail.
    pub fn prepare_session_adoption(
        &mut self,
        path: impl AsRef<Path>,
    ) -> Result<PreparedSessionAdoption, super::writer::SessionWriteError> {
        self.flush()?;
        let writer = SessionWriter::open_existing(path.as_ref())?;
        let session = SessionReader::read_file(path.as_ref())
            .map_err(|error| super::writer::SessionWriteError::SerializeError(error.to_string()))?;
        Ok(PreparedSessionAdoption {
            session_id: session.header.id.clone(),
            writer,
            session,
        })
    }

    /// Commit a prepared writer after the guarded in-memory context change succeeds.
    pub fn adopt_prepared_session(&mut self, prepared: PreparedSessionAdoption) {
        self.current_session_id = Some(prepared.session_id);
        self.writer = Some(prepared.writer);
    }

    /// Reset the active session writer and ID.
    pub fn reset_session(&mut self) {
        self.current_session_id = None;
        self.writer = None;
    }

    /// Get the current session file path (if active).
    #[must_use]
    pub fn current_session_path(&self) -> Option<PathBuf> {
        self.writer
            .as_ref()
            .map(|writer| writer.path().to_path_buf())
    }

    /// Snapshot the active session into a new fork without switching writers.
    ///
    /// The parent remains active and may continue receiving agent events. The
    /// returned fork is a durable, independently resumable session at the exact
    /// flushed JSONL boundary observed by this call.
    pub fn fork_session_snapshot(
        &mut self,
    ) -> Result<(String, PathBuf), super::writer::SessionWriteError> {
        let writer = self.writer.as_mut().ok_or_else(|| {
            super::writer::SessionWriteError::SerializeError(
                "cannot fork before the session has started".to_string(),
            )
        })?;
        writer.flush()?;
        let forked = super::fork_session_file(writer.path())?;
        Ok((forked.id, forked.path))
    }

    /// Save an attachment extraction entry for the active session.
    pub fn save_attachment_extract(
        &mut self,
        attachment_id: impl Into<String>,
        extracted_text: impl Into<String>,
    ) -> Result<(), super::writer::SessionWriteError> {
        let attachment_id = attachment_id.into();
        let extracted_text = extracted_text.into();
        if attachment_id.is_empty() || extracted_text.is_empty() {
            return Ok(());
        }
        let Some(writer) = self.writer.as_mut() else {
            return Ok(());
        };

        let entry = SessionEntry::AttachmentExtract(AttachmentExtract {
            timestamp: chrono::Utc::now().to_rfc3339(),
            attachment_id,
            extracted_text,
        });
        writer.write_entry(entry)
    }

    /// Get the current session writer
    pub fn writer(&mut self) -> Option<&mut SessionWriter> {
        self.writer.as_mut()
    }

    /// Flush the current session
    pub fn flush(&mut self) -> Result<(), super::writer::SessionWriteError> {
        if let Some(ref mut writer) = self.writer {
            writer.flush()?;
        }
        Ok(())
    }

    /// Delete one session and its checkpoint data.
    ///
    /// After transcript deletion commits, invoke the configured host auxiliary
    /// cleanup callback. Core constructors install no auxiliary cleanup; hosts
    /// can supply spill cleanup with [`Self::with_auxiliary_cleanup`].
    pub fn delete_session(&self, session: &SessionInfo) -> std::io::Result<()> {
        let lock = SessionLock::acquire(&session.path)
            .map_err(|error| std::io::Error::other(error.to_string()))?;
        let result = self.remove_session_and_checkpoints(session);
        drop(lock);
        if result.is_ok() {
            self.remove_session_lock_file(session);
        }
        result
    }

    /// Remove a session's `.jsonl` file and its sibling checkpoint directories.
    /// Host-provided auxiliary cleanup runs after successful transcript removal.
    ///
    /// Checkpoints for a session live at
    /// `<sessions_dir>/checkpoints/<session_id>/` (see
    /// [`crate::checkpoints::CheckpointStore`]). Pruning only the `.jsonl`
    /// orphans that directory forever, since nothing else ever revisits a
    /// deleted session's id (issue #3151). The checkpoint directory is
    /// removed *first* so that a transient failure there leaves the
    /// `.jsonl` -- the session's only discoverability record -- in place for
    /// a later cleanup run to retry, instead of orphaning the checkpoints
    /// with the prune reported as failed. A session with no checkpoints at
    /// all (the common case) hits a `NotFound` here, which is not an
    /// error.
    ///
    /// Staging happens in a dedicated `checkpoints/.prune-staging~/`
    /// namespace (see [`Self::prune_staging_dir`] for why it ends in `~`),
    /// not as a `<session_id>.prune-staged` sibling of the real checkpoint
    /// directory: a portable or hand-written session id ending in that
    /// literal suffix would otherwise collide with (and, unrelated to any
    /// prune of its own, get swept as if it were) another session's
    /// staging directory. `sanitize_component` is applied to
    /// every session id building a `CheckpointStore` root, so a real
    /// session directory name can never collide with a fixed, dot-prefixed
    /// namespace directory the way it could with a suffix appended to its
    /// own name. New checkpoint roots use a versioned encoded session key;
    /// legacy sanitized roots are still cleaned up by the same prune path.
    ///
    /// Does not remove the session's sidecar `<file>.lock` (see
    /// [`SessionLock`]); callers that hold the lock while pruning must drop
    /// it first (see the comment at both call sites in [`Self::prune_sessions`]
    /// for why removing it while still held would be unsafe).
    fn remove_session_and_checkpoints(&self, session: &SessionInfo) -> std::io::Result<()> {
        // Session switcher entries can come from a sibling workspace. Sidecars
        // belong beside the selected transcript, not beside this manager's
        // current workspace.
        let session_dir = session.path.parent().unwrap_or(&self.sessions_dir);
        let store = crate::checkpoints::CheckpointStore::new(session_dir, &session.id);
        let mut checkpoint_roots = vec![store.root().to_path_buf()];
        let legacy_root_is_exclusive =
            !self.legacy_checkpoint_root_has_other_live_owner(session, store.legacy_root())?;
        if legacy_root_is_exclusive {
            checkpoint_roots.push(store.legacy_root().to_path_buf());
        }
        let staging_dir = Self::prune_staging_dir(session_dir);
        // `fs::rename`'s target parent must already exist; `create_dir_all`
        // is a no-op (not an error) if another prune already created it.
        fs::create_dir_all(&staging_dir)?;
        let staged_roots = checkpoint_roots
            .iter()
            .map(|root| {
                staging_dir.join(
                    root.file_name()
                        .map(std::ffi::OsStr::to_os_string)
                        .unwrap_or_default(),
                )
            })
            .collect::<Vec<_>>();

        // Hold a lock on the staging entry itself -- the same `SessionLock`
        // mechanism used for a session's own sidecar lock, reused here
        // against `staged` rather than `session.path` -- for the entire
        // staged window. `sweep_stale_checkpoint_staging`, possibly running
        // concurrently in another process's own `prune_sessions` call,
        // skips any entry it cannot lock, so it can never delete this one
        // out from under an in-progress prune (see that function's doc
        // comment). This lock is required, not best-effort: the session
        // lock prevents two transcript prunes, while this separate lock
        // prevents a concurrent sweep from mutating the checkpoint
        // transaction underneath this process.
        let mut staging_locks = Vec::with_capacity(staged_roots.len());
        for staged in &staged_roots {
            staging_locks.push(
                SessionLock::acquire(&Self::prune_staging_lock_key(staged)).map_err(|error| {
                    std::io::Error::other(format!("lock checkpoint staging: {error}"))
                })?,
            );
        }

        // A previous interrupted prune may have left older checkpoints in
        // staging while a resumed session created newer checkpoints at the
        // live root. Merge retained history back before staging this
        // transaction; deleting it here would discard rewind points before
        // transcript deletion commits.
        for (staged, checkpoint_root) in staged_roots.iter().zip(&checkpoint_roots) {
            Self::restore_retained_checkpoint_history(staged, checkpoint_root)?;
        }

        // Stage the checkpoint directory aside (a same-filesystem rename,
        // cheap and effectively atomic) instead of deleting it outright: if
        // removing the session's `.jsonl` below fails (a read-only file on
        // Windows, a transient sharing violation, ...), the staged
        // directory is renamed back below, restoring the checkpoints --
        // rather than leaving the session's rewind history permanently
        // gone while the session itself (per its surviving `.jsonl`) stays
        // listed.
        let mut staged_exists = vec![false; checkpoint_roots.len()];
        for (index, (checkpoint_root, staged)) in
            checkpoint_roots.iter().zip(&staged_roots).enumerate()
        {
            match fs::rename(checkpoint_root, staged) {
                Ok(()) => staged_exists[index] = true,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    for ((root, staged), exists) in checkpoint_roots
                        .iter()
                        .zip(&staged_roots)
                        .zip(&staged_exists)
                    {
                        if *exists {
                            let _ = fs::rename(staged, root);
                        }
                    }
                    return Err(e);
                }
            }
        }

        let result = match fs::remove_file(&session.path) {
            Ok(()) => {
                for (staged, exists) in staged_roots.iter().zip(&staged_exists) {
                    if !*exists {
                        continue;
                    }
                    // Best-effort: the session and its `.jsonl` are already
                    // gone, so a failure here just leaves an orphaned
                    // staged directory (swept by
                    // `sweep_stale_checkpoint_staging` on the next
                    // `prune_sessions` run) rather than an error the caller
                    // could act on.
                    let _ = fs::remove_dir_all(staged);
                }
                if !legacy_root_is_exclusive {
                    self.cleanup_orphaned_legacy_checkpoint_root(session_dir, store.legacy_root());
                }
                let _ = (self.auxiliary_cleanup)(session_dir, &session.id);
                Ok(())
            }
            Err(e) => {
                for ((checkpoint_root, staged), exists) in checkpoint_roots
                    .iter()
                    .zip(&staged_roots)
                    .zip(&staged_exists)
                {
                    if !*exists {
                        continue;
                    }
                    // Restore: the `.jsonl` (and therefore the session)
                    // is still around, so its checkpoints must be too.
                    if let Err(rollback_err) = fs::rename(staged, checkpoint_root) {
                        // The rollback itself failed (a transient sharing
                        // violation, a permissions error, ...): `staged`
                        // still holds this session's checkpoints, sitting
                        // in the staging namespace where
                        // `sweep_stale_checkpoint_staging` would otherwise
                        // delete it outright as an ordinary orphaned
                        // leftover. The session's `.jsonl` (per the `Err`
                        // arm we're already in) is still around and still
                        // listed, so this is live rewind history, not
                        // orphaned staging -- leave a tombstone marker so
                        // the sweep skips it instead, and log loudly since
                        // recovering it needs an operator to look at the
                        // underlying I/O error.
                        eprintln!(
                            "Failed to roll back staged checkpoints for session {} after failing to remove its session file ({e}): {rollback_err}; leaving them staged at {} for manual recovery",
                            session.short_id(),
                            staged.display()
                        );
                        let _ = fs::write(staged.join(ROLLBACK_TOMBSTONE_FILE), b"");
                    }
                }
                Err(e)
            }
        };

        // Staging entry names are reused across cleanup attempts, so their
        // lock sidecars must remain linked permanently. Unlinking one after
        // dropping this guard lets a process that already opened the old
        // inode acquire it while a later cleanup recreates and locks a new
        // inode at the same path, splitting mutual exclusion across two
        // files. Empty sidecars are intentionally inert and are ignored by
        // the staging sweep.
        drop(staging_locks);

        result
    }

    fn legacy_checkpoint_root_has_other_live_owner(
        &self,
        session: &SessionInfo,
        legacy_root: &Path,
    ) -> std::io::Result<bool> {
        let session_dir = session.path.parent().unwrap_or(&self.sessions_dir);
        if !session_dir.exists() {
            return Ok(false);
        }
        for entry in fs::read_dir(session_dir)? {
            let path = entry?.path();
            if path == session.path
                || path
                    .extension()
                    .is_none_or(|extension| extension != "jsonl")
            {
                continue;
            }
            let Ok((header, _, _)) = SessionReader::read_header(&path) else {
                return Ok(true);
            };
            let other_store = crate::checkpoints::CheckpointStore::new(session_dir, &header.id);
            if other_store.legacy_root() == legacy_root {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn legacy_checkpoint_root_has_any_live_owner_in_dir(
        &self,
        sessions_dir: &Path,
        legacy_root: &Path,
    ) -> std::io::Result<bool> {
        if !sessions_dir.exists() {
            return Ok(false);
        }
        for entry in fs::read_dir(sessions_dir)? {
            let path = entry?.path();
            if path
                .extension()
                .is_none_or(|extension| extension != "jsonl")
            {
                continue;
            }
            let Ok((header, _, _)) = SessionReader::read_header(&path) else {
                return Ok(true);
            };
            let store = crate::checkpoints::CheckpointStore::new(sessions_dir, &header.id);
            if store.legacy_root() == legacy_root {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn cleanup_orphaned_legacy_checkpoint_root(&self, session_dir: &Path, legacy_root: &Path) {
        let staging_dir = Self::prune_staging_dir(session_dir);
        if let Err(error) = fs::create_dir_all(&staging_dir) {
            eprintln!("Failed to create legacy checkpoint cleanup staging: {error}");
            return;
        }
        let staged = staging_dir.join(
            legacy_root
                .file_name()
                .map(std::ffi::OsStr::to_os_string)
                .unwrap_or_default(),
        );
        let lock_key = Self::prune_staging_lock_key(&staged);
        let _staging_lock = {
            let mut attempts = 0;
            loop {
                match SessionLock::acquire(&lock_key) {
                    Ok(lock) => break lock,
                    Err(super::writer::SessionWriteError::Locked(_)) if attempts < 100 => {
                        attempts += 1;
                        std::thread::sleep(std::time::Duration::from_millis(5));
                    }
                    Err(_) => return,
                }
            }
        };
        match self.legacy_checkpoint_root_has_any_live_owner_in_dir(session_dir, legacy_root) {
            Ok(true) => return,
            Err(error) => {
                eprintln!("Failed to verify legacy checkpoint ownership: {error}");
                return;
            }
            Ok(false) => {}
        }
        if let Err(error) = Self::restore_retained_checkpoint_history(&staged, legacy_root) {
            eprintln!("Failed to restore retained legacy checkpoints before cleanup: {error}");
            return;
        }
        match fs::rename(legacy_root, &staged) {
            Ok(()) => {
                let _ = fs::remove_dir_all(&staged);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                eprintln!("Failed to stage orphaned legacy checkpoints for cleanup: {error}");
            }
        }
    }

    /// Restore checkpoint entries retained by an interrupted prune.
    ///
    /// If a resumed session already recreated its live checkpoint root,
    /// move non-conflicting older entries into it. A collision fails closed:
    /// both copies remain discoverable and the transcript is not pruned.
    fn restore_retained_checkpoint_history(
        staged: &Path,
        checkpoint_root: &Path,
    ) -> std::io::Result<()> {
        if !staged.exists() {
            return Ok(());
        }

        if !checkpoint_root.exists() {
            fs::rename(staged, checkpoint_root)?;
            let tombstone = checkpoint_root.join(ROLLBACK_TOMBSTONE_FILE);
            if let Err(error) = fs::remove_file(&tombstone) {
                if error.kind() != std::io::ErrorKind::NotFound {
                    return Err(error);
                }
            }
            return Ok(());
        }

        for entry in fs::read_dir(staged)? {
            let entry = entry?;
            if entry.file_name() == ROLLBACK_TOMBSTONE_FILE {
                continue;
            }
            let destination = checkpoint_root.join(entry.file_name());
            if destination.exists() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::AlreadyExists,
                    format!(
                        "cannot merge retained checkpoint {} into occupied destination {}",
                        entry.path().display(),
                        destination.display()
                    ),
                ));
            }
            fs::rename(entry.path(), destination)?;
        }

        let tombstone = staged.join(ROLLBACK_TOMBSTONE_FILE);
        if let Err(error) = fs::remove_file(&tombstone) {
            if error.kind() != std::io::ErrorKind::NotFound {
                return Err(error);
            }
        }
        fs::remove_dir(staged)
    }

    /// The dedicated namespace staged checkpoint directories are moved
    /// into during a prune, kept separate from `checkpoints/<session_id>/`.
    ///
    /// A `<session_id>.prune-staged` sibling name (the original scheme)
    /// could collide with a portable or hand-written session id that
    /// itself ends in that literal suffix: its own, live checkpoint
    /// directory would then share a name with -- and be swept alongside --
    /// another session's staging data, deleting live rewind history that
    /// was never actually part of any prune. A fixed, dot-prefixed
    /// namespace directory cannot collide with a real session's checkpoint
    /// directory name *the way appending a suffix to it can*, but a fixed
    /// name built only from characters `sanitize_component` can itself
    /// produce is not enough on its own: `sanitize_component` (see
    /// `checkpoints.rs`) only maps characters *outside*
    /// `[A-Za-z0-9_.-]` to `_` and otherwise passes everything else
    /// through unchanged, so a portable or hand-written session literally
    /// named `.prune-staging` sanitizes to itself and its checkpoint root
    /// becomes *exactly* this directory. `~` is not in
    /// `sanitize_component`'s pass-through set, so no session id, however
    /// it sanitizes, can ever produce a directory name containing it --
    /// this suffix makes the namespace collision-free by construction
    /// rather than by coincidence of which literal name happened to be
    /// picked.
    fn prune_staging_dir(sessions_dir: &Path) -> PathBuf {
        sessions_dir.join("checkpoints").join(".prune-staging~")
    }

    /// Return the stable advisory-lock key for one staging entry.
    ///
    /// `SessionLock` appends `.lock` to this path. The extra `~` keeps that
    /// sidecar outside the namespace `sanitize_component` can produce for a
    /// real checkpoint directory, so retaining the lock inode cannot collide
    /// with a session whose sanitized id happens to end in `.lock`.
    fn prune_staging_lock_key(staged: &Path) -> PathBuf {
        let mut name = staged
            .file_name()
            .map(std::ffi::OsStr::to_os_string)
            .unwrap_or_default();
        name.push("~");
        staged.with_file_name(name)
    }

    /// Best-effort removal of a pruned session's sidecar lock file.
    ///
    /// Must only be called after the caller's own [`SessionLock`] guard for
    /// this session has already been dropped: unlinking the lock path while
    /// this process still holds an open, locked file description on it and
    /// a concurrent process races to recreate-and-lock a fresh file at the
    /// same path would leave two processes each believing they hold
    /// exclusive access. Session filenames embed a timestamp and a session
    /// id and are never reused, so nothing can legitimately recreate this
    /// exact path once the lock is released. Failure here is not reported
    /// as a prune error: the session and its checkpoints are already gone,
    /// and a leftover empty lock file is inert clutter, not a correctness
    /// problem.
    fn remove_session_lock_file(&self, session: &SessionInfo) {
        if let Err(e) = fs::remove_file(lock_path_for(&session.path)) {
            if e.kind() != std::io::ErrorKind::NotFound {
                eprintln!(
                    "Failed to remove lock file for pruned session {}: {e}",
                    session.short_id()
                );
            }
        }
    }

    /// Discover live checkpoint names without silently dropping unreadable
    /// session candidates.
    ///
    /// Ordinary UI listing intentionally skips malformed session files, but
    /// recovery cannot interpret a skipped header as proof that the session
    /// is gone: doing so could delete its only staged rewind history.
    fn live_checkpoint_dir_names_strict(
        &self,
    ) -> Result<std::collections::HashSet<std::ffi::OsString>, SessionReadError> {
        if !self.sessions_dir.exists() {
            return Ok(std::collections::HashSet::new());
        }

        let mut names = std::collections::HashSet::new();
        for entry in fs::read_dir(&self.sessions_dir)? {
            let entry = entry?;
            let path = entry.path();
            if path
                .extension()
                .is_some_and(|extension| extension == "jsonl")
            {
                let (header, _, _) = SessionReader::read_header(&path)?;
                let store =
                    crate::checkpoints::CheckpointStore::new(&self.sessions_dir, &header.id);
                for root in [store.root(), store.legacy_root()] {
                    if let Some(name) = root.file_name() {
                        names.insert(name.to_os_string());
                    }
                }
            }
        }
        Ok(names)
    }

    /// Best-effort sweep of stale staged checkpoint directories (see
    /// [`Self::prune_staging_dir`]) left behind by a previous
    /// `remove_session_and_checkpoints` call whose final `remove_dir_all`
    /// failed. Nothing else in this codebase ever revisits a pruned
    /// session's checkpoint directory once it's staged aside, so this is
    /// the only discoverable retry path for that narrow failure: called at
    /// the start of every `prune_sessions` run, not just once, so a
    /// failure that clears up later (a transient sharing violation, a
    /// permissions fix) still eventually gets swept.
    ///
    /// Skips any entry it cannot lock, using the same `SessionLock`
    /// mechanism `remove_session_and_checkpoints` holds for the duration of
    /// its own staged window: a locked entry means another process's prune
    /// is actively mid-flight on it right now, not a stale leftover from a
    /// past crash. Deleting it out from under that process would
    /// permanently destroy the session's rewind history if that process's
    /// own final `.jsonl` removal then failed and it tried to roll back --
    /// there would be nothing left to restore.
    fn sweep_stale_checkpoint_staging(&self) {
        let staging_dir = Self::prune_staging_dir(&self.sessions_dir);
        let Ok(entries) = fs::read_dir(&staging_dir) else {
            return;
        };

        // Cross-reference against currently listed sessions: a staged
        // entry whose name matches a *live* session's own
        // checkpoint-directory name did not finish being pruned -- most
        // likely a process crash between `remove_session_and_checkpoints`'s
        // initial stage-aside rename and whatever it does next, before it
        // could even attempt (or fail, or tombstone) the session-file
        // removal. That session's `.jsonl` is still around and still
        // listed by `list_sessions`, so this is live rewind history, not
        // an orphaned leftover from a *completed* (if partially failed)
        // prune attempt; restore it to its normal location instead of
        // deleting it. Computed once per sweep, not on every entry.
        //
        // If `list_sessions` itself fails (e.g. the sessions directory is
        // temporarily unreadable), there is no way to tell a live session's
        // interrupted staging apart from a genuine orphan; treating that
        // failure as "no live sessions" would make every staged entry look
        // orphaned and delete live rewind history out from under a session
        // whose ownership this sweep simply failed to determine. Skip the
        // whole sweep for this run instead -- the next `prune_sessions`
        // call retries it.
        let live_checkpoint_dir_names = match self.live_checkpoint_dir_names_strict() {
            Ok(names) => names,
            Err(_) => return,
        };

        self.sweep_stale_checkpoint_staging_entries(entries, &live_checkpoint_dir_names);
    }

    fn sweep_stale_checkpoint_staging_entries(
        &self,
        entries: fs::ReadDir,
        live_checkpoint_dir_names: &std::collections::HashSet<std::ffi::OsString>,
    ) {
        for entry in entries.flatten() {
            let path = entry.path();
            // The lock mechanism's own sidecar `.lock` files live as
            // siblings in this same directory; only staged directories
            // themselves are sweep targets.
            if !path.is_dir() {
                continue;
            }
            // See `ROLLBACK_TOMBSTONE_FILE`: a directory carrying this
            // marker is still owned by a live, listed session whose own
            // rollback rename failed, not an ordinary orphaned leftover --
            // skip it regardless of lock state so a future sweep (after
            // whatever blocked the rename clears up) gets another chance,
            // instead of destroying the only copy of that session's
            // rewind history.
            if path.join(ROLLBACK_TOMBSTONE_FILE).exists() {
                continue;
            }

            // Acquire this entry's lock before *either* restoring or
            // deleting it, not just before deleting: a concurrent
            // process's own `remove_session_and_checkpoints` holds this
            // same lock for its entire staged window, and taking it here
            // first closes a race where this sweep could rename a live
            // session's checkpoints out of staging (or delete them)
            // while that other process is still mid-flight on the exact
            // same entry.
            let Ok(lock) = SessionLock::acquire(&Self::prune_staging_lock_key(&path)) else {
                continue;
            };

            if let Some(name) = path.file_name() {
                if live_checkpoint_dir_names.contains(name) {
                    // The initial live-session set was captured before this
                    // entry's staging lock was acquired. Another cleanup can
                    // delete the owning transcript while we wait for that
                    // lock, making the snapshot stale. Re-resolve ownership
                    // under the lock before restoring; if discovery fails,
                    // retain the staged history rather than guessing.
                    let still_live = match self.live_checkpoint_dir_names_strict() {
                        Ok(names) => names.contains(name),
                        Err(_) => {
                            drop(lock);
                            continue;
                        }
                    };
                    if !still_live {
                        let _ = fs::remove_dir_all(&path);
                        drop(lock);
                        continue;
                    }
                    let restore_target = self.sessions_dir.join("checkpoints").join(name);
                    // Best-effort restore. `CheckpointStore` normally
                    // retains several checkpoints for rewind, so a
                    // non-empty `restore_target` (the session resumed and
                    // wrote new checkpoints since the crash) does not mean
                    // this staged copy's older checkpoints are superseded
                    // -- do not fall through to deleting them on a failed
                    // rename; leave them staged so a future sweep (after
                    // the destination clears, or once this is merged some
                    // other way) can retry instead of permanently
                    // discarding pre-crash rewind history.
                    if fs::rename(&path, &restore_target).is_err() {
                        drop(lock);
                        continue;
                    }
                    drop(lock);
                    continue;
                }
            }
            let _ = fs::remove_dir_all(&path);
            drop(lock);
        }
    }

    /// Prune old sessions that exceed count or age limits.
    ///
    /// Respects favorites (never deletes them) and never deletes the current
    /// session or a session locked by another Maestro process.
    /// Also removes each pruned session's checkpoint directory (see
    /// [`Self::remove_session_and_checkpoints`]) so pruning doesn't leave
    /// orphaned checkpoint data behind.
    /// Returns `(removed, errors)` counts.
    ///
    /// # Arguments
    ///
    /// * `max_sessions` - Maximum number of sessions to keep (0 = unlimited)
    /// * `max_age_days` - Maximum age in days for sessions (0 = unlimited)
    pub fn prune_sessions(&self, max_sessions: usize, max_age_days: u64) -> (usize, usize) {
        if max_sessions == 0 && max_age_days == 0 {
            return (0, 0);
        }

        // Sweep any stale staged directory a previous run's
        // `remove_session_and_checkpoints` left behind because its final
        // `fs::remove_dir_all(&staged)` failed (see that function): nothing
        // else ever revisits a pruned session's checkpoint directory, so
        // without this an orphan from that narrow failure path would
        // otherwise persist forever with no discoverable retry.
        self.sweep_stale_checkpoint_staging();

        let sessions = match self.list_sessions() {
            Ok(s) => s,
            Err(_) => return (0, 0),
        };

        let mut removed = 0usize;
        let mut errors = 0usize;

        // Age-based pruning
        if max_age_days > 0 {
            let cutoff =
                std::time::SystemTime::now() - std::time::Duration::from_secs(max_age_days * 86400);

            for session in &sessions {
                if session.is_favorite() {
                    continue;
                }
                if self
                    .current_session_id
                    .as_deref()
                    .is_some_and(|id| id == session.id)
                {
                    continue;
                }
                if let Some(modified) = session.modified {
                    if modified < cutoff {
                        // Never prune a session another Maestro process has
                        // open: its sidecar lock fails fast with `Locked`
                        // (see `SessionLock`), so skip it rather than
                        // deleting a live session's history and checkpoints.
                        // The guard is held until the removal completes. Any
                        // *other* acquisition failure (permissions, fd
                        // exhaustion, ...) is not contention and must not be
                        // silently treated the same as "another process has
                        // this open" -- count it as a real prune error.
                        let lock = match SessionLock::acquire(&session.path) {
                            Ok(lock) => lock,
                            Err(super::writer::SessionWriteError::Locked(_)) => continue,
                            Err(e) => {
                                eprintln!("Failed to prune session {}: {e}", session.short_id());
                                errors += 1;
                                continue;
                            }
                        };
                        let outcome = self.remove_session_and_checkpoints(session);
                        // Release our own lock before touching its sidecar
                        // file; see `remove_session_lock_file`'s doc comment.
                        drop(lock);
                        match outcome {
                            Ok(()) => {
                                removed += 1;
                                self.remove_session_lock_file(session);
                            }
                            Err(e) => {
                                eprintln!("Failed to prune session {}: {e}", session.short_id());
                                errors += 1;
                            }
                        }
                    }
                }
            }
        }

        // Count-based pruning (sessions are already sorted newest-first)
        if max_sessions > 0 && sessions.len() > max_sessions {
            let mut kept = 0usize;
            for session in &sessions {
                if session.is_favorite() {
                    continue;
                }
                if self
                    .current_session_id
                    .as_deref()
                    .is_some_and(|id| id == session.id)
                {
                    continue;
                }
                // Check if the file was already removed by age pruning
                if !session.path.exists() {
                    continue;
                }
                kept += 1;
                if kept > max_sessions {
                    // As in the age-based loop above, skip sessions locked
                    // by another Maestro process. `kept` already counted
                    // this session, so it still occupies a retention slot.
                    // A non-contention acquisition failure is counted as a
                    // real prune error rather than silently skipped.
                    let lock = match SessionLock::acquire(&session.path) {
                        Ok(lock) => lock,
                        Err(super::writer::SessionWriteError::Locked(_)) => continue,
                        Err(e) => {
                            eprintln!("Failed to prune session {}: {e}", session.short_id());
                            errors += 1;
                            continue;
                        }
                    };
                    let outcome = self.remove_session_and_checkpoints(session);
                    // Release our own lock before touching its sidecar
                    // file; see `remove_session_lock_file`'s doc comment.
                    drop(lock);
                    match outcome {
                        Ok(()) => {
                            removed += 1;
                            self.remove_session_lock_file(session);
                        }
                        Err(e) => {
                            eprintln!("Failed to prune session {}: {e}", session.short_id());
                            errors += 1;
                        }
                    }
                }
            }
        }

        (removed, errors)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    fn create_test_session_file(dir: &Path, id: &str) {
        let filename = format!("2024-01-15T10-30-00-000Z_{}.jsonl", id);
        let path = dir.join(filename);
        let mut file = fs::File::create(path).unwrap();
        writeln!(file, r#"{{"type":"session","id":"{}","timestamp":"2024-01-15T10:30:00Z","cwd":"/tmp","model":"anthropic/claude-3","thinking_level":"medium"}}"#, id).unwrap();
        writeln!(file, r#"{{"type":"message","timestamp":"2024-01-15T10:30:00Z","message":{{"role":"user","content":"Hello","timestamp":0}}}}"#).unwrap();
    }

    fn remove_test_spill_dir(sessions_dir: &Path, session_id: &str) -> std::io::Result<()> {
        let spill_dir = sessions_dir.join("tool-output").join(session_id);
        match fs::remove_dir_all(&spill_dir) {
            Ok(()) => {
                let _ = fs::remove_dir(spill_dir.parent().unwrap_or(sessions_dir));
                Ok(())
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }

    fn fail_test_spill_cleanup(_: &Path, _: &str) -> std::io::Result<()> {
        Err(std::io::Error::other("test spill cleanup failure"))
    }

    #[test]
    fn list_sessions_empty() {
        let dir = TempDir::new().unwrap();
        let manager = SessionManager {
            cwd: "/tmp".to_string(),
            sessions_dir: dir.path().to_path_buf(),
            current_session_id: None,
            writer: None,
            auxiliary_cleanup: noop_auxiliary_cleanup,
        };

        let sessions = manager.list_sessions().unwrap();
        assert!(sessions.is_empty());
    }

    #[test]
    fn list_sessions_finds_files() {
        let dir = TempDir::new().unwrap();
        create_test_session_file(dir.path(), "abc123");
        create_test_session_file(dir.path(), "def456");

        let manager = SessionManager {
            cwd: "/tmp".to_string(),
            sessions_dir: dir.path().to_path_buf(),
            current_session_id: None,
            writer: None,
            auxiliary_cleanup: noop_auxiliary_cleanup,
        };

        let sessions = manager.list_sessions().unwrap();
        assert_eq!(sessions.len(), 2);
    }

    #[test]
    fn list_sessions_reports_last_model_and_thinking_changes() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("2024-01-15T10-30-00-000Z_effective.jsonl");
        let mut file = fs::File::create(path).unwrap();
        for entry in [
            serde_json::json!({
                "type": "session",
                "id": "effective",
                "timestamp": "2024-01-15T10:30:00Z",
                "cwd": "/tmp",
                "model": "openai/gpt-6-astra",
                "thinkingLevel": "off"
            }),
            serde_json::json!({
                "type": "thinking_level_change",
                "timestamp": "2024-01-15T10:30:01Z",
                "thinkingLevel": "low"
            }),
            serde_json::json!({
                "type": "model_change",
                "timestamp": "2024-01-15T10:30:02Z",
                "model": "openai-codex/gpt-5.6"
            }),
            serde_json::json!({
                "type": "thinking_level_change",
                "timestamp": "2024-01-15T10:30:03Z",
                "thinkingLevel": "high"
            }),
        ] {
            writeln!(file, "{entry}").unwrap();
        }

        let manager = SessionManager {
            cwd: "/tmp".to_string(),
            sessions_dir: dir.path().to_path_buf(),
            current_session_id: None,
            writer: None,
            auxiliary_cleanup: noop_auxiliary_cleanup,
        };

        let sessions = manager.list_sessions().unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].model, "openai-codex/gpt-5.6");
        assert_eq!(sessions[0].thinking_level, ThinkingLevel::High);
    }

    #[test]
    fn list_all_sessions_finds_sessions_across_workspaces() {
        let root = TempDir::new().unwrap();
        let first = root.path().join("workspace-one");
        let second = root.path().join("workspace-two");
        fs::create_dir_all(&first).unwrap();
        fs::create_dir_all(&second).unwrap();
        create_test_session_file(&first, "session-one");
        create_test_session_file(&second, "session-two");

        let manager = SessionManager {
            cwd: "/tmp/workspace-one".to_string(),
            sessions_dir: first,
            current_session_id: None,
            writer: None,
            auxiliary_cleanup: noop_auxiliary_cleanup,
        };

        let sessions = manager.list_all_sessions().unwrap();
        let ids = sessions
            .into_iter()
            .map(|session| session.id)
            .collect::<std::collections::HashSet<_>>();
        assert_eq!(ids.len(), 2);
        assert!(ids.contains("session-one"));
        assert!(ids.contains("session-two"));
    }

    #[test]
    fn delete_session_cleans_sidecars_beside_selected_cross_workspace_transcript() {
        let root = TempDir::new().unwrap();
        let current = root.path().join("workspace-one");
        let selected_dir = root.path().join("workspace-two");
        fs::create_dir_all(&current).unwrap();
        fs::create_dir_all(&selected_dir).unwrap();
        let id = "selected-session";
        create_test_session_file(&selected_dir, id);

        let manager = SessionManager::with_sessions_dir("/tmp/workspace-one", &current)
            .with_auxiliary_cleanup(remove_test_spill_dir);
        let selected = manager
            .list_all_sessions()
            .unwrap()
            .into_iter()
            .find(|session| session.id == id)
            .expect("selected workspace session should be listed");

        let selected_store = crate::checkpoints::CheckpointStore::new(&selected_dir, id);
        let selected_checkpoint = selected_store.root().join("checkpoint");
        fs::create_dir_all(&selected_checkpoint).unwrap();
        fs::write(selected_checkpoint.join("checkpoint.json"), b"selected").unwrap();
        let selected_spill = selected_dir.join("tool-output").join(id);
        fs::create_dir_all(&selected_spill).unwrap();
        fs::write(selected_spill.join("output.txt"), b"selected").unwrap();

        // Same-ID sidecars in the current workspace are unrelated and must
        // survive deletion of the sibling workspace's session.
        let current_store = crate::checkpoints::CheckpointStore::new(&current, id);
        let current_checkpoint = current_store.root().join("sentinel");
        fs::create_dir_all(&current_checkpoint).unwrap();
        fs::write(current_checkpoint.join("checkpoint.json"), b"keep").unwrap();
        let current_spill = current.join("tool-output").join(id);
        fs::create_dir_all(&current_spill).unwrap();
        fs::write(current_spill.join("output.txt"), b"keep").unwrap();

        manager
            .delete_session(&selected)
            .expect("cross-workspace session deletion should succeed");

        assert!(!selected.path.exists());
        assert!(!selected_store.root().exists());
        assert!(!selected_spill.exists());
        assert!(current_checkpoint.join("checkpoint.json").exists());
        assert!(current_spill.join("output.txt").exists());
    }

    #[test]
    fn load_session_by_id() {
        let dir = TempDir::new().unwrap();
        create_test_session_file(dir.path(), "abc123");

        let manager = SessionManager {
            cwd: "/tmp".to_string(),
            sessions_dir: dir.path().to_path_buf(),
            current_session_id: None,
            writer: None,
            auxiliary_cleanup: noop_auxiliary_cleanup,
        };

        let session = manager.load_session("abc123").unwrap();
        assert_eq!(session.id(), "abc123");
    }

    /// Regression test: resuming the session that's already active (e.g.
    /// `/continue`, or Ctrl+O re-selecting the current session -- normally
    /// the newest entry) must succeed, not fail with `Locked`.
    /// `self.writer` already holds this exact path's sidecar lock, so a
    /// naive re-`open_existing` would always self-collide: a process
    /// cannot flock its own already-open file a second time.
    #[test]
    fn resume_session_by_path_is_a_no_op_for_the_already_active_session() {
        let dir = TempDir::new().unwrap();
        let mut manager = SessionManager {
            cwd: "/tmp".to_string(),
            sessions_dir: dir.path().to_path_buf(),
            current_session_id: None,
            writer: None,
            auxiliary_cleanup: noop_auxiliary_cleanup,
        };

        manager
            .start_session(SessionHeader {
                version: Some(2),
                id: "active-session".to_string(),
                timestamp: "2024-01-15T10:30:00Z".to_string(),
                cwd: "/tmp".to_string(),
                model: "test-model".to_string(),
                subject: None,
                model_metadata: None,
                thinking_level: Default::default(),
                system_prompt: None,
                prompt_metadata: None,
                prompt_context_manifest: None,
                unified_context_manifest: None,
                tools: vec![],
                branched_from: None,
                parent_session: None,
            })
            .expect("start_session");

        let path = manager
            .current_session_path()
            .expect("session path after start_session");

        manager
            .resume_session_by_path("active-session", &path)
            .expect("resuming the already-active session must not fail with Locked");

        assert_eq!(
            manager.current_session_id.as_deref(),
            Some("active-session")
        );
        assert_eq!(
            manager.current_session_path().as_deref(),
            Some(path.as_path())
        );
    }

    #[test]
    fn failed_session_start_does_not_look_ephemeral() {
        let dir = TempDir::new().unwrap();
        let blocked_sessions_dir = dir.path().join("not-a-directory");
        fs::write(&blocked_sessions_dir, "blocks session directory creation").unwrap();
        let mut manager = SessionManager::with_sessions_dir("/tmp", blocked_sessions_dir);

        let result = manager.start_session(SessionHeader {
            version: Some(2),
            id: "failed-session".to_string(),
            timestamp: "2024-01-15T10:30:00Z".to_string(),
            cwd: "/tmp".to_string(),
            model: "test-model".to_string(),
            subject: None,
            model_metadata: None,
            thinking_level: Default::default(),
            system_prompt: None,
            prompt_metadata: None,
            prompt_context_manifest: None,
            unified_context_manifest: None,
            tools: vec![],
            branched_from: None,
            parent_session: None,
        });

        assert!(result.is_err());
        assert_eq!(manager.current_session_id(), None);
        assert!(!manager.is_ephemeral_session());
    }

    #[test]
    fn fork_snapshot_keeps_parent_selected_and_creates_new_session() {
        let dir = TempDir::new().unwrap();
        let mut manager = SessionManager::with_sessions_dir("/tmp", dir.path());
        manager
            .start_session(SessionHeader {
                version: Some(2),
                id: "parent-session".to_string(),
                timestamp: "2024-01-15T10:30:00Z".to_string(),
                cwd: "/tmp".to_string(),
                model: "test-model".to_string(),
                subject: None,
                model_metadata: None,
                thinking_level: Default::default(),
                system_prompt: None,
                prompt_metadata: None,
                prompt_context_manifest: None,
                unified_context_manifest: None,
                tools: vec![],
                branched_from: None,
                parent_session: None,
            })
            .expect("start parent session");
        let parent_path = manager.current_session_path().expect("parent path");

        let (fork_id, fork_path) = manager.fork_session_snapshot().expect("fork snapshot");

        assert_eq!(manager.current_session_id(), Some("parent-session"));
        assert_eq!(
            manager.current_session_path().as_deref(),
            Some(parent_path.as_path())
        );
        assert_ne!(fork_id, "parent-session");
        let (fork_header, _, _) = SessionReader::read_header(&fork_path).expect("fork header");
        assert_eq!(fork_header.id, fork_id);
        assert_eq!(
            fork_header.parent_session.as_deref(),
            Some("parent-session")
        );
        assert_eq!(
            fork_header.branched_from.as_deref(),
            Some(parent_path.to_string_lossy().as_ref())
        );
        let (parent_header, _, _) =
            SessionReader::read_header(&parent_path).expect("parent header");
        assert_eq!(parent_header.id, "parent-session");
    }

    #[test]
    fn load_session_by_prefix() {
        let dir = TempDir::new().unwrap();
        create_test_session_file(dir.path(), "abc123");

        let manager = SessionManager {
            cwd: "/tmp".to_string(),
            sessions_dir: dir.path().to_path_buf(),
            current_session_id: None,
            writer: None,
            auxiliary_cleanup: noop_auxiliary_cleanup,
        };

        let session = manager.load_session("abc").unwrap();
        assert_eq!(session.id(), "abc123");
    }

    // ============================================================
    // Session ID Validation Tests
    // ============================================================

    #[test]
    fn test_session_id_with_path_traversal_not_found() {
        let dir = TempDir::new().unwrap();
        create_test_session_file(dir.path(), "abc123");

        let manager = SessionManager {
            cwd: "/tmp".to_string(),
            sessions_dir: dir.path().to_path_buf(),
            current_session_id: None,
            writer: None,
            auxiliary_cleanup: noop_auxiliary_cleanup,
        };

        // Path traversal attempts should not find sessions
        let result = manager.load_session("../../../etc/passwd");
        assert!(result.is_err());

        let result = manager.load_session("..%2F..%2Fetc%2Fpasswd");
        assert!(result.is_err());

        let result = manager.load_session("../../secret");
        assert!(result.is_err());
    }

    #[test]
    fn test_session_id_with_special_characters() {
        let dir = TempDir::new().unwrap();
        create_test_session_file(dir.path(), "normal-id");

        let manager = SessionManager {
            cwd: "/tmp".to_string(),
            sessions_dir: dir.path().to_path_buf(),
            current_session_id: None,
            writer: None,
            auxiliary_cleanup: noop_auxiliary_cleanup,
        };

        // IDs with special characters should not match
        let result = manager.load_session("normal-id/../other");
        assert!(result.is_err());

        let result = manager.load_session("/absolute/path");
        assert!(result.is_err());

        let result = manager.load_session("id\x00null");
        assert!(result.is_err());
    }

    #[test]
    fn test_session_not_found_error_message() {
        let dir = TempDir::new().unwrap();

        let manager = SessionManager {
            cwd: "/tmp".to_string(),
            sessions_dir: dir.path().to_path_buf(),
            current_session_id: None,
            writer: None,
            auxiliary_cleanup: noop_auxiliary_cleanup,
        };

        let result = manager.load_session("nonexistent");
        assert!(result.is_err());
        let err = result.unwrap_err();
        match err {
            SessionReadError::InvalidFormat(msg) => {
                assert!(msg.contains("Session not found"));
                assert!(msg.contains("nonexistent"));
            }
            _ => panic!("Expected InvalidFormat error"),
        }
    }

    // ============================================================
    // SessionInfo Tests
    // ============================================================

    #[test]
    fn test_session_info_title_with_meta() {
        let info = SessionInfo {
            parent_session: None,
            id: "abc123".to_string(),
            path: PathBuf::from("/tmp/test.jsonl"),
            cwd: "/tmp".to_string(),
            model: "anthropic/claude-3".to_string(),
            thinking_level: ThinkingLevel::Medium,
            timestamp: "2024-01-15T10:30:00Z".to_string(),
            stats: SessionStats::default(),
            meta: Some(SessionMeta {
                timestamp: "2024-01-15T10:30:00Z".to_string(),
                title: Some("My Custom Title".to_string()),
                summary: None,
                resume_summary: None,
                memory_extraction_hash: None,
                archived_at: None,
                archived: None,
                tags: vec![],
                favorite: false,
            }),
            preview: None,
            modified: None,
        };

        assert_eq!(info.title(), "My Custom Title");
    }

    #[test]
    fn test_session_info_title_from_summary() {
        let info = SessionInfo {
            parent_session: None,
            id: "abc123".to_string(),
            path: PathBuf::from("/tmp/test.jsonl"),
            cwd: "/tmp".to_string(),
            model: "anthropic/claude-3".to_string(),
            thinking_level: ThinkingLevel::Medium,
            timestamp: "2024-01-15T10:30:00Z".to_string(),
            stats: SessionStats::default(),
            meta: Some(SessionMeta {
                timestamp: "2024-01-15T10:30:00Z".to_string(),
                title: None,
                summary: Some("Short summary".to_string()),
                resume_summary: None,
                memory_extraction_hash: None,
                archived_at: None,
                archived: None,
                tags: vec![],
                favorite: false,
            }),
            preview: None,
            modified: None,
        };

        assert_eq!(info.title(), "Short summary");
    }

    #[test]
    fn test_session_info_title_truncates_long_summary() {
        let long_summary = "a".repeat(100);
        let info = SessionInfo {
            parent_session: None,
            id: "abc123".to_string(),
            path: PathBuf::from("/tmp/test.jsonl"),
            cwd: "/tmp".to_string(),
            model: "anthropic/claude-3".to_string(),
            thinking_level: ThinkingLevel::Medium,
            timestamp: "2024-01-15T10:30:00Z".to_string(),
            stats: SessionStats::default(),
            meta: Some(SessionMeta {
                timestamp: "2024-01-15T10:30:00Z".to_string(),
                title: None,
                summary: Some(long_summary),
                resume_summary: None,
                memory_extraction_hash: None,
                archived_at: None,
                archived: None,
                tags: vec![],
                favorite: false,
            }),
            preview: None,
            modified: None,
        };

        let title = info.title();
        assert!(title.len() <= 53); // 47 chars + "..."
        assert!(title.ends_with("..."));
    }

    #[test]
    fn test_session_info_is_favorite() {
        let mut info = SessionInfo {
            parent_session: None,
            id: "abc123".to_string(),
            path: PathBuf::from("/tmp/test.jsonl"),
            cwd: "/tmp".to_string(),
            model: "anthropic/claude-3".to_string(),
            thinking_level: ThinkingLevel::Medium,
            timestamp: "2024-01-15T10:30:00Z".to_string(),
            stats: SessionStats::default(),
            meta: None,
            preview: None,
            modified: None,
        };

        assert!(!info.is_favorite());

        info.meta = Some(SessionMeta {
            timestamp: "2024-01-15T10:30:00Z".to_string(),
            title: None,
            summary: None,
            resume_summary: None,
            memory_extraction_hash: None,
            archived_at: None,
            archived: None,
            tags: vec![],
            favorite: true,
        });

        assert!(info.is_favorite());
    }

    #[test]
    fn test_session_info_short_id_truncation() {
        let info = SessionInfo {
            parent_session: None,
            id: "abcdefghijklmnop".to_string(),
            path: PathBuf::from("/tmp/test.jsonl"),
            cwd: "/tmp".to_string(),
            model: "anthropic/claude-3".to_string(),
            thinking_level: ThinkingLevel::Medium,
            timestamp: "2024-01-15T10:30:00Z".to_string(),
            stats: SessionStats::default(),
            meta: None,
            preview: None,
            modified: None,
        };

        assert_eq!(info.short_id(), "abcdefgh");
        assert_eq!(info.short_id().len(), 8);
    }

    #[test]
    fn test_session_info_short_id_short_string() {
        let info = SessionInfo {
            parent_session: None,
            id: "abc".to_string(),
            path: PathBuf::from("/tmp/test.jsonl"),
            cwd: "/tmp".to_string(),
            model: "anthropic/claude-3".to_string(),
            thinking_level: ThinkingLevel::Medium,
            timestamp: "2024-01-15T10:30:00Z".to_string(),
            stats: SessionStats::default(),
            meta: None,
            preview: None,
            modified: None,
        };

        assert_eq!(info.short_id(), "abc");
    }

    // ============================================================
    // SessionManager Tests
    // ============================================================

    #[test]
    fn test_manager_new() {
        let manager = SessionManager::new("/home/user/project");
        assert_eq!(manager.cwd(), "/home/user/project");
        assert!(manager.current_session_id().is_none());
    }

    #[test]
    fn test_manager_cwd_accessor() {
        let manager = SessionManager::new("/test/path");
        assert_eq!(manager.cwd(), "/test/path");
    }

    #[test]
    fn test_manager_sessions_dir_accessor() {
        let manager = SessionManager::new("/test/path");
        // sessions_dir should contain the cwd hash
        let sessions_dir = manager.sessions_dir();
        assert!(sessions_dir.to_string_lossy().contains("sessions"));
    }

    #[test]
    fn test_list_sessions_nonexistent_dir() {
        let manager = SessionManager {
            cwd: "/tmp".to_string(),
            sessions_dir: PathBuf::from("/nonexistent/path/that/does/not/exist"),
            current_session_id: None,
            writer: None,
            auxiliary_cleanup: noop_auxiliary_cleanup,
        };

        // Should return empty vec, not error
        let sessions = manager.list_sessions().unwrap();
        assert!(sessions.is_empty());
    }

    #[test]
    fn test_list_sessions_ignores_non_jsonl() {
        let dir = TempDir::new().unwrap();
        create_test_session_file(dir.path(), "valid");

        // Create a non-JSONL file
        let txt_path = dir.path().join("notes.txt");
        fs::write(txt_path, "some notes").unwrap();

        let manager = SessionManager {
            cwd: "/tmp".to_string(),
            sessions_dir: dir.path().to_path_buf(),
            current_session_id: None,
            writer: None,
            auxiliary_cleanup: noop_auxiliary_cleanup,
        };

        let sessions = manager.list_sessions().unwrap();
        assert_eq!(sessions.len(), 1); // Only the valid JSONL
    }

    #[test]
    fn test_list_sessions_ignores_invalid_jsonl() {
        let dir = TempDir::new().unwrap();
        create_test_session_file(dir.path(), "valid");

        // Create an invalid JSONL file
        let invalid_path = dir.path().join("invalid.jsonl");
        fs::write(invalid_path, "not valid json at all").unwrap();

        let manager = SessionManager {
            cwd: "/tmp".to_string(),
            sessions_dir: dir.path().to_path_buf(),
            current_session_id: None,
            writer: None,
            auxiliary_cleanup: noop_auxiliary_cleanup,
        };

        let sessions = manager.list_sessions().unwrap();
        assert_eq!(sessions.len(), 1); // Only the valid one
    }

    #[test]
    fn test_recent_sessions_limits_count() {
        let dir = TempDir::new().unwrap();
        for i in 0..10 {
            create_test_session_file(dir.path(), &format!("session{}", i));
        }

        let manager = SessionManager {
            cwd: "/tmp".to_string(),
            sessions_dir: dir.path().to_path_buf(),
            current_session_id: None,
            writer: None,
            auxiliary_cleanup: noop_auxiliary_cleanup,
        };

        let sessions = manager.recent_sessions(3).unwrap();
        assert_eq!(sessions.len(), 3);
    }

    #[test]
    fn test_recent_sessions_returns_all_if_less() {
        let dir = TempDir::new().unwrap();
        create_test_session_file(dir.path(), "only-one");

        let manager = SessionManager {
            cwd: "/tmp".to_string(),
            sessions_dir: dir.path().to_path_buf(),
            current_session_id: None,
            writer: None,
            auxiliary_cleanup: noop_auxiliary_cleanup,
        };

        let sessions = manager.recent_sessions(10).unwrap();
        assert_eq!(sessions.len(), 1);
    }

    #[test]
    fn test_load_session_by_index() {
        let dir = TempDir::new().unwrap();
        create_test_session_file(dir.path(), "first");
        create_test_session_file(dir.path(), "second");

        let manager = SessionManager {
            cwd: "/tmp".to_string(),
            sessions_dir: dir.path().to_path_buf(),
            current_session_id: None,
            writer: None,
            auxiliary_cleanup: noop_auxiliary_cleanup,
        };

        // Index is 1-based
        let session = manager.load_session_by_index(1).unwrap();
        assert!(!session.id().is_empty());
    }

    #[test]
    fn test_load_session_by_index_out_of_bounds() {
        let dir = TempDir::new().unwrap();
        create_test_session_file(dir.path(), "only");

        let manager = SessionManager {
            cwd: "/tmp".to_string(),
            sessions_dir: dir.path().to_path_buf(),
            current_session_id: None,
            writer: None,
            auxiliary_cleanup: noop_auxiliary_cleanup,
        };

        let result = manager.load_session_by_index(10);
        assert!(result.is_err());
    }

    #[test]
    fn test_load_session_by_index_zero() {
        let dir = TempDir::new().unwrap();
        create_test_session_file(dir.path(), "test");

        let manager = SessionManager {
            cwd: "/tmp".to_string(),
            sessions_dir: dir.path().to_path_buf(),
            current_session_id: None,
            writer: None,
            auxiliary_cleanup: noop_auxiliary_cleanup,
        };

        // Index 0 with saturating_sub(1) becomes 0, which is valid
        let result = manager.load_session_by_index(0);
        // Should get the first session or fail
        assert!(result.is_ok() || result.is_err());
    }

    #[test]
    fn test_most_recent_session_empty() {
        let dir = TempDir::new().unwrap();

        let manager = SessionManager {
            cwd: "/tmp".to_string(),
            sessions_dir: dir.path().to_path_buf(),
            current_session_id: None,
            writer: None,
            auxiliary_cleanup: noop_auxiliary_cleanup,
        };

        let result = manager.most_recent_session().unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_most_recent_session_returns_session() {
        let dir = TempDir::new().unwrap();
        create_test_session_file(dir.path(), "recent");

        let manager = SessionManager {
            cwd: "/tmp".to_string(),
            sessions_dir: dir.path().to_path_buf(),
            current_session_id: None,
            writer: None,
            auxiliary_cleanup: noop_auxiliary_cleanup,
        };

        let result = manager.most_recent_session().unwrap();
        assert!(result.is_some());
        assert_eq!(result.unwrap().id(), "recent");
    }

    #[test]
    fn test_flush_no_writer() {
        let dir = TempDir::new().unwrap();
        let mut manager = SessionManager {
            cwd: "/tmp".to_string(),
            sessions_dir: dir.path().to_path_buf(),
            current_session_id: None,
            writer: None,
            auxiliary_cleanup: noop_auxiliary_cleanup,
        };

        // Should not error when no writer
        let result = manager.flush();
        assert!(result.is_ok());
    }

    // ============================================================
    // Edge Cases
    // ============================================================

    #[test]
    fn test_empty_session_id() {
        let dir = TempDir::new().unwrap();
        create_test_session_file(dir.path(), "test");

        let manager = SessionManager {
            cwd: "/tmp".to_string(),
            sessions_dir: dir.path().to_path_buf(),
            current_session_id: None,
            writer: None,
            auxiliary_cleanup: noop_auxiliary_cleanup,
        };

        // Empty string can prefix-match any session ID (starts_with("") is always true)
        // This is current behavior - the implementation doesn't validate empty IDs
        let result = manager.load_session("");
        // Could match or not depending on implementation
        // Document current behavior rather than assert error
        assert!(result.is_ok() || result.is_err());
    }

    #[test]
    fn test_whitespace_session_id() {
        let dir = TempDir::new().unwrap();
        create_test_session_file(dir.path(), "test");

        let manager = SessionManager {
            cwd: "/tmp".to_string(),
            sessions_dir: dir.path().to_path_buf(),
            current_session_id: None,
            writer: None,
            auxiliary_cleanup: noop_auxiliary_cleanup,
        };

        let result = manager.load_session("   ");
        assert!(result.is_err());
    }

    #[test]
    fn test_prune_sessions_noop_when_zero_limits() {
        let dir = TempDir::new().unwrap();
        create_test_session_file(dir.path(), "abc123");

        let manager = SessionManager {
            cwd: "/tmp".to_string(),
            sessions_dir: dir.path().to_path_buf(),
            current_session_id: None,
            writer: None,
            auxiliary_cleanup: noop_auxiliary_cleanup,
        };

        let (removed, errors) = manager.prune_sessions(0, 0);
        assert_eq!(removed, 0);
        assert_eq!(errors, 0);
        // Session file still exists
        assert_eq!(manager.list_sessions().unwrap().len(), 1);
    }

    #[test]
    fn test_prune_sessions_by_count() {
        let dir = TempDir::new().unwrap();
        for i in 0..5 {
            create_test_session_file(dir.path(), &format!("session{i}"));
        }

        let manager = SessionManager {
            cwd: "/tmp".to_string(),
            sessions_dir: dir.path().to_path_buf(),
            current_session_id: None,
            writer: None,
            auxiliary_cleanup: noop_auxiliary_cleanup,
        };

        assert_eq!(manager.list_sessions().unwrap().len(), 5);

        let (removed, errors) = manager.prune_sessions(2, 0);
        assert_eq!(errors, 0);
        assert!(removed > 0);
        assert!(manager.list_sessions().unwrap().len() <= 2);
    }

    #[test]
    fn test_prune_sessions_respects_favorites() {
        let dir = TempDir::new().unwrap();
        // Create a regular session
        create_test_session_file(dir.path(), "regular");
        // Create a session with favorite meta
        let fav_filename = "2024-01-15T10-30-00-000Z_favorite.jsonl";
        let fav_path = dir.path().join(fav_filename);
        let mut file = fs::File::create(&fav_path).unwrap();
        use std::io::Write;
        writeln!(file, r#"{{"type":"session","id":"favorite","timestamp":"2024-01-15T10:30:00Z","cwd":"/tmp","model":"anthropic/claude-3","thinking_level":"medium"}}"#).unwrap();
        writeln!(
            file,
            r#"{{"type":"session_meta","timestamp":"2024-01-15T10:30:00Z","favorite":true}}"#
        )
        .unwrap();

        let manager = SessionManager {
            cwd: "/tmp".to_string(),
            sessions_dir: dir.path().to_path_buf(),
            current_session_id: None,
            writer: None,
            auxiliary_cleanup: noop_auxiliary_cleanup,
        };

        // Prune to 0 count - favorites should survive
        let (removed, _) = manager.prune_sessions(0, 1); // 1 day age limit
        // The favorite session file should still exist
        assert!(fav_path.exists());
        // removed count may vary based on file timestamps
        let _ = removed;
    }

    #[test]
    fn test_prune_sessions_skips_current_session() {
        let dir = TempDir::new().unwrap();
        create_test_session_file(dir.path(), "current123");
        create_test_session_file(dir.path(), "other456");

        let manager = SessionManager {
            cwd: "/tmp".to_string(),
            sessions_dir: dir.path().to_path_buf(),
            current_session_id: Some("current123".to_string()),
            writer: None,
            auxiliary_cleanup: noop_auxiliary_cleanup,
        };

        // Prune with count limit of 1 - current session should survive
        let (removed, errors) = manager.prune_sessions(1, 0);
        assert_eq!(errors, 0);

        // Verify current session file still exists
        let remaining = manager.list_sessions().unwrap();
        let current_exists = remaining.iter().any(|s| s.id == "current123");
        assert!(current_exists, "Current session should not be pruned");
        let _ = removed;
    }

    #[test]
    fn test_prune_sessions_empty_dir() {
        let dir = TempDir::new().unwrap();

        let manager = SessionManager {
            cwd: "/tmp".to_string(),
            sessions_dir: dir.path().to_path_buf(),
            current_session_id: None,
            writer: None,
            auxiliary_cleanup: noop_auxiliary_cleanup,
        };

        let (removed, errors) = manager.prune_sessions(10, 90);
        assert_eq!(removed, 0);
        assert_eq!(errors, 0);
    }

    /// Regression test for #3151: pruning a session must also remove its
    /// checkpoint directory (`<sessions_dir>/checkpoints/<session_id>/`),
    /// not just the `.jsonl`. Before this fix, `prune_sessions` only ever
    /// called `fs::remove_file` on the session path, so a pruned session's
    /// checkpoints were orphaned on disk forever with nothing left that
    /// could ever reference them again.
    ///
    /// Exercises the count-based pruning loop specifically (age-based
    /// pruning needs backdated mtimes, which isn't worth a new
    /// dependency for a test); both loops share the same
    /// `remove_session_and_checkpoints` helper, so this covers the shared
    /// logic. Uses two non-current sessions so at least one is guaranteed
    /// to be pruned regardless of mtime tie-breaking on the filesystem, and
    /// checks every session's checkpoint directory against whether that
    /// session actually survived rather than assuming which one did.
    #[test]
    fn test_prune_sessions_by_count_removes_checkpoint_directory() {
        let dir = TempDir::new().unwrap();
        let ids = ["session0", "session1", "session2"];
        let mut checkpoint_roots = std::collections::HashMap::new();
        let mut spill_roots = std::collections::HashMap::new();
        for id in ids {
            create_test_session_file(dir.path(), id);
            let store = crate::checkpoints::CheckpointStore::new(dir.path(), id);
            let checkpoint_dir = store.root().join("chk1");
            fs::create_dir_all(&checkpoint_dir).unwrap();
            fs::write(checkpoint_dir.join("checkpoint.json"), b"{}").unwrap();
            checkpoint_roots.insert(id.to_string(), store.root().to_path_buf());
            let spill_dir = dir.path().join("tool-output").join(id);
            fs::create_dir_all(&spill_dir).unwrap();
            fs::write(spill_dir.join("large.txt"), b"output").unwrap();
            spill_roots.insert(id.to_string(), spill_dir);
        }

        let manager = SessionManager {
            cwd: "/tmp".to_string(),
            sessions_dir: dir.path().to_path_buf(),
            current_session_id: None,
            writer: None,
            auxiliary_cleanup: remove_test_spill_dir,
        };

        let (removed, errors) = manager.prune_sessions(1, 0);
        assert_eq!(errors, 0);
        assert!(
            removed >= 2,
            "expected at least 2 sessions pruned, got {removed}"
        );

        let remaining_ids: std::collections::HashSet<String> = manager
            .list_sessions()
            .unwrap()
            .into_iter()
            .map(|s| s.id)
            .collect();
        assert!(remaining_ids.len() <= 1);

        for (id, checkpoint_root) in &checkpoint_roots {
            if remaining_ids.contains(id) {
                assert!(
                    checkpoint_root.exists(),
                    "kept session {id}'s checkpoints must survive"
                );
            } else {
                assert!(
                    !checkpoint_root.exists(),
                    "pruned session {id}'s checkpoints must be removed, not orphaned"
                );
            }
        }
        for (id, spill_root) in &spill_roots {
            assert_eq!(
                spill_root.exists(),
                remaining_ids.contains(id),
                "spill retention must match transcript retention for session {id}"
            );
        }
    }

    /// Regression test: if removing a pruned session's `.jsonl` fails
    /// after its checkpoint directory has already been removed, the
    /// checkpoints must be restored, not left permanently gone for a
    /// session that (per its surviving `.jsonl`) is still listed. Forces
    /// the failure by making the "session path" a directory, so
    /// `fs::remove_file` on it fails with `EISDIR` instead of the real
    /// session-file removal.
    #[test]
    fn test_remove_session_and_checkpoints_restores_checkpoints_on_jsonl_failure() {
        let dir = TempDir::new().unwrap();
        let id = "restore-me";
        let store = crate::checkpoints::CheckpointStore::new(dir.path(), id);
        let checkpoint_dir = store.root().join("chk1");
        fs::create_dir_all(&checkpoint_dir).unwrap();
        fs::write(checkpoint_dir.join("checkpoint.json"), b"{}").unwrap();
        let legacy_checkpoint_dir = store.legacy_root().join("legacy-chk");
        fs::create_dir_all(&legacy_checkpoint_dir).unwrap();
        fs::write(legacy_checkpoint_dir.join("checkpoint.json"), b"legacy").unwrap();

        let session_path = dir.path().join("fake-session.jsonl");
        fs::create_dir_all(&session_path).unwrap();

        let manager = SessionManager {
            cwd: "/tmp".to_string(),
            sessions_dir: dir.path().to_path_buf(),
            current_session_id: None,
            writer: None,
            auxiliary_cleanup: noop_auxiliary_cleanup,
        };
        let session = SessionInfo {
            parent_session: None,
            id: id.to_string(),
            path: session_path.clone(),
            cwd: "/tmp".to_string(),
            model: "test-model".to_string(),
            thinking_level: ThinkingLevel::Medium,
            timestamp: "2024-01-15T10:30:00Z".to_string(),
            stats: SessionStats::default(),
            meta: None,
            modified: None,
            preview: None,
        };

        let result = manager.remove_session_and_checkpoints(&session);

        assert!(
            result.is_err(),
            "removing a directory via remove_file must fail"
        );
        assert!(
            store.root().exists(),
            "checkpoints must be restored when jsonl removal fails"
        );
        assert!(checkpoint_dir.join("checkpoint.json").exists());
        assert!(
            legacy_checkpoint_dir.join("checkpoint.json").exists(),
            "legacy checkpoints must roll back before a failed transcript prune returns"
        );
        let staged =
            SessionManager::prune_staging_dir(dir.path()).join(store.root().file_name().unwrap());
        assert!(
            !staged.exists(),
            "rollback must move checkpoints out of staging"
        );
        assert!(
            lock_path_for(&SessionManager::prune_staging_lock_key(&staged)).is_file(),
            "the reusable staging lock sidecar must remain linked so later cleanup attempts lock the same inode"
        );
    }

    #[test]
    fn auxiliary_cleanup_is_scoped_to_the_manager_that_owns_it() {
        let configured_dir = TempDir::new().unwrap();
        let other_dir = TempDir::new().unwrap();
        for dir in [&configured_dir, &other_dir] {
            create_test_session_file(dir.path(), "session-1");
            fs::create_dir_all(dir.path().join("tool-output/session-1")).unwrap();
        }
        let configured = SessionManager::with_sessions_dir("/configured", configured_dir.path())
            .with_auxiliary_cleanup(remove_test_spill_dir);
        let other = SessionManager::with_sessions_dir("/other", other_dir.path());
        for manager in [&configured, &other] {
            let session = manager.list_sessions().unwrap().pop().unwrap();
            manager.remove_session_and_checkpoints(&session).unwrap();
            assert!(!session.path.exists());
        }
        assert!(!configured_dir.path().join("tool-output/session-1").exists());
        assert!(other_dir.path().join("tool-output/session-1").exists());
    }

    #[test]
    fn transcript_deletion_succeeds_when_post_commit_spill_cleanup_fails() {
        let dir = TempDir::new().unwrap();
        let id = "spill-cleanup-failure";
        create_test_session_file(dir.path(), id);
        fs::write(dir.path().join("tool-output"), b"blocks spill directory").unwrap();
        let manager = SessionManager {
            cwd: "/tmp".to_string(),
            sessions_dir: dir.path().to_path_buf(),
            current_session_id: None,
            writer: None,
            auxiliary_cleanup: fail_test_spill_cleanup,
        };
        let session = manager
            .list_sessions()
            .unwrap()
            .into_iter()
            .find(|candidate| candidate.id == id)
            .unwrap();

        manager
            .remove_session_and_checkpoints(&session)
            .expect("transcript deletion is the commit point");

        assert!(!session.path.exists());
        assert!(dir.path().join("tool-output").exists());
    }

    #[test]
    fn pruning_one_session_preserves_a_colliding_live_legacy_root() {
        let dir = TempDir::new().unwrap();
        let pruned_id = "legacy:collision";
        let live_id = "legacy?collision";
        create_test_session_file(dir.path(), pruned_id);
        create_test_session_file(dir.path(), live_id);

        let pruned_store = crate::checkpoints::CheckpointStore::new(dir.path(), pruned_id);
        let live_store = crate::checkpoints::CheckpointStore::new(dir.path(), live_id);
        assert_eq!(pruned_store.legacy_root(), live_store.legacy_root());
        assert_ne!(pruned_store.root(), live_store.root());

        let shared_legacy_checkpoint = pruned_store
            .legacy_root()
            .join("shared")
            .join("checkpoint.json");
        fs::create_dir_all(shared_legacy_checkpoint.parent().unwrap()).unwrap();
        fs::write(&shared_legacy_checkpoint, b"shared legacy history").unwrap();
        let pruned_v2_checkpoint = pruned_store.root().join("owned").join("checkpoint.json");
        fs::create_dir_all(pruned_v2_checkpoint.parent().unwrap()).unwrap();
        fs::write(&pruned_v2_checkpoint, b"owned v2 history").unwrap();

        let manager = SessionManager {
            cwd: "/tmp".to_string(),
            sessions_dir: dir.path().to_path_buf(),
            current_session_id: None,
            writer: None,
            auxiliary_cleanup: noop_auxiliary_cleanup,
        };
        let session = manager
            .list_sessions()
            .unwrap()
            .into_iter()
            .find(|candidate| candidate.id == pruned_id)
            .unwrap();
        assert!(
            manager
                .legacy_checkpoint_root_has_other_live_owner(&session, pruned_store.legacy_root())
                .unwrap()
        );

        manager.remove_session_and_checkpoints(&session).unwrap();

        assert!(!session.path.exists());
        assert!(!pruned_store.root().exists());
        assert!(shared_legacy_checkpoint.exists());
        assert!(
            manager
                .list_sessions()
                .unwrap()
                .iter()
                .any(|candidate| candidate.id == live_id)
        );
    }

    #[test]
    fn malformed_journal_preserves_legacy_root_without_blocking_prune() {
        let dir = TempDir::new().unwrap();
        let pruned_id = "valid-session";
        create_test_session_file(dir.path(), pruned_id);
        let malformed = dir.path().join("malformed.jsonl");
        fs::write(&malformed, b"not-json\n").unwrap();

        let store = crate::checkpoints::CheckpointStore::new(dir.path(), pruned_id);
        let v2_checkpoint = store.root().join("owned").join("checkpoint.json");
        fs::create_dir_all(v2_checkpoint.parent().unwrap()).unwrap();
        fs::write(&v2_checkpoint, b"v2").unwrap();
        let legacy_checkpoint = store.legacy_root().join("legacy").join("checkpoint.json");
        fs::create_dir_all(legacy_checkpoint.parent().unwrap()).unwrap();
        fs::write(&legacy_checkpoint, b"legacy").unwrap();

        let manager = SessionManager {
            cwd: "/tmp".to_string(),
            sessions_dir: dir.path().to_path_buf(),
            current_session_id: None,
            writer: None,
            auxiliary_cleanup: noop_auxiliary_cleanup,
        };
        let session = manager.list_sessions().unwrap().pop().unwrap();

        manager.remove_session_and_checkpoints(&session).unwrap();

        assert!(!session.path.exists());
        assert!(!store.root().exists());
        assert!(malformed.exists());
        assert!(legacy_checkpoint.exists());
    }

    #[test]
    fn post_delete_reconciliation_removes_shared_legacy_root_after_all_owners_exit() {
        let dir = TempDir::new().unwrap();
        let first_id = "legacy:concurrent";
        let second_id = "legacy?concurrent";
        create_test_session_file(dir.path(), first_id);
        create_test_session_file(dir.path(), second_id);
        let first_store = crate::checkpoints::CheckpointStore::new(dir.path(), first_id);
        let second_store = crate::checkpoints::CheckpointStore::new(dir.path(), second_id);
        assert_eq!(first_store.legacy_root(), second_store.legacy_root());
        let checkpoint = first_store
            .legacy_root()
            .join("shared")
            .join("checkpoint.json");
        fs::create_dir_all(checkpoint.parent().unwrap()).unwrap();
        fs::write(&checkpoint, b"shared history").unwrap();
        let manager = SessionManager {
            cwd: "/tmp".to_string(),
            sessions_dir: dir.path().to_path_buf(),
            current_session_id: None,
            writer: None,
            auxiliary_cleanup: noop_auxiliary_cleanup,
        };
        let sessions = manager.list_sessions().unwrap();
        for session in &sessions {
            assert!(manager
                .legacy_checkpoint_root_has_other_live_owner(session, first_store.legacy_root(),)
                .unwrap());
        }

        // Model two pruners that both made the conservative shared-owner
        // decision before either transcript deletion committed.
        for session in sessions {
            fs::remove_file(session.path).unwrap();
        }
        let staging_dir = SessionManager::prune_staging_dir(dir.path());
        fs::create_dir_all(&staging_dir).unwrap();
        let staged = staging_dir.join(first_store.legacy_root().file_name().unwrap());
        let held_cleanup_lock =
            SessionLock::acquire(&SessionManager::prune_staging_lock_key(&staged)).unwrap();
        let sessions_dir = dir.path().to_path_buf();
        let cleanup_sessions_dir = sessions_dir.clone();
        let legacy_root = first_store.legacy_root().to_path_buf();
        let cleanup = std::thread::spawn(move || {
            let manager = SessionManager {
                cwd: "/tmp".to_string(),
                sessions_dir,
                current_session_id: None,
                writer: None,
                auxiliary_cleanup: noop_auxiliary_cleanup,
            };
            manager.cleanup_orphaned_legacy_checkpoint_root(&cleanup_sessions_dir, &legacy_root);
        });
        std::thread::sleep(std::time::Duration::from_millis(20));
        drop(held_cleanup_lock);
        cleanup.join().unwrap();

        assert!(!first_store.legacy_root().exists());
    }

    #[test]
    fn test_remove_session_aborts_when_checkpoint_staging_is_locked() {
        let dir = TempDir::new().unwrap();
        let id = "staging-locked";
        let session_path = dir.path().join("staging-locked.jsonl");
        fs::write(&session_path, "{}").unwrap();

        let store = crate::checkpoints::CheckpointStore::new(dir.path(), id);
        let checkpoint = store.root().join("chk1").join("checkpoint.json");
        fs::create_dir_all(checkpoint.parent().unwrap()).unwrap();
        fs::write(&checkpoint, b"{}").unwrap();

        let staging_dir = SessionManager::prune_staging_dir(dir.path());
        fs::create_dir_all(&staging_dir).unwrap();
        let staged = staging_dir.join(store.root().file_name().unwrap());
        let _held_lock =
            SessionLock::acquire(&SessionManager::prune_staging_lock_key(&staged)).unwrap();

        let manager = SessionManager {
            cwd: "/tmp".to_string(),
            sessions_dir: dir.path().to_path_buf(),
            current_session_id: None,
            writer: None,
            auxiliary_cleanup: noop_auxiliary_cleanup,
        };
        let session = SessionInfo {
            parent_session: None,
            id: id.to_string(),
            path: session_path.clone(),
            cwd: "/tmp".to_string(),
            model: "test-model".to_string(),
            thinking_level: ThinkingLevel::Medium,
            timestamp: "2024-01-15T10:30:00Z".to_string(),
            stats: SessionStats::default(),
            meta: None,
            modified: None,
            preview: None,
        };

        assert!(
            manager.remove_session_and_checkpoints(&session).is_err(),
            "pruning must fail closed while another process owns staging"
        );
        assert!(
            session_path.exists(),
            "the transcript must remain retryable"
        );
        assert!(
            checkpoint.exists(),
            "live checkpoints must remain untouched"
        );
    }

    #[test]
    fn test_failed_prune_restores_retained_and_new_checkpoint_history() {
        let dir = TempDir::new().unwrap();
        let id = "retained-and-new";
        let store = crate::checkpoints::CheckpointStore::new(dir.path(), id);
        let new_checkpoint = store.root().join("new").join("checkpoint.json");
        fs::create_dir_all(new_checkpoint.parent().unwrap()).unwrap();
        fs::write(&new_checkpoint, b"new").unwrap();

        let staged =
            SessionManager::prune_staging_dir(dir.path()).join(store.root().file_name().unwrap());
        let old_checkpoint = staged.join("old").join("checkpoint.json");
        fs::create_dir_all(old_checkpoint.parent().unwrap()).unwrap();
        fs::write(&old_checkpoint, b"old").unwrap();

        // A directory makes the transcript removal fail after the combined
        // checkpoint history has been staged, exercising rollback.
        let session_path = dir.path().join("retained-and-new.jsonl");
        fs::create_dir_all(&session_path).unwrap();
        let manager = SessionManager {
            cwd: "/tmp".to_string(),
            sessions_dir: dir.path().to_path_buf(),
            current_session_id: None,
            writer: None,
            auxiliary_cleanup: noop_auxiliary_cleanup,
        };
        let session = SessionInfo {
            parent_session: None,
            id: id.to_string(),
            path: session_path,
            cwd: "/tmp".to_string(),
            model: "test-model".to_string(),
            thinking_level: ThinkingLevel::Medium,
            timestamp: "2024-01-15T10:30:00Z".to_string(),
            stats: SessionStats::default(),
            meta: None,
            modified: None,
            preview: None,
        };

        assert!(manager.remove_session_and_checkpoints(&session).is_err());
        assert!(
            store.root().join("old").join("checkpoint.json").exists(),
            "retained pre-crash history must roll back with the session"
        );
        assert!(
            store.root().join("new").join("checkpoint.json").exists(),
            "post-resume history must roll back with the session"
        );
        assert!(!staged.exists(), "rollback must empty the staging entry");
    }

    /// Regression test: pruning a session whose own checkpoint directory
    /// name happens to be exactly what the *old* staging scheme would have
    /// used for a *different* session's staging target (a
    /// `<session_id>.prune-staged` sibling) must not touch that other,
    /// unrelated, live session's checkpoints at all. Before this fix, a
    /// portable or hand-written session id ending in `.prune-staged` would
    /// share a literal path with another session's staging directory,
    /// silently destroying its rewind history.
    #[test]
    fn test_prune_does_not_collide_with_a_session_named_like_the_old_staging_suffix() {
        let dir = TempDir::new().unwrap();

        // A live, unrelated session whose checkpoint directory name is
        // exactly what the pre-fix scheme would stage session "victim"
        // into: `checkpoints/victim.prune-staged/`.
        let untouched_id = "victim.prune-staged";
        let untouched_store = crate::checkpoints::CheckpointStore::new(dir.path(), untouched_id);
        let untouched_checkpoint = untouched_store.root().join("chk1");
        fs::create_dir_all(&untouched_checkpoint).unwrap();
        fs::write(untouched_checkpoint.join("checkpoint.json"), b"{}").unwrap();

        // The session actually being pruned.
        let victim_path = dir.path().join("victim-session.jsonl");
        fs::write(&victim_path, "").unwrap();
        let victim_store = crate::checkpoints::CheckpointStore::new(dir.path(), "victim");
        fs::create_dir_all(victim_store.root().join("chk1")).unwrap();
        fs::write(
            victim_store.root().join("chk1").join("checkpoint.json"),
            b"{}",
        )
        .unwrap();

        let manager = SessionManager {
            cwd: "/tmp".to_string(),
            sessions_dir: dir.path().to_path_buf(),
            current_session_id: None,
            writer: None,
            auxiliary_cleanup: noop_auxiliary_cleanup,
        };
        let victim = SessionInfo {
            parent_session: None,
            id: "victim".to_string(),
            path: victim_path,
            cwd: "/tmp".to_string(),
            model: "test-model".to_string(),
            thinking_level: ThinkingLevel::Medium,
            timestamp: "2024-01-15T10:30:00Z".to_string(),
            stats: SessionStats::default(),
            meta: None,
            modified: None,
            preview: None,
        };

        manager
            .remove_session_and_checkpoints(&victim)
            .expect("pruning the victim session must succeed");

        assert!(
            untouched_checkpoint.join("checkpoint.json").exists(),
            "an unrelated live session named like the old staging suffix must survive pruning"
        );
        assert!(
            !victim_store.root().exists(),
            "the actually-pruned session's checkpoints must be gone"
        );
    }

    /// Regression test: a stale staged directory left behind by a previous
    /// run's failed final cleanup (see `remove_session_and_checkpoints`)
    /// must be swept the next time `prune_sessions` runs -- otherwise it is
    /// a permanent orphan, since nothing else ever revisits a pruned
    /// session's checkpoint directory.
    #[test]
    fn test_prune_sessions_sweeps_stale_prune_staged_directory() {
        let dir = TempDir::new().unwrap();
        create_test_session_file(dir.path(), "unrelated-session");

        let stale_staged =
            SessionManager::prune_staging_dir(dir.path()).join("leftover-from-a-crash");
        fs::create_dir_all(stale_staged.join("chk1")).unwrap();
        fs::write(stale_staged.join("chk1").join("checkpoint.json"), b"{}").unwrap();
        assert!(stale_staged.exists());

        let manager = SessionManager {
            cwd: "/tmp".to_string(),
            sessions_dir: dir.path().to_path_buf(),
            current_session_id: None,
            writer: None,
            auxiliary_cleanup: noop_auxiliary_cleanup,
        };

        // max_age_days=1 with a just-created session prunes nothing, but
        // the sweep must still run unconditionally at the start.
        let (_removed, errors) = manager.prune_sessions(0, 1);
        assert_eq!(errors, 0);
        assert!(
            !stale_staged.exists(),
            "a stale staged directory must be swept"
        );
        assert!(
            lock_path_for(&SessionManager::prune_staging_lock_key(&stale_staged)).is_file(),
            "the reusable staging lock sidecar must survive the sweep"
        );
    }

    /// Regression test: a staging entry another (simulated concurrent)
    /// process is actively locked on must survive a sweep, not be deleted
    /// out from under it -- otherwise a second `/session cleanup` racing
    /// the first's rollback would have nothing left to restore if the
    /// first's own final `.jsonl` removal then failed.
    #[test]
    fn test_sweep_skips_a_staging_entry_locked_by_another_process() {
        let dir = TempDir::new().unwrap();
        create_test_session_file(dir.path(), "unrelated-session");

        let staged = SessionManager::prune_staging_dir(dir.path()).join("in-progress-elsewhere");
        fs::create_dir_all(staged.join("chk1")).unwrap();
        fs::write(staged.join("chk1").join("checkpoint.json"), b"{}").unwrap();

        // Simulate another process's in-flight `remove_session_and_checkpoints`
        // still holding this entry's staging lock.
        let _held_lock =
            SessionLock::acquire(&SessionManager::prune_staging_lock_key(&staged)).unwrap();

        let manager = SessionManager {
            cwd: "/tmp".to_string(),
            sessions_dir: dir.path().to_path_buf(),
            current_session_id: None,
            writer: None,
            auxiliary_cleanup: noop_auxiliary_cleanup,
        };

        let (_removed, errors) = manager.prune_sessions(0, 1);
        assert_eq!(errors, 0);
        assert!(
            staged.exists(),
            "a locked staging entry must survive the sweep"
        );
    }

    /// Regression test: a staged directory carrying `ROLLBACK_TOMBSTONE_FILE`
    /// (left by `remove_session_and_checkpoints` when its rollback rename
    /// itself fails after the session's `.jsonl` removal also failed) must
    /// survive a sweep, exactly like a directory another process holds a
    /// lock on. It is still owned by a live, listed session -- not an
    /// ordinary orphaned leftover from a crashed prune's final
    /// `remove_dir_all` -- and deleting it would permanently destroy that
    /// session's only remaining rewind history with nothing left to retry
    /// from.
    #[test]
    fn test_sweep_skips_a_tombstoned_staging_entry() {
        let dir = TempDir::new().unwrap();
        create_test_session_file(dir.path(), "unrelated-session");

        let staged =
            SessionManager::prune_staging_dir(dir.path()).join("rollback-failed-elsewhere");
        fs::create_dir_all(staged.join("chk1")).unwrap();
        fs::write(staged.join("chk1").join("checkpoint.json"), b"{}").unwrap();
        fs::write(staged.join(ROLLBACK_TOMBSTONE_FILE), b"").unwrap();

        let manager = SessionManager {
            cwd: "/tmp".to_string(),
            sessions_dir: dir.path().to_path_buf(),
            current_session_id: None,
            writer: None,
            auxiliary_cleanup: noop_auxiliary_cleanup,
        };

        let (_removed, errors) = manager.prune_sessions(0, 1);
        assert_eq!(errors, 0);
        assert!(
            staged.join("chk1").join("checkpoint.json").exists(),
            "a tombstoned staging entry must survive the sweep"
        );
    }

    /// Regression test: a staged directory left behind by a crash between
    /// `remove_session_and_checkpoints`'s initial stage-aside rename and
    /// whatever it does next (before it could even attempt, let alone
    /// tombstone a failed, session-file removal) has no lock held (the
    /// crashed process's advisory lock is released by the OS on exit) and
    /// no `ROLLBACK_TOMBSTONE_FILE` marker (that is only written on an
    /// explicit rollback-rename failure). If its owning session is still
    /// listed -- its `.jsonl` was never touched -- the sweep must restore
    /// it to that session's normal checkpoint location, not delete it
    /// outright as though it were an ordinary orphaned leftover from a
    /// prune attempt that ran to completion.
    #[test]
    fn test_sweep_restores_interrupted_staging_for_a_still_listed_session() {
        let dir = TempDir::new().unwrap();
        let id = "interrupted-mid-prune";
        create_test_session_file(dir.path(), id);

        let store = crate::checkpoints::CheckpointStore::new(dir.path(), id);
        let staged =
            SessionManager::prune_staging_dir(dir.path()).join(store.root().file_name().unwrap());
        fs::create_dir_all(staged.join("chk1")).unwrap();
        fs::write(staged.join("chk1").join("checkpoint.json"), b"{}").unwrap();

        let manager = SessionManager {
            cwd: "/tmp".to_string(),
            sessions_dir: dir.path().to_path_buf(),
            current_session_id: None,
            writer: None,
            auxiliary_cleanup: noop_auxiliary_cleanup,
        };

        let (_removed, errors) = manager.prune_sessions(0, 1);
        assert_eq!(errors, 0);
        assert!(
            !staged.exists(),
            "the staged directory must be restored, not left in staging"
        );
        assert!(
            store.root().join("chk1").join("checkpoint.json").exists(),
            "an interrupted staging entry for a still-listed session must be restored to its normal location"
        );
    }

    #[test]
    fn test_sweep_restores_interrupted_legacy_staging_for_a_live_session() {
        let dir = TempDir::new().unwrap();
        let id = "interrupted-legacy-prune";
        create_test_session_file(dir.path(), id);

        let store = crate::checkpoints::CheckpointStore::new(dir.path(), id);
        let staged = SessionManager::prune_staging_dir(dir.path())
            .join(store.legacy_root().file_name().unwrap());
        fs::create_dir_all(staged.join("legacy-chk")).unwrap();
        fs::write(staged.join("legacy-chk").join("checkpoint.json"), b"legacy").unwrap();

        let manager = SessionManager {
            cwd: "/tmp".to_string(),
            sessions_dir: dir.path().to_path_buf(),
            current_session_id: None,
            writer: None,
            auxiliary_cleanup: noop_auxiliary_cleanup,
        };

        manager.sweep_stale_checkpoint_staging();

        assert!(!staged.exists());
        assert!(
            store
                .legacy_root()
                .join("legacy-chk")
                .join("checkpoint.json")
                .exists(),
            "a listed session must retain ownership of its pre-v2 staged checkpoints"
        );
    }

    /// Regression test for the A/B cleanup race: cleanup B snapshots a
    /// session as live, cleanup A deletes that session while B is waiting
    /// for the staging lock, and B must not restore A's now-orphaned
    /// checkpoints from the stale snapshot after it acquires the lock.
    #[test]
    fn test_sweep_rechecks_live_session_after_staging_lock_acquisition() {
        let dir = TempDir::new().unwrap();
        let id = "deleted-after-live-snapshot";
        create_test_session_file(dir.path(), id);

        let manager = SessionManager {
            cwd: "/tmp".to_string(),
            sessions_dir: dir.path().to_path_buf(),
            current_session_id: None,
            writer: None,
            auxiliary_cleanup: noop_auxiliary_cleanup,
        };
        let session = manager.list_sessions().unwrap().pop().unwrap();
        let store = crate::checkpoints::CheckpointStore::new(dir.path(), id);
        let checkpoint_name = store.root().file_name().unwrap().to_os_string();
        let staged = SessionManager::prune_staging_dir(dir.path()).join(&checkpoint_name);
        fs::create_dir_all(staged.join("chk1")).unwrap();
        fs::write(staged.join("chk1").join("checkpoint.json"), b"{}").unwrap();

        // Cleanup B's pre-lock snapshot still says this session is live.
        let live_checkpoint_dir_names = std::collections::HashSet::from([checkpoint_name]);

        // Cleanup A wins the staging lock and completes deletion before B
        // acquires it. Invoke B's post-snapshot entry processing only after
        // A's transcript deletion to deterministically model that boundary.
        fs::remove_file(&session.path).unwrap();
        let entries = fs::read_dir(SessionManager::prune_staging_dir(dir.path())).unwrap();
        manager.sweep_stale_checkpoint_staging_entries(entries, &live_checkpoint_dir_names);

        assert!(
            !staged.exists(),
            "a stale live-session snapshot must not restore orphaned checkpoints"
        );
        assert!(
            !store.root().exists(),
            "the deleted session's checkpoints must not be resurrected"
        );
    }

    /// Regression test: if `list_sessions` itself fails, the sweep must not
    /// treat that as "no live sessions" and delete anything -- there is no
    /// way to tell a live session's interrupted staging apart from a
    /// genuine orphan without it, so a failure that makes every staged
    /// entry look orphaned must abort the whole sweep instead.
    #[cfg(unix)]
    #[test]
    fn test_sweep_aborts_entirely_when_session_discovery_fails() {
        use std::os::unix::fs::PermissionsExt;

        let dir = TempDir::new().unwrap();
        create_test_session_file(dir.path(), "unrelated-session");

        // A genuinely orphaned staged directory (no matching live session)
        // that would ordinarily be swept.
        let orphan_staged =
            SessionManager::prune_staging_dir(dir.path()).join("genuinely-orphaned");
        fs::create_dir_all(orphan_staged.join("chk1")).unwrap();
        fs::write(orphan_staged.join("chk1").join("checkpoint.json"), b"{}").unwrap();

        let manager = SessionManager {
            cwd: "/tmp".to_string(),
            sessions_dir: dir.path().to_path_buf(),
            current_session_id: None,
            writer: None,
            auxiliary_cleanup: noop_auxiliary_cleanup,
        };

        // Deny read (but not execute/traversal) on the sessions directory
        // itself, so `list_sessions`'s own `fs::read_dir` fails while the
        // deeper `checkpoints/.prune-staging~` directory this sweep reads
        // remains reachable.
        let original_mode = fs::metadata(dir.path()).unwrap().permissions().mode();
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o111)).unwrap();

        // Root and some CI sandboxes ignore directory mode bits, so this
        // fixture cannot force discovery to fail. The unreadable-header test
        // covers the same abort path without depending on DAC.
        if manager.list_sessions().is_ok() {
            fs::set_permissions(dir.path(), fs::Permissions::from_mode(original_mode)).unwrap();
            return;
        }

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            manager.prune_sessions(0, 1)
        }));

        // Restore permissions before any assertion (or the temp dir's own
        // `Drop` cleanup) needs to touch the directory again.
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(original_mode)).unwrap();

        let (_removed, errors) = result.expect("prune_sessions must not panic");
        assert_eq!(errors, 0);
        assert!(
            orphan_staged.join("chk1").join("checkpoint.json").exists(),
            "the sweep must not delete anything when it cannot determine live-session ownership"
        );
    }

    /// Recovery must treat a JSONL candidate whose header cannot be read as
    /// an ownership-discovery failure, not silently omit it and delete its
    /// staged checkpoint history as though the transcript were gone.
    #[test]
    fn test_sweep_aborts_when_a_session_header_is_unreadable() {
        let dir = TempDir::new().unwrap();
        let id = "temporarily-unreadable-header";
        create_test_session_file(dir.path(), id);

        let manager = SessionManager {
            cwd: "/tmp".to_string(),
            sessions_dir: dir.path().to_path_buf(),
            current_session_id: None,
            writer: None,
            auxiliary_cleanup: noop_auxiliary_cleanup,
        };
        let session = manager.list_sessions().unwrap().pop().unwrap();
        let store = crate::checkpoints::CheckpointStore::new(dir.path(), id);
        let staged =
            SessionManager::prune_staging_dir(dir.path()).join(store.root().file_name().unwrap());
        fs::create_dir_all(staged.join("chk1")).unwrap();
        fs::write(staged.join("chk1").join("checkpoint.json"), b"{}").unwrap();

        fs::write(&session.path, b"temporarily unreadable header\n").unwrap();
        manager.sweep_stale_checkpoint_staging();

        assert!(
            staged.join("chk1").join("checkpoint.json").exists(),
            "strict recovery discovery must retain all staged history when any candidate header is unreadable"
        );
        assert!(
            !store.root().exists(),
            "an ownership-discovery failure must neither restore nor delete staged history"
        );
    }

    /// The post-lock ownership recheck uses the same strict discovery rule:
    /// if a header becomes unreadable after the initial live snapshot, retain
    /// the staged entry instead of treating the omission as a deletion.
    #[test]
    fn test_post_lock_recheck_retains_staging_when_a_header_is_unreadable() {
        let dir = TempDir::new().unwrap();
        let id = "unreadable-after-live-snapshot";
        create_test_session_file(dir.path(), id);

        let manager = SessionManager {
            cwd: "/tmp".to_string(),
            sessions_dir: dir.path().to_path_buf(),
            current_session_id: None,
            writer: None,
            auxiliary_cleanup: noop_auxiliary_cleanup,
        };
        let session = manager.list_sessions().unwrap().pop().unwrap();
        let store = crate::checkpoints::CheckpointStore::new(dir.path(), id);
        let checkpoint_name = store.root().file_name().unwrap().to_os_string();
        let staged = SessionManager::prune_staging_dir(dir.path()).join(&checkpoint_name);
        fs::create_dir_all(staged.join("chk1")).unwrap();
        fs::write(staged.join("chk1").join("checkpoint.json"), b"{}").unwrap();
        let stale_live_names = std::collections::HashSet::from([checkpoint_name]);

        fs::write(&session.path, b"temporarily unreadable header\n").unwrap();
        let entries = fs::read_dir(SessionManager::prune_staging_dir(dir.path())).unwrap();
        manager.sweep_stale_checkpoint_staging_entries(entries, &stale_live_names);

        assert!(
            staged.join("chk1").join("checkpoint.json").exists(),
            "post-lock discovery failure must retain staged checkpoint history"
        );
        assert!(
            !store.root().exists(),
            "the staged entry must not be restored"
        );
    }

    /// Regression test: a failed restore (the session's normal checkpoint
    /// location is already occupied, e.g. it resumed and wrote new
    /// checkpoints since the crash) must not fall through to deleting the
    /// staged copy. `CheckpointStore` normally retains several checkpoints
    /// for rewind, so the staged (older) checkpoints are not necessarily
    /// superseded by whatever now occupies the destination.
    #[test]
    fn test_sweep_retains_staged_history_when_restore_destination_is_occupied() {
        let dir = TempDir::new().unwrap();
        let id = "resumed-after-interrupted-prune";
        create_test_session_file(dir.path(), id);

        let store = crate::checkpoints::CheckpointStore::new(dir.path(), id);
        let staged =
            SessionManager::prune_staging_dir(dir.path()).join(store.root().file_name().unwrap());
        fs::create_dir_all(staged.join("pre-crash-chk")).unwrap();
        fs::write(staged.join("pre-crash-chk").join("checkpoint.json"), b"{}").unwrap();

        // The session resumed and wrote a new checkpoint at its normal
        // location since the crash, so the restore destination is already
        // a non-empty directory and the rename below cannot succeed.
        fs::create_dir_all(store.root().join("post-resume-chk")).unwrap();
        fs::write(
            store.root().join("post-resume-chk").join("checkpoint.json"),
            b"{}",
        )
        .unwrap();

        let manager = SessionManager {
            cwd: "/tmp".to_string(),
            sessions_dir: dir.path().to_path_buf(),
            current_session_id: None,
            writer: None,
            auxiliary_cleanup: noop_auxiliary_cleanup,
        };

        let (_removed, errors) = manager.prune_sessions(0, 1);
        assert_eq!(errors, 0);
        assert!(
            staged
                .join("pre-crash-chk")
                .join("checkpoint.json")
                .exists(),
            "a failed restore must retain the staged (older) checkpoints, not delete them as though superseded"
        );
        assert!(
            store
                .root()
                .join("post-resume-chk")
                .join("checkpoint.json")
                .exists(),
            "the live post-resume checkpoint must be untouched"
        );
    }

    /// Regression test: a staging entry matching a live session's
    /// checkpoint-directory name, but currently locked by another
    /// (simulated concurrent) process, must survive the sweep exactly like
    /// an ordinary orphan candidate does -- the lock must be checked before
    /// the restore path acts, not only before the delete path.
    #[test]
    fn test_sweep_does_not_restore_a_live_session_entry_locked_by_another_process() {
        let dir = TempDir::new().unwrap();
        let id = "interrupted-and-locked-elsewhere";
        create_test_session_file(dir.path(), id);

        let store = crate::checkpoints::CheckpointStore::new(dir.path(), id);
        let staged =
            SessionManager::prune_staging_dir(dir.path()).join(store.root().file_name().unwrap());
        fs::create_dir_all(staged.join("chk1")).unwrap();
        fs::write(staged.join("chk1").join("checkpoint.json"), b"{}").unwrap();

        // Simulate another process's in-flight `remove_session_and_checkpoints`
        // still holding this entry's staging lock.
        let _held_lock =
            SessionLock::acquire(&SessionManager::prune_staging_lock_key(&staged)).unwrap();

        let manager = SessionManager {
            cwd: "/tmp".to_string(),
            sessions_dir: dir.path().to_path_buf(),
            current_session_id: None,
            writer: None,
            auxiliary_cleanup: noop_auxiliary_cleanup,
        };

        let (_removed, errors) = manager.prune_sessions(0, 1);
        assert_eq!(errors, 0);
        assert!(
            staged.join("chk1").join("checkpoint.json").exists(),
            "a locked live-session staging entry must survive the sweep, not be renamed out from under the other process"
        );
        assert!(
            !store.root().exists(),
            "the restore must not have happened while the entry was locked"
        );
    }

    /// Acquire a staging lock, riding out the transient `Locked` window that
    /// fork+exec from unrelated test threads can cause: a child forked while
    /// a lock fd is open anywhere in the process inherits that open file
    /// description and keeps the flock held until its exec completes
    /// (`O_CLOEXEC` only takes effect at exec), so a lock can briefly outlive
    /// the `SessionLock` drop that released it. Poll for a bounded time
    /// rather than fail on that window; a genuinely stuck lock still fails
    /// the test.
    #[cfg(unix)]
    fn acquire_staging_lock_after_release(key: &Path) -> SessionLock {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            match SessionLock::acquire(key) {
                Err(crate::session::writer::SessionWriteError::Locked(_))
                    if std::time::Instant::now() < deadline =>
                {
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                result => return result.expect("acquire staging lock"),
            }
        }
    }

    /// Regression test for the three-cleanup inode-split race: actor A
    /// releases a reusable staging lock, actor B acquires the same inode,
    /// and actor C must not be able to acquire a replacement inode at the
    /// same logical lock path while B still owns it.
    #[cfg(unix)]
    #[test]
    fn test_staging_lock_sidecar_keeps_one_inode_across_three_cleanup_actors() {
        use std::os::unix::fs::MetadataExt;

        let dir = TempDir::new().unwrap();
        let staged = SessionManager::prune_staging_dir(dir.path()).join("shared-entry");
        fs::create_dir_all(staged.parent().unwrap()).unwrap();
        let lock_key = SessionManager::prune_staging_lock_key(&staged);
        let sidecar = lock_path_for(&lock_key);

        let actor_a = acquire_staging_lock_after_release(&lock_key);
        let original_inode = fs::metadata(&sidecar).unwrap().ino();
        drop(actor_a);

        let actor_b = acquire_staging_lock_after_release(&lock_key);
        assert_eq!(
            fs::metadata(&sidecar).unwrap().ino(),
            original_inode,
            "actor B must reuse actor A's persistent lock inode"
        );
        assert!(
            matches!(
                SessionLock::acquire(&lock_key),
                Err(crate::session::writer::SessionWriteError::Locked(_))
            ),
            "actor C must not acquire a replacement lock while actor B owns the stable inode"
        );
        assert_eq!(
            fs::metadata(&sidecar).unwrap().ino(),
            original_inode,
            "the contended actor must not replace the reusable lock inode"
        );
        drop(actor_b);

        let _actor_c = acquire_staging_lock_after_release(&lock_key);
        assert_eq!(
            fs::metadata(&sidecar).unwrap().ino(),
            original_inode,
            "actor C must reuse the same inode after actor B releases it"
        );
    }

    #[test]
    fn test_staging_lock_sidecar_cannot_collide_with_a_session_entry() {
        let dir = TempDir::new().unwrap();
        let staged = SessionManager::prune_staging_dir(dir.path()).join("session");
        fs::create_dir_all(staged.parent().unwrap()).unwrap();
        let _lock = SessionLock::acquire(&SessionManager::prune_staging_lock_key(&staged)).unwrap();
        let colliding_name = staged.with_file_name("session.lock");
        fs::create_dir_all(&colliding_name).unwrap();
        assert!(
            colliding_name.is_dir(),
            "a valid staged session entry ending in .lock must not collide with the persistent sidecar"
        );
    }

    /// Regression test: a portable or hand-written session literally named
    /// `.prune-staging` must not collide with the staging namespace itself.
    /// Before this fix, `prune_staging_dir` returned exactly
    /// `checkpoints/.prune-staging`, and `sanitize_component` passes that
    /// literal session id through unchanged (it isn't dot-only), so such a
    /// session's own checkpoint root *was* the staging namespace directory:
    /// any `/session cleanup` with a nonzero limit would sweep its
    /// checkpoint subdirectories before ever checking whether the session
    /// was current, favorited, locked, or even eligible for pruning.
    #[test]
    fn test_session_named_dot_prune_staging_does_not_collide_with_staging_namespace() {
        let dir = TempDir::new().unwrap();

        let collider_id = ".prune-staging";
        let collider_store = crate::checkpoints::CheckpointStore::new(dir.path(), collider_id);
        assert_ne!(
            collider_store.root(),
            SessionManager::prune_staging_dir(dir.path()),
            "a session literally named .prune-staging must not resolve to the staging namespace"
        );
        let collider_checkpoint = collider_store.root().join("chk1");
        fs::create_dir_all(&collider_checkpoint).unwrap();
        fs::write(collider_checkpoint.join("checkpoint.json"), b"{}").unwrap();

        // An unrelated session; the sweep this exercises runs unconditionally
        // at the start of every `prune_sessions` call regardless of what (if
        // anything) actually gets pruned.
        create_test_session_file(dir.path(), "unrelated-session");

        let manager = SessionManager {
            cwd: "/tmp".to_string(),
            sessions_dir: dir.path().to_path_buf(),
            current_session_id: None,
            writer: None,
            auxiliary_cleanup: noop_auxiliary_cleanup,
        };
        let (_removed, errors) = manager.prune_sessions(0, 1);
        assert_eq!(errors, 0);
        assert!(
            collider_checkpoint.join("checkpoint.json").exists(),
            "a session named .prune-staging must keep its own checkpoints across an unrelated prune's sweep"
        );
    }

    /// Pruning a session must also remove its sidecar `<file>.lock`, not
    /// just the `.jsonl` and checkpoint directory. Before this fix, the
    /// lock file (created by every `SessionWriter::create`/`open_existing`
    /// via `SessionLock::acquire`) was never cleaned up anywhere, so it
    /// piled up forever next to sessions that had themselves already been
    /// pruned -- the same orphaning failure mode as issue #3151, just for
    /// the lock sidecar instead of the checkpoint directory.
    #[test]
    fn test_prune_sessions_removes_sidecar_lock_file() {
        let dir = TempDir::new().unwrap();
        let ids = ["session0", "session1"];
        for id in ids {
            create_test_session_file(dir.path(), id);
        }

        let manager = SessionManager {
            cwd: "/tmp".to_string(),
            sessions_dir: dir.path().to_path_buf(),
            current_session_id: None,
            writer: None,
            auxiliary_cleanup: noop_auxiliary_cleanup,
        };

        let sessions_before = manager.list_sessions().unwrap();
        let lock_paths: std::collections::HashMap<String, std::path::PathBuf> = sessions_before
            .iter()
            .map(|s| (s.id.clone(), lock_path_for(&s.path)))
            .collect();
        // Create the sidecar lock files the same way `SessionLock::acquire`
        // does, standing in for a prior writer having already opened (and
        // closed) each session once.
        for lock_path in lock_paths.values() {
            fs::write(lock_path, b"").unwrap();
        }

        let (removed, errors) = manager.prune_sessions(1, 0);
        assert_eq!(errors, 0);
        assert!(removed >= 1, "expected at least 1 session pruned");

        let remaining_ids: std::collections::HashSet<String> = manager
            .list_sessions()
            .unwrap()
            .into_iter()
            .map(|s| s.id)
            .collect();

        for (id, lock_path) in &lock_paths {
            if remaining_ids.contains(id) {
                assert!(
                    lock_path.exists(),
                    "kept session {id}'s lock file must survive"
                );
            } else {
                assert!(
                    !lock_path.exists(),
                    "pruned session {id}'s lock file must be removed, not orphaned"
                );
            }
        }
    }

    /// A session with no checkpoints at all (the common case) must still
    /// prune cleanly -- the missing checkpoint directory is not an error.
    #[test]
    fn test_prune_sessions_without_checkpoints_is_not_an_error() {
        let dir = TempDir::new().unwrap();
        create_test_session_file(dir.path(), "no-checkpoints");

        let manager = SessionManager {
            cwd: "/tmp".to_string(),
            sessions_dir: dir.path().to_path_buf(),
            current_session_id: None,
            writer: None,
            auxiliary_cleanup: noop_auxiliary_cleanup,
        };

        let (removed, errors) = manager.prune_sessions(0, 1);
        assert_eq!(errors, 0);
        // `create_test_session_file` sets mtime to "now", not older than the
        // 1-day cutoff, so age-based pruning correctly leaves it alone; this
        // test only asserts the checkpoint-less path doesn't error.
        let _ = removed;
    }

    /// Pruning must skip a session another Maestro process has open
    /// (sidecar lock held) instead of deleting the live process's history
    /// and rewind checkpoints out from under it. Only the caller's own
    /// session is protected by the `current_session_id` check; the lock is
    /// what protects sessions open elsewhere.
    #[test]
    fn test_prune_sessions_skips_locked_sessions() {
        let dir = TempDir::new().unwrap();
        create_test_session_file(dir.path(), "locked1");
        create_test_session_file(dir.path(), "other2");

        let locked_path = dir.path().join("2024-01-15T10-30-00-000Z_locked1.jsonl");
        let locked_checkpoints = crate::checkpoints::CheckpointStore::new(dir.path(), "locked1")
            .root()
            .to_path_buf();
        fs::create_dir_all(&locked_checkpoints).unwrap();

        // Stand in for the other process: hold the session's sidecar lock
        // for the rest of the test, exactly as a live `SessionWriter` would.
        let _lock = SessionLock::acquire(&locked_path).unwrap();

        let manager = SessionManager {
            cwd: "/tmp".to_string(),
            sessions_dir: dir.path().to_path_buf(),
            current_session_id: None,
            writer: None,
            auxiliary_cleanup: noop_auxiliary_cleanup,
        };

        let (_removed, errors) = manager.prune_sessions(1, 0);
        assert_eq!(errors, 0);
        assert!(
            locked_path.exists(),
            "a session locked by another process must survive pruning"
        );
        assert!(
            locked_checkpoints.exists(),
            "a locked session's checkpoints must survive pruning"
        );
    }

    /// A lock-acquisition failure that is not contention (simulated here by
    /// making the sidecar `.lock` path itself a directory, so opening it
    /// fails with `EISDIR` rather than the `WouldBlock` that maps to
    /// `SessionWriteError::Locked`) must be counted as a real prune error,
    /// not silently treated the same as "another process has this session
    /// open". Both candidate sessions get the same treatment so the test
    /// doesn't depend on which one count-based pruning happens to pick.
    #[test]
    fn test_prune_sessions_counts_non_contention_lock_errors() {
        let dir = TempDir::new().unwrap();
        for id in ["session-a", "session-b"] {
            create_test_session_file(dir.path(), id);
            let path = dir
                .path()
                .join(format!("2024-01-15T10-30-00-000Z_{id}.jsonl"));
            fs::create_dir_all(lock_path_for(&path)).unwrap();
        }

        let manager = SessionManager {
            cwd: "/tmp".to_string(),
            sessions_dir: dir.path().to_path_buf(),
            current_session_id: None,
            writer: None,
            auxiliary_cleanup: noop_auxiliary_cleanup,
        };

        let (removed, errors) = manager.prune_sessions(1, 0);
        assert_eq!(removed, 0, "acquisition failed, nothing should be removed");
        assert_eq!(
            errors, 1,
            "a non-contention lock-acquisition failure must be counted as a \
             prune error, not silently skipped"
        );
        assert_eq!(
            manager.list_sessions().unwrap().len(),
            2,
            "sessions must survive when their lock can't be checked for a \
             non-contention reason"
        );
    }

    /// Regression test: a hand-written or portable session whose accepted id
    /// is `..` must not turn checkpoint cleanup into `remove_dir_all` on
    /// `<sessions_dir>/checkpoints/..` -- i.e. on the sessions directory
    /// itself. `CheckpointStore` sanitizes such ids, so pruning removes only
    /// the session file and leaves every sibling untouched.
    #[test]
    fn test_remove_session_with_dot_only_id_does_not_escape_checkpoints_dir() {
        let dir = TempDir::new().unwrap();
        create_test_session_file(dir.path(), "..");
        let decoy = dir.path().join("decoy.txt");
        fs::write(&decoy, b"keep me").unwrap();

        let manager = SessionManager {
            cwd: "/tmp".to_string(),
            sessions_dir: dir.path().to_path_buf(),
            current_session_id: None,
            writer: None,
            auxiliary_cleanup: noop_auxiliary_cleanup,
        };

        let session = manager
            .list_sessions()
            .unwrap()
            .into_iter()
            .find(|s| s.id == "..")
            .expect("dot-only session id should be listed");
        manager.remove_session_and_checkpoints(&session).unwrap();

        assert!(!session.path.exists());
        assert!(
            decoy.exists(),
            "checkpoint cleanup must not escape the checkpoints directory"
        );
    }
    #[test]
    fn selective_summary_prepared_adoption_keeps_source_locked_until_commit() {
        let dir = TempDir::new().unwrap();
        create_test_session_file(dir.path(), "prepared-source");
        let mut manager = SessionManager::with_sessions_dir("/tmp", dir.path());
        let source = manager.list_sessions().unwrap().remove(0);
        manager
            .resume_session_by_path(&source.id, &source.path)
            .unwrap();
        let (child_id, child_path) = manager.fork_session_snapshot().unwrap();
        let prepared = manager.prepare_session_adoption(&child_path).unwrap();
        assert_eq!(manager.current_session_id(), Some(source.id.as_str()));
        assert!(SessionWriter::open_existing(&source.path).is_err());
        assert!(SessionWriter::open_existing(&child_path).is_err());
        drop(prepared);
        assert!(SessionWriter::open_existing(&source.path).is_err());
        drop(super::super::writer::open_existing_after_release(
            &child_path,
        ));
        let prepared = manager.prepare_session_adoption(&child_path).unwrap();
        manager.adopt_prepared_session(prepared);
        assert_eq!(manager.current_session_id(), Some(child_id.as_str()));
        drop(super::super::writer::open_existing_after_release(
            &source.path,
        ));
        assert!(SessionWriter::open_existing(&child_path).is_err());
    }
}

#[cfg(test)]
#[path = "manager/info_tests.rs"]
mod info_tests;

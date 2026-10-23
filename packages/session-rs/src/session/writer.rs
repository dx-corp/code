//! Session writer
//!
//! Writes session entries to JSONL files with batching.
//!
//! # Cross-process locking
//!
//! Every [`SessionWriter`] holds an advisory lock on a sidecar `<file>.lock`
//! next to the session file for as long as it is alive (see [`SessionLock`]).
//! A session file can be open for append in the TUI, a `maestro -r <id>` CLI
//! resume, and a headless supervisor simultaneously; without mutual
//! exclusion, [`open_existing`](SessionWriter::open_existing)'s
//! check-and-possibly-truncate of a torn tail (see [`truncate_torn_tail`])
//! cannot tell a live writer's in-flight partial line from a crashed one and
//! will truncate the live writer's bytes out from under it, permanently
//! corrupting the session (see issue #3150). The lock is acquired
//! non-blocking and fails fast with [`SessionWriteError::Locked`] rather than
//! blocking indefinitely, which is the right call for an interactive CLI/TUI:
//! a second resume attempt almost always means "another maestro session for
//! this id is already open", and that is actionable to report immediately
//! rather than hang.

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, BufWriter, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use super::entries::{SessionEntry, SessionHeader};

/// Default batch size for writes
const DEFAULT_BATCH_SIZE: usize = 25;

/// Error type for session writing
#[derive(Debug)]
pub enum SessionWriteError {
    IoError(std::io::Error),
    SerializeError(String),
    /// Another process already holds the advisory lock on this session file.
    ///
    /// Carries the path to the session file (not the sidecar lock file) for
    /// a message that matches what the user typed/selected.
    Locked(PathBuf),
}

impl std::fmt::Display for SessionWriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SessionWriteError::IoError(e) => write!(f, "IO error: {e}"),
            SessionWriteError::SerializeError(msg) => write!(f, "Serialize error: {msg}"),
            SessionWriteError::Locked(path) => write!(
                f,
                "session is open in another process: {} is locked; close the other maestro \
                 session (TUI, CLI resume, or headless run) using this session before resuming \
                 here",
                path.display()
            ),
        }
    }
}

impl std::error::Error for SessionWriteError {}

impl From<std::io::Error> for SessionWriteError {
    fn from(e: std::io::Error) -> Self {
        SessionWriteError::IoError(e)
    }
}

/// Session writer with batching
pub struct SessionWriter {
    /// Path to the session file
    path: PathBuf,
    /// Pending entries to write
    buffer: Vec<SessionEntry>,
    /// Batch size before auto-flush
    batch_size: usize,
    /// Whether the session header has been written
    header_written: bool,
    /// Derived position/count for the exclusively owned append-only transcript.
    /// Rebuilt on writer reopen or truncation; never another durable authority.
    user_turn_cursor: Option<(u64, usize)>,
    /// Advisory cross-process lock held for the lifetime of this writer.
    /// See the module docs and [`SessionLock`] for why this exists.
    _lock: SessionLock,
}

impl SessionWriter {
    /// Create a new session writer
    pub fn new(path: impl AsRef<Path>) -> Result<Self, SessionWriteError> {
        let path = path.as_ref().to_path_buf();

        // Ensure parent directory exists
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        // Acquired before anything else touches `path` so that `create()`
        // (which calls this) and `open_existing()` share one choke point:
        // any two `SessionWriter`s over the same session file, in any
        // process, contend for the same sidecar lock.
        let lock = SessionLock::acquire(&path)?;

        Ok(Self {
            path,
            buffer: Vec::new(),
            batch_size: DEFAULT_BATCH_SIZE,
            header_written: false,
            user_turn_cursor: None,
            _lock: lock,
        })
    }

    /// Create a new session with a header
    pub fn create(
        path: impl AsRef<Path>,
        header: SessionHeader,
    ) -> Result<Self, SessionWriteError> {
        let mut writer = Self::new(path)?;
        writer.write_entry(SessionEntry::Session(header))?;
        writer.header_written = true;
        Ok(writer)
    }

    /// Open an existing session file for appending (header already present)
    pub fn open_existing(path: impl AsRef<Path>) -> Result<Self, SessionWriteError> {
        let path = path.as_ref().to_path_buf();

        if !path.exists() {
            return Err(SessionWriteError::IoError(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "Session file not found",
            )));
        }

        // Lock BEFORE inspecting/truncating a torn tail. This is the crux of
        // the fix for #3150: `truncate_torn_tail`'s read-then-truncate is a
        // check-and-act that must be atomic with respect to any other
        // process resuming (or still writing to) this same session file. If
        // a live writer holds this lock, `acquire` fails fast here instead
        // of racing that writer's in-flight append.
        let lock = SessionLock::acquire(&path)?;

        truncate_torn_tail(&path)?;

        Ok(Self {
            path,
            buffer: Vec::new(),
            batch_size: DEFAULT_BATCH_SIZE,
            header_written: true,
            user_turn_cursor: None,
            _lock: lock,
        })
    }

    /// Set the batch size
    #[must_use]
    pub fn batch_size(mut self, size: usize) -> Self {
        self.batch_size = size;
        self
    }

    /// Get the file path
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Write an entry (buffers until batch size reached)
    pub fn write_entry(&mut self, entry: SessionEntry) -> Result<(), SessionWriteError> {
        self.buffer.push(entry);

        if self.buffer.len() >= self.batch_size {
            self.flush()?;
        }

        Ok(())
    }

    /// Write multiple entries
    pub fn write_entries(&mut self, entries: Vec<SessionEntry>) -> Result<(), SessionWriteError> {
        for entry in entries {
            self.write_entry(entry)?;
        }
        Ok(())
    }

    /// Flush all buffered entries to disk
    pub fn flush(&mut self) -> Result<(), SessionWriteError> {
        if self.buffer.is_empty() {
            return Ok(());
        }

        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;

        let mut writer = BufWriter::new(file);

        for entry in self.buffer.drain(..) {
            let mut json = serde_json::to_string(&entry)
                .map_err(|e| SessionWriteError::SerializeError(e.to_string()))?;
            // A large JSON body bypasses BufWriter. Include the delimiter in
            // the same write instead of leaving it in the buffer until later.
            json.push('\n');
            writer.write_all(json.as_bytes())?;
        }

        writer.flush()?;
        Ok(())
    }

    /// Flush and count saved user turns, scanning only newly appended JSONL.
    /// Checkpoint capture needs this scalar, not a reconstructed conversation.
    pub fn saved_user_turn_count(&mut self) -> Result<usize, SessionWriteError> {
        self.flush()?;
        let file = match File::open(&self.path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(0),
            Err(error) => return Err(error.into()),
        };
        let len = file.metadata()?.len();
        let (mut offset, mut count) = self
            .user_turn_cursor
            .filter(|(offset, _)| *offset <= len)
            .unwrap_or_default();
        let mut reader = BufReader::new(file);
        reader.seek(SeekFrom::Start(offset))?;
        let mut line = String::new();
        // Publish the cursor only after the complete suffix validates. Failure
        // leaves the previous cursor available for an exact retry.
        while super::fork::read_bounded_line(&mut reader, &mut line)? > 0 {
            if !line.ends_with('\n') {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "incomplete saved session entry",
                )
                .into());
            }
            if super::reader::is_user_message_line(&line)? {
                count += 1;
            }
            offset += line.len() as u64;
        }
        self.user_turn_cursor = Some((offset, count));
        Ok(count)
    }

    /// Check if header has been written
    #[must_use]
    pub fn has_header(&self) -> bool {
        self.header_written
    }

    /// Get the number of buffered entries
    #[must_use]
    pub fn buffered_count(&self) -> usize {
        self.buffer.len()
    }
}

impl Drop for SessionWriter {
    fn drop(&mut self) {
        // Flush remaining entries on drop
        let _ = self.flush();
    }
}

/// Truncate a torn trailing line left by a crash mid-write.
///
/// Complete entries are always written with a trailing newline, so a final
/// line that is unterminated or fails to parse is a partial write. Removing
/// it before appending keeps new entries from being concatenated onto the
/// corrupt line, which would poison the session file permanently.
fn truncate_torn_tail(path: &Path) -> Result<(), SessionWriteError> {
    let contents = fs::read(path)?;
    if contents.is_empty() {
        return Ok(());
    }

    let ends_with_newline = contents.last() == Some(&b'\n');
    let body = if ends_with_newline {
        &contents[..contents.len() - 1]
    } else {
        &contents[..]
    };
    let line_start = body
        .iter()
        .rposition(|&b| b == b'\n')
        .map_or(0, |pos| pos + 1);
    let last_line = &body[line_start..];

    let torn = !ends_with_newline
        || (!last_line.is_empty() && serde_json::from_slice::<SessionEntry>(last_line).is_err());
    if torn {
        eprintln!(
            "Truncating torn trailing line in session file {} before appending",
            path.display()
        );
        OpenOptions::new()
            .write(true)
            .open(path)?
            .set_len(line_start as u64)?;
    }
    Ok(())
}

/// Advisory lock coordinating cross-process access to one session file.
///
/// Held for the lifetime of the owning [`SessionWriter`]; the lock is
/// explicitly released when this value is dropped, even if a child process
/// inherited the underlying file handle.
///
/// The lock is taken on a sidecar `<session-file>.lock` file rather than the
/// session `.jsonl` itself. Locking the data file directly would work too
/// (advisory locks compose fine with `O_APPEND`), but a sidecar keeps the
/// two concerns independent: the lock file is never read, written, rotated,
/// or truncated by anything in this module, so there is no chance of the
/// locking mechanism interacting with the crash-recovery logic it protects.
pub struct SessionLock {
    file: File,
}

impl Drop for SessionLock {
    fn drop(&mut self) {
        // A fork or duplicated handle can retain the open file description.
        // Closing our handle alone would leave that child's inherited lock held.
        if let Err(error) = self.file.unlock() {
            eprintln!("Could not release session lock: {error}");
        }
    }
}

impl SessionLock {
    /// Take an exclusive advisory lock on `session_path`'s sidecar lock
    /// file, failing immediately with [`SessionWriteError::Locked`] if
    /// another process already holds it.
    ///
    /// Fails fast rather than blocking: this is called from CLI/TUI startup
    /// paths where hanging with no explanation is worse than a clear error
    /// telling the user another session is already open. There is no
    /// server-side coordinator that would make waiting-with-a-timeout more
    /// correct than an immediate, actionable failure here.
    pub fn acquire(session_path: &Path) -> Result<Self, SessionWriteError> {
        let lock_path = lock_path_for(session_path);
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&lock_path)?;

        match file.try_lock() {
            Ok(()) => Ok(Self { file }),
            Err(std::fs::TryLockError::WouldBlock) => {
                Err(SessionWriteError::Locked(session_path.to_path_buf()))
            }
            Err(std::fs::TryLockError::Error(error)) => Err(SessionWriteError::IoError(error)),
        }
    }

    /// Report whether any live holder — in this process or another one —
    /// currently owns the advisory lock for `session_path`.
    ///
    /// Callers that need the lock should just call [`acquire`](Self::acquire)
    /// and handle [`SessionWriteError::Locked`]. This is for the read-only
    /// question "is someone still working on this session?", where taking the
    /// lock would be wrong.
    ///
    /// Probes with a *shared* lock rather than an exclusive one so two
    /// concurrent probes cannot report each other as a holder. The probe is
    /// released before returning, so the only acquisition it can delay is one
    /// that races inside this call.
    ///
    /// The lock is per open file description on both platforms
    /// (`flock(2)` on Unix, `LockFileEx` on Windows), so a lease this same
    /// process holds through a different [`SessionLock`] is reported as held,
    /// and dropping this probe does not release it.
    ///
    /// A missing lock file — or a missing session directory — means nothing
    /// can be holding the lock, and reports `false`.
    pub fn is_held(session_path: &Path) -> Result<bool, SessionWriteError> {
        let lock_path = lock_path_for(session_path);
        let file = match OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&lock_path)
        {
            Ok(file) => file,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(SessionWriteError::IoError(e)),
        };

        match file.try_lock_shared() {
            Ok(()) => {
                // Release explicitly even if a concurrent fork retained this fd.
                file.unlock()?;
                Ok(false)
            }
            Err(std::fs::TryLockError::WouldBlock) => Ok(true),
            Err(std::fs::TryLockError::Error(error)) => Err(SessionWriteError::IoError(error)),
        }
    }
}

/// Compute the sidecar lock path for a session file: `foo.jsonl` ->
/// `foo.jsonl.lock`, next to it in the same directory.
pub(crate) fn lock_path_for(session_path: &Path) -> PathBuf {
    let mut name = session_path
        .file_name()
        .map(std::ffi::OsStr::to_os_string)
        .unwrap_or_default();
    name.push(".lock");
    session_path.with_file_name(name)
}

/// Generate a session filename
pub fn generate_session_filename(session_id: &str) -> String {
    let now = chrono::Utc::now();
    let timestamp = now.format("%Y-%m-%dT%H-%M-%S-%3fZ");
    format!("{timestamp}_{session_id}.jsonl")
}

/// Sanitize a path for use in session directory names
pub fn sanitize_path_for_dirname(path: &str) -> String {
    path.replace(['/', '\\', ':'], "-")
        .trim_matches('-')
        .to_string()
}

/// Get the sessions directory for a working directory
pub fn sessions_dir(cwd: &str) -> PathBuf {
    let home = dirs::home_dir()
        .or_else(|| std::env::var_os("USERPROFILE").map(PathBuf::from))
        .unwrap_or_else(std::env::temp_dir);
    let sanitized = sanitize_path_for_dirname(cwd);
    home.join(".composer")
        .join("agent")
        .join("sessions")
        .join(format!("--{sanitized}--"))
}

/// Reacquiring a session lock right after its holder dropped can
/// transiently observe `Locked` when an unrelated test thread forks a
/// child while the lock fd is open: the child inherits that open file
/// description and keeps the flock held until its exec completes
/// (`O_CLOEXEC` only takes effect at exec). Poll briefly past that
/// window; a genuinely held lock still fails the test.
#[cfg(test)]
pub(super) fn open_existing_after_release(path: &Path) -> SessionWriter {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        match SessionWriter::open_existing(path) {
            Err(SessionWriteError::Locked(_)) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            result => return result.expect("open existing session after writer drop"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use tempfile::TempDir;

    #[test]
    fn saved_turn_count_tracks_flushed_appends_and_reopen() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("count.jsonl");
        let mut writer = SessionWriter::new(&path).unwrap();
        assert_eq!(writer.saved_user_turn_count().unwrap(), 0);
        // Use complete serialized message entries so the normal buffered writer
        // and reopened reader exercise the same transcript contract.
        for index in 0..64 {
            let entry: SessionEntry = serde_json::from_value(serde_json::json!({
                "type": "message", "timestamp": "2026-09-10T00:00:00Z",
                "message": {"role": "user", "content": format!("request {index} 🦀")}
            }))
            .unwrap();
            writer.write_entry(entry).unwrap();
            assert_eq!(writer.saved_user_turn_count().unwrap(), index + 1);
            assert_eq!(writer.saved_user_turn_count().unwrap(), index + 1);
        }
        drop(writer);
        let mut reopened = open_existing_after_release(&path);
        assert_eq!(reopened.saved_user_turn_count().unwrap(), 64);
    }

    #[test]
    fn saved_turn_count_retries_incomplete_suffix_without_double_counting() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("count.jsonl");
        let mut writer = SessionWriter::new(&path).unwrap();
        let line = "{\"type\":\"message\",\"timestamp\":\"2026-09-10T00:00:00Z\",\"message\":{\"role\":\"user\",\"content\":\"hello\"}}\n";
        fs::write(&path, line).unwrap();
        assert_eq!(writer.saved_user_turn_count().unwrap(), 1);
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        write!(file, "{line}{{").unwrap();
        assert!(writer.saved_user_turn_count().is_err());
        writeln!(file, "\"type\":\"custom\"}}").unwrap();
        assert_eq!(writer.saved_user_turn_count().unwrap(), 2);
        fs::write(&path, line).unwrap();
        assert_eq!(writer.saved_user_turn_count().unwrap(), 1);
    }

    #[test]
    #[ignore = "measures repeated turn counting over cumulative compaction records"]
    fn saved_turn_count_scaling_probe() {
        let baseline = std::env::var_os("MAESTRO_SESSION_COUNT_BASELINE").is_some();
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("long-session.jsonl");
        let header: SessionHeader = serde_json::from_value(serde_json::json!({
            "type": "session", "version": 2, "id": "count-probe",
            "timestamp": "2026-09-10T00:00:00Z", "cwd": "/tmp", "model": "gpt-4o"
        }))
        .unwrap();
        let mut writer = SessionWriter::create(&path, header).unwrap();
        let mut elapsed = std::time::Duration::ZERO;
        let mut requests = Vec::new();
        for index in 0..1000 {
            let prompt = format!("request-{index}: {}", "evidence ".repeat(28));
            requests.push(prompt.clone());
            writer
                .write_entry(
                    serde_json::from_value(serde_json::json!({
                        "type": "message", "timestamp": "2026-09-10T00:00:00Z",
                        "message": {"role": "user", "content": prompt}
                    }))
                    .unwrap(),
                )
                .unwrap();
            if index % 25 == 24 {
                let continuation = maestro_context::compaction::ContinuationRecord {
                    user_requests: requests.clone(),
                    ..Default::default()
                };
                writer
                    .write_entry(
                        serde_json::from_value(serde_json::json!({
                            "type": "compaction", "timestamp": "2026-09-10T00:00:00Z",
                            "summary": "earlier work", "firstKeptEntryIndex": 0,
                            "tokensBefore": 5000, "continuation": continuation
                        }))
                        .unwrap(),
                    )
                    .unwrap();
            }
            writer.flush().unwrap();
            let started = std::time::Instant::now();
            let count = if baseline {
                super::super::SessionReader::read_file(&path)
                    .unwrap()
                    .stats
                    .user_messages
            } else {
                writer.saved_user_turn_count().unwrap()
            };
            elapsed += started.elapsed();
            assert_eq!(count, index + 1);
        }
        let started = std::time::Instant::now();
        let (boundary, count) = super::super::rewind_boundary_with_turn_count(&path, 10).unwrap();
        assert_eq!(count, 1000);
        assert!(boundary > 0);
        eprintln!(
            "TURN_COUNT_PROBE {}",
            serde_json::json!({
                "baseline": baseline, "turns": 1000, "saved_bytes": fs::metadata(path).unwrap().len(),
                "count_elapsed_ms": elapsed.as_millis(), "rewind_scan_ms": started.elapsed().as_millis()
            })
        );
    }

    #[test]
    fn writer_creates_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.jsonl");

        let header = SessionHeader {
            version: Some(2),
            id: "test123".to_string(),
            timestamp: "2024-01-15T10:30:00Z".to_string(),
            cwd: "/tmp".to_string(),
            model: "anthropic/claude-3".to_string(),
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
        };

        let mut writer = SessionWriter::create(&path, header).unwrap();
        writer.flush().unwrap();

        assert!(path.exists());
    }

    #[test]
    fn writer_batches_writes() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.jsonl");

        let mut writer = SessionWriter::new(&path).unwrap().batch_size(3);

        // Write 2 entries (below batch size)
        writer
            .write_entry(SessionEntry::ThinkingLevelChange(
                super::super::entries::ThinkingLevelChange {
                    timestamp: "2024-01-15T10:30:00Z".to_string(),
                    thinking_level: super::super::entries::ThinkingLevel::High,
                },
            ))
            .unwrap();

        // File shouldn't exist yet (buffered)
        assert!(!path.exists());

        // Write 2 more (triggers flush at 3)
        writer
            .write_entry(SessionEntry::ThinkingLevelChange(
                super::super::entries::ThinkingLevelChange {
                    timestamp: "2024-01-15T10:31:00Z".to_string(),
                    thinking_level: super::super::entries::ThinkingLevel::Low,
                },
            ))
            .unwrap();
        writer
            .write_entry(SessionEntry::ThinkingLevelChange(
                super::super::entries::ThinkingLevelChange {
                    timestamp: "2024-01-15T10:32:00Z".to_string(),
                    thinking_level: super::super::entries::ThinkingLevel::Medium,
                },
            ))
            .unwrap();

        // Should have flushed now
        assert!(path.exists());
    }

    #[test]
    fn is_held_reports_a_live_lock_without_taking_it() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.jsonl");

        // Nothing has ever locked this session.
        assert!(!SessionLock::is_held(&path).expect("probe an unlocked session"));

        let lock = SessionLock::acquire(&path).expect("acquire the session lock");
        assert!(SessionLock::is_held(&path).expect("probe a locked session"));
        // The probe must not have released the live lock, and must not have
        // taken one of its own.
        assert!(SessionLock::is_held(&path).expect("probe a locked session again"));
        assert!(matches!(
            SessionLock::acquire(&path),
            Err(SessionWriteError::Locked(_))
        ));

        drop(lock);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while SessionLock::is_held(&path).expect("probe a released session") {
            assert!(
                std::time::Instant::now() < deadline,
                "released session lock stayed held after the child-fd inheritance window"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    #[test]
    fn session_lock_contends_with_legacy_fd_lock_writers_and_probes() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.jsonl");
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(lock_path_for(&path))
            .unwrap();
        let mut legacy = fd_lock::RwLock::new(file);
        let legacy_writer = legacy.try_write().unwrap();
        assert!(SessionLock::is_held(&path).unwrap());
        assert!(matches!(
            SessionLock::acquire(&path),
            Err(SessionWriteError::Locked(_))
        ));
        drop(legacy_writer);
        let current = SessionLock::acquire(&path).unwrap();
        assert!(
            matches!(legacy.try_read(), Err(error) if error.kind() == io::ErrorKind::WouldBlock)
        );
        assert!(
            matches!(legacy.try_write(), Err(error) if error.kind() == io::ErrorKind::WouldBlock)
        );
        drop(current);
        drop(legacy.try_write().unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn released_session_lock_does_not_wait_for_inherited_file_description() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.jsonl");
        let lock = SessionLock::acquire(&path).unwrap();
        // dup/try_clone models the same open file description retained by fork.
        let inherited = lock.file.try_clone().unwrap();
        assert!(matches!(
            SessionLock::acquire(&path),
            Err(SessionWriteError::Locked(_))
        ));
        drop(lock);
        assert!(!SessionLock::is_held(&path).unwrap());
        let next = SessionLock::acquire(&path).unwrap();
        drop(inherited);
        assert!(SessionLock::is_held(&path).unwrap());
        drop(next);
    }

    #[test]
    fn is_held_reports_false_for_a_session_directory_that_is_gone() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("missing").join("test.jsonl");

        assert!(!SessionLock::is_held(&path).expect("probe a missing session directory"));
    }

    #[test]
    fn sanitize_path() {
        assert_eq!(
            sanitize_path_for_dirname("/Users/john/projects/myapp"),
            "Users-john-projects-myapp"
        );
    }

    #[test]
    fn generate_filename() {
        let filename = generate_session_filename("abc123");
        assert!(filename.ends_with("_abc123.jsonl"));
    }

    #[test]
    fn sessions_dir_uses_home_or_temp_dir() {
        let cwd = "/Users/john/projects/myapp";
        let dir = sessions_dir(cwd);
        let sanitized = sanitize_path_for_dirname(cwd);
        let suffix = PathBuf::from(".composer")
            .join("agent")
            .join("sessions")
            .join(format!("--{}--", sanitized));

        assert!(dir.ends_with(&suffix));

        if let Some(home) = dirs::home_dir() {
            assert!(dir.starts_with(&home));
        } else {
            assert!(dir.starts_with(std::env::temp_dir()));
        }
    }

    #[test]
    fn open_existing_truncates_torn_tail_before_appending() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.jsonl");

        let header = SessionHeader {
            version: Some(2),
            id: "test123".to_string(),
            timestamp: "2024-01-15T10:30:00Z".to_string(),
            cwd: "/tmp".to_string(),
            model: "anthropic/claude-3".to_string(),
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
        };

        let mut writer = SessionWriter::create(&path, header).unwrap();
        writer.flush().unwrap();
        // A real crash also ends the process, releasing its advisory lock.
        // Drop the writer to model that before writing the torn tail below.
        drop(writer);

        // Simulate a crash mid-`writeln!`: partial JSON with no trailing newline.
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        write!(file, r#"{{"type":"message","timestamp":"2024-01-15T10:30"#).unwrap();
        drop(file);

        let mut writer = open_existing_after_release(&path);
        writer
            .write_entry(SessionEntry::ThinkingLevelChange(
                super::super::entries::ThinkingLevelChange {
                    timestamp: "2024-01-15T10:31:00Z".to_string(),
                    thinking_level: super::super::entries::ThinkingLevel::High,
                },
            ))
            .unwrap();
        writer.flush().unwrap();

        // Every line must parse; the torn fragment must be gone.
        let contents = fs::read_to_string(&path).unwrap();
        assert!(!contents.contains("10:30\""));
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines.len(), 2);
        for line in lines {
            serde_json::from_str::<SessionEntry>(line).unwrap();
        }
    }

    fn test_header(id: &str) -> SessionHeader {
        SessionHeader {
            version: Some(2),
            id: id.to_string(),
            timestamp: "2024-01-15T10:30:00Z".to_string(),
            cwd: "/tmp".to_string(),
            model: "anthropic/claude-3".to_string(),
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
        }
    }

    /// Regression test for #3150: a second `open_existing` against a
    /// session file that a live (not crashed) writer still holds open must
    /// fail fast rather than truncate the live writer's in-flight bytes.
    ///
    /// Before the fix in this commit, `open_existing` had no notion of
    /// mutual exclusion: it would happily run `truncate_torn_tail` against
    /// writer A's exact-same on-disk signature as a crash (a partial line
    /// with no trailing newline), truncating writer A's unflushed append.
    /// When writer A later completed that write, `O_APPEND` would place the
    /// completed bytes at the new EOF, producing a newline-terminated
    /// garbage line the reader's crash-recovery cannot detect -- permanent,
    /// silent corruption. Stashing the lock (`git stash` the diff in
    /// `writer.rs`) reproduces exactly that: this test's `assert!(matches!(
    /// second, Err(SessionWriteError::Locked(_))))` fails because `second`
    /// succeeds instead, and the "untouched while locked" assertion below
    /// fails because the torn tail is gone (truncated to the header line).
    #[test]
    fn open_existing_fails_fast_against_a_live_writer_instead_of_truncating() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.jsonl");

        // Writer A: a live process, not a crashed one. It is still holding
        // the file (and, with the fix, the lock) for the rest of this test.
        let mut writer_a = SessionWriter::create(&path, test_header("test123")).unwrap();
        writer_a.flush().unwrap();

        // Writer A mid-append: on disk this looks byte-for-byte identical to
        // a crash (partial JSON, no trailing newline) -- that is the whole
        // point of the bug. The difference invisible to `truncate_torn_tail`
        // is that writer A's process (and lock) is still alive.
        {
            let mut file = OpenOptions::new().append(true).open(&path).unwrap();
            write!(file, r#"{{"type":"message","timestamp":"2024-01-15T10:30"#).unwrap();
        }

        // A second process resuming the same session must fail fast, not
        // silently truncate writer A's unflushed bytes.
        let second = SessionWriter::open_existing(&path);
        match &second {
            Err(SessionWriteError::Locked(_)) => {}
            Err(other) => {
                panic!("expected Locked error while writer A is still open, got {other:?}")
            }
            Ok(_) => panic!(
                "expected Locked error while writer A is still open, got Ok(_) -- \
                 a live writer's session file was truncated instead of blocked"
            ),
        }

        // The file must be completely untouched: still torn, header intact.
        let contents_while_locked = fs::read_to_string(&path).unwrap();
        assert!(
            contents_while_locked.contains("10:30"),
            "second open_existing must not truncate a live writer's in-flight bytes"
        );
        assert!(contents_while_locked.contains("test123"));

        // Writer A's process exits; dropping it releases the lock exactly
        // like process exit would.
        drop(writer_a);

        // Now the same torn tail left behind is a *real* crash signature
        // (no writer holds it anymore), so a resume must succeed and the
        // existing torn-tail recovery must still kick in.
        let mut writer_b = open_existing_after_release(&path);
        writer_b
            .write_entry(SessionEntry::ThinkingLevelChange(
                super::super::entries::ThinkingLevelChange {
                    timestamp: "2024-01-15T10:31:00Z".to_string(),
                    thinking_level: super::super::entries::ThinkingLevel::High,
                },
            ))
            .unwrap();
        writer_b.flush().unwrap();

        let contents = fs::read_to_string(&path).unwrap();
        assert!(!contents.contains("10:30\""));
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines.len(), 2);
        for line in lines {
            serde_json::from_str::<SessionEntry>(line).unwrap();
        }
    }

    #[test]
    fn locked_error_message_names_the_session_path_not_the_lock_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.jsonl");

        let mut writer_a = SessionWriter::create(&path, test_header("test123")).unwrap();
        writer_a.flush().unwrap();
        let second = SessionWriter::open_existing(&path);

        let Err(err) = second else {
            panic!("expected the second open_existing to fail while writer A holds the lock");
        };
        let message = err.to_string();
        assert!(message.contains(&path.display().to_string()));
        assert!(!message.contains(".lock"));
    }

    #[test]
    fn lock_path_is_a_sidecar_next_to_the_session_file() {
        let session_path = PathBuf::from("/tmp/sessions/2024_abc.jsonl");
        assert_eq!(
            lock_path_for(&session_path),
            PathBuf::from("/tmp/sessions/2024_abc.jsonl.lock")
        );
    }
}

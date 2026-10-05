//! The local, file-backed `dex_loop::Log`: one append-only JSONL file per
//! thread, fenced the way `dex-runtime`'s `PgLog` fences a Postgres row.
//!
//! This writes a fresh, dex-loop-native event stream — one JSON object per
//! line, `{"cursor": N, "event": {...}}` — rather than reusing Maestro's
//! existing `maestro-session` JSONL format, so this first slice can be
//! staged without touching Maestro's current session persistence. See
//! `docs/design/maestro-on-dex-loop.md` for the cutover that unifies them.
//!
//! Unlike `PgLog`, consecutive `TextDelta` calls are not coalesced into one
//! row: `append_text` writes one row per call. `dex_loop::Log`'s contract
//! permits this ("the engine never assumes one row per call"); coalescing
//! is a production-log optimization, not a correctness requirement, and is
//! left for the real cutover.

use std::fs::{self, OpenOptions};
use std::io::{self, BufRead, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use dex_loop::{Cursor, Event, Fenced, Log, ThreadId};
use serde::{Deserialize, Serialize};

use crate::lease::{self, Meta};

/// Reading or decoding the durable log failed. Distinct from `Fenced`,
/// which is the `Log` trait's own error for a refused *write*; this is
/// returned by the host-side helpers (`acquire`, `read_all`) a caller uses
/// before the engine ever sees the log.
#[derive(Debug, thiserror::Error)]
pub enum LogError {
    #[error("io error: {0}")]
    Io(#[from] io::Error),
    #[error("event at line {line} does not decode: {source}")]
    Decode {
        line: usize,
        source: serde_json::Error,
    },
    #[error("background task panicked: {0}")]
    Join(#[from] tokio::task::JoinError),
}

impl From<LogError> for Fenced {
    fn from(error: LogError) -> Self {
        Fenced::new(error.to_string())
    }
}

#[derive(Serialize, Deserialize)]
struct Row {
    cursor: Cursor,
    event: Event,
}

fn thread_dir(root: &Path, thread: &ThreadId) -> PathBuf {
    root.join(sanitize(&thread.org))
        .join(sanitize(&thread.workspace))
        .join(sanitize(&thread.thread))
}

/// Confines a `ThreadId` field to one path component: a stray `/` or `..`
/// in a tenant-controlled id must not let one thread's log escape into
/// another thread's directory, or above `root` entirely.
fn sanitize(part: &str) -> String {
    let cleaned: String = part
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if cleaned.is_empty() || cleaned == "." || cleaned == ".." {
        "_".to_owned()
    } else {
        cleaned
    }
}

fn log_path(dir: &Path) -> PathBuf {
    dir.join("log.jsonl")
}

enum AppendOutcome {
    Fenced { held: u64, current: u64 },
    Wrote(Vec<Cursor>),
}

/// Blocking: appends `events` to `dir`'s log under the same lock that
/// guards the generation check, so the check and the write are atomic
/// together — the same guarantee `PgLog::write_rows` gets from doing both
/// inside one Postgres transaction.
fn append_locked(
    dir: &Path,
    expected_generation: u64,
    events: &[Event],
) -> io::Result<AppendOutcome> {
    lease::with_locked_meta(dir, |meta: &mut Meta| {
        if meta.generation != expected_generation {
            return Ok(AppendOutcome::Fenced {
                held: expected_generation,
                current: meta.generation,
            });
        }
        if events.is_empty() {
            return Ok(AppendOutcome::Wrote(Vec::new()));
        }
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_path(dir))?;
        let mut cursors = Vec::with_capacity(events.len());
        let mut buffer = Vec::new();
        for event in events {
            meta.last_cursor += 1;
            let cursor = Cursor(meta.last_cursor);
            let row = Row {
                cursor,
                event: event.clone(),
            };
            serde_json::to_writer(&mut buffer, &row).map_err(io::Error::other)?;
            buffer.push(b'\n');
            cursors.push(cursor);
        }
        file.write_all(&buffer)?;
        file.sync_data()?;
        Ok(AppendOutcome::Wrote(cursors))
    })
}

/// Blocking: bumps `dir`'s generation, invalidating any earlier lease for
/// the same thread.
fn acquire_locked(dir: &Path) -> io::Result<u64> {
    lease::with_locked_meta(dir, |meta: &mut Meta| {
        meta.generation += 1;
        Ok(meta.generation)
    })
}

/// Blocking: every row in `dir`'s log, oldest first. Takes the same lock
/// `append_locked` takes exclusively, shared, so this never overlaps an
/// in-progress `write_all` and cannot observe a torn trailing line.
fn read_all_locked(dir: &Path) -> Result<Vec<(Cursor, Event)>, LogError> {
    lease::with_shared_lock(dir, || read_rows(dir))
        .map_err(LogError::from)?
        .into_iter()
        .enumerate()
        .map(|(index, line)| {
            serde_json::from_str::<Row>(&line)
                .map(|row| (row.cursor, row.event))
                .map_err(|source| LogError::Decode {
                    line: index + 1,
                    source,
                })
        })
        .collect()
}

/// Every non-empty line in `dir`'s log file, unparsed. Split out of
/// `read_all_locked` so decode errors (which need the line index) happen
/// outside the lock.
fn read_rows(dir: &Path) -> io::Result<Vec<String>> {
    let path = log_path(dir);
    let file = match fs::File::open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    io::BufReader::new(file)
        .lines()
        .filter(|line| !matches!(line, Ok(line) if line.trim().is_empty()))
        .collect()
}

struct Inner {
    dir: PathBuf,
    generation: u64,
    lost: AtomicBool,
}

/// The `dex_loop::Log` for one thread under one lease. Cloning shares the
/// same lease: every clone observes the same "lost" state once any of them
/// sees a fenced write, matching `PgLog`'s `Clone` semantics.
#[derive(Clone)]
pub struct LocalLog {
    inner: Arc<Inner>,
}

impl LocalLog {
    /// Acquires a fresh lease for `thread` under `root`, bumping the
    /// persisted generation so any earlier lease for the same thread is
    /// fenced out on its next write.
    pub async fn acquire(root: impl AsRef<Path>, thread: &ThreadId) -> Result<Self, LogError> {
        let dir = thread_dir(root.as_ref(), thread);
        let generation = {
            let dir = dir.clone();
            tokio::task::spawn_blocking(move || acquire_locked(&dir)).await??
        };
        Ok(Self {
            inner: Arc::new(Inner {
                dir,
                generation,
                lost: AtomicBool::new(false),
            }),
        })
    }

    /// Every event in the thread's log, oldest first — for
    /// `dex_loop::rehydrate`. Call this once, right after `acquire`, before
    /// handing the log to `Engine::run`.
    pub async fn read_all(&self) -> Result<Vec<(Cursor, Event)>, LogError> {
        let dir = self.inner.dir.clone();
        tokio::task::spawn_blocking(move || read_all_locked(&dir)).await?
    }

    /// Start observing after the existing rows, under the append lock.
    pub(crate) async fn observer_offset(&self) -> Result<u64, LogError> {
        let dir = self.inner.dir.clone();
        tokio::task::spawn_blocking(move || {
            lease::with_shared_lock(&dir, || match fs::metadata(log_path(&dir)) {
                Ok(metadata) => Ok(metadata.len()),
                Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(0),
                Err(error) => Err(error),
            })
        })
        .await?
        .map_err(LogError::from)
    }

    /// A bounded page of accepted rows, without rereading earlier payloads.
    /// The generation and committed cursor are checked under the append lock:
    /// a failed write or another lease cannot become observer output.
    pub(crate) async fn read_observed(
        &self,
        offset: u64,
        limit: usize,
    ) -> Result<(u64, Vec<(Cursor, Event)>), LogError> {
        let dir = self.inner.dir.clone();
        let generation = self.inner.generation;
        tokio::task::spawn_blocking(move || {
            lease::with_shared_lock(&dir, || {
                let meta = lease::read_meta(&dir)?;
                if meta.generation != generation {
                    return Err(io::Error::other("the observed log lease was superseded"));
                }
                let file = match fs::File::open(log_path(&dir)) {
                    Ok(file) => file,
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {
                        return Ok((offset, Vec::new()));
                    }
                    Err(error) => return Err(error),
                };
                let mut reader = io::BufReader::new(file);
                reader.seek(SeekFrom::Start(offset))?;
                let mut next_offset = offset;
                let mut rows = Vec::with_capacity(limit);
                let mut line = String::new();
                while rows.len() < limit {
                    line.clear();
                    if reader.read_line(&mut line)? == 0 || !line.ends_with('\n') {
                        break;
                    }
                    if !line.trim().is_empty() {
                        let row: Row = serde_json::from_str(&line)
                            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
                        if row.cursor.0 > meta.last_cursor {
                            break;
                        }
                        rows.push((row.cursor, row.event));
                    }
                    next_offset = reader.stream_position()?;
                }
                Ok((next_offset, rows))
            })
            .map_err(LogError::from)
        })
        .await?
    }

    fn ensure_live(&self) -> Result<(), Fenced> {
        if self.inner.lost.load(Ordering::Acquire) {
            return Err(Fenced::new("lease lost"));
        }
        Ok(())
    }

    async fn append_events(&self, events: Vec<Event>) -> Result<Vec<Cursor>, Fenced> {
        self.ensure_live()?;
        if events.is_empty() {
            return Ok(Vec::new());
        }
        let dir = self.inner.dir.clone();
        let generation = self.inner.generation;
        let outcome = tokio::task::spawn_blocking(move || append_locked(&dir, generation, &events))
            .await
            .map_err(|error| Fenced::new(format!("background append task panicked: {error}")))?
            .map_err(|error| Fenced::new(format!("log write failed: {error}")))?;
        match outcome {
            AppendOutcome::Fenced { held, current } => {
                self.inner.lost.store(true, Ordering::Release);
                Err(Fenced::new(format!(
                    "lease generation moved: held {held}, now {current}"
                )))
            }
            AppendOutcome::Wrote(cursors) => Ok(cursors),
        }
    }
}

impl Log for LocalLog {
    async fn append(&self, events: &[Event]) -> Result<Vec<Cursor>, Fenced> {
        self.append_events(events.to_vec()).await
    }

    /// No coalescing (see the module doc): each call is its own row.
    async fn append_text(&self, text: String) -> Result<(), Fenced> {
        self.append_events(vec![Event::TextDelta { text }])
            .await
            .map(|_| ())
    }

    async fn control_since(&self, after: Cursor) -> Result<Vec<(Cursor, Event)>, Fenced> {
        self.ensure_live()?;
        let all = self.read_all().await.map_err(Fenced::from)?;
        Ok(all
            .into_iter()
            .filter(|(cursor, event)| *cursor > after && event.is_control())
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dex_loop::{PrincipalId, TurnId};
    use tempfile::TempDir;

    fn thread() -> ThreadId {
        ThreadId {
            org: "org-1".into(),
            workspace: "ws-1".into(),
            thread: "thread-1".into(),
        }
    }

    fn user_message(text: &str) -> Event {
        Event::UserMessage {
            interaction_mode: dex_loop::InteractionMode::Unspecified,
            turn: TurnId::new("t1"),
            message_id: None,
            model_binding: None,
            voice: None,
            principal: PrincipalId::new("alice"),
            text: text.into(),
            attachments: Vec::new(),
            client_tools: Vec::new(),
            authorized_tools: Vec::new(),
            approval_mode: dex_loop::ApprovalMode::Headless,
        }
    }

    #[tokio::test]
    async fn observer_pages_resume_after_existing_rows_without_duplicates() {
        let dir = TempDir::new().expect("tempdir");
        let log = LocalLog::acquire(dir.path(), &thread())
            .await
            .expect("acquire");
        log.append(&[user_message("already visible")])
            .await
            .expect("prefix");
        let mut offset = log.observer_offset().await.expect("offset");
        for index in 0..5 {
            log.append_text(format!("delta-{index}"))
                .await
                .expect("text");
        }
        let mut seen = Vec::new();
        loop {
            let (next, page) = log.read_observed(offset, 2).await.expect("page");
            assert!(page.len() <= 2);
            if page.is_empty() {
                assert_eq!(next, offset, "EOF does not advance the reader");
                break;
            }
            assert!(next > offset);
            offset = next;
            seen.extend(page);
        }
        assert_eq!(
            seen,
            (0..5)
                .map(|index| (
                    Cursor(index + 2),
                    Event::TextDelta {
                        text: format!("delta-{index}")
                    }
                ))
                .collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn observer_never_delivers_uncommitted_or_partial_trailing_rows() {
        let dir = TempDir::new().expect("tempdir");
        let log = LocalLog::acquire(dir.path(), &thread())
            .await
            .expect("acquire");
        log.append(&[Event::Interrupted])
            .await
            .expect("accepted event");
        // A failed append can leave bytes in the file without advancing the
        // committed cursor. A partial trailing line cannot be decoded yet.
        let mut file = OpenOptions::new()
            .append(true)
            .open(log_path(&log.inner.dir))
            .expect("log");
        serde_json::to_writer(
            &mut file,
            &Row {
                cursor: Cursor(2),
                event: Event::TextDelta {
                    text: "not committed".into(),
                },
            },
        )
        .expect("row");
        file.write_all(b"\n{\"cursor\":3").expect("partial row");
        let (offset, rows) = log.read_observed(0, 32).await.expect("accepted rows");
        assert_eq!(rows, vec![(Cursor(1), Event::Interrupted)]);
        let (next, rows) = log
            .read_observed(offset, 32)
            .await
            .expect("uncommitted tail");
        assert!(rows.is_empty());
        assert_eq!(next, offset);
        lease::with_locked_meta(&log.inner.dir, |meta| {
            meta.last_cursor = 2;
            Ok(())
        })
        .expect("commit complete row");
        let (next, rows) = log.read_observed(offset, 32).await.expect("committed row");
        assert_eq!(
            rows,
            vec![(
                Cursor(2),
                Event::TextDelta {
                    text: "not committed".into()
                }
            )]
        );
        let (end, rows) = log.read_observed(next, 32).await.expect("partial tail");
        assert!(rows.is_empty());
        assert_eq!(end, next);
    }

    #[tokio::test]
    async fn observer_cannot_read_writes_from_a_replacement_lease() {
        let dir = TempDir::new().expect("tempdir");
        let first = LocalLog::acquire(dir.path(), &thread())
            .await
            .expect("first");
        let second = LocalLog::acquire(dir.path(), &thread())
            .await
            .expect("second");
        second
            .append(&[Event::Interrupted])
            .await
            .expect("replacement write");
        assert!(
            matches!(first.read_observed(0, 32).await, Err(error) if error.to_string().contains("superseded"))
        );
    }

    #[tokio::test]
    async fn appended_events_round_trip_with_increasing_cursors() {
        let dir = TempDir::new().expect("tempdir");
        let log = LocalLog::acquire(dir.path(), &thread())
            .await
            .expect("acquire");
        let cursors = log
            .append(&[user_message("hi"), Event::Interrupted])
            .await
            .expect("append");
        assert_eq!(cursors, vec![Cursor(1), Cursor(2)]);
        let all = log.read_all().await.expect("read");
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].0, Cursor(1));
        assert_eq!(all[1].0, Cursor(2));
    }

    #[tokio::test]
    async fn control_since_only_returns_control_events_after_the_cursor() {
        let dir = TempDir::new().expect("tempdir");
        let log = LocalLog::acquire(dir.path(), &thread())
            .await
            .expect("acquire");
        log.append(&[user_message("hi")]).await.expect("append");
        let interrupt = Event::Interrupt {
            principal: PrincipalId::new("alice"),
        };
        log.append(std::slice::from_ref(&interrupt))
            .await
            .expect("append");
        let control = log.control_since(Cursor::START).await.expect("control");
        assert_eq!(control, vec![(Cursor(2), interrupt)]);
    }

    #[tokio::test]
    async fn a_second_acquire_fences_the_first_leases_next_write() {
        let dir = TempDir::new().expect("tempdir");
        let first = LocalLog::acquire(dir.path(), &thread())
            .await
            .expect("first acquire");
        let _second = LocalLog::acquire(dir.path(), &thread())
            .await
            .expect("second acquire");
        let result = first.append(&[user_message("late")]).await;
        assert!(matches!(result, Err(error) if error.reason.contains("lease generation moved")));
    }

    #[tokio::test]
    async fn a_fenced_lease_stays_fenced_without_rechecking_disk() {
        let dir = TempDir::new().expect("tempdir");
        let first = LocalLog::acquire(dir.path(), &thread())
            .await
            .expect("first acquire");
        let _second = LocalLog::acquire(dir.path(), &thread())
            .await
            .expect("second acquire");
        assert!(first.append(&[user_message("one")]).await.is_err());
        // A third acquire would let a *fresh* lease through, but `first` is
        // latched lost in-process and must not silently recover.
        assert!(first.append(&[user_message("two")]).await.is_err());
    }

    #[tokio::test]
    async fn empty_thread_reads_back_as_no_events() {
        let dir = TempDir::new().expect("tempdir");
        let log = LocalLog::acquire(dir.path(), &thread())
            .await
            .expect("acquire");
        assert!(log.read_all().await.expect("read").is_empty());
    }

    #[tokio::test]
    async fn thread_id_components_cannot_escape_the_root() {
        let dir = TempDir::new().expect("tempdir");
        let escaping = ThreadId {
            org: "../../etc".into(),
            workspace: "ws".into(),
            thread: "t".into(),
        };
        // `sanitize` keeps literal `.` characters (legitimate in an org or
        // workspace name), so a sanitized component can still *contain* two
        // dots; the property that matters is that no component *is* `..`,
        // which is what would let `Path::join` walk back up to a parent.
        let path = thread_dir(dir.path(), &escaping);
        assert!(path.starts_with(dir.path()));
        assert!(
            path.strip_prefix(dir.path())
                .expect("under root")
                .components()
                .all(|component| component != std::path::Component::ParentDir)
        );

        let log = LocalLog::acquire(dir.path(), &escaping)
            .await
            .expect("acquire");
        log.append(&[user_message("hi")]).await.expect("append");
        let mut entries = fs::read_dir(dir.path()).expect("read root");
        let child = entries.next().expect("one child").expect("entry");
        assert!(child.path().starts_with(dir.path()));
    }
}

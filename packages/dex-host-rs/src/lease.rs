//! Generation fencing for [`crate::log::LocalLog`], mirroring `dex-runtime`'s
//! `PgLog`: a lease holds one generation number, persisted next to the log
//! instead of in a Postgres row. `acquire` invalidates any earlier lease for
//! the same thread by bumping the persisted generation under an exclusive
//! file lock; every later write re-checks the persisted value against the
//! generation this lease captured, under the same lock, so a superseded
//! writer is refused instead of silently appending after a new owner took
//! over.

use std::fs::{self, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The durable state fenced together, the same way `PgLog`'s thread row
/// holds `lease_generation` and (via `MAX(cursor)`) the last cursor in one
/// row locked by one transaction.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub(crate) struct Meta {
    pub(crate) generation: u64,
    pub(crate) last_cursor: i64,
}

/// Runs `body` with an exclusive lock on `lock_path`'s file, creating the
/// lock file (and its parent directory) if needed. Blocking: callers on an
/// async runtime must run this inside `spawn_blocking`.
pub(crate) fn with_locked_meta<R>(
    dir: &Path,
    body: impl FnOnce(&mut Meta) -> io::Result<R>,
) -> io::Result<R> {
    fs::create_dir_all(dir)?;
    let lock_file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(lock_path(dir))?;
    let mut lock = fd_lock::RwLock::new(lock_file);
    let _guard = lock.write()?;
    let mut meta = read_meta(dir)?;
    let result = body(&mut meta)?;
    write_meta(dir, &meta)?;
    Ok(result)
}

/// Runs `body` with a shared lock on the same lock file `with_locked_meta`
/// takes exclusively: any number of readers may hold this at once, but none
/// of them overlaps a writer's `write_all` to the log file, so a reader
/// never observes a torn trailing line from a write still in progress.
/// Blocking: callers on an async runtime must run this inside
/// `spawn_blocking`.
pub(crate) fn with_shared_lock<R>(
    dir: &Path,
    body: impl FnOnce() -> io::Result<R>,
) -> io::Result<R> {
    fs::create_dir_all(dir)?;
    let lock_file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(lock_path(dir))?;
    let lock = fd_lock::RwLock::new(lock_file);
    let _guard = lock.read()?;
    body()
}

pub(crate) fn lock_path(dir: &Path) -> PathBuf {
    dir.join("lease.lock")
}

pub(crate) fn meta_path(dir: &Path) -> PathBuf {
    dir.join("meta.json")
}

pub(crate) fn read_meta(dir: &Path) -> io::Result<Meta> {
    match fs::read_to_string(meta_path(dir)) {
        Ok(text) => serde_json::from_str(&text)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Meta::default()),
        Err(error) => Err(error),
    }
}

fn write_meta(dir: &Path, meta: &Meta) -> io::Result<()> {
    let path = meta_path(dir);
    let temp = path.with_extension("json.tmp");
    let contents = serde_json::to_vec(meta).map_err(io::Error::other)?;
    fs::write(&temp, contents)?;
    fs::rename(&temp, &path)
}

/// Also used directly by tests that need a file handle open past this
/// module, e.g. to assert the lock file exists.
#[cfg(test)]
pub(crate) fn open_lock_file(dir: &Path) -> io::Result<fs::File> {
    OpenOptions::new().read(true).open(lock_path(dir))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn first_acquire_starts_at_generation_one() {
        let dir = TempDir::new().expect("tempdir");
        let generation = with_locked_meta(dir.path(), |meta| {
            meta.generation += 1;
            Ok(meta.generation)
        })
        .expect("acquire");
        assert_eq!(generation, 1);
    }

    #[test]
    fn concurrent_acquires_are_serialized_and_strictly_increasing() {
        let dir = TempDir::new().expect("tempdir");
        let dir_path = dir.path().to_path_buf();
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let dir_path = dir_path.clone();
                std::thread::spawn(move || {
                    with_locked_meta(&dir_path, |meta| {
                        meta.generation += 1;
                        Ok(meta.generation)
                    })
                    .expect("acquire")
                })
            })
            .collect();
        let mut generations: Vec<u64> = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect();
        generations.sort_unstable();
        assert_eq!(generations, (1..=8).collect::<Vec<_>>());
    }

    #[test]
    fn meta_persists_across_separate_locked_sections() {
        let dir = TempDir::new().expect("tempdir");
        with_locked_meta(dir.path(), |meta| {
            meta.generation = 5;
            meta.last_cursor = 12;
            Ok(())
        })
        .expect("write");
        let meta = read_meta(dir.path()).expect("read");
        assert_eq!(meta.generation, 5);
        assert_eq!(meta.last_cursor, 12);
    }

    #[test]
    fn shared_lock_waits_for_an_in_progress_exclusive_lock() {
        use std::sync::mpsc;
        use std::time::Duration;

        let dir = TempDir::new().expect("tempdir");
        let dir_path = dir.path().to_path_buf();
        let (writer_started, wait_for_writer_started) = mpsc::channel();
        let (release_writer, wait_to_release_writer) = mpsc::channel::<()>();

        let writer = std::thread::spawn({
            let dir_path = dir_path.clone();
            move || {
                with_locked_meta(&dir_path, |meta| {
                    meta.generation += 1;
                    writer_started.send(()).unwrap();
                    // Hold the exclusive lock until the reader has had a
                    // chance to observe it is blocked.
                    wait_to_release_writer
                        .recv_timeout(Duration::from_secs(5))
                        .unwrap();
                    Ok(())
                })
                .expect("writer")
            }
        });
        wait_for_writer_started
            .recv_timeout(Duration::from_secs(5))
            .unwrap();

        let reader =
            std::thread::spawn(move || with_shared_lock(&dir_path, || Ok(())).expect("reader"));
        // The reader cannot finish before the writer releases; if it did,
        // the shared lock was not actually exclusive against the writer.
        std::thread::sleep(Duration::from_millis(50));
        assert!(
            !reader.is_finished(),
            "reader must wait for the writer's exclusive lock"
        );

        release_writer.send(()).unwrap();
        writer.join().unwrap();
        reader.join().unwrap();
    }

    #[test]
    fn lock_file_is_created_alongside_meta() {
        let dir = TempDir::new().expect("tempdir");
        with_locked_meta(dir.path(), |meta| {
            meta.generation += 1;
            Ok(())
        })
        .expect("write");
        open_lock_file(dir.path()).expect("lock file exists");
    }
}

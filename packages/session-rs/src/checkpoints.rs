//! File-level checkpoints for `/rewind files`.
//!
//! Each user prompt submission in the TUI records a lightweight checkpoint of the
//! git worktree so the files an agent turn modifies can be restored later. This
//! mirrors grok-build's rewind behavior without ever creating commits or stashes
//! in the user's repository: all checkpoint data lives under the per-session
//! storage directory (`<sessions_dir>/checkpoints/<session_key>/<checkpoint_id>/`).
//!
//! # Snapshot mechanism (two-phase)
//!
//! A naive "snapshot everything before the turn" is too expensive on large
//! worktrees, and a naive "diff after the turn" cannot recover pre-turn content
//! for files that were clean at turn start. This module combines both:
//!
//! - **Begin (pre-turn)**: capture `git status --porcelain -z --no-renames` and
//!   the HEAD commit. For every *already dirty* tracked file, snapshot the full
//!   worktree content into the checkpoint's blob store. Untracked paths are
//!   recorded by name only (never snapshotted, so untracked dumps like
//!   `node_modules/` are never read).
//! - **Finalize (post-turn)**: re-run `git status` and diff it against the
//!   pre-turn snapshot. For each file the turn touched, the pre-turn content is
//!   either the begin-phase blob (dirty files) or `git show <head>:<path>`
//!   (files that were clean at turn start — the blob object exists in the repo
//!   regardless of later commits). The pre-turn blob and the post-turn content
//!   hash are recorded in a self-contained `checkpoint.json` manifest.
//!
//! # Restore semantics
//!
//! `/rewind files` restores the most recent checkpoint:
//!
//! - Modified/deleted files are written back **only if** their current content
//!   hash still equals the recorded post-turn hash. A mismatch means the user
//!   (or another tool) edited the file after the turn; the file is skipped with
//!   a warning and never clobbered.
//! - Untracked files created during the turn are deleted only if they are
//!   unchanged since the turn ended.
//! - The checkpoint is consumed (popped) once applied.
//!
//! Non-git directories: checkpointing silently no-ops and `/rewind files`
//! reports that checkpoints require a git worktree.

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use maestro_workspace::git;

/// Maximum number of checkpoints retained per session (FIFO eviction).
pub const MAX_CHECKPOINTS_PER_SESSION: usize = 20;

/// Safety cap on expanding an untracked directory created during a turn.
/// Larger directories (e.g. a freshly installed `node_modules/`) are not
/// recorded, so they are never deleted on rewind.
const MAX_EXPANDED_UNTRACKED_FILES: usize = 1000;

const MANIFEST_FILE: &str = "checkpoint.json";
const RETENTION_FILE: &str = "retention.json";

/// Information lost by eviction cannot be reconstructed from retained manifests:
/// gaps can also be turns with no file changes. Record the boundary before deletion.
#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RetentionBoundary {
    earliest_turn: usize,
    unlinked_eviction: bool,
}

/// On-disk store for one session's checkpoints.
pub struct CheckpointStore {
    root: PathBuf,
    legacy_root: PathBuf,
}

impl CheckpointStore {
    #[must_use]
    pub fn new(sessions_dir: &Path, session_id: &str) -> Self {
        let checkpoints_root = sessions_dir.join("checkpoints");
        Self {
            root: checkpoints_root.join(encode_session_component(session_id)),
            legacy_root: checkpoints_root.join(sanitize_component(session_id)),
        }
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The pre-v2 root used by existing sessions. New checkpoints are never
    /// written here, but old checkpoints remain readable and removable.
    pub(crate) fn legacy_root(&self) -> &Path {
        &self.legacy_root
    }

    /// Load all finalized checkpoints, oldest first. Corrupt or half-written
    /// checkpoint directories are ignored.
    #[must_use]
    pub fn list(&self) -> Vec<Checkpoint> {
        // Read v2 first so a checkpoint ID present in both roots resolves to
        // the collision-proof copy rather than the legacy copy.
        let mut by_id = HashMap::new();
        for root in [&self.root, &self.legacy_root] {
            for checkpoint in read_checkpoints_from_root(root) {
                by_id.entry(checkpoint.id.clone()).or_insert(checkpoint);
            }
        }
        let mut checkpoints = by_id.into_values().collect::<Vec<_>>();
        checkpoints.sort_by(|a, b| a.created_at.cmp(&b.created_at).then(a.id.cmp(&b.id)));
        checkpoints
    }

    #[must_use]
    pub fn latest(&self) -> Option<Checkpoint> {
        self.list().pop()
    }

    fn save(&self, checkpoint: &Checkpoint) -> io::Result<()> {
        let dir = self.new_checkpoint_dir(&checkpoint.id);
        // Usually a no-op by the time this runs (`begin_turn` already
        // created `dir` via the same synced path), but kept consistent
        // with it rather than a bare `fs::create_dir_all` in case `save`
        // is ever reached some other way.
        crate::fs_atomic::create_dir_all_synced(&dir)?;
        let bytes = serde_json::to_vec_pretty(checkpoint)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
        crate::fs_atomic::write_atomic(dir.join(MANIFEST_FILE), bytes)
    }

    pub fn remove(&self, id: &str) -> io::Result<()> {
        let component = sanitize_component(id);
        let mut first_error = None;
        for root in [&self.root, &self.legacy_root] {
            match fs::remove_dir_all(root.join(&component)) {
                Ok(()) => {}
                Err(err) if err.kind() == io::ErrorKind::NotFound => {}
                Err(err) => {
                    first_error.get_or_insert(err);
                }
            }
        }
        match first_error {
            Some(err) => Err(err),
            None => Ok(()),
        }
    }

    fn new_checkpoint_dir(&self, id: &str) -> PathBuf {
        self.root.join(sanitize_component(id))
    }

    fn checkpoint_dir_for_restore(&self, id: &str) -> PathBuf {
        let component = sanitize_component(id);
        let v2_dir = self.root.join(&component);
        if v2_dir.join(MANIFEST_FILE).is_file() {
            v2_dir
        } else {
            self.legacy_root.join(component)
        }
    }

    fn retention_boundary(&self) -> io::Result<RetentionBoundary> {
        match fs::read(self.root.join(RETENTION_FILE)) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                Ok(RetentionBoundary::default())
            }
            Err(error) => Err(error),
        }
    }

    fn save_retention_boundary(&self, boundary: &RetentionBoundary) -> io::Result<()> {
        crate::fs_atomic::write_atomic(
            self.root.join(RETENTION_FILE),
            serde_json::to_vec(boundary)?,
        )
    }

    /// Keep only the newest `keep` checkpoints. Persist lost coverage before
    /// deleting data; failure must leave the old snapshots recoverable.
    pub fn evict(&self, keep: usize) -> io::Result<()> {
        let checkpoints = self.list();
        if checkpoints.len() <= keep {
            return Ok(());
        }
        let evicted = &checkpoints[..checkpoints.len() - keep];
        let mut boundary = self.retention_boundary()?;
        for checkpoint in evicted {
            if let Some(index) = checkpoint.user_turn_index {
                boundary.earliest_turn = boundary.earliest_turn.max(index.saturating_add(1));
            } else {
                boundary.unlinked_eviction = true;
            }
        }
        self.save_retention_boundary(&boundary)?;
        for checkpoint in evicted {
            self.remove(&checkpoint.id)?;
        }
        Ok(())
    }
}

/// What happened to a file during the turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryKind {
    /// Tracked file content changed (or a deleted tracked file reappeared).
    Modified,
    /// Tracked file deleted during the turn.
    Deleted,
    /// File created during the turn (untracked, or previously absent).
    Created,
}

/// One file recorded in a checkpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileEntry {
    /// Repo-relative path (forward slashes).
    pub path: String,
    pub kind: EntryKind,
    /// SHA-256 of the pre-turn content; blob stored at `blobs/<hash>`.
    /// `None` means the file did not exist before the turn.
    pub pre_blob: Option<String>,
    /// SHA-256 of the post-turn content. `None` means the file was absent
    /// after the turn.
    pub post_hash: Option<String>,
    /// Post content exceeded the bounded review snapshot budget. The streamed
    /// hash still protects rewind and deletion of created files.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub post_snapshot_oversized: bool,
}

/// A finalized checkpoint manifest.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Checkpoint {
    pub id: String,
    /// RFC 3339 timestamp; also used for FIFO ordering.
    pub created_at: String,
    /// Excerpt of the prompt that started the turn.
    pub prompt: String,
    pub repo_root: PathBuf,
    /// HEAD commit at turn start (used to recover clean files' content).
    pub head: Option<String>,
    pub entries: Vec<FileEntry>,
    /// Number of saved user turns before this checkpoint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_turn_index: Option<usize>,
}

impl Checkpoint {
    /// Short id for display.
    #[must_use]
    pub fn short_id(&self) -> &str {
        self.id.get(..8).unwrap_or(&self.id)
    }
}

/// Pre-turn snapshot awaiting turn completion.
pub struct PendingTurn {
    cleanup: PendingDirectory,
    pub user_turn_index: Option<usize>,
    store: CheckpointStore,
    id: String,
    prompt: String,
    created_at: String,
    repo_root: PathBuf,
    head: Option<String>,
    /// Dirty tracked files at turn start: path → pre-turn content hash
    /// (`None` = file absent from the worktree at turn start).
    pre_dirty: HashMap<String, Option<String>>,
    /// Untracked paths at turn start. Never snapshotted or restored.
    pre_untracked: HashSet<String>,
    /// Dirty files whose content could not be read; never restored.
    unreadable: HashSet<String>,
}

struct PendingDirectory(Option<PathBuf>);

impl Drop for PendingDirectory {
    fn drop(&mut self) {
        if let Some(path) = &self.0 {
            let _ = fs::remove_dir_all(path);
        }
    }
}

/// Capture the pre-turn snapshot. Returns `None` when `cwd` is not inside a
/// git worktree (checkpointing silently no-ops there).
#[must_use]
pub fn begin_turn(
    cwd: &Path,
    sessions_dir: &Path,
    session_id: &str,
    prompt: &str,
) -> Option<PendingTurn> {
    begin_turn_inner(cwd, sessions_dir, session_id, prompt, false)
}

/// Bounded native review capture. Unreadable/oversized preimages invalidate the
/// review rather than silently publishing an empty or partial turn.
#[must_use]
pub fn begin_turn_snapshot(
    cwd: &Path,
    sessions_dir: &Path,
    session_id: &str,
    prompt: &str,
) -> Option<PendingTurn> {
    begin_turn_inner(cwd, sessions_dir, session_id, prompt, true)
}

fn begin_turn_inner(
    cwd: &Path,
    sessions_dir: &Path,
    session_id: &str,
    prompt: &str,
    bounded: bool,
) -> Option<PendingTurn> {
    let repo_root = dunce::canonicalize(git::repo_root(cwd)?).ok()?;
    let head = git_text(&repo_root, &["rev-parse", "--verify", "HEAD"]);
    let mut status = status_snapshot(&repo_root)?;
    if bounded {
        status.untracked = expand_native_untracked(&repo_root, &status.untracked)?;
        status.dirty.extend(status.untracked.iter().cloned());
    }

    let store = CheckpointStore::new(sessions_dir, session_id);
    let id = format!(
        "{}-{}",
        chrono::Utc::now().format("%Y%m%dT%H%M%S%.3fZ"),
        &uuid::Uuid::new_v4().simple().to_string()[..8]
    );
    let cp_dir = store.new_checkpoint_dir(&id);
    // `create_dir_all_synced` (not a bare `fs::create_dir_all`) so this
    // checkpoint directory's creation is itself durable: for a turn that
    // only creates new files, `store_blob` below is never called, making
    // this the *only* thing that creates `cp_dir` and its ancestors --
    // a bare `fs::create_dir_all` here would let `write_atomic` later see
    // an already-existing parent and skip its own new-directory fsync
    // logic entirely, so a power loss right after `finalize_turn` returns
    // could still make the whole checkpoint (and its manifest) disappear.
    crate::fs_atomic::create_dir_all_synced(cp_dir.join("blobs")).ok()?;

    let mut pre_dirty = HashMap::new();
    let mut unreadable = HashSet::new();
    let mut budget = 512 * 1024;
    for path in status.dirty {
        match checked_worktree_path(&repo_root, &path).and_then(|path| {
            if !bounded {
                return fs::read(path);
            }
            let mut bytes = Vec::new();
            fs::File::open(path)?
                .take(budget + 1)
                .read_to_end(&mut bytes)?;
            if bytes.len() as u64 > budget {
                return Err(io::Error::new(
                    io::ErrorKind::FileTooLarge,
                    "Preimage exceeds review limit",
                ));
            }
            budget = budget.saturating_sub(bytes.len() as u64);
            Ok(bytes)
        }) {
            Ok(bytes) => match store_blob(&cp_dir, &bytes) {
                Ok(hash) => {
                    pre_dirty.insert(path, Some(hash));
                }
                Err(_) => {
                    unreadable.insert(path);
                }
            },
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                pre_dirty.insert(path, None);
            }
            Err(_) => {
                // Unreadable (permissions, gitlink dir, ...): never restore it.
                unreadable.insert(path);
            }
        }
    }

    Some(PendingTurn {
        cleanup: PendingDirectory(Some(cp_dir)),
        user_turn_index: None,
        store,
        id,
        prompt: prompt_excerpt(prompt),
        created_at: chrono::Utc::now().to_rfc3339(),
        repo_root,
        head,
        pre_dirty,
        pre_untracked: status.untracked,
        unreadable,
    })
}

/// Diff the worktree against the pre-turn snapshot and persist a checkpoint
/// for everything the turn changed. Returns `None` when nothing changed (the
/// pending directory is removed) or the repository disappeared.
pub fn finalize_turn(pending: PendingTurn) -> io::Result<Option<Checkpoint>> {
    finalize_turn_inner(pending, false)
}

/// Retain a completed native turn even when it made no changes. Both sides are
/// saved in the existing checkpoint store; later reads never consult the worktree.
pub fn finalize_turn_snapshot(pending: PendingTurn) -> io::Result<Option<Checkpoint>> {
    finalize_turn_inner(pending, true)
}

fn finalize_turn_inner(
    mut pending: PendingTurn,
    retain_empty: bool,
) -> io::Result<Option<Checkpoint>> {
    let cp_dir = pending.store.new_checkpoint_dir(&pending.id);
    let Some(mut post) = status_snapshot(&pending.repo_root) else {
        let _ = fs::remove_dir_all(&cp_dir);
        return Ok(None);
    };

    if retain_empty {
        post.untracked = expand_native_untracked(&pending.repo_root, &post.untracked)
            .ok_or_else(|| io::Error::other("Untracked file list exceeds review limit"))?;
    }
    if retain_empty && !pending.unreadable.is_empty() {
        return Err(io::Error::other("Pre-turn files could not be captured"));
    }
    let mut post_budget = 512 * 1024;
    let dirty_pre_size = if retain_empty {
        pending
            .pre_dirty
            .values()
            .flatten()
            .filter_map(|hash| fs::metadata(cp_dir.join("blobs").join(hash)).ok())
            .map(|metadata| metadata.len())
            .sum::<u64>()
    } else {
        0
    };
    let mut pre_budget = (512_u64 * 1024).saturating_sub(dirty_pre_size);
    let mut entries: Vec<FileEntry> = Vec::new();

    // Tracked candidates: anything dirty before or after the turn.
    let mut candidates: HashSet<&str> = pending.pre_dirty.keys().map(String::as_str).collect();
    candidates.extend(post.dirty.iter().map(String::as_str));
    // A turn may commit its edits before ending. Compare the saved beginning
    // HEAD to the final worktree; current HEAD alone would report pristine.
    let committed_paths = if retain_empty {
        let mut command = Command::new("git");
        if let Some(head) = pending.head.as_deref() {
            command.args(["diff", "--name-only", "-z", "--no-renames", head, "--"]);
        } else {
            command.args(["ls-files", "-z"]);
        }
        let output = command.current_dir(&pending.repo_root).output()?;
        if !output.status.success() {
            return Err(io::Error::other(
                "Cannot enumerate turn changes from saved HEAD",
            ));
        }
        String::from_utf8(output.stdout)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?
            .split('\0')
            .filter(|path| !path.is_empty())
            .map(str::to_string)
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    candidates.extend(committed_paths.iter().map(String::as_str));
    for path in candidates {
        if pending.unreadable.contains(path) {
            continue;
        }
        if pending.pre_untracked.contains(path) && !retain_empty {
            // Pre-existing untracked file: content was never snapshotted.
            continue;
        }
        let pre_blob = match pending.pre_dirty.get(path) {
            Some(hash) => hash.clone(),
            None => {
                // Clean at turn start: pre-turn content is the HEAD blob. If
                // the file is not in HEAD it was created (and staged) during
                // the turn.
                if retain_empty
                    && pending
                        .head
                        .as_deref()
                        .and_then(|head| {
                            git_text(
                                &pending.repo_root,
                                &["cat-file", "-s", &format!("{head}:{path}")],
                            )
                        })
                        .and_then(|size| size.trim().parse::<u64>().ok())
                        .is_some_and(|size| size > pre_budget)
                {
                    return Err(io::Error::new(
                        io::ErrorKind::FileTooLarge,
                        "Preimage exceeds review limit",
                    ));
                }
                pending
                    .head
                    .as_deref()
                    .and_then(|head| head_blob(&pending.repo_root, head, path))
                    .map(|bytes| {
                        if retain_empty {
                            if bytes.len() as u64 > pre_budget {
                                return Err(io::Error::new(
                                    io::ErrorKind::FileTooLarge,
                                    "Preimage exceeds review limit",
                                ));
                            }
                            pre_budget -= bytes.len() as u64;
                        }
                        store_blob(&cp_dir, &bytes)
                    })
                    .transpose()?
            }
        };
        let (post_hash, post_snapshot_oversized) =
            match checked_worktree_path(&pending.repo_root, path)
                .and_then(|path| snapshot_file(&cp_dir, &path, retain_empty, &mut post_budget))
            {
                Ok(snapshot) => snapshot,
                Err(error) if retain_empty => return Err(error),
                Err(_) => continue,
            };
        if pre_blob == post_hash {
            continue;
        }
        let kind = match (&pre_blob, &post_hash) {
            (None, Some(_)) => EntryKind::Created,
            (Some(_), None) => EntryKind::Deleted,
            _ => EntryKind::Modified,
        };
        entries.push(FileEntry {
            path: path.to_string(),
            kind,
            pre_blob,
            post_hash,
            post_snapshot_oversized,
        });
    }

    // Untracked files created during the turn.
    for path in post.untracked.difference(&pending.pre_untracked) {
        if path.ends_with('/') {
            // Collapsed untracked directory: expand to per-file entries so
            // rewind can delete files individually and skip changed ones.
            let mut files = Vec::new();
            collect_files(
                &pending.repo_root.join(path),
                &pending.repo_root,
                &mut files,
            );
            if files.len() > MAX_EXPANDED_UNTRACKED_FILES {
                if retain_empty {
                    return Err(io::Error::other("Created file list exceeds review limit"));
                }
                continue;
            }
            for (rel, abs) in files {
                let snapshot = snapshot_file(&cp_dir, &abs, retain_empty, &mut post_budget);
                if retain_empty && snapshot.is_err() {
                    return Err(snapshot.expect_err("capture failed"));
                }
                if let Ok((Some(hash), post_snapshot_oversized)) = snapshot {
                    entries.push(FileEntry {
                        path: rel,
                        kind: EntryKind::Created,
                        pre_blob: None,
                        post_hash: Some(hash),
                        post_snapshot_oversized,
                    });
                }
            }
        } else {
            let snapshot = checked_worktree_path(&pending.repo_root, path)
                .and_then(|path| snapshot_file(&cp_dir, &path, retain_empty, &mut post_budget));
            if retain_empty && snapshot.is_err() {
                return Err(snapshot.expect_err("capture failed"));
            }
            if let Ok((Some(hash), post_snapshot_oversized)) = snapshot {
                entries.push(FileEntry {
                    path: path.clone(),
                    kind: EntryKind::Created,
                    pre_blob: None,
                    post_hash: Some(hash),
                    post_snapshot_oversized,
                });
            }
        }
    }

    if entries.is_empty() && !retain_empty {
        let _ = fs::remove_dir_all(&cp_dir);
        return Ok(None);
    }

    entries.sort_by(|a, b| a.path.cmp(&b.path));
    let checkpoint = Checkpoint {
        user_turn_index: pending.user_turn_index,
        id: pending.id,
        created_at: pending.created_at,
        prompt: pending.prompt,
        repo_root: pending.repo_root,
        head: pending.head,
        entries,
    };
    // Begin snapshots all pre-existing dirty files. Keep only data actually
    // referenced by this turn, including shared hashes referenced by any entry.
    let referenced: HashSet<&str> = checkpoint
        .entries
        .iter()
        .flat_map(|entry| [entry.pre_blob.as_deref(), entry.post_hash.as_deref()])
        .flatten()
        .collect();
    for blob in fs::read_dir(cp_dir.join("blobs"))? {
        let blob = blob?;
        if !referenced.contains(blob.file_name().to_string_lossy().as_ref()) {
            fs::remove_file(blob.path())?;
        }
    }
    pending.store.save(&checkpoint)?;
    pending.cleanup.0 = None;
    pending.store.evict(MAX_CHECKPOINTS_PER_SESSION)?;
    Ok(Some(checkpoint))
}

/// Bounded read of durable before/after content for one finalized turn.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnSnapshotFile {
    pub path: String,
    pub kind: EntryKind,
    pub availability: &'static str,
    pub before_content: Option<String>,
    pub after_content: Option<String>,
}

impl CheckpointStore {
    /// Returns None for missing or evicted turns. Missing old post blobs remain
    /// unavailable, rather than being reconstructed from today's worktree.
    pub fn turn_snapshot(
        &self,
        turn_index: Option<usize>,
        workspace: &Path,
    ) -> io::Result<Option<(usize, Vec<TurnSnapshotFile>)>> {
        let checkpoint = self.list().into_iter().rev().find(|checkpoint| {
            checkpoint
                .user_turn_index
                .is_some_and(|index| turn_index.is_none_or(|requested| index == requested))
        });
        let Some(checkpoint) = checkpoint else {
            return Ok(None);
        };
        let Some(repo_root) =
            git::repo_root(workspace).and_then(|path| dunce::canonicalize(path).ok())
        else {
            return Ok(None);
        };
        if checkpoint.repo_root != repo_root {
            return Ok(None);
        }
        if checkpoint.entries.len() > 1000
            || checkpoint
                .entries
                .iter()
                .map(|entry| entry.path.len())
                .sum::<usize>()
                > 128 * 1024
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Checkpoint file list exceeds review limit",
            ));
        }
        let dir = self.checkpoint_dir_for_restore(&checkpoint.id);
        let mut budget = 512 * 1024;
        let mut files = Vec::new();
        for entry in checkpoint.entries {
            let mut read = |hash: Option<&str>| -> io::Result<Vec<u8>> {
                let Some(hash) = hash else {
                    return Ok(Vec::new());
                };
                if hash.len() != 64 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "Invalid checkpoint blob hash",
                    ));
                }
                let path = dir.join("blobs").join(hash);
                let size = fs::metadata(&path)?.len();
                if size > budget {
                    return Err(io::Error::new(
                        io::ErrorKind::FileTooLarge,
                        "Snapshot exceeds review limit",
                    ));
                }
                let mut bytes = Vec::new();
                fs::File::open(path)?
                    .take(budget + 1)
                    .read_to_end(&mut bytes)?;
                if bytes.len() as u64 > budget {
                    return Err(io::Error::new(
                        io::ErrorKind::FileTooLarge,
                        "Snapshot exceeds review limit",
                    ));
                }
                if sha256(&bytes) != hash {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "Checkpoint content is corrupt",
                    ));
                }
                budget = budget.saturating_sub(bytes.len() as u64);
                Ok(bytes)
            };
            let content = if entry.post_snapshot_oversized {
                Err(io::Error::new(
                    io::ErrorKind::FileTooLarge,
                    "Snapshot exceeds review limit",
                ))
            } else {
                read(entry.pre_blob.as_deref()).and_then(|before| {
                    read(entry.post_hash.as_deref()).map(|after| (before, after))
                })
            };
            let (availability, before_content, after_content) = match content {
                Ok((before, after)) if before.contains(&0) || after.contains(&0) => {
                    ("binary", None, None)
                }
                Ok((before, after)) => {
                    match (String::from_utf8(before), String::from_utf8(after)) {
                        (Ok(before), Ok(after)) => ("patch", Some(before), Some(after)),
                        _ => ("binary", None, None),
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::FileTooLarge => {
                    ("truncated", None, None)
                }
                Err(_) => ("unavailable", None, None),
            };
            files.push(TurnSnapshotFile {
                path: entry.path,
                kind: entry.kind,
                availability,
                before_content,
                after_content,
            });
        }
        Ok(Some((
            checkpoint.user_turn_index.expect("selected linked turn"),
            files,
        )))
    }
}

fn expand_native_untracked(root: &Path, paths: &HashSet<String>) -> Option<HashSet<String>> {
    let mut expanded = HashSet::new();
    for path in paths {
        if path.ends_with('/') {
            let mut files = Vec::new();
            collect_files(&root.join(path), root, &mut files);
            expanded.extend(files.into_iter().map(|(relative, _)| relative));
        } else {
            expanded.insert(path.clone());
        }
        if expanded.len() > MAX_EXPANDED_UNTRACKED_FILES {
            return None;
        }
    }
    Some(expanded)
}

fn snapshot_file(
    cp_dir: &Path,
    path: &Path,
    retain: bool,
    budget: &mut u64,
) -> io::Result<(Option<String>, bool)> {
    if !retain {
        return file_hash(path).map(|hash| (hash, false));
    }
    let Some(hash) = file_hash(path)? else {
        return Ok((None, false));
    };
    // Hashing remains streaming. Retention never allocates an unbounded file.
    let mut bytes = Vec::new();
    fs::File::open(path)?
        .take(*budget + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > *budget {
        return Ok((Some(hash), true));
    }
    if sha256(&bytes) != hash {
        return Err(io::Error::other("File changed during checkpoint capture"));
    }
    store_blob(cp_dir, &bytes)?;
    *budget = budget.saturating_sub(bytes.len() as u64);
    Ok((Some(hash), false))
}

/// Outcome of restoring one checkpoint.
#[derive(Debug, Default)]
pub struct RestoreReport {
    pub checkpoint_id: String,
    pub prompt: String,
    /// Files whose pre-turn content was written back.
    pub restored: Vec<String>,
    /// Agent-created files deleted.
    pub deleted: Vec<String>,
    /// Files changed after the turn; left untouched.
    pub skipped: Vec<String>,
    /// Agent-created files already gone; nothing to do.
    pub gone: Vec<String>,
    /// Per-file failures; other files may already have been restored.
    pub failed: Vec<String>,
}

/// Restore the most recent checkpoint, consuming it. Returns `None` when the
/// session has no checkpoints.
pub fn restore_latest(store: &CheckpointStore) -> io::Result<Option<RestoreReport>> {
    let Some(checkpoint) = store.latest() else {
        return Ok(None);
    };
    restore_checkpoint(store, &checkpoint).map(Some)
}

/// Restore a specific checkpoint: write back snapshotted pre-turn content for
/// files the turn modified and delete files the turn created — but only when
/// the file is byte-identical to its post-turn state. Anything the user
/// touched after the turn is skipped. The checkpoint is consumed afterwards.
pub fn restore_checkpoint(
    store: &CheckpointStore,
    checkpoint: &Checkpoint,
) -> io::Result<RestoreReport> {
    let cp_dir = store.checkpoint_dir_for_restore(&checkpoint.id);
    let mut report = RestoreReport {
        checkpoint_id: checkpoint.id.clone(),
        prompt: checkpoint.prompt.clone(),
        ..RestoreReport::default()
    };

    for entry in &checkpoint.entries {
        let abs = match checked_worktree_path(&checkpoint.repo_root, &entry.path) {
            Ok(path) => path,
            Err(error) => {
                report.failed.push(format!("{}: {error}", entry.path));
                continue;
            }
        };
        let current_hash = match file_hash(&abs) {
            Ok(hash) => hash,
            Err(error) => {
                report.failed.push(format!("{}: {error}", entry.path));
                continue;
            }
        };
        if current_hash == entry.post_hash {
            // Post-turn state intact: safe to revert.
            match &entry.pre_blob {
                Some(hash) => {
                    let bytes = match read_blob(&cp_dir, hash) {
                        Ok(bytes) if sha256(&bytes) == *hash => bytes,
                        Ok(_) => {
                            report
                                .failed
                                .push(format!("{}: checkpoint content is corrupt", entry.path));
                            continue;
                        }
                        Err(error) => {
                            report.failed.push(format!("{}: {error}", entry.path));
                            continue;
                        }
                    };
                    // Atomic: this overwrites a file in the user's working
                    // tree, so a torn write here would corrupt their source
                    // file, not just internal checkpoint state. `write_atomic`
                    // itself preserves the existing file's permissions (e.g.
                    // executable bits on a restored script) as part of its
                    // own durability sync, so there is no separate
                    // `set_permissions` step here that could itself be lost
                    // to a crash between the write succeeding and the mode
                    // change landing.
                    if let Err(error) = crate::fs_atomic::write_atomic(&abs, &bytes) {
                        report.failed.push(format!("{}: {error}", entry.path));
                        continue;
                    }
                    report.restored.push(entry.path.clone());
                }
                None => {
                    // File created during the turn: remove it and prune any
                    // directories the turn created with it.
                    if let Err(error) = fs::remove_file(&abs) {
                        report.failed.push(format!("{}: {error}", entry.path));
                        continue;
                    }
                    prune_empty_dirs(abs.parent(), &checkpoint.repo_root);
                    report.deleted.push(entry.path.clone());
                }
            }
        } else if entry.kind == EntryKind::Created && current_hash.is_none() {
            report.gone.push(entry.path.clone());
        } else {
            report.skipped.push(entry.path.clone());
        }
    }

    if report.failed.is_empty() {
        if let Err(error) = store.remove(&checkpoint.id) {
            report.failed.push(format!("checkpoint cleanup: {error}"));
        }
    }
    Ok(report)
}

/// Copy retained checkpoint history into a new conversation branch.
/// Turn coordinates do not depend on the rewritten JSONL header length.
pub fn fork_before_turn(
    source: &CheckpointStore,
    target: &CheckpointStore,
    first_turn: usize,
) -> io::Result<()> {
    let mut boundary = source.retention_boundary()?;
    // A conversation-only fork can precede the source's coverage floor. Its
    // first new turn establishes a new file baseline, not the missing past.
    boundary.earliest_turn = if boundary.unlinked_eviction {
        first_turn
    } else {
        boundary.earliest_turn.min(first_turn)
    };
    boundary.unlinked_eviction = false;
    target.save_retention_boundary(&boundary)?;
    for checkpoint in source.list().into_iter().filter(|checkpoint| {
        checkpoint
            .user_turn_index
            .is_some_and(|index| index < first_turn)
    }) {
        let source_dir = source.checkpoint_dir_for_restore(&checkpoint.id);
        let target_dir = target.new_checkpoint_dir(&checkpoint.id);
        for hash in checkpoint
            .entries
            .iter()
            .filter_map(|entry| entry.pre_blob.as_ref())
        {
            let bytes = read_blob(&source_dir, hash)?;
            if sha256(&bytes) != *hash {
                return Err(io::Error::other("Retained checkpoint content is corrupt."));
            }
            crate::fs_atomic::create_dir_all_synced(target_dir.join("blobs"))?;
            store_blob(&target_dir, &bytes)?;
        }
        for hash in checkpoint
            .entries
            .iter()
            .filter_map(|entry| entry.post_hash.as_ref())
        {
            if source_dir.join("blobs").join(hash).is_file() {
                let bytes = read_blob(&source_dir, hash)?;
                if sha256(&bytes) != *hash {
                    return Err(io::Error::other("Retained checkpoint content is corrupt."));
                }
                crate::fs_atomic::create_dir_all_synced(target_dir.join("blobs"))?;
                store_blob(&target_dir, &bytes)?;
            }
        }
        target.save(&checkpoint)?;
    }
    Ok(())
}

/// Simulate the hash guards newest-first without modifying files.
pub fn preview_turns(
    store: &CheckpointStore,
    first_turn: usize,
) -> io::Result<(Vec<String>, Vec<String>)> {
    let mut simulated: HashMap<String, Option<String>> = HashMap::new();
    let mut protected = HashSet::new();
    let mut candidates = std::collections::BTreeSet::new();
    for checkpoint in checkpoints_for_turns(store, first_turn)?.into_iter().rev() {
        for entry in checkpoint.entries {
            if protected.contains(&entry.path) {
                continue;
            }
            let current = match simulated.get(&entry.path) {
                Some(hash) => hash.clone(),
                None => match checked_worktree_path(&checkpoint.repo_root, &entry.path)
                    .and_then(|path| file_hash(&path))
                {
                    Ok(hash) => hash,
                    Err(_) => {
                        protected.insert(entry.path);
                        continue;
                    }
                },
            };
            if current == entry.post_hash {
                simulated.insert(entry.path.clone(), entry.pre_blob.clone());
                candidates.insert(entry.path);
            } else {
                protected.insert(entry.path);
            }
        }
    }
    Ok((
        candidates.into_iter().collect(),
        protected.into_iter().collect(),
    ))
}

/// Restore a suffix of recorded turns, retaining manual changes across the chain.
pub fn restore_turns(store: &CheckpointStore, first_turn: usize) -> io::Result<Vec<RestoreReport>> {
    let checkpoints = checkpoints_for_turns(store, first_turn)?;
    let mut reports = Vec::new();
    let mut protected = HashSet::new();
    for mut checkpoint in checkpoints.into_iter().rev() {
        checkpoint
            .entries
            .retain(|entry| !protected.contains(&entry.path));
        let report = restore_checkpoint(store, &checkpoint)?;
        protected.extend(report.skipped.iter().cloned());
        // A failed newer write cannot authorize an older write to the same path.
        if !report.failed.is_empty() {
            reports.push(report);
            break;
        }
        reports.push(report);
    }
    Ok(reports)
}

pub fn checkpoints_for_turns(
    store: &CheckpointStore,
    first_turn: usize,
) -> io::Result<Vec<Checkpoint>> {
    let boundary = store.retention_boundary()?;
    if boundary.unlinked_eviction {
        return Err(io::Error::other(
            "Older file checkpoints without turn links were removed. Use /rewind files for retained checkpoints.",
        ));
    }
    if first_turn < boundary.earliest_turn {
        return Err(io::Error::other(format!(
            "File history before turn {} is no longer retained. Rewind the conversation separately or choose a newer turn.",
            boundary.earliest_turn + 1
        )));
    }
    let checkpoints = store.list();
    if checkpoints
        .iter()
        .any(|checkpoint| checkpoint.user_turn_index.is_none())
    {
        return Err(io::Error::other(
            "These checkpoints predate saved turn links. Use /rewind files to restore them separately.",
        ));
    }
    Ok(checkpoints
        .into_iter()
        .filter(|checkpoint| {
            checkpoint
                .user_turn_index
                .is_some_and(|index| index >= first_turn)
        })
        .collect())
}

// --- git helpers ---------------------------------------------------------

struct StatusSnapshot {
    /// Repo-relative paths of dirty tracked files (staged and/or unstaged).
    dirty: Vec<String>,
    /// Repo-relative paths of untracked files (dirs collapsed with trailing `/`).
    untracked: HashSet<String>,
}

fn status_snapshot(repo_root: &Path) -> Option<StatusSnapshot> {
    let output = Command::new("git")
        .args([
            "status",
            "--porcelain=v1",
            "-z",
            "--no-renames",
            "--untracked-files=normal",
        ])
        .current_dir(repo_root)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let mut dirty = Vec::new();
    let mut untracked = HashSet::new();
    for record in output.stdout.split(|byte| *byte == 0) {
        if record.len() < 4 {
            continue;
        }
        let path = String::from_utf8_lossy(&record[3..]).into_owned();
        if &record[..2] == b"??" {
            untracked.insert(path);
        } else {
            dirty.push(path);
        }
    }
    Some(StatusSnapshot { dirty, untracked })
}

fn git_text(repo_root: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(repo_root)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if text.is_empty() { None } else { Some(text) }
}

/// Fetch the committed content of `path` at `head`.
fn head_blob(repo_root: &Path, head: &str, path: &str) -> Option<Vec<u8>> {
    let output = Command::new("git")
        .arg("show")
        .arg(format!("{head}:{path}"))
        .current_dir(repo_root)
        .output()
        .ok()?;
    if output.status.success() {
        Some(output.stdout)
    } else {
        None
    }
}

// --- file helpers --------------------------------------------------------

fn sha256(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

/// SHA-256 of a file's content; `Ok(None)` when the file does not exist.
fn file_hash(path: &Path) -> io::Result<Option<String>> {
    let mut file = match fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(Some(format!("{:x}", hasher.finalize())))
}

/// Never follow a replaced symlink or a manifest path outside the worktree.
/// This guards observed paths; callers must still preserve the content hash
/// check because another editor can change files between preview and restore.
fn checked_worktree_path(root: &Path, relative: &str) -> io::Result<PathBuf> {
    // Captures store a resolved root, including platform aliases such as /tmp.
    // A different resolution later means an ancestor was replaced or moved.
    if dunce::canonicalize(root)? != root {
        return Err(io::Error::other("checkpoint worktree location has changed"));
    }
    let path = Path::new(relative);
    if path.as_os_str().is_empty()
        || path
            .components()
            .any(|part| !matches!(part, std::path::Component::Normal(_)))
    {
        return Err(io::Error::other("checkpoint path is not worktree-relative"));
    }
    let mut current = root.to_path_buf();
    for part in path.components() {
        current.push(part);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(io::Error::other("checkpoint path contains a symlink"));
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(current)
}

fn store_blob(cp_dir: &Path, bytes: &[u8]) -> io::Result<String> {
    let hash = sha256(bytes);
    let blob_path = cp_dir.join("blobs").join(&hash);
    if !blob_path.exists() {
        // The blob's filename *is* its content hash, so a torn write here
        // (name present, content wrong) would be silently trusted by
        // `read_blob` on a later restore and written back into the user's
        // repo without any integrity check. Atomic write removes the torn
        // half-write window entirely.
        crate::fs_atomic::write_atomic(&blob_path, bytes)?;
    }
    Ok(hash)
}

fn read_blob(cp_dir: &Path, hash: &str) -> io::Result<Vec<u8>> {
    fs::read(cp_dir.join("blobs").join(hash))
}

/// Recursively collect regular files under `dir` as (repo-relative, absolute)
/// pairs. Best effort; symlinks and unreadable entries are skipped.
fn collect_files(dir: &Path, repo_root: &Path, out: &mut Vec<(String, PathBuf)>) {
    if out.len() > MAX_EXPANDED_UNTRACKED_FILES {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if out.len() > MAX_EXPANDED_UNTRACKED_FILES {
            break;
        }
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            collect_files(&path, repo_root, out);
        } else if file_type.is_file() {
            if let Ok(rel) = path.strip_prefix(repo_root) {
                out.push((rel.to_string_lossy().replace('\\', "/"), path.clone()));
            }
        }
    }
}

/// Remove empty parent directories left behind by a deleted file, up to (but
/// not including) the repo root.
fn prune_empty_dirs(mut dir: Option<&Path>, repo_root: &Path) {
    while let Some(current) = dir {
        if current == repo_root || !current.starts_with(repo_root) {
            break;
        }
        if fs::remove_dir(current).is_err() {
            break;
        }
        dir = current.parent();
    }
}

fn sanitize_component(value: &str) -> String {
    let sanitized: String = value
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect();
    // An empty or dot-only component does not name a child directory at
    // all: joining it is a no-op or resolves to the parent (".."), which
    // would let `remove_dir_all` escape the checkpoints directory and
    // delete unrelated session data. Map such components to a safe name.
    if sanitized.chars().all(|c| c == '.') {
        "_".repeat(sanitized.len().max(1))
    } else {
        sanitized
    }
}

/// Encode a session ID without collapsing distinct IDs onto one directory.
/// The `v2~` prefix keeps the empty ID distinct from any path-like component
/// and makes the result impossible for the legacy sanitizer to produce (`~`
/// is not in its pass-through set).
fn encode_session_component(value: &str) -> String {
    let mut encoded = String::with_capacity(3 + value.len() * 2);
    encoded.push_str("v2~");
    for byte in value.as_bytes() {
        write!(&mut encoded, "{byte:02x}").expect("writing to a String cannot fail");
    }
    encoded
}

fn read_checkpoints_from_root(root: &Path) -> Vec<Checkpoint> {
    let mut checkpoints = Vec::new();
    let Ok(entries) = fs::read_dir(root) else {
        return checkpoints;
    };
    for entry in entries.flatten() {
        let manifest = entry.path().join(MANIFEST_FILE);
        let Ok(bytes) = fs::read(&manifest) else {
            continue;
        };
        if let Ok(checkpoint) = serde_json::from_slice::<Checkpoint>(&bytes) {
            checkpoints.push(checkpoint);
        }
    }
    checkpoints
}

fn prompt_excerpt(prompt: &str) -> String {
    const MAX_LEN: usize = 80;
    let condensed = prompt.split_whitespace().collect::<Vec<_>>().join(" ");
    if condensed.chars().count() <= MAX_LEN {
        return condensed;
    }
    let mut excerpt: String = condensed.chars().take(MAX_LEN - 3).collect();
    excerpt.push_str("...");
    excerpt
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_git(dir: &Path, args: &[&str]) {
        let output = Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .expect("git invocation failed");
        assert!(
            output.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
    }

    struct Fixture {
        _tmp: tempfile::TempDir,
        repo: PathBuf,
        sessions: PathBuf,
    }

    /// A temp git repo with one committed file (`a.rs`) and a separate temp
    /// sessions directory.
    fn git_fixture() -> Fixture {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let sessions = tmp.path().join("sessions");
        fs::create_dir_all(&repo).unwrap();
        fs::create_dir_all(&sessions).unwrap();
        run_git(&repo, &["init", "--quiet"]);
        run_git(&repo, &["config", "user.email", "test@example.com"]);
        run_git(&repo, &["config", "user.name", "Test"]);
        fs::write(repo.join("a.rs"), "original\n").unwrap();
        run_git(&repo, &["add", "a.rs"]);
        run_git(&repo, &["commit", "--quiet", "-m", "init"]);
        Fixture {
            _tmp: tmp,
            repo,
            sessions,
        }
    }

    fn store(fx: &Fixture) -> CheckpointStore {
        CheckpointStore::new(&fx.sessions, "session-1")
    }

    fn assert_overlapping_sessions_preserve_later_edits(
        fx: &Fixture,
        first_cwd: &Path,
        other_cwd: &Path,
    ) {
        fs::write(fx.repo.join("safe.rs"), "safe original\n").unwrap();
        fs::write(fx.repo.join("deleted.rs"), "deleted original\n").unwrap();
        fs::write(fx.repo.join("nested/other.rs"), "other original\n").unwrap();
        run_git(&fx.repo, &["add", "."]);
        run_git(
            &fx.repo,
            &["commit", "--quiet", "-m", "shared workspace files"],
        );

        let mut pending = begin_turn(first_cwd, &fx.sessions, "session-1", "first edit").unwrap();
        pending.user_turn_index = Some(0);
        fs::write(fx.repo.join("a.rs"), "first session\n").unwrap();
        fs::write(fx.repo.join("safe.rs"), "first safe edit\n").unwrap();
        fs::remove_file(fx.repo.join("deleted.rs")).unwrap();
        fs::write(fx.repo.join("created.rs"), "first created\n").unwrap();
        let first = finalize_turn(pending).unwrap().unwrap();

        let mut pending = begin_turn(other_cwd, &fx.sessions, "session-2", "other edit").unwrap();
        pending.user_turn_index = Some(0);
        fs::write(fx.repo.join("a.rs"), "other session\n").unwrap();
        fs::write(fx.repo.join("deleted.rs"), "other recreated\n").unwrap();
        fs::write(fx.repo.join("created.rs"), "other extended\n").unwrap();
        fs::write(fx.repo.join("nested/other.rs"), "other unrelated edit\n").unwrap();
        let other = finalize_turn(pending).unwrap().unwrap();
        assert_eq!(first.repo_root, other.repo_root);
        assert_eq!(first.repo_root, dunce::canonicalize(&fx.repo).unwrap());

        let (candidates, skipped) = preview_turns(&store(fx), 0).unwrap();
        assert_eq!(candidates, ["safe.rs"]);
        assert_eq!(
            skipped.into_iter().collect::<HashSet<_>>(),
            HashSet::from([
                "a.rs".to_string(),
                "created.rs".to_string(),
                "deleted.rs".to_string(),
            ])
        );
        let reports = restore_turns(&store(fx), 0).unwrap();
        assert_eq!(reports.len(), 1);
        assert!(reports[0].failed.is_empty());
        assert_eq!(reports[0].restored, ["safe.rs"]);
        assert!(reports[0].deleted.is_empty());
        assert_eq!(reports[0].skipped.len(), 3);
        for (path, content) in [
            ("a.rs", "other session\n"),
            ("created.rs", "other extended\n"),
            ("deleted.rs", "other recreated\n"),
            ("nested/other.rs", "other unrelated edit\n"),
            ("safe.rs", "safe original\n"),
        ] {
            assert_eq!(fs::read_to_string(fx.repo.join(path)).unwrap(), content);
        }
        assert!(store(fx).list().is_empty());
        let other_store = CheckpointStore::new(&fx.sessions, "session-2");
        assert_eq!(
            other_store.list().len(),
            1,
            "restoring one session keeps the other's history"
        );
        let report = restore_latest(&other_store).unwrap().unwrap();
        assert!(report.failed.is_empty());
        assert!(report.skipped.is_empty());
        assert_eq!(
            fs::read_to_string(fx.repo.join("a.rs")).unwrap(),
            "first session\n"
        );
        assert_eq!(
            fs::read_to_string(fx.repo.join("created.rs")).unwrap(),
            "other extended\n",
            "pre-existing untracked files remain outside the second session's checkpoint"
        );
        assert!(!fx.repo.join("deleted.rs").exists());
        assert_eq!(
            fs::read_to_string(fx.repo.join("nested/other.rs")).unwrap(),
            "other original\n"
        );
    }

    #[test]
    fn overlapping_session_roots_preserve_other_sessions_later_edits() {
        // A root and its nested cwd name the same Git worktree. Sharing it
        // must still allow restoring unchanged files, with per-file guards
        // protecting tracked, created, and recreated files owned by others.
        for (first_nested, other_nested) in [(false, false), (false, true), (true, false)] {
            let fx = git_fixture();
            let nested = fx.repo.join("nested");
            fs::create_dir(&nested).unwrap();
            let first_cwd = if first_nested { &nested } else { &fx.repo };
            let other_cwd = if other_nested { &nested } else { &fx.repo };
            assert_overlapping_sessions_preserve_later_edits(&fx, first_cwd, other_cwd);
        }
    }

    #[cfg(unix)]
    #[test]
    fn aliased_nested_session_preserves_other_sessions_later_edits() {
        let fx = git_fixture();
        let nested = fx.repo.join("nested");
        fs::create_dir(&nested).unwrap();
        let alias = fx._tmp.path().join("alias");
        std::os::unix::fs::symlink(&nested, &alias).unwrap();
        assert_overlapping_sessions_preserve_later_edits(&fx, &fx.repo, &alias);
    }

    #[test]
    fn restore_rechecks_manual_edits_after_preview() {
        let fx = git_fixture();
        let mut pending = begin_turn(&fx.repo, &fx.sessions, "session-1", "edit").unwrap();
        pending.user_turn_index = Some(0);
        fs::write(fx.repo.join("a.rs"), "agent edit\n").unwrap();
        fs::write(fx.repo.join("created.rs"), "agent created\n").unwrap();
        finalize_turn(pending).unwrap().unwrap();
        let (candidates, skipped) = preview_turns(&store(&fx), 0).unwrap();
        assert_eq!(candidates, ["a.rs", "created.rs"]);
        assert!(skipped.is_empty());

        fs::write(fx.repo.join("a.rs"), "manual edit after preview\n").unwrap();
        fs::write(
            fx.repo.join("created.rs"),
            "manual extension after preview\n",
        )
        .unwrap();
        let reports = restore_turns(&store(&fx), 0).unwrap();
        assert!(reports[0].failed.is_empty());
        assert!(reports[0].restored.is_empty());
        assert!(reports[0].deleted.is_empty());
        assert_eq!(reports[0].skipped, ["a.rs", "created.rs"]);
        assert_eq!(
            fs::read_to_string(fx.repo.join("a.rs")).unwrap(),
            "manual edit after preview\n"
        );
        assert_eq!(
            fs::read_to_string(fx.repo.join("created.rs")).unwrap(),
            "manual extension after preview\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn restore_rechecks_symlink_alias_after_preview() {
        let fx = git_fixture();
        let mut pending = begin_turn(&fx.repo, &fx.sessions, "session-1", "edit").unwrap();
        pending.user_turn_index = Some(0);
        fs::write(fx.repo.join("a.rs"), "agent edit\n").unwrap();
        finalize_turn(pending).unwrap().unwrap();
        assert_eq!(
            preview_turns(&store(&fx), 0).unwrap(),
            (vec!["a.rs".to_string()], vec![])
        );
        let outside = fx._tmp.path().join("outside.rs");
        // Matching bytes must not authorize a write through a new alias.
        fs::write(&outside, "agent edit\n").unwrap();
        fs::remove_file(fx.repo.join("a.rs")).unwrap();
        std::os::unix::fs::symlink(&outside, fx.repo.join("a.rs")).unwrap();

        let reports = restore_turns(&store(&fx), 0).unwrap();
        assert_eq!(reports[0].failed.len(), 1);
        assert!(reports[0].restored.is_empty());
        assert_eq!(fs::read_to_string(&outside).unwrap(), "agent edit\n");
        assert!(
            fs::symlink_metadata(fx.repo.join("a.rs"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(store(&fx).list().len(), 1);
    }

    #[test]
    fn restore_rejects_manifest_paths_outside_worktree() {
        for relative in [
            "../outside.rs",
            "/outside.rs",
            "",
            ".",
            "nested/../../outside.rs",
        ] {
            let fx = git_fixture();
            let outside = fx._tmp.path().join("outside.rs");
            fs::write(&outside, "agent edit\n").unwrap();
            let mut pending = begin_turn(&fx.repo, &fx.sessions, "session-1", "edit").unwrap();
            pending.user_turn_index = Some(0);
            fs::write(fx.repo.join("a.rs"), "agent edit\n").unwrap();
            let mut checkpoint = finalize_turn(pending).unwrap().unwrap();
            checkpoint.entries[0].path = relative.to_string();
            store(&fx).save(&checkpoint).unwrap();

            let (candidates, skipped) = preview_turns(&store(&fx), 0).unwrap();
            assert!(candidates.is_empty());
            assert_eq!(skipped, [relative]);
            let reports = restore_turns(&store(&fx), 0).unwrap();
            assert_eq!(reports[0].failed.len(), 1);
            assert!(reports[0].restored.is_empty());
            assert_eq!(fs::read_to_string(&outside).unwrap(), "agent edit\n");
            assert_eq!(
                fs::read_to_string(fx.repo.join("a.rs")).unwrap(),
                "agent edit\n"
            );
            assert_eq!(store(&fx).list().len(), 1);
        }
    }

    #[test]
    fn abandoned_capture_releases_its_directory() {
        let fx = git_fixture();
        fs::write(fx.repo.join("a.rs"), "dirty before capture").unwrap();
        for _ in 0..3 {
            let pending = begin_turn(&fx.repo, &fx.sessions, "session-1", "abandon").unwrap();
            let dir = pending.store.new_checkpoint_dir(&pending.id);
            assert!(dir.exists());
            drop(pending);
            assert!(!dir.exists(), "abandoned capture must release its blobs");
        }
    }

    #[test]
    fn native_turn_snapshot_uses_retained_content_and_isolates_turn_session_workspace() {
        let fx = git_fixture();
        let mut pending = begin_turn_snapshot(&fx.repo, &fx.sessions, "session-1", "edit").unwrap();
        pending.user_turn_index = Some(0);
        fs::write(fx.repo.join("a.rs"), "first saved change\n").unwrap();
        finalize_turn_snapshot(pending).unwrap().unwrap();
        fs::write(fx.repo.join("a.rs"), "later unrelated content\n").unwrap();
        let (index, files) = store(&fx)
            .turn_snapshot(Some(0), &fx.repo)
            .unwrap()
            .unwrap();
        assert_eq!(index, 0);
        assert_eq!(
            files[0].after_content.as_deref(),
            Some("first saved change\n")
        );
        assert_eq!(files[0].availability, "patch");
        assert!(
            store(&fx)
                .turn_snapshot(Some(1), &fx.repo)
                .unwrap()
                .is_none()
        );
        assert!(
            CheckpointStore::new(&fx.sessions, "another-session")
                .turn_snapshot(Some(0), &fx.repo)
                .unwrap()
                .is_none()
        );
        let other = git_fixture();
        assert!(
            store(&fx)
                .turn_snapshot(Some(0), &other.repo)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn native_empty_turn_is_saved_and_abandoned_capture_cleans_up() {
        let fx = git_fixture();
        let mut pending =
            begin_turn_snapshot(&fx.repo, &fx.sessions, "session-1", "empty").unwrap();
        pending.user_turn_index = Some(0);
        finalize_turn_snapshot(pending).unwrap().unwrap();
        assert!(
            store(&fx)
                .turn_snapshot(Some(0), &fx.repo)
                .unwrap()
                .unwrap()
                .1
                .is_empty()
        );
        let pending = begin_turn_snapshot(&fx.repo, &fx.sessions, "session-1", "abandon").unwrap();
        let directory = pending.store.new_checkpoint_dir(&pending.id);
        drop(pending);
        assert!(!directory.exists());
    }

    #[test]
    fn native_snapshot_missing_binary_corrupt_and_oversized_stay_distinct() {
        let fx = git_fixture();
        let mut pending =
            begin_turn_snapshot(&fx.repo, &fx.sessions, "session-1", "binary").unwrap();
        pending.user_turn_index = Some(0);
        fs::write(fx.repo.join("new.bin"), [0, 1, 2]).unwrap();
        let checkpoint = finalize_turn_snapshot(pending).unwrap().unwrap();
        let files = store(&fx)
            .turn_snapshot(Some(0), &fx.repo)
            .unwrap()
            .unwrap()
            .1;
        assert_eq!(files[0].availability, "binary");
        let hash = checkpoint.entries[0].post_hash.as_ref().unwrap();
        let blob = store(&fx)
            .new_checkpoint_dir(&checkpoint.id)
            .join("blobs")
            .join(hash);
        fs::write(&blob, "corrupt").unwrap();
        assert_eq!(
            store(&fx)
                .turn_snapshot(Some(0), &fx.repo)
                .unwrap()
                .unwrap()
                .1[0]
                .availability,
            "unavailable"
        );
        fs::remove_file(blob).unwrap();
        assert_eq!(
            store(&fx)
                .turn_snapshot(Some(0), &fx.repo)
                .unwrap()
                .unwrap()
                .1[0]
                .availability,
            "unavailable"
        );
        let mut pending =
            begin_turn_snapshot(&fx.repo, &fx.sessions, "session-1", "large").unwrap();
        pending.user_turn_index = Some(1);
        fs::write(fx.repo.join("large.bin"), vec![b'x'; 512 * 1024 + 1]).unwrap();
        let checkpoint = finalize_turn_snapshot(pending).unwrap().unwrap();
        assert!(checkpoint.entries[0].post_hash.is_some());
        assert!(checkpoint.entries[0].post_snapshot_oversized);
        assert_eq!(
            store(&fx)
                .turn_snapshot(Some(1), &fx.repo)
                .unwrap()
                .unwrap()
                .1[0]
                .availability,
            "truncated"
        );
        assert!(
            !store(&fx)
                .new_checkpoint_dir(&checkpoint.id)
                .join("blobs")
                .join(checkpoint.entries[0].post_hash.as_ref().unwrap())
                .exists()
        );
    }

    #[test]
    fn native_snapshot_captures_existing_and_new_untracked_files() {
        let fx = git_fixture();
        fs::create_dir(fx.repo.join("drafts")).unwrap();
        fs::write(fx.repo.join("drafts/existing.txt"), "before draft").unwrap();
        fs::write(fx.repo.join("stable.txt"), "unchanged").unwrap();
        let mut pending =
            begin_turn_snapshot(&fx.repo, &fx.sessions, "session-1", "draft").unwrap();
        pending.user_turn_index = Some(0);
        fs::write(fx.repo.join("drafts/existing.txt"), "after draft").unwrap();
        fs::write(fx.repo.join("drafts/new.txt"), "new draft").unwrap();
        finalize_turn_snapshot(pending).unwrap().unwrap();
        let files = store(&fx)
            .turn_snapshot(Some(0), &fx.repo)
            .unwrap()
            .unwrap()
            .1;
        assert_eq!(
            files
                .iter()
                .map(|file| file.path.as_str())
                .collect::<Vec<_>>(),
            ["drafts/existing.txt", "drafts/new.txt"]
        );
        assert_eq!(files[0].before_content.as_deref(), Some("before draft"));
        assert_eq!(files[0].after_content.as_deref(), Some("after draft"));
        assert_eq!(files[1].before_content.as_deref(), Some(""));
    }

    #[test]
    fn native_snapshot_reads_edit_and_commit_from_saved_start_head() {
        let fx = git_fixture();
        let mut pending =
            begin_turn_snapshot(&fx.repo, &fx.sessions, "session-1", "commit").unwrap();
        pending.user_turn_index = Some(0);
        let original = fs::read_to_string(fx.repo.join("a.rs")).unwrap();
        fs::write(fx.repo.join("a.rs"), "committed new content\n").unwrap();
        run_git(&fx.repo, &["add", "a.rs"]);
        run_git(&fx.repo, &["commit", "--quiet", "-m", "turn change"]);
        finalize_turn_snapshot(pending).unwrap().unwrap();
        let files = store(&fx)
            .turn_snapshot(Some(0), &fx.repo)
            .unwrap()
            .unwrap()
            .1;
        assert_eq!(files[0].before_content.as_deref(), Some(original.as_str()));
        assert_eq!(
            files[0].after_content.as_deref(),
            Some("committed new content\n")
        );
    }

    #[test]
    fn native_post_content_budget_is_shared_across_files() {
        let fx = git_fixture();
        let mut pending =
            begin_turn_snapshot(&fx.repo, &fx.sessions, "session-1", "several files").unwrap();
        pending.user_turn_index = Some(0);
        for name in ["large-a.txt", "large-b.txt", "large-c.txt"] {
            fs::write(fx.repo.join(name), vec![b'x'; 256 * 1024]).unwrap();
        }
        let checkpoint = finalize_turn_snapshot(pending).unwrap().unwrap();
        // Identical hashes can share disk content, but each retained pre/post
        // pair still consumes the capture budget and omitted entries are typed.
        assert!(
            checkpoint
                .entries
                .iter()
                .any(|entry| entry.post_snapshot_oversized)
        );
        let total = fs::read_dir(store(&fx).new_checkpoint_dir(&checkpoint.id).join("blobs"))
            .unwrap()
            .map(|entry| entry.unwrap().metadata().unwrap().len())
            .sum::<u64>();
        assert!(total <= 512 * 1024);
        assert!(
            store(&fx)
                .turn_snapshot(Some(0), &fx.repo)
                .unwrap()
                .unwrap()
                .1
                .iter()
                .any(|file| file.availability == "truncated")
        );
    }

    #[test]
    fn native_large_dirty_preimage_does_not_publish_pristine_turn() {
        let fx = git_fixture();
        fs::write(fx.repo.join("a.rs"), vec![b'x'; 512 * 1024 + 1]).unwrap();
        let pending =
            begin_turn_snapshot(&fx.repo, &fx.sessions, "session-1", "large dirty").unwrap();
        assert!(finalize_turn_snapshot(pending).is_err());
        assert!(
            store(&fx)
                .turn_snapshot(Some(0), &fx.repo)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn finalized_checkpoint_keeps_only_referenced_blobs() {
        let fx = git_fixture();
        fs::write(fx.repo.join("b.rs"), "other original").unwrap();
        run_git(&fx.repo, &["add", "b.rs"]);
        run_git(&fx.repo, &["commit", "--quiet", "-m", "second file"]);
        fs::write(fx.repo.join("a.rs"), "stable dirty content".repeat(1024)).unwrap();
        let pending = begin_turn(&fx.repo, &fx.sessions, "session-1", "change b").unwrap();
        fs::write(fx.repo.join("b.rs"), "changed b").unwrap();
        let checkpoint = finalize_turn(pending).unwrap().unwrap();
        let blobs = fs::read_dir(store(&fx).new_checkpoint_dir(&checkpoint.id).join("blobs"))
            .unwrap()
            .count();
        assert_eq!(
            blobs, 1,
            "unchanged dirty file must not accumulate in retained checkpoints"
        );
        assert_eq!(
            restore_latest(&store(&fx)).unwrap().unwrap().restored,
            ["b.rs"]
        );
    }

    #[test]
    fn untracked_expansion_stops_at_overflow_sentinel() {
        let fx = git_fixture();
        let dir = fx.repo.join("generated");
        fs::create_dir(&dir).unwrap();
        for index in 0..MAX_EXPANDED_UNTRACKED_FILES + 50 {
            fs::write(dir.join(index.to_string()), "x").unwrap();
        }
        let mut files = Vec::new();
        collect_files(&dir, &fx.repo, &mut files);
        assert_eq!(files.len(), MAX_EXPANDED_UNTRACKED_FILES + 1);
    }

    #[cfg(unix)]
    #[test]
    fn restore_refuses_replaced_symlink_parent() {
        let fx = git_fixture();
        let nested = fx.repo.join("nested");
        fs::create_dir(&nested).unwrap();
        fs::write(nested.join("file"), "before").unwrap();
        run_git(&fx.repo, &["add", "nested/file"]);
        run_git(&fx.repo, &["commit", "--quiet", "-m", "nested file"]);
        let pending = begin_turn(&fx.repo, &fx.sessions, "session-1", "edit").unwrap();
        fs::write(nested.join("file"), "after").unwrap();
        finalize_turn(pending).unwrap().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("file"), "after").unwrap();
        fs::remove_dir_all(&nested).unwrap();
        std::os::unix::fs::symlink(outside.path(), &nested).unwrap();
        let report = restore_latest(&store(&fx)).unwrap().unwrap();
        assert!(!report.failed.is_empty());
        assert_eq!(
            fs::read_to_string(outside.path().join("file")).unwrap(),
            "after"
        );
        assert_eq!(
            store(&fx).list().len(),
            1,
            "retain a failed checkpoint for recovery"
        );
    }

    #[cfg(unix)]
    #[test]
    fn restore_refuses_replaced_ancestor_above_worktree() {
        let fx = git_fixture();
        let holder = fx._tmp.path().join("holder");
        fs::create_dir(&holder).unwrap();
        let repo = holder.join("repo");
        fs::rename(&fx.repo, &repo).unwrap();
        let pending = begin_turn(&repo, &fx.sessions, "session-1", "edit").unwrap();
        fs::write(repo.join("a.rs"), "after").unwrap();
        finalize_turn(pending).unwrap().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::create_dir(outside.path().join("repo")).unwrap();
        fs::write(outside.path().join("repo/a.rs"), "after").unwrap();
        fs::rename(&holder, fx._tmp.path().join("original-holder")).unwrap();
        std::os::unix::fs::symlink(outside.path(), &holder).unwrap();
        let report = restore_latest(&store(&fx)).unwrap().unwrap();
        assert!(!report.failed.is_empty());
        assert_eq!(
            fs::read_to_string(outside.path().join("repo/a.rs")).unwrap(),
            "after"
        );
    }

    #[test]
    fn fork_after_legacy_eviction_can_establish_new_file_history() {
        let fx = git_fixture();
        let pending = begin_turn(&fx.repo, &fx.sessions, "session-1", "legacy").unwrap();
        fs::write(fx.repo.join("a.rs"), "parent").unwrap();
        finalize_turn(pending).unwrap().unwrap();
        store(&fx).evict(0).unwrap();
        let child = CheckpointStore::new(&fx.sessions, "child");
        fork_before_turn(&store(&fx), &child, 0).unwrap();
        let mut pending = begin_turn(&fx.repo, &fx.sessions, "child", "new edit").unwrap();
        pending.user_turn_index = Some(0);
        fs::write(fx.repo.join("a.rs"), "child").unwrap();
        finalize_turn(pending).unwrap().unwrap();
        assert!(preview_turns(&store(&fx), 0).is_err());
        assert!(
            restore_turns(&child, 0)
                .unwrap()
                .iter()
                .all(|r| r.failed.is_empty())
        );
        assert_eq!(fs::read_to_string(fx.repo.join("a.rs")).unwrap(), "parent");
    }

    #[test]
    fn rewind_turns_unwinds_repeated_edits_and_preserves_manual_changes() {
        let fx = git_fixture();
        fs::write(fx.repo.join("manual.rs"), "original").unwrap();
        run_git(&fx.repo, &["add", "manual.rs"]);
        run_git(&fx.repo, &["commit", "--quiet", "-m", "manual fixture"]);
        for index in 0..2 {
            let mut pending = begin_turn(&fx.repo, &fx.sessions, "session-1", "edit").unwrap();
            pending.user_turn_index = Some(index);
            fs::write(fx.repo.join("a.rs"), format!("agent {index}")).unwrap();
            fs::write(fx.repo.join("manual.rs"), format!("agent {index}")).unwrap();
            finalize_turn(pending).unwrap().unwrap();
        }
        fs::write(fx.repo.join("manual.rs"), "later manual edit").unwrap();
        let reports = restore_turns(&store(&fx), 0).unwrap();
        assert_eq!(reports.len(), 2);
        assert!(reports.iter().all(|report| report.failed.is_empty()));
        assert_eq!(
            fs::read_to_string(fx.repo.join("a.rs")).unwrap(),
            "original\n"
        );
        assert_eq!(
            fs::read_to_string(fx.repo.join("manual.rs")).unwrap(),
            "later manual edit"
        );
        assert!(
            reports
                .iter()
                .any(|report| report.skipped.contains(&"manual.rs".into()))
        );
    }

    #[test]
    fn rewind_turns_reports_missing_blob_without_claiming_complete_restore() {
        let fx = git_fixture();
        let mut pending = begin_turn(&fx.repo, &fx.sessions, "session-1", "edit").unwrap();
        pending.user_turn_index = Some(0);
        fs::write(fx.repo.join("a.rs"), "agent edit").unwrap();
        let checkpoint = finalize_turn(pending).unwrap().unwrap();
        let blob = checkpoint.entries[0].pre_blob.as_ref().unwrap();
        fs::remove_file(
            store(&fx)
                .new_checkpoint_dir(&checkpoint.id)
                .join("blobs")
                .join(blob),
        )
        .unwrap();
        let reports = restore_turns(&store(&fx), 0).unwrap();
        assert_eq!(reports[0].failed.len(), 1);
        assert!(reports[0].restored.is_empty());
        assert_eq!(
            fs::read_to_string(fx.repo.join("a.rs")).unwrap(),
            "agent edit"
        );
        assert_eq!(store(&fx).list().len(), 1);
    }

    #[test]
    fn dot_only_and_empty_ids_sanitize_to_safe_components() {
        // ".." joined onto the checkpoints dir resolves to the sessions dir
        // itself, and "" joins as a no-op; `remove_dir_all` on either would
        // delete unrelated session data instead of one session's
        // checkpoints.
        assert_eq!(sanitize_component(".."), "__");
        assert_eq!(sanitize_component("."), "_");
        assert_eq!(sanitize_component(""), "_");
        assert_eq!(sanitize_component("..."), "___");
        // Dots inside an otherwise normal component are unaffected.
        assert_eq!(sanitize_component("v1.2.3"), "v1.2.3");
    }

    #[test]
    fn store_root_for_dot_only_session_id_stays_under_checkpoints_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let store = CheckpointStore::new(tmp.path(), "..");
        assert_eq!(store.root(), tmp.path().join("checkpoints").join("v2~2e2e"));
    }

    #[test]
    fn session_components_do_not_collide_and_stay_under_checkpoints_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let checkpoints_root = tmp.path().join("checkpoints");
        let slash = CheckpointStore::new(tmp.path(), "team/a");
        let question = CheckpointStore::new(tmp.path(), "team?a");
        let dots = CheckpointStore::new(tmp.path(), "..");
        let underscores = CheckpointStore::new(tmp.path(), "__");
        let encoded_name = CheckpointStore::new(tmp.path(), "a");
        let legacy_name = CheckpointStore::new(tmp.path(), "v2-61");

        assert_ne!(slash.root(), question.root());
        assert_ne!(dots.root(), underscores.root());
        assert_ne!(encoded_name.root(), legacy_name.legacy_root());
        for root in [
            slash.root(),
            question.root(),
            dots.root(),
            underscores.root(),
        ] {
            assert!(root.starts_with(&checkpoints_root));
            assert!(
                root.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("v2~")),
                "new checkpoint roots must use the versioned encoding: {}",
                root.display()
            );
        }
    }

    #[test]
    fn legacy_checkpoints_are_listed_and_removed_alongside_v2_data() {
        let tmp = tempfile::tempdir().unwrap();
        let store = CheckpointStore::new(tmp.path(), "legacy/session");
        let checkpoint = Checkpoint {
            user_turn_index: None,
            id: "cp-legacy".to_string(),
            created_at: "2026-08-01T00:00:00Z".to_string(),
            prompt: "legacy".to_string(),
            repo_root: tmp.path().to_path_buf(),
            head: None,
            entries: Vec::new(),
        };
        let legacy_dir = store.legacy_root().join(&checkpoint.id);
        fs::create_dir_all(&legacy_dir).unwrap();
        fs::write(
            legacy_dir.join(MANIFEST_FILE),
            serde_json::to_vec(&checkpoint).unwrap(),
        )
        .unwrap();
        let v2_dir = store.new_checkpoint_dir(&checkpoint.id);
        fs::create_dir_all(&v2_dir).unwrap();

        let listed = store.list();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].prompt, "legacy");

        store.remove(&checkpoint.id).unwrap();
        assert!(!legacy_dir.exists());
        assert!(!v2_dir.exists());
    }

    #[test]
    fn begin_turn_noops_outside_git() {
        let tmp = tempfile::tempdir().unwrap();
        let sessions = tmp.path().join("sessions");
        fs::create_dir_all(&sessions).unwrap();
        assert!(begin_turn(tmp.path(), &sessions, "s1", "prompt").is_none());
    }

    #[test]
    fn finalize_without_changes_yields_no_checkpoint() {
        let fx = git_fixture();
        let pending = begin_turn(&fx.repo, &fx.sessions, "session-1", "do nothing").unwrap();
        let checkpoint = finalize_turn(pending).unwrap();
        assert!(checkpoint.is_none());
        assert!(store(&fx).list().is_empty());
    }

    #[test]
    fn captures_modified_and_created_files_and_restores_them() {
        let fx = git_fixture();

        // The user has uncommitted work in progress; it must survive a rewind.
        fs::write(fx.repo.join("wip.rs"), "user wip\n").unwrap();
        run_git(&fx.repo, &["add", "wip.rs"]);

        let pending = begin_turn(&fx.repo, &fx.sessions, "session-1", "change things").unwrap();
        fs::write(fx.repo.join("a.rs"), "agent edit\n").unwrap();
        fs::write(fx.repo.join("wip.rs"), "agent overwrote wip\n").unwrap();
        fs::write(fx.repo.join("new.txt"), "agent file\n").unwrap();

        let checkpoint = finalize_turn(pending).unwrap().expect("checkpoint");
        assert_eq!(checkpoint.entries.len(), 3);
        assert_eq!(store(&fx).list().len(), 1);

        let report = restore_latest(&store(&fx)).unwrap().expect("restore");
        assert_eq!(report.restored.len(), 2);
        assert_eq!(report.deleted, vec!["new.txt".to_string()]);
        assert!(report.skipped.is_empty());

        assert_eq!(
            fs::read_to_string(fx.repo.join("a.rs")).unwrap(),
            "original\n"
        );
        assert_eq!(
            fs::read_to_string(fx.repo.join("wip.rs")).unwrap(),
            "user wip\n"
        );
        assert!(!fx.repo.join("new.txt").exists());

        // The checkpoint was consumed.
        assert!(store(&fx).list().is_empty());
        assert!(restore_latest(&store(&fx)).unwrap().is_none());
    }

    #[test]
    fn restores_files_deleted_during_turn() {
        let fx = git_fixture();
        let pending = begin_turn(&fx.repo, &fx.sessions, "session-1", "delete a.rs").unwrap();
        fs::remove_file(fx.repo.join("a.rs")).unwrap();
        let checkpoint = finalize_turn(pending).unwrap().expect("checkpoint");
        assert_eq!(checkpoint.entries.len(), 1);
        assert_eq!(checkpoint.entries[0].kind, EntryKind::Deleted);

        let report = restore_latest(&store(&fx)).unwrap().expect("restore");
        assert_eq!(report.restored, vec!["a.rs".to_string()]);
        assert_eq!(
            fs::read_to_string(fx.repo.join("a.rs")).unwrap(),
            "original\n"
        );
    }

    #[test]
    fn restore_recreates_deleted_parent_tree_through_atomic_writer() {
        let fx = git_fixture();
        let nested = fx.repo.join("nested/deep/file.txt");
        fs::create_dir_all(nested.parent().unwrap()).unwrap();
        fs::write(&nested, "original\n").unwrap();
        run_git(&fx.repo, &["add", "nested/deep/file.txt"]);
        run_git(&fx.repo, &["commit", "--quiet", "-m", "add nested file"]);

        let pending =
            begin_turn(&fx.repo, &fx.sessions, "session-1", "delete nested tree").unwrap();
        fs::remove_file(&nested).unwrap();
        fs::remove_dir(nested.parent().unwrap()).unwrap();
        fs::remove_dir(fx.repo.join("nested")).unwrap();
        finalize_turn(pending).unwrap().expect("checkpoint");

        let report = restore_latest(&store(&fx)).unwrap().expect("restore");

        assert_eq!(report.restored, vec!["nested/deep/file.txt".to_string()]);
        assert_eq!(fs::read_to_string(&nested).unwrap(), "original\n");
    }

    /// Restoring a file replaces it via rename; the pre-existing file's
    /// permissions (e.g. an executable script's mode) must survive.
    #[cfg(unix)]
    #[test]
    fn restore_preserves_executable_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let fx = git_fixture();
        let script = fx.repo.join("run.sh");
        fs::write(&script, "#!/bin/sh\necho original\n").unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        run_git(&fx.repo, &["add", "run.sh"]);
        run_git(&fx.repo, &["commit", "--quiet", "-m", "add script"]);

        let pending = begin_turn(&fx.repo, &fx.sessions, "session-1", "edit script").unwrap();
        fs::write(&script, "#!/bin/sh\necho agent edit\n").unwrap();
        finalize_turn(pending).unwrap().expect("checkpoint");

        let report = restore_latest(&store(&fx)).unwrap().expect("restore");
        assert_eq!(report.restored, vec!["run.sh".to_string()]);
        assert_eq!(
            fs::read_to_string(&script).unwrap(),
            "#!/bin/sh\necho original\n"
        );
        assert_eq!(
            fs::metadata(&script).unwrap().permissions().mode() & 0o777,
            0o755,
            "restore must preserve the executable bits"
        );
    }

    #[test]
    fn skips_files_the_user_edited_after_the_turn() {
        let fx = git_fixture();
        let pending = begin_turn(&fx.repo, &fx.sessions, "session-1", "edit").unwrap();
        fs::write(fx.repo.join("a.rs"), "agent edit\n").unwrap();
        fs::write(fx.repo.join("new.txt"), "agent file\n").unwrap();
        finalize_turn(pending).unwrap().expect("checkpoint");

        // User edits both files after the turn ended.
        fs::write(fx.repo.join("a.rs"), "user follow-up edit\n").unwrap();
        fs::write(fx.repo.join("new.txt"), "user extended\n").unwrap();

        let report = restore_latest(&store(&fx)).unwrap().expect("restore");
        assert!(report.restored.is_empty());
        assert!(report.deleted.is_empty());
        assert_eq!(report.skipped.len(), 2);
        assert_eq!(
            fs::read_to_string(fx.repo.join("a.rs")).unwrap(),
            "user follow-up edit\n"
        );
        assert_eq!(
            fs::read_to_string(fx.repo.join("new.txt")).unwrap(),
            "user extended\n"
        );
    }

    #[test]
    fn cleans_up_untracked_directories_created_during_turn() {
        let fx = git_fixture();
        let pending = begin_turn(&fx.repo, &fx.sessions, "session-1", "scaffold").unwrap();
        fs::create_dir_all(fx.repo.join("gen/nested")).unwrap();
        fs::write(fx.repo.join("gen/nested/out.txt"), "generated\n").unwrap();
        fs::write(fx.repo.join("gen/top.txt"), "generated top\n").unwrap();
        let checkpoint = finalize_turn(pending).unwrap().expect("checkpoint");
        assert_eq!(checkpoint.entries.len(), 2);
        assert!(
            checkpoint
                .entries
                .iter()
                .all(|e| e.kind == EntryKind::Created)
        );

        let report = restore_latest(&store(&fx)).unwrap().expect("restore");
        assert_eq!(report.deleted.len(), 2);
        assert!(!fx.repo.join("gen").exists(), "empty dirs are pruned");
    }

    #[test]
    fn ignores_pre_existing_untracked_files() {
        let fx = git_fixture();
        fs::write(fx.repo.join("scratch.txt"), "user scratch\n").unwrap();

        let pending = begin_turn(&fx.repo, &fx.sessions, "session-1", "edit scratch").unwrap();
        fs::write(fx.repo.join("scratch.txt"), "agent touched it\n").unwrap();

        // Out of scope: pre-existing untracked content is never snapshotted,
        // so the turn produced no restorable checkpoint.
        assert!(finalize_turn(pending).unwrap().is_none());
        assert_eq!(
            fs::read_to_string(fx.repo.join("scratch.txt")).unwrap(),
            "agent touched it\n"
        );
    }

    #[test]
    fn rewind_rejects_evicted_history_before_changing_any_files() {
        let fx = git_fixture();
        for index in 0..3 {
            let mut pending = begin_turn(&fx.repo, &fx.sessions, "session-1", "edit").unwrap();
            pending.user_turn_index = Some(index);
            fs::write(fx.repo.join("a.rs"), format!("turn-{index}")).unwrap();
            finalize_turn(pending).unwrap().unwrap();
        }
        store(&fx).evict(2).unwrap();
        assert!(preview_turns(&store(&fx), 0).is_err());
        assert!(restore_turns(&store(&fx), 0).is_err());
        assert_eq!(fs::read_to_string(fx.repo.join("a.rs")).unwrap(), "turn-2");
        // Reopen the store: the coverage limit survives a process restart.
        assert!(checkpoints_for_turns(&store(&fx), 0).is_err());
        assert!(
            restore_turns(&store(&fx), 1)
                .unwrap()
                .iter()
                .all(|r| r.failed.is_empty())
        );
        assert_eq!(fs::read_to_string(fx.repo.join("a.rs")).unwrap(), "turn-0");
    }

    #[test]
    fn unavailable_retention_metadata_preserves_files_and_checkpoints() {
        let fx = git_fixture();
        let mut pending = begin_turn(&fx.repo, &fx.sessions, "session-1", "edit").unwrap();
        pending.user_turn_index = Some(0);
        fs::write(fx.repo.join("a.rs"), "after").unwrap();
        finalize_turn(pending).unwrap().unwrap();
        fs::create_dir(store(&fx).root().join(RETENTION_FILE)).unwrap();
        assert!(store(&fx).evict(0).is_err());
        assert_eq!(store(&fx).list().len(), 1);
        assert!(restore_turns(&store(&fx), 0).is_err());
        assert_eq!(fs::read_to_string(fx.repo.join("a.rs")).unwrap(), "after");
    }

    #[test]
    fn evicts_oldest_checkpoints_beyond_cap() {
        let fx = git_fixture();
        for round in 0..MAX_CHECKPOINTS_PER_SESSION + 2 {
            let pending = begin_turn(&fx.repo, &fx.sessions, "session-1", "round").unwrap();
            fs::write(fx.repo.join("a.rs"), format!("round {round}\n")).unwrap();
            finalize_turn(pending).unwrap().expect("checkpoint");
        }
        let checkpoints = store(&fx).list();
        assert_eq!(checkpoints.len(), MAX_CHECKPOINTS_PER_SESSION);
        // FIFO: the two oldest rounds were evicted.
        let restored = restore_latest(&store(&fx)).unwrap().expect("restore");
        assert_eq!(restored.restored, vec!["a.rs".to_string()]);
        assert_eq!(
            fs::read_to_string(fx.repo.join("a.rs")).unwrap(),
            format!("round {}\n", MAX_CHECKPOINTS_PER_SESSION)
        );
    }

    #[test]
    fn list_skips_a_torn_or_corrupt_manifest_without_panicking() {
        let fx = git_fixture();

        // A real checkpoint, finalized normally.
        let pending = begin_turn(&fx.repo, &fx.sessions, "session-1", "good").unwrap();
        fs::write(fx.repo.join("a.rs"), "changed\n").unwrap();
        finalize_turn(pending).unwrap().expect("checkpoint");

        // Simulate a crash mid-write of a second checkpoint's manifest: the
        // directory exists (created before the manifest write) but
        // checkpoint.json is truncated/invalid JSON.
        let store = store(&fx);
        let torn_dir = store.root().join("torn-checkpoint");
        fs::create_dir_all(&torn_dir).unwrap();
        fs::write(
            torn_dir.join(MANIFEST_FILE),
            "{\"id\": \"torn\", \"entries\":",
        )
        .unwrap();

        let checkpoints = store.list();
        assert_eq!(
            checkpoints.len(),
            1,
            "the torn checkpoint must be skipped, not crash the whole list"
        );
        assert_ne!(checkpoints[0].id, "torn");
    }
}

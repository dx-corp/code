//! Session index: a small on-disk cache of session summaries so listing UIs
//! (the TUI session switcher) can render previews without re-parsing every
//! JSONL file on each open.
//!
//! Adopted from codex's rollout state index (`codex-rs/rollout/src/state_db.rs`):
//! the filesystem stays the source of truth and the index is a rebuildable
//! cache. Invalidation follows the same mtime+size stamp used by the search
//! index in [`crate::search_cli`]: unchanged files are served from
//! `~/.composer/session-index.json`, changed files are re-read, and entries
//! for deleted files are pruned on every collect.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::entries::SessionEntry;
use super::reader::SessionReader;

const CACHE_SCHEMA_VERSION: u32 = 2;

/// Maximum characters kept for the first-user-message preview.
const MAX_PREVIEW_CHARS: usize = 160;

/// Cached summary of one session file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionIndexEntry {
    /// Session id from the file header.
    pub id: String,
    /// Persisted fork lineage from the session header.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session: Option<String>,
    /// Working directory the session was started in.
    pub cwd: String,
    /// RFC 3339 session start timestamp from the file header.
    pub started_at: String,
    /// Collapsed first user message, when the session has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
    /// User-set title from session metadata, when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Favorite flag from session metadata.
    #[serde(default)]
    pub favorite: bool,
    /// Total message count (user + assistant + tool results).
    pub message_count: usize,
}

/// A session index entry paired with its file location and freshness stamp.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexedSession {
    /// Absolute path to the session JSONL file.
    pub path: PathBuf,
    /// File modification time in milliseconds since the Unix epoch.
    pub modified_ms: u64,
    /// Cached summary of the file.
    pub entry: SessionIndexEntry,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CachedFile {
    mtime_ms: u64,
    len: u64,
    entry: SessionIndexEntry,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SessionIndexCache {
    #[serde(default)]
    version: u32,
    #[serde(default)]
    files: HashMap<String, CachedFile>,
}

/// Default on-disk location for the session index
/// (`~/.composer/session-index.json`, sibling of the search index).
#[must_use]
pub fn default_index_path() -> Option<PathBuf> {
    // sessions_dir is `~/.composer/agent/sessions/<slug>`; the index spans all slugs.
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let dir = super::writer::sessions_dir(&cwd.to_string_lossy());
    dir.parent()
        .and_then(Path::parent)
        .and_then(Path::parent)
        .map(|composer| composer.join("session-index.json"))
}

/// Collect summaries for every session under `root`, using `index_path` to
/// avoid re-reading unchanged files. Unparseable files are skipped and never
/// cached, so torn writes are retried on the next collect. Results are sorted
/// by file modification time, newest first.
#[must_use]
pub fn collect_sessions(root: &Path, index_path: Option<&Path>) -> Vec<IndexedSession> {
    let mut cache = index_path.map(load_cache).unwrap_or_default();
    let mut sessions = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    if root.is_dir() {
        collect_from_root(root, &mut cache, &mut seen, &mut sessions);
    }

    // Prune cache entries for files that no longer exist.
    cache.files.retain(|path, _| seen.contains(path));
    if let Some(path) = index_path {
        cache.version = CACHE_SCHEMA_VERSION;
        save_cache(path, &cache);
    }

    sessions.sort_by_key(|session| std::cmp::Reverse(session.modified_ms));
    sessions
}

fn load_cache(path: &Path) -> SessionIndexCache {
    let Ok(raw) = fs::read_to_string(path) else {
        return SessionIndexCache::default();
    };
    match serde_json::from_str::<SessionIndexCache>(&raw) {
        Ok(cache) if cache.version == CACHE_SCHEMA_VERSION => cache,
        _ => SessionIndexCache::default(),
    }
}

fn save_cache(path: &Path, cache: &SessionIndexCache) {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            let _ = fs::create_dir_all(parent);
        }
    }
    let Ok(raw) = serde_json::to_string(cache) else {
        return;
    };

    // Multiple Maestro processes can each rebuild and save this cache around
    // the same time (e.g. two sessions open in different projects, or one
    // running `maestro fork` while another's switcher refreshes). A plain
    // `fs::write` truncates the file in place before writing the new
    // contents, so a concurrent reader (`load_cache`, running in another
    // process) can observe a torn, half-written file if it reads between
    // the truncate and the write completing. Writing to a per-process temp
    // file first and renaming it over the target makes the swap atomic on
    // the same filesystem: readers always see either the complete old file
    // or the complete new file, never a partial one. A concurrent writer's
    // work can still be superseded by whichever rename lands last, but that
    // only costs a wasted re-parse of some files on the next open -- this
    // is a rebuildable cache, not a source of truth (see the module doc).
    let tmp_path = path.with_extension(format!("json.tmp.{}", std::process::id()));
    if fs::write(&tmp_path, raw).is_ok() {
        let _ = fs::rename(&tmp_path, path);
    }
}

fn file_stamp(metadata: &fs::Metadata) -> (u64, u64) {
    let mtime_ms = metadata
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    (mtime_ms, metadata.len())
}

fn collect_from_root(
    root: &Path,
    cache: &mut SessionIndexCache,
    seen: &mut HashSet<String>,
    sessions: &mut Vec<IndexedSession>,
) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_from_root(&path, cache, seen, sessions);
        } else if path.extension().is_some_and(|ext| ext == "jsonl") {
            collect_from_file(&path, cache, seen, sessions);
        }
    }
}

fn collect_from_file(
    path: &Path,
    cache: &mut SessionIndexCache,
    seen: &mut HashSet<String>,
    sessions: &mut Vec<IndexedSession>,
) {
    let key = path.to_string_lossy().to_string();
    let Ok(metadata) = fs::metadata(path) else {
        return;
    };
    let (mtime_ms, len) = file_stamp(&metadata);
    seen.insert(key.clone());

    if let Some(cached) = cache.files.get(&key) {
        if cached.mtime_ms == mtime_ms && cached.len == len {
            sessions.push(IndexedSession {
                path: path.to_path_buf(),
                modified_ms: mtime_ms,
                entry: cached.entry.clone(),
            });
            return;
        }
    }

    let Some(entry) = entry_from_file(path) else {
        // Do not cache failures: a partially-written file should be retried.
        return;
    };
    cache.files.insert(
        key,
        CachedFile {
            mtime_ms,
            len,
            entry: entry.clone(),
        },
    );
    sessions.push(IndexedSession {
        path: path.to_path_buf(),
        modified_ms: mtime_ms,
        entry,
    });
}

/// Build one index entry from a session file. Costs one header read (which
/// counts messages without parsing them) plus a bounded scan for the first
/// user message preview.
fn entry_from_file(path: &Path) -> Option<SessionIndexEntry> {
    let (header, stats, meta) = SessionReader::read_header(path).ok()?;
    Some(SessionIndexEntry {
        id: header.id,
        parent_session: header.parent_session,
        cwd: header.cwd,
        started_at: header.timestamp,
        preview: first_user_message_preview(path),
        title: meta.as_ref().and_then(|m| m.title.clone()),
        favorite: meta.as_ref().is_some_and(|m| m.favorite),
        message_count: stats.total_messages(),
    })
}

/// Scan for the first user message and return a single-line, length-capped
/// preview. Stops at the first hit, so the cost is tiny for typical sessions
/// where the opening prompt sits near the top of the file.
fn first_user_message_preview(path: &Path) -> Option<String> {
    let file = fs::File::open(path).ok()?;
    for line in BufReader::new(file).lines() {
        let Ok(line) = line else {
            return None;
        };
        if !(line.contains("\"type\":\"message\"") && line.contains("\"role\":\"user\"")) {
            continue;
        }
        if let Ok(SessionEntry::Message(entry)) = serde_json::from_str::<SessionEntry>(&line) {
            let preview = collapse_preview(&entry.message.text_content());
            if !preview.is_empty() {
                return Some(preview);
            }
        }
    }
    None
}

fn collapse_preview(text: &str) -> String {
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= MAX_PREVIEW_CHARS {
        return collapsed;
    }
    collapsed.chars().take(MAX_PREVIEW_CHARS).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    fn write_session(dir: &Path, name: &str, id: &str, user_text: &str) -> PathBuf {
        fs::create_dir_all(dir).unwrap();
        let path = dir.join(name);
        let mut file = fs::File::create(&path).unwrap();
        writeln!(
            file,
            r#"{{"type":"session","id":"{id}","timestamp":"2024-01-15T10:30:00Z","cwd":"/tmp/project","model":"openai/gpt-5.2","thinkingLevel":"medium"}}"#
        )
        .unwrap();
        writeln!(
            file,
            r#"{{"type":"message","timestamp":"2024-01-15T10:30:01Z","message":{{"role":"user","content":"{user_text}","timestamp":0}}}}"#
        )
        .unwrap();
        writeln!(
            file,
            r#"{{"type":"message","timestamp":"2024-01-15T10:30:02Z","message":{{"role":"assistant","content":[{{"type":"text","text":"On it."}}],"timestamp":1}}}}"#
        )
        .unwrap();
        path
    }

    #[test]
    fn index_builds_entries_across_project_dirs() {
        let temp = TempDir::new().unwrap();
        let root = temp.path().join("sessions");
        write_session(
            &root.join("--tmp-project-a--"),
            "a.jsonl",
            "session-a",
            "refactor the indexer",
        );
        write_session(
            &root.join("--tmp-project-b--"),
            "b.jsonl",
            "session-b",
            "ship it",
        );
        let index_path = temp.path().join("session-index.json");

        let sessions = collect_sessions(&root, Some(&index_path));

        assert_eq!(sessions.len(), 2);
        let a = sessions
            .iter()
            .find(|s| s.entry.id == "session-a")
            .expect("session-a indexed");
        assert_eq!(a.entry.cwd, "/tmp/project");
        assert_eq!(a.entry.started_at, "2024-01-15T10:30:00Z");
        assert_eq!(a.entry.preview.as_deref(), Some("refactor the indexer"));
        assert_eq!(a.entry.message_count, 2);
        assert!(index_path.exists(), "index file persisted");
    }

    #[test]
    fn index_invalidates_entries_when_files_change() {
        let temp = TempDir::new().unwrap();
        let root = temp.path().join("sessions");
        let dir = root.join("--tmp-project-a--");
        let path = write_session(&dir, "a.jsonl", "session-a", "first prompt");
        let index_path = temp.path().join("session-index.json");

        let first = collect_sessions(&root, Some(&index_path));
        assert_eq!(first[0].entry.message_count, 2);

        // Append another message; the size stamp changes and forces a re-read.
        let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
        writeln!(
            file,
            r#"{{"type":"message","timestamp":"2024-01-15T10:30:03Z","message":{{"role":"user","content":"and another thing","timestamp":2}}}}"#
        )
        .unwrap();
        drop(file);

        let second = collect_sessions(&root, Some(&index_path));
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].entry.message_count, 3);
        assert_eq!(second[0].entry.preview.as_deref(), Some("first prompt"));
    }

    #[test]
    fn fork_parent_survives_cache_reload_and_legacy_cache_upgrade() {
        let temp = TempDir::new().unwrap();
        let root = temp.path().join("sessions");
        let path = write_session(&root.join("project"), "child.jsonl", "child", "opening");
        let text = fs::read_to_string(&path).unwrap().replace(
            "\"id\":\"child\"",
            "\"id\":\"child\",\"parentSession\":\"parent\"",
        );
        fs::write(&path, text).unwrap();
        let cache = temp.path().join("session-index.json");
        let first = collect_sessions(&root, Some(&cache));
        assert_eq!(first[0].entry.parent_session.as_deref(), Some("parent"));
        let again = collect_sessions(&root, Some(&cache));
        assert_eq!(again[0].entry.parent_session.as_deref(), Some("parent"));
        let mut legacy: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&cache).unwrap()).unwrap();
        legacy["version"] = 1.into();
        for file in legacy["files"].as_object_mut().unwrap().values_mut() {
            file["entry"]
                .as_object_mut()
                .unwrap()
                .remove("parentSession");
        }
        fs::write(&cache, serde_json::to_string(&legacy).unwrap()).unwrap();
        let upgraded = collect_sessions(&root, Some(&cache));
        assert_eq!(upgraded[0].entry.parent_session.as_deref(), Some("parent"));
    }

    #[test]
    fn index_prunes_entries_for_deleted_files() {
        let temp = TempDir::new().unwrap();
        let root = temp.path().join("sessions");
        let dir = root.join("--tmp-project-a--");
        let path_a = write_session(&dir, "a.jsonl", "session-a", "keep me");
        write_session(&dir, "b.jsonl", "session-b", "delete me");
        let index_path = temp.path().join("session-index.json");

        assert_eq!(collect_sessions(&root, Some(&index_path)).len(), 2);

        fs::remove_file(&path_a).unwrap();
        let sessions = collect_sessions(&root, Some(&index_path));

        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].entry.id, "session-b");

        let raw = fs::read_to_string(&index_path).unwrap();
        let cache: SessionIndexCache = serde_json::from_str(&raw).unwrap();
        assert_eq!(cache.files.len(), 1, "stale entry pruned from disk cache");
        assert!(!raw.contains("session-a"));
    }

    /// Regression test for the review finding on #3129: `save_cache` must
    /// write through a per-process temp file and rename it into place
    /// rather than truncating `index_path` in place, so a concurrent reader
    /// in another Maestro process never observes a half-written file. This
    /// checks the observable contract (no leftover temp file, valid final
    /// content) rather than the rename's atomicity directly, which isn't
    /// practical to assert from a single-threaded test.
    #[test]
    fn save_cache_leaves_no_temp_file_behind_and_produces_valid_json() {
        let temp = TempDir::new().unwrap();
        let root = temp.path().join("sessions");
        write_session(
            &root.join("--tmp-project-a--"),
            "a.jsonl",
            "session-a",
            "atomic write check",
        );
        let index_path = temp.path().join("session-index.json");

        let _ = collect_sessions(&root, Some(&index_path));

        let dir_entries: Vec<_> = fs::read_dir(temp.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        assert!(
            !dir_entries.iter().any(|name| name.contains(".tmp.")),
            "no temp artifact should survive a successful save: {dir_entries:?}"
        );
        assert!(index_path.exists());
        let raw = fs::read_to_string(&index_path).unwrap();
        let cache: SessionIndexCache = serde_json::from_str(&raw).expect("valid JSON, not torn");
        assert_eq!(cache.files.len(), 1);
    }

    #[test]
    fn corrupt_index_triggers_full_rebuild() {
        let temp = TempDir::new().unwrap();
        let root = temp.path().join("sessions");
        write_session(
            &root.join("--tmp-project-a--"),
            "a.jsonl",
            "session-a",
            "still works",
        );
        let index_path = temp.path().join("session-index.json");
        fs::write(&index_path, b"not json").unwrap();

        let sessions = collect_sessions(&root, Some(&index_path));

        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].entry.id, "session-a");
    }

    #[test]
    fn unparseable_files_are_skipped_and_not_cached() {
        let temp = TempDir::new().unwrap();
        let root = temp.path().join("sessions");
        let dir = root.join("--tmp-project-a--");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("torn.jsonl"), b"{\"type\":\"session\"\n").unwrap();
        write_session(&dir, "ok.jsonl", "session-ok", "hello");
        let index_path = temp.path().join("session-index.json");

        let sessions = collect_sessions(&root, Some(&index_path));

        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].entry.id, "session-ok");
        let raw = fs::read_to_string(&index_path).unwrap();
        assert!(!raw.contains("torn.jsonl"));
    }

    #[test]
    fn preview_collapses_whitespace_and_caps_length() {
        let long = format!("{}\n{}", "word ".repeat(100), "tail");
        let preview = collapse_preview(&long);
        assert!(!preview.contains('\n'));
        assert_eq!(preview.chars().count(), MAX_PREVIEW_CHARS);

        assert_eq!(collapse_preview("  one\n two  "), "one two");
    }
}

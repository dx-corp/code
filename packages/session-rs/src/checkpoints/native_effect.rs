//! A first-command baseline reused only inside governed native effect wrappers.
use super::*;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Baseline {
    excluded_root: Option<String>,
    id: String,
    prompt: String,
    created_at: String,
    repo_root: PathBuf,
    head: Option<String>,
    pre_dirty: HashMap<String, Option<String>>,
    pre_untracked: HashSet<String>,
    unreadable: HashSet<String>,
}

/// Keep the original preimages across several approved commands in one turn.
/// A corrupt or lost baseline stays unavailable; it is never recaptured after
/// an effect and incorrectly presented as the beginning of the turn.
pub fn begin_or_resume(cwd: &Path, root: &Path, key: &str) -> Option<PendingTurn> {
    let store = CheckpointStore::new(root, key);
    let path = store.root().join("native-baseline.json");
    if path.exists() {
        let bytes = fs::read(&path).ok()?;
        if bytes.len() > 1024 * 1024 {
            return None;
        }
        let baseline: Baseline = serde_json::from_slice(&bytes).ok()?;
        if baseline.repo_root != dunce::canonicalize(git::repo_root(cwd)?).ok()?
            || baseline.id.is_empty()
            || baseline.id.contains(['/', '\\'])
            || baseline.id == "."
            || baseline.id == ".."
        {
            return None;
        }
        // Finalization prunes blobs not used by that command's net diff. Keep
        // the first dirty preimages separately for a later command in the turn.
        let blobs = store.root().join(&baseline.id).join("blobs");
        fs::create_dir_all(&blobs).ok()?;
        for hash in baseline.pre_dirty.values().flatten() {
            fs::copy(
                store.root().join("native-baseline-blobs").join(hash),
                blobs.join(hash),
            )
            .ok()?;
        }
        return Some(PendingTurn {
            cleanup: PendingDirectory(None),
            excluded_root: baseline.excluded_root,
            user_turn_index: Some(0),
            store,
            id: baseline.id,
            prompt: baseline.prompt,
            created_at: baseline.created_at,
            repo_root: baseline.repo_root,
            head: baseline.head,
            pre_dirty: baseline.pre_dirty,
            pre_untracked: baseline.pre_untracked,
            unreadable: baseline.unreadable,
        });
    }
    let started = store.root().join("native-capture-started");
    if started.exists() {
        return None;
    }
    crate::fs_atomic::create_dir_all_synced(store.root()).ok()?;
    crate::fs_atomic::write_atomic(&started, b"1").ok()?;
    let repo = dunce::canonicalize(git::repo_root(cwd)?).ok()?;
    let cwd = dunce::canonicalize(cwd).ok()?;
    let excluded = cwd
        .strip_prefix(repo)
        .ok()?
        .join(".dex-home")
        .to_string_lossy()
        .replace('\\', "/");
    let mut pending = begin_turn_inner(
        &cwd,
        root,
        key,
        "Governed native turn",
        true,
        Some(excluded),
    )?;
    let baseline = Baseline {
        excluded_root: pending.excluded_root.clone(),
        id: pending.id.clone(),
        prompt: pending.prompt.clone(),
        created_at: pending.created_at.clone(),
        repo_root: pending.repo_root.clone(),
        head: pending.head.clone(),
        pre_dirty: pending.pre_dirty.clone(),
        pre_untracked: pending.pre_untracked.clone(),
        unreadable: pending.unreadable.clone(),
    };
    let retained = store.root().join("native-baseline-blobs");
    fs::create_dir_all(&retained).ok()?;
    for hash in baseline.pre_dirty.values().flatten() {
        fs::copy(
            store.root().join(&baseline.id).join("blobs").join(hash),
            retained.join(hash),
        )
        .ok()?;
    }
    crate::fs_atomic::write_atomic(&path, serde_json::to_vec(&baseline).ok()?).ok()?;
    pending.cleanup.0 = None;
    pending.user_turn_index = Some(0);
    Some(pending)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn git(cwd: &Path, args: &[&str]) {
        assert!(
            Command::new("git")
                .args(args)
                .current_dir(cwd)
                .status()
                .unwrap()
                .success()
        );
    }
    #[test]
    fn several_effects_keep_first_preimages_and_include_untracked_files() {
        let temp = tempfile::tempdir().unwrap();
        let cwd = temp.path().join("repo");
        fs::create_dir(&cwd).unwrap();
        git(&cwd, &["init", "-q"]);
        fs::write(cwd.join("source.txt"), "before\n").unwrap();
        git(&cwd, &["add", "source.txt"]);
        git(
            &cwd,
            &[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.com",
                "commit",
                "-qm",
                "initial",
            ],
        );
        let root = temp.path().join("snapshots");
        fs::write(cwd.join("dirty.txt"), "original dirty\n").unwrap();
        fs::create_dir_all(cwd.join(".dex-home/native-code")).unwrap();
        fs::write(
            cwd.join(".dex-home/native-code/sessions.json"),
            "private prompt",
        )
        .unwrap();
        let first = begin_or_resume(&cwd, &root, "turn").unwrap();
        fs::write(cwd.join("source.txt"), "middle\n").unwrap();
        fs::write(cwd.join("new.txt"), "new first\n").unwrap();
        finalize_turn_snapshot(first).unwrap().unwrap();
        let second = begin_or_resume(&cwd, &root, "turn").unwrap();
        fs::write(cwd.join("source.txt"), "after\n").unwrap();
        fs::write(cwd.join("new.txt"), "new final\n").unwrap();
        fs::write(cwd.join("dirty.txt"), "second effect\n").unwrap();
        fs::write(
            cwd.join(".dex-home/native-code/sessions.json"),
            "later prompt",
        )
        .unwrap();
        finalize_turn_snapshot(second).unwrap().unwrap();
        let (_, files) = CheckpointStore::new(&root, "turn")
            .turn_snapshot(Some(0), &cwd)
            .unwrap()
            .unwrap();
        assert!(files.iter().all(|file| !file.path.starts_with(".dex-home")));
        let source = files.iter().find(|file| file.path == "source.txt").unwrap();
        assert_eq!(source.before_content.as_deref(), Some("before\n"));
        assert_eq!(source.after_content.as_deref(), Some("after\n"));
        let created = files.iter().find(|file| file.path == "new.txt").unwrap();
        assert_eq!(created.kind, EntryKind::Created);
        assert_eq!(created.before_content.as_deref(), Some(""));
        assert_eq!(created.after_content.as_deref(), Some("new final\n"));
        let dirty = files.iter().find(|file| file.path == "dirty.txt").unwrap();
        assert_eq!(dirty.before_content.as_deref(), Some("original dirty\n"));
        assert_eq!(dirty.after_content.as_deref(), Some("second effect\n"));
    }
}

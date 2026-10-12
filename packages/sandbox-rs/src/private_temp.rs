//! Private temp roots shared by caches and native checkpoint storage.
use std::path::{Path, PathBuf};

/// Prepare an owner-only root without taking over another user's legacy root.
/// Existing roots owned by this user retain their path so persisted native
/// checkpoints keep their original first-command baseline.
pub fn prepare_private_temp_directory(temp_dir: &Path, name: &str) -> std::io::Result<PathBuf> {
    if Path::new(name).components().count() != 1
        || !matches!(
            Path::new(name).components().next(),
            Some(std::path::Component::Normal(_))
        )
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "private temp directory name must be one path component",
        ));
    }
    let temp_dir = dunce::canonicalize(temp_dir)?;
    super::ensure_trusted_temp_directory(&temp_dir)?;
    let root = private_temp_directory_path(&temp_dir, name);
    super::ensure_private_directory(&root)?;
    Ok(root)
}

pub(super) fn private_temp_directory_path(temp_dir: &Path, name: &str) -> PathBuf {
    #[cfg(unix)]
    {
        directory_path_for_uid(temp_dir, name, unsafe { libc::geteuid() })
    }
    #[cfg(not(unix))]
    {
        temp_dir.join(name)
    }
}

#[cfg(unix)]
fn directory_path_for_uid(temp_dir: &Path, name: &str, uid: u32) -> PathBuf {
    use std::os::unix::fs::MetadataExt as _;
    let current = temp_dir.join(format!("{name}-{uid}"));
    // Once selected, this root owns the persisted first-command baseline.
    // A legacy root appearing or changing ownership must not replace it.
    match std::fs::symlink_metadata(&current) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        _ => return current,
    }
    let legacy = temp_dir.join(name);
    match std::fs::symlink_metadata(&legacy) {
        // Keep malformed and current-owner paths subject to the existing
        // no-follow, owner and mode checks; only a foreign directory is ignored.
        Ok(metadata) if !metadata.is_dir() || metadata.uid() == uid => legacy,
        Ok(_) => current,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => current,
        Err(_) => legacy,
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _, symlink};

    #[test]
    fn foreign_legacy_owner_selects_a_separate_stable_root() {
        let temp = tempfile::tempdir().unwrap();
        let legacy = temp.path().join("effects");
        std::fs::create_dir(&legacy).unwrap();
        std::fs::set_permissions(&legacy, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::write(legacy.join("baseline"), b"original").unwrap();
        let owner = std::fs::metadata(&legacy).unwrap().uid();
        let other_uid = owner.checked_add(1).unwrap();
        let selected = directory_path_for_uid(temp.path(), "effects", other_uid);
        assert_eq!(selected, temp.path().join(format!("effects-{other_uid}")));
        assert_eq!(
            selected,
            directory_path_for_uid(temp.path(), "effects", other_uid)
        );
        assert_eq!(std::fs::read(legacy.join("baseline")).unwrap(), b"original");
        assert_eq!(std::fs::metadata(&legacy).unwrap().mode() & 0o777, 0o700);
    }

    #[test]
    fn current_owner_keeps_the_legacy_checkpoint_root() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let legacy = temp.path().join("effects");
        std::fs::create_dir(&legacy).unwrap();
        std::fs::write(legacy.join("baseline"), b"original").unwrap();
        assert_eq!(
            prepare_private_temp_directory(temp.path(), "effects").unwrap(),
            legacy
        );
        assert_eq!(std::fs::read(legacy.join("baseline")).unwrap(), b"original");
        assert_eq!(std::fs::metadata(&legacy).unwrap().mode() & 0o777, 0o700);
    }

    #[test]
    fn selected_uid_root_rejects_symlinks_and_shared_permissions() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let uid = unsafe { libc::geteuid() };
        let root = temp.path().join(format!("effects-{uid}"));
        let target = tempfile::tempdir().unwrap();
        symlink(target.path(), &root).unwrap();
        assert_eq!(
            prepare_private_temp_directory(temp.path(), "effects")
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::PermissionDenied
        );
        std::fs::remove_file(&root).unwrap();
        std::fs::create_dir(&root).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o777)).unwrap();
        assert_eq!(
            prepare_private_temp_directory(temp.path(), "effects")
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::PermissionDenied
        );
    }
}

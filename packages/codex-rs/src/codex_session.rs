use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const CODEX_THREAD_BINDING_VERSION: u32 = 1;
const UNIQUE_NAME_RETRIES: u32 = 64;

static UNIQUE_NAME_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Exact app-server thread key. The workspace is canonicalized before storage.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodexSessionKey {
    pub profile: String,
    pub workspace: PathBuf,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
}

impl CodexSessionKey {
    pub fn new(
        profile: impl Into<String>,
        workspace: impl AsRef<Path>,
        model: impl AsRef<str>,
    ) -> Result<Self> {
        let workspace = dunce::canonicalize(workspace.as_ref()).with_context(|| {
            format!(
                "canonicalize Codex workspace {}",
                workspace.as_ref().display()
            )
        })?;
        Ok(Self {
            profile: profile.into(),
            workspace,
            model: normalize_codex_model(model.as_ref()),
            session_id: None,
        })
    }

    pub fn with_session_id(mut self, session_id: Option<&str>) -> Self {
        self.session_id = session_id
            .map(str::trim)
            .filter(|session_id| !session_id.is_empty())
            .map(str::to_owned);
        self
    }

    fn storage_id(&self) -> String {
        let mut hasher = Sha256::new();
        hasher.update(self.profile.as_bytes());
        hasher.update([0]);
        hasher.update(self.workspace.to_string_lossy().as_bytes());
        hasher.update([0]);
        hasher.update(self.model.as_bytes());
        if let Some(session_id) = &self.session_id {
            hasher.update([0]);
            hasher.update(session_id.as_bytes());
        }
        hex_lower(&hasher.finalize())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodexCapabilities {
    pub resume: bool,
    pub dynamic_tools: bool,
    /// Maestro implements native steering; the provider must advertise it too.
    /// Legacy manifests did not declare it and therefore cannot enable it.
    #[serde(default)]
    pub active_steering: bool,
}

impl Default for CodexCapabilities {
    fn default() -> Self {
        Self {
            resume: true,
            dynamic_tools: true,
            active_steering: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodexSessionManifest {
    pub key: CodexSessionKey,
    pub approval_policy: String,
    pub sandbox: String,
    pub capabilities: CodexCapabilities,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodexSessionOpen {
    Resumed,
    Created,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodexThreadBinding {
    pub version: u32,
    pub key: CodexSessionKey,
    pub thread_id: String,
    pub protocol_version: Option<String>,
    pub updated_at: u64,
}

impl CodexThreadBinding {
    pub fn new(
        key: CodexSessionKey,
        thread_id: impl Into<String>,
        protocol_version: Option<String>,
        updated_at: u64,
    ) -> Self {
        Self {
            version: CODEX_THREAD_BINDING_VERSION,
            key,
            thread_id: thread_id.into(),
            protocol_version,
            updated_at,
        }
    }

    pub fn fresh(
        key: CodexSessionKey,
        thread_id: impl Into<String>,
        protocol_version: Option<String>,
    ) -> Self {
        Self::new(key, thread_id, protocol_version, unix_seconds())
    }

    pub fn load_at(state_root: &Path, key: &CodexSessionKey) -> Result<Option<Self>> {
        let path = binding_path(state_root, key);
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
        };
        let binding = match serde_json::from_slice::<Self>(&bytes) {
            Ok(binding) => binding,
            Err(_) => {
                quarantine_path(&path)?;
                return Ok(None);
            }
        };
        if binding.version != CODEX_THREAD_BINDING_VERSION
            || binding.key != *key
            || binding.thread_id.trim().is_empty()
        {
            quarantine_path(&path)?;
            return Ok(None);
        }
        Ok(Some(binding))
    }

    pub fn store_at(&self, state_root: &Path) -> Result<()> {
        ensure_private_dir(&bindings_dir(state_root))?;
        let path = self.path_at(state_root);
        let json = serde_json::to_vec_pretty(self)?;
        atomic_owner_only_write(&path, &json)?;
        Ok(())
    }

    pub fn quarantine_at(state_root: &Path, key: &CodexSessionKey) -> Result<bool> {
        quarantine_path(&binding_path(state_root, key))
    }

    pub fn path_for_key_at(state_root: &Path, key: &CodexSessionKey) -> PathBuf {
        binding_path(state_root, key)
    }

    pub fn path_at(&self, state_root: &Path) -> PathBuf {
        binding_path(state_root, &self.key)
    }
}

fn normalize_codex_model(model: &str) -> String {
    let lowered = model.trim().to_ascii_lowercase();
    lowered
        .strip_prefix("openai-codex/")
        .or_else(|| lowered.strip_prefix("codex/"))
        .unwrap_or(&lowered)
        .to_owned()
}

fn bindings_dir(state_root: &Path) -> PathBuf {
    state_root.join("codex").join("thread-bindings")
}

fn binding_path(state_root: &Path, key: &CodexSessionKey) -> PathBuf {
    bindings_dir(state_root).join(format!("{}.json", key.storage_id()))
}

fn quarantine_path(path: &Path) -> Result<bool> {
    if !path.exists() {
        return Ok(false);
    }
    let quarantine_dir = path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("quarantine");
    ensure_private_dir(&quarantine_dir)?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("binding.json");
    rename_no_clobber_with_unique_name(path, &quarantine_dir, file_name)
        .with_context(|| format!("quarantine invalid Codex thread binding {}", path.display()))?;
    Ok(true)
}

fn atomic_owner_only_write(path: &Path, contents: &[u8]) -> Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("binding.json");
    let mut last_error = None;
    for attempt in 0..UNIQUE_NAME_RETRIES {
        let tmp = parent.join(format!("{file_name}.tmp.{}", unique_suffix(attempt)));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = match options.open(&tmp) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                last_error = Some(error);
                continue;
            }
            Err(error) => return Err(error).with_context(|| format!("create {}", tmp.display())),
        };
        let write_result = file
            .write_all(contents)
            .and_then(|()| file.sync_all())
            .with_context(|| format!("write {}", tmp.display()));
        drop(file);
        if let Err(error) = write_result {
            let _ = fs::remove_file(&tmp);
            return Err(error);
        }
        if let Err(error) = replace_file(&tmp, path)
            .with_context(|| format!("replace {} with {}", tmp.display(), path.display()))
        {
            let _ = fs::remove_file(&tmp);
            return Err(error);
        }
        return Ok(());
    }
    Err(last_error
        .unwrap_or_else(|| io::Error::new(io::ErrorKind::AlreadyExists, "temp name collision")))
    .with_context(|| format!("create unique temporary file for {}", path.display()))
}

#[cfg(unix)]
fn replace_file(source: &Path, target: &Path) -> io::Result<()> {
    fs::rename(source, target)
}

#[cfg(windows)]
fn replace_file(source: &Path, target: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;

    use windows_sys::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };

    fn wide_path(path: &Path) -> io::Result<Vec<u16>> {
        let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
        if wide.contains(&0) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "path contains interior NUL",
            ));
        }
        wide.push(0);
        Ok(wide)
    }

    let source = wide_path(source)?;
    let target = wide_path(target)?;
    let replaced = unsafe {
        MoveFileExW(
            source.as_ptr(),
            target.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if replaced == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn rename_no_clobber_with_unique_name(
    source: &Path,
    target_dir: &Path,
    file_name: &str,
) -> Result<PathBuf> {
    let mut last_error = None;
    for attempt in 0..UNIQUE_NAME_RETRIES {
        let target = target_dir.join(format!("{file_name}.{}", unique_suffix(attempt)));
        match hard_link_then_remove(source, &target) {
            Ok(()) => return Ok(target),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                last_error = Some(error);
                continue;
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(target),
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("move {} to {}", source.display(), target.display()));
            }
        }
    }
    Err(last_error.unwrap_or_else(|| {
        io::Error::new(io::ErrorKind::AlreadyExists, "quarantine name collision")
    }))
    .with_context(|| format!("create unique quarantine file for {}", source.display()))
}

fn hard_link_then_remove(source: &Path, target: &Path) -> io::Result<()> {
    fs::hard_link(source, target)?;
    match fs::remove_file(source) {
        Ok(()) => Ok(()),
        Err(error) => {
            let _ = fs::remove_file(target);
            Err(error)
        }
    }
}

fn ensure_private_dir(path: &Path) -> Result<()> {
    fs::create_dir_all(path).with_context(|| format!("create {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .with_context(|| format!("chmod 0700 {}", path.display()))?;
    }
    Ok(())
}

fn unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn unix_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0)
}

fn unique_suffix(attempt: u32) -> String {
    let counter = UNIQUE_NAME_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!(
        "{}.{}.{}.{}",
        unix_nanos(),
        std::process::id(),
        counter,
        attempt
    )
}

fn hex_lower(bytes: &[u8]) -> String {
    const TABLE: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(TABLE[(byte >> 4) as usize] as char);
        out.push(TABLE[(byte & 0x0f) as usize] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    #[test]
    fn legacy_capabilities_preserve_choices_without_claiming_active_steering() {
        let capabilities: CodexCapabilities =
            serde_json::from_str(r#"{"resume":false,"dynamic_tools":true}"#).unwrap();
        let serialized = serde_json::to_value(&capabilities).unwrap();
        assert_eq!(serialized["resume"], false);
        assert_eq!(serialized["dynamic_tools"], true);
        assert_eq!(serialized["active_steering"], false);
        assert_eq!(
            serde_json::from_value::<CodexCapabilities>(serialized).unwrap(),
            capabilities
        );
    }

    #[test]
    fn exact_binding_round_trips_without_sensitive_fields() -> anyhow::Result<()> {
        let state_root = tempfile::tempdir()?;
        let workspace = tempfile::tempdir()?;
        let key = CodexSessionKey::new("work", workspace.path(), "OPENAI-CODEX/GPT-5.5")?;
        let binding = CodexThreadBinding::new(
            key.clone(),
            "thread-123",
            Some("2025-01-01".to_owned()),
            1_725_000_000,
        );

        binding.store_at(state_root.path())?;
        let raw = fs::read_to_string(binding.path_at(state_root.path()))?;

        assert!(!raw.contains("prompt"));
        assert!(!raw.contains("restored"));
        assert!(!raw.contains("arguments"));
        assert_eq!(
            CodexThreadBinding::load_at(state_root.path(), &key)?,
            Some(binding)
        );
        Ok(())
    }

    #[test]
    fn explicit_sessions_have_distinct_bindings_and_legacy_json_remains_readable()
    -> anyhow::Result<()> {
        let state_root = tempfile::tempdir()?;
        let workspace = tempfile::tempdir()?;
        let legacy_key = CodexSessionKey::new("work", workspace.path(), "gpt-5.5")?;
        let first_key = legacy_key.clone().with_session_id(Some("context-1"));
        let repeated_key = legacy_key.clone().with_session_id(Some(" context-1 "));
        let second_key = legacy_key.clone().with_session_id(Some("context-2"));

        assert_eq!(first_key, repeated_key);
        assert_ne!(
            binding_path(state_root.path(), &first_key),
            binding_path(state_root.path(), &second_key)
        );
        assert_ne!(
            binding_path(state_root.path(), &legacy_key),
            binding_path(state_root.path(), &first_key)
        );

        let legacy_binding =
            CodexThreadBinding::new(legacy_key.clone(), "legacy-thread", None, 1_725_000_000);
        let mut serialized = serde_json::to_value(&legacy_binding)?;
        serialized["key"]
            .as_object_mut()
            .expect("key object")
            .remove("session_id");
        let legacy_path = binding_path(state_root.path(), &legacy_key);
        fs::create_dir_all(legacy_path.parent().expect("binding parent"))?;
        fs::write(&legacy_path, serde_json::to_vec_pretty(&serialized)?)?;

        assert_eq!(
            CodexThreadBinding::load_at(state_root.path(), &legacy_key)?,
            Some(legacy_binding)
        );
        Ok(())
    }

    #[test]
    fn binding_store_replaces_existing_exact_key_binding() -> anyhow::Result<()> {
        let state_root = tempfile::tempdir()?;
        let workspace = tempfile::tempdir()?;
        let key = CodexSessionKey::new("work", workspace.path(), "gpt-5.5")?;
        let first = CodexThreadBinding::new(key.clone(), "thread-old", None, 1_725_000_000);
        let replacement = CodexThreadBinding::new(
            key.clone(),
            "thread-new",
            Some("2025-01-01".to_owned()),
            1_725_000_001,
        );

        first.store_at(state_root.path())?;
        let path = first.path_at(state_root.path());
        replacement.store_at(state_root.path())?;

        assert_eq!(
            CodexThreadBinding::load_at(state_root.path(), &key)?,
            Some(replacement)
        );
        let temp_files = fs::read_dir(path.parent().unwrap())?
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_name().to_string_lossy().contains(".tmp."))
            .count();
        assert_eq!(temp_files, 0);
        Ok(())
    }

    #[test]
    fn profile_workspace_or_model_mismatch_never_resumes() -> anyhow::Result<()> {
        let state_root = tempfile::tempdir()?;
        let workspace = tempfile::tempdir()?;
        let other_workspace = tempfile::tempdir()?;
        let key = CodexSessionKey::new("work", workspace.path(), "openai-codex/gpt-5.5")?;
        let binding = CodexThreadBinding::new(key.clone(), "thread-123", None, 1_725_000_000);
        binding.store_at(state_root.path())?;

        assert!(
            CodexThreadBinding::load_at(
                state_root.path(),
                &CodexSessionKey::new("personal", workspace.path(), "openai-codex/gpt-5.5")?
            )?
            .is_none()
        );
        assert!(
            CodexThreadBinding::load_at(
                state_root.path(),
                &CodexSessionKey::new("work", other_workspace.path(), "openai-codex/gpt-5.5")?
            )?
            .is_none()
        );
        assert!(
            CodexThreadBinding::load_at(
                state_root.path(),
                &CodexSessionKey::new("work", workspace.path(), "openai-codex/gpt-5.1")?
            )?
            .is_none()
        );
        assert_eq!(
            CodexThreadBinding::load_at(state_root.path(), &key)?,
            Some(binding)
        );
        Ok(())
    }

    #[test]
    fn corrupt_binding_is_quarantined_without_returning_content() -> anyhow::Result<()> {
        let state_root = tempfile::tempdir()?;
        let workspace = tempfile::tempdir()?;
        let key = CodexSessionKey::new("work", workspace.path(), "gpt-5.5")?;
        fs::create_dir_all(binding_path(state_root.path(), &key).parent().unwrap())?;
        fs::write(
            binding_path(state_root.path(), &key),
            "prompt=secret\ncommand=rm -rf workspace\n",
        )?;

        assert!(CodexThreadBinding::load_at(state_root.path(), &key)?.is_none());
        let quarantine_dir = bindings_dir(state_root.path()).join("quarantine");
        let quarantined = fs::read_dir(quarantine_dir)?.count();
        assert_eq!(quarantined, 1);
        assert!(!binding_path(state_root.path(), &key).exists());
        Ok(())
    }

    #[test]
    fn binding_store_ignores_stale_temp_files_for_the_same_key() -> anyhow::Result<()> {
        let state_root = tempfile::tempdir()?;
        let workspace = tempfile::tempdir()?;
        let key = CodexSessionKey::new("work", workspace.path(), "gpt-5.5")?;
        let binding = CodexThreadBinding::new(key.clone(), "thread-123", None, 1_725_000_000);
        let path = binding.path_at(state_root.path());
        fs::create_dir_all(path.parent().unwrap())?;
        fs::write(
            path.with_extension(format!("json.tmp.{}", std::process::id())),
            "stale",
        )?;

        binding.store_at(state_root.path())?;

        assert_eq!(
            CodexThreadBinding::load_at(state_root.path(), &key)?,
            Some(binding)
        );
        Ok(())
    }

    #[test]
    fn concurrent_same_process_stores_for_one_key_do_not_collide() -> anyhow::Result<()> {
        let state_root = tempfile::tempdir()?;
        let workspace = tempfile::tempdir()?;
        let key = CodexSessionKey::new("work", workspace.path(), "gpt-5.5")?;
        let handles: Vec<_> = (0..8)
            .map(|index| {
                let state_root = state_root.path().to_path_buf();
                let key = key.clone();
                std::thread::spawn(move || {
                    CodexThreadBinding::new(
                        key,
                        format!("thread-{index}"),
                        None,
                        1_725_000_000 + index,
                    )
                    .store_at(&state_root)
                })
            })
            .collect();

        for handle in handles {
            handle.join().expect("store thread")?;
        }
        let loaded = CodexThreadBinding::load_at(state_root.path(), &key)?
            .expect("one store should win the final binding");
        assert!(loaded.thread_id.starts_with("thread-"));
        Ok(())
    }

    #[test]
    fn repeated_quarantine_for_same_key_never_overwrites() -> anyhow::Result<()> {
        let state_root = tempfile::tempdir()?;
        let workspace = tempfile::tempdir()?;
        let key = CodexSessionKey::new("work", workspace.path(), "gpt-5.5")?;
        let path = binding_path(state_root.path(), &key);
        fs::create_dir_all(path.parent().unwrap())?;

        fs::write(&path, "first invalid binding")?;
        assert!(CodexThreadBinding::load_at(state_root.path(), &key)?.is_none());
        fs::write(&path, "second invalid binding")?;
        assert!(CodexThreadBinding::load_at(state_root.path(), &key)?.is_none());

        let quarantine_dir = bindings_dir(state_root.path()).join("quarantine");
        let mut contents: Vec<_> = fs::read_dir(quarantine_dir)?
            .map(|entry| fs::read_to_string(entry?.path()).map_err(anyhow::Error::from))
            .collect::<Result<Vec<_>>>()?;
        contents.sort();
        assert_eq!(
            contents,
            vec![
                "first invalid binding".to_owned(),
                "second invalid binding".to_owned()
            ]
        );
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn binding_files_are_owner_only() -> anyhow::Result<()> {
        use std::os::unix::fs::PermissionsExt;

        let state_root = tempfile::tempdir()?;
        let workspace = tempfile::tempdir()?;
        let key = CodexSessionKey::new("work", workspace.path(), "gpt-5.5")?;
        let binding = CodexThreadBinding::new(key, "thread-123", None, 1_725_000_000);
        binding.store_at(state_root.path())?;

        let file_mode = fs::metadata(binding.path_at(state_root.path()))?
            .permissions()
            .mode()
            & 0o777;
        let dir_mode = fs::metadata(bindings_dir(state_root.path()))?
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(file_mode, 0o600);
        assert_eq!(dir_mode, 0o700);
        Ok(())
    }
}

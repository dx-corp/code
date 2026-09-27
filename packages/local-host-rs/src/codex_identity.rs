//! Workspace-bound Codex identity selection and credential health.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use base64::Engine;
use serde::{Deserialize, Serialize};

const EXPIRING_WINDOW_SECS: u64 = 15 * 60;
const PROFILE_FILE_NAME: &str = "codex-auth-profiles.json";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CodexAuthState {
    SignedOut,
    Ready,
    Expiring,
    Expired,
    Invalid,
}

impl std::fmt::Display for CodexAuthState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let value = match self {
            Self::SignedOut => "signed_out",
            Self::Ready => "ready",
            Self::Expiring => "expiring",
            Self::Expired => "expired",
            Self::Invalid => "invalid",
        };
        f.write_str(value)
    }
}

impl CodexAuthState {
    pub fn is_usable(self) -> bool {
        matches!(self, Self::Ready | Self::Expiring)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CodexAuthHealth {
    pub state: CodexAuthState,
    pub auth_mode: Option<String>,
    pub expires_at: Option<u64>,
    pub account_label: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CodexIdentityProfile {
    pub codex_home: PathBuf,
    #[serde(default)]
    pub workspace: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexIdentitySelection {
    pub profile_name: String,
    pub codex_home: PathBuf,
    pub workspace_boundary: Option<PathBuf>,
}

impl CodexIdentitySelection {
    #[must_use]
    pub fn auth_path(&self) -> PathBuf {
        self.codex_home.join("auth.json")
    }

    #[must_use]
    pub fn child_env(&self) -> HashMap<String, String> {
        HashMap::from([(
            "CODEX_HOME".to_owned(),
            self.codex_home.to_string_lossy().into_owned(),
        )])
    }
}

#[derive(Debug, Deserialize)]
struct CodexIdentityProfilesFile {
    #[serde(default)]
    profiles: HashMap<String, CodexIdentityProfile>,
}

#[derive(Debug, Default, Deserialize)]
struct AuthFile {
    #[serde(default)]
    auth_mode: Option<String>,
    #[serde(rename = "OPENAI_API_KEY")]
    openai_api_key: Option<String>,
    #[serde(default)]
    tokens: Option<AuthTokens>,
}

#[derive(Debug, Default, Deserialize)]
struct AuthTokens {
    #[serde(default)]
    access_token: Option<String>,
    #[serde(default)]
    account_id: Option<String>,
}

#[must_use]
pub fn codex_identity_profile_path() -> Option<PathBuf> {
    crate::path_utils::maestro_home_dir().map(|home| home.join(PROFILE_FILE_NAME))
}

pub fn resolve_codex_identity(
    requested_profile: Option<&str>,
    workspace: &Path,
) -> Result<CodexIdentitySelection> {
    let default_codex_home =
        crate::codex_auth::codex_home().context("Codex home is unavailable")?;
    let profile_file = codex_identity_profile_path()
        .context("Maestro home is unavailable for Codex auth profiles")?;
    resolve_codex_identity_from(
        &profile_file,
        requested_profile,
        workspace,
        &default_codex_home,
    )
}

pub fn resolve_codex_identity_from(
    profile_file: &Path,
    requested_profile: Option<&str>,
    workspace: &Path,
    default_codex_home: &Path,
) -> Result<CodexIdentitySelection> {
    let requested_profile = requested_profile.map(str::trim);
    if matches!(requested_profile, Some("")) {
        bail!("Codex auth profile name cannot be empty");
    }

    let raw = match fs::read_to_string(profile_file) {
        Ok(raw) => raw,
        Err(error)
            if requested_profile.is_none_or(|profile| profile == "default")
                && error.kind() == std::io::ErrorKind::NotFound =>
        {
            return Ok(CodexIdentitySelection {
                profile_name: "default".to_owned(),
                codex_home: default_codex_home.to_path_buf(),
                workspace_boundary: None,
            });
        }
        Err(error) => {
            let profile_name = requested_profile.unwrap_or("default");
            return Err(error).with_context(|| {
                format!(
                    "Codex auth profile {profile_name:?} is not configured (missing {})",
                    profile_file.display()
                )
            });
        }
    };
    let profiles: CodexIdentityProfilesFile = serde_json::from_str(&raw)
        .with_context(|| format!("invalid Codex auth profile file {}", profile_file.display()))?;
    let canonical_workspace = dunce::canonicalize(workspace)
        .with_context(|| format!("workspace {} is unavailable", workspace.display()))?;

    let select_profile = |profile_name: &str,
                          profile: &CodexIdentityProfile|
     -> Result<CodexIdentitySelection> {
        if !profile.codex_home.is_absolute() {
            bail!("Codex auth profile {profile_name:?} codex_home must be an absolute path");
        }

        let workspace_boundary = profile
            .workspace
            .as_ref()
            .map(|boundary| {
                if !boundary.is_absolute() {
                    bail!("Codex auth profile {profile_name:?} workspace must be an absolute path");
                }
                let canonical_boundary = dunce::canonicalize(boundary).with_context(|| {
                    format!(
                        "Codex auth profile {profile_name:?} workspace {} is unavailable",
                        boundary.display()
                    )
                })?;
                if !canonical_workspace.starts_with(&canonical_boundary) {
                    bail!(
                        "Codex auth profile {profile_name:?} is bound to workspace {}",
                        canonical_boundary.display()
                    );
                }
                Ok(canonical_boundary)
            })
            .transpose()?;

        Ok(CodexIdentitySelection {
            profile_name: profile_name.to_owned(),
            codex_home: profile.codex_home.clone(),
            workspace_boundary,
        })
    };

    if let Some(requested_profile) = requested_profile {
        if requested_profile == "default" {
            if let Some(profile) = profiles.profiles.get("default") {
                return select_profile("default", profile);
            }
            return Ok(CodexIdentitySelection {
                profile_name: "default".to_owned(),
                codex_home: default_codex_home.to_path_buf(),
                workspace_boundary: None,
            });
        }
        let profile = profiles.profiles.get(requested_profile).with_context(|| {
            format!("Codex auth profile {requested_profile:?} is not configured")
        })?;
        return select_profile(requested_profile, profile);
    }

    let mut matches = Vec::new();
    for (profile_name, profile) in &profiles.profiles {
        let Some(boundary) = profile.workspace.as_ref() else {
            continue;
        };
        if !boundary.is_absolute() {
            bail!(
                "Codex auth profile {profile_name:?} has invalid workspace; use an absolute existing path"
            );
        }
        let canonical_boundary = dunce::canonicalize(boundary).with_context(|| {
            format!("Codex auth profile {profile_name:?} workspace is unavailable")
        })?;
        if canonical_workspace.starts_with(&canonical_boundary) {
            matches.push((profile_name.as_str(), profile));
        }
    }

    match matches.as_slice() {
        [] => Ok(CodexIdentitySelection {
            profile_name: "default".to_owned(),
            codex_home: default_codex_home.to_path_buf(),
            workspace_boundary: None,
        }),
        [(profile_name, profile)] => select_profile(profile_name, profile),
        _ => bail!(
            "multiple Codex auth profiles own workspace {}",
            canonical_workspace.display()
        ),
    }
}

#[must_use]
pub fn inspect_codex_auth(path: &Path) -> CodexAuthHealth {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs());
    inspect_codex_auth_at(path, now)
}

#[must_use]
pub fn inspect_codex_auth_at(path: &Path, now_epoch_secs: u64) -> CodexAuthHealth {
    let Ok(raw) = fs::read_to_string(path) else {
        return health(CodexAuthState::SignedOut, None, None, None);
    };
    let Ok(file) = serde_json::from_str::<AuthFile>(&raw) else {
        return health(CodexAuthState::Invalid, None, None, None);
    };
    let auth_mode = clean(file.auth_mode);
    if clean(file.openai_api_key).is_some() {
        return health(CodexAuthState::Ready, auth_mode, None, None);
    }

    let tokens = file.tokens.unwrap_or_default();
    let account_label = clean(tokens.account_id)
        .as_deref()
        .and_then(redact_account_id);
    let Some(token) = clean(tokens.access_token) else {
        return health(CodexAuthState::SignedOut, auth_mode, None, account_label);
    };
    let expires_at = jwt_expiry(&token);
    let state = match expires_at {
        Some(expiry) if expiry <= now_epoch_secs => CodexAuthState::Expired,
        Some(expiry) if expiry.saturating_sub(now_epoch_secs) <= EXPIRING_WINDOW_SECS => {
            CodexAuthState::Expiring
        }
        Some(_) | None if !token.contains('.') => CodexAuthState::Ready,
        Some(_) => CodexAuthState::Ready,
        None => CodexAuthState::Invalid,
    };
    health(state, auth_mode, expires_at, account_label)
}

fn health(
    state: CodexAuthState,
    auth_mode: Option<String>,
    expires_at: Option<u64>,
    account_label: Option<String>,
) -> CodexAuthHealth {
    CodexAuthHealth {
        state,
        auth_mode,
        expires_at,
        account_label,
    }
}

fn clean(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

fn jwt_expiry(token: &str) -> Option<u64> {
    let mut segments = token.split('.');
    let _header = segments.next()?;
    let payload = segments.next()?;
    let _signature = segments.next()?;
    if segments.next().is_some() {
        return None;
    }
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()?;
    serde_json::from_slice::<serde_json::Value>(&bytes)
        .ok()?
        .get("exp")
        .and_then(serde_json::Value::as_u64)
}

fn redact_account_id(account_id: &str) -> Option<String> {
    let suffix = account_id.chars().rev().take(4).collect::<Vec<_>>();
    if suffix.is_empty() {
        return None;
    }
    let suffix = suffix.into_iter().rev().collect::<String>();
    let prefix = account_id
        .split_once('-')
        .map_or("", |(prefix, _)| prefix)
        .chars()
        .take(8)
        .collect::<String>();
    Some(if prefix.is_empty() {
        format!("…{suffix}")
    } else {
        format!("{prefix}…{suffix}")
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;
    use std::fs;

    fn write_profile_file(root: &Path, workspace: &Path, codex_home: &Path) -> PathBuf {
        let path = root.join("codex-auth-profiles.json");
        fs::write(
            &path,
            serde_json::json!({
                "profiles": {
                    "work": {
                        "codex_home": codex_home,
                        "workspace": workspace,
                    }
                }
            })
            .to_string(),
        )
        .expect("profile file");
        path
    }

    fn jwt_with_exp(exp: u64) -> String {
        let header = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(br#"{"alg":"none"}"#);
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::json!({ "exp": exp }).to_string());
        format!("{header}.{payload}.signature")
    }

    fn write_auth(path: &Path, token: &str, account_id: &str) {
        fs::create_dir_all(path.parent().expect("auth parent")).expect("auth dir");
        fs::write(
            path,
            serde_json::json!({
                "auth_mode": "chatgpt",
                "tokens": {
                    "access_token": token,
                    "account_id": account_id,
                }
            })
            .to_string(),
        )
        .expect("auth file");
    }

    #[test]
    fn named_profile_is_scoped_to_its_workspace_tree() {
        let root = tempfile::tempdir().expect("root");
        let workspace = root.path().join("workspace");
        let child = workspace.join("repo");
        let codex_home = root.path().join("codex-work");
        fs::create_dir_all(&child).expect("workspace");
        let profile_file = write_profile_file(root.path(), &workspace, &codex_home);

        let selected = resolve_codex_identity_from(
            &profile_file,
            Some("work"),
            &child,
            &root.path().join("default"),
        )
        .expect("select work profile");
        assert_eq!(selected.profile_name, "work");
        assert_eq!(selected.codex_home, codex_home);
        assert_eq!(
            selected.child_env().get("CODEX_HOME").map(String::as_str),
            Some(selected.codex_home.to_string_lossy().as_ref())
        );

        let outside = root.path().join("outside");
        fs::create_dir_all(&outside).expect("outside");
        let error = resolve_codex_identity_from(
            &profile_file,
            Some("work"),
            &outside,
            &root.path().join("default"),
        )
        .expect_err("workspace mismatch must fail closed");
        assert!(error.to_string().contains("workspace"));
    }

    #[test]
    fn explicit_default_profile_does_not_follow_workspace_profile_mapping() {
        let root = tempfile::tempdir().expect("root");
        let workspace = root.path().join("workspace");
        let named_home = root.path().join("codex-work");
        let default_home = root.path().join("codex-default");
        fs::create_dir_all(&workspace).expect("workspace");
        let profile_file = write_profile_file(root.path(), &workspace, &named_home);

        let inferred = resolve_codex_identity_from(&profile_file, None, &workspace, &default_home)
            .expect("workspace profile");
        assert_eq!(inferred.profile_name, "work");

        let explicit =
            resolve_codex_identity_from(&profile_file, Some("default"), &workspace, &default_home)
                .expect("explicit default profile");
        assert_eq!(explicit.profile_name, "default");
        assert_eq!(explicit.codex_home, default_home);
    }

    #[test]
    fn missing_named_profile_never_falls_back_to_default() {
        let root = tempfile::tempdir().expect("root");
        let workspace = root.path().join("workspace");
        fs::create_dir_all(&workspace).expect("workspace");
        let profile_file = root.path().join("missing.json");
        let error = resolve_codex_identity_from(
            &profile_file,
            Some("other-user"),
            &workspace,
            &root.path().join("default"),
        )
        .expect_err("missing profile");
        assert!(error.to_string().contains("other-user"));
    }

    #[test]
    fn workspace_owner_is_selected_without_an_explicit_profile() {
        let root = tempfile::tempdir().expect("root");
        let workspace = root.path().join("workspace");
        let child = workspace.join("repo");
        let codex_home = root.path().join("codex-work");
        fs::create_dir_all(&child).expect("workspace");
        let profile_file = write_profile_file(root.path(), &workspace, &codex_home);

        let selected =
            resolve_codex_identity_from(&profile_file, None, &child, &root.path().join("default"))
                .expect("select workspace owner");
        let canonical_workspace = dunce::canonicalize(&workspace).expect("canonical workspace");
        assert_eq!(selected.profile_name, "work");
        assert_eq!(selected.codex_home, codex_home);
        assert_eq!(
            selected.workspace_boundary.as_deref(),
            Some(canonical_workspace.as_path())
        );
    }

    #[test]
    fn no_matching_workspace_owner_falls_back_to_default() {
        let root = tempfile::tempdir().expect("root");
        let workspace = root.path().join("workspace");
        let child = workspace.join("repo");
        let other_workspace = root.path().join("other-workspace");
        let codex_home = root.path().join("codex-work");
        let default_codex_home = root.path().join("default");
        fs::create_dir_all(&child).expect("workspace");
        fs::create_dir_all(&other_workspace).expect("other workspace");
        let profile_file = write_profile_file(root.path(), &other_workspace, &codex_home);

        let selected =
            resolve_codex_identity_from(&profile_file, None, &child, &default_codex_home)
                .expect("fall back to default");
        assert_eq!(selected.profile_name, "default");
        assert_eq!(selected.codex_home, default_codex_home);
        assert_eq!(selected.workspace_boundary, None);
    }

    #[test]
    fn implicit_selection_fails_closed_on_relative_workspace_profile() {
        let root = tempfile::tempdir().expect("root");
        let workspace = root.path().join("workspace");
        let default_codex_home = root.path().join("default");
        let profile_file = root.path().join("codex-auth-profiles.json");
        fs::create_dir_all(&workspace).expect("workspace");
        fs::write(
            &profile_file,
            serde_json::json!({
                "profiles": {
                    "work": {
                        "codex_home": root.path().join("codex-work"),
                        "workspace": "relative/workspace"
                    }
                }
            })
            .to_string(),
        )
        .expect("profile file");

        let error =
            resolve_codex_identity_from(&profile_file, None, &workspace, &default_codex_home)
                .expect_err("relative workspace must fail closed");
        let message = error.to_string();
        assert!(message.contains("Codex auth profile \"work\" has invalid workspace"));
        assert!(!message.contains("relative/workspace"));
        assert!(!message.contains(default_codex_home.to_string_lossy().as_ref()));
    }

    #[test]
    fn implicit_selection_fails_closed_on_uncanonicalizable_workspace_profile() {
        let root = tempfile::tempdir().expect("root");
        let workspace = root.path().join("workspace");
        let default_codex_home = root.path().join("default");
        let missing_workspace = root.path().join("missing-workspace");
        let profile_file = root.path().join("codex-auth-profiles.json");
        fs::create_dir_all(&workspace).expect("workspace");
        fs::write(
            &profile_file,
            serde_json::json!({
                "profiles": {
                    "work": {
                        "codex_home": root.path().join("codex-work"),
                        "workspace": missing_workspace
                    }
                }
            })
            .to_string(),
        )
        .expect("profile file");

        let error =
            resolve_codex_identity_from(&profile_file, None, &workspace, &default_codex_home)
                .expect_err("missing workspace must fail closed");
        let message = error.to_string();
        assert!(message.contains("Codex auth profile \"work\" workspace is unavailable"));
        assert!(!message.contains(missing_workspace.to_string_lossy().as_ref()));
        assert!(!message.contains(default_codex_home.to_string_lossy().as_ref()));
    }

    #[test]
    fn ambiguous_workspace_owner_fails_closed() {
        let root = tempfile::tempdir().expect("root");
        let workspace = root.path().join("workspace");
        let child = workspace.join("repo");
        let codex_home = root.path().join("codex-work");
        let other_codex_home = root.path().join("codex-personal");
        let profile_file = root.path().join("codex-auth-profiles.json");
        fs::create_dir_all(&child).expect("workspace");
        fs::write(
            &profile_file,
            serde_json::json!({
                "profiles": {
                    "work": {
                        "codex_home": codex_home,
                        "workspace": workspace,
                    },
                    "personal": {
                        "codex_home": other_codex_home,
                        "workspace": workspace,
                    }
                }
            })
            .to_string(),
        )
        .expect("profile file");

        let error =
            resolve_codex_identity_from(&profile_file, None, &child, &root.path().join("default"))
                .expect_err("ambiguous owner must fail closed");
        assert!(
            error
                .to_string()
                .contains("multiple Codex auth profiles own workspace")
        );
    }

    #[test]
    fn stale_workspace_profile_fails_closed_during_implicit_selection() {
        let root = tempfile::tempdir().expect("root");
        let workspace = root.path().join("workspace");
        let child = workspace.join("repo");
        let codex_home = root.path().join("codex-work");
        let default_codex_home = root.path().join("default");
        let stale_workspace = root.path().join("stale-workspace");
        let profile_file = root.path().join("codex-auth-profiles.json");
        fs::create_dir_all(&child).expect("workspace");
        fs::write(
            &profile_file,
            serde_json::json!({
                "profiles": {
                    "stale": {
                        "codex_home": root.path().join("codex-stale"),
                        "workspace": stale_workspace,
                    },
                    "work": {
                        "codex_home": codex_home,
                        "workspace": workspace,
                    }
                }
            })
            .to_string(),
        )
        .expect("profile file");

        let error = resolve_codex_identity_from(&profile_file, None, &child, &default_codex_home)
            .expect_err("stale workspace profile must fail closed");
        assert!(
            error
                .to_string()
                .contains("Codex auth profile \"stale\" workspace is unavailable")
        );
        assert!(!error.to_string().contains("stale-workspace"));
    }

    #[test]
    fn auth_health_distinguishes_ready_expiring_expired_and_invalid() {
        let root = tempfile::tempdir().expect("root");
        let auth = root.path().join("auth.json");
        let now = 10_000;

        write_auth(
            &auth,
            &jwt_with_exp(now + EXPIRING_WINDOW_SECS + 1),
            "acct-secret-1234",
        );
        let ready = inspect_codex_auth_at(&auth, now);
        assert_eq!(ready.state, CodexAuthState::Ready);
        assert_eq!(ready.expires_at, Some(now + EXPIRING_WINDOW_SECS + 1));
        assert_eq!(ready.account_label.as_deref(), Some("acct…1234"));

        write_auth(
            &auth,
            &jwt_with_exp(now + EXPIRING_WINDOW_SECS),
            "acct-secret-1234",
        );
        assert_eq!(
            inspect_codex_auth_at(&auth, now).state,
            CodexAuthState::Expiring
        );

        write_auth(&auth, &jwt_with_exp(now), "acct-secret-1234");
        assert_eq!(
            inspect_codex_auth_at(&auth, now).state,
            CodexAuthState::Expired
        );

        fs::write(&auth, "{not-json").expect("invalid auth");
        assert_eq!(
            inspect_codex_auth_at(&auth, now).state,
            CodexAuthState::Invalid
        );
    }
}

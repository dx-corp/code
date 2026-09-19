//! Native `maestro update` implementation.

use std::env;
use std::fs;
use std::fs::OpenOptions;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use base64::Engine as _;
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use fd_lock::RwLock as FileLock;
use semver::Version;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use wait_timeout::ChildExt;

const LEGACY_CHANNEL_MANIFEST_BASE_URL: &str =
    "https://storage.googleapis.com/evalops-prod-maestro-releases/maestro/channels";
const GITHUB_RELEASES_API_URL: &str = "https://api.github.com/repos/evalops/maestro/releases";
const GITHUB_STABLE_LATEST_MANIFEST_URL: &str =
    "https://github.com/evalops/maestro/releases/latest/download/channel-manifest.json";
const GITHUB_RELEASES_PAGE_SIZE: usize = 100;
const GITHUB_RELEASES_MAX_PAGES: usize = 10;
const CHANNEL_MANIFEST_SCHEMA: &str = "evalops.maestro.release-channel.v1";
const STABLE_CHANNEL_KEY_ID: &str = "stable-2026-08-0c3df2ac";
const PRERELEASE_CHANNEL_KEY_ID: &str = "preview-2026-08-912a0dab";
const STABLE_CHANNEL_PUBLIC_KEY: &str = "IYgvaSwf2E9DioyEZ6Qcp/QMD1xpsjS0JgYluAAt0pE=";
const PRERELEASE_CHANNEL_PUBLIC_KEY: &str = "4DS+odrY7y1PMg7o4s0jY1FkgcPQb8jjdy0Nst05soA=";
const DEFAULT_CHECK_TIMEOUT: Duration = Duration::from_secs(5);
const DEFAULT_STARTUP_CHECK_TIMEOUT: Duration = Duration::from_millis(350);
const DEFAULT_STARTUP_RETRY: Duration = Duration::from_hours(24);
const INSTALL_TIMEOUT: Duration = Duration::from_mins(1);
const MAX_UPDATE_HISTORY: usize = 32;
const UPDATE_STATUS_SCHEMA: &str = "evalops.maestro.update-status.v1";
const UPDATE_HISTORY_SCHEMA: &str = "evalops.maestro.update-history.v1";
const INSTALL_RECEIPT_SCHEMA: &str = "evalops.maestro.install-receipt.v1";
const RELEASE_METADATA_SCHEMA: &str = "evalops.maestro.release-metadata.v1";
const UPDATE_HISTORY_FILE: &str = "update-history.json";
const INSTALL_RECEIPT_FILE: &str = "install-receipt.json";
const RELEASE_METADATA_FILE: &str = "release-metadata.json";
static UPDATE_ATTEMPT_COUNTER: AtomicU64 = AtomicU64::new(0);
#[cfg(unix)]
const INSTALL_CLEANUP_GRACE: Duration = Duration::from_secs(2);
const EMBEDDED_INSTALLER: &str = include_str!("../../../scripts/install.sh");

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct ReleaseArtifactReceipt {
    name: String,
    #[serde(default)]
    digest: Option<String>,
    #[serde(default)]
    runtime_passport: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct ReleaseReceipt {
    schema_version: String,
    #[serde(default)]
    source_sha: Option<String>,
    #[serde(default)]
    artifacts: Vec<ReleaseArtifactReceipt>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct VersionMetadata {
    version: String,
    #[serde(default)]
    schema_version: Option<String>,
    #[serde(default)]
    notes: Option<String>,
    #[serde(default)]
    release_notes: Option<String>,
    #[serde(default)]
    release_tag: Option<String>,
    #[serde(default)]
    release_url: Option<String>,
    #[serde(default)]
    receipt: Option<ReleaseReceipt>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct ChannelManifest {
    schema_version: String,
    channel: String,
    key_id: String,
    version: String,
    release_tag: String,
    release_url: String,
    #[serde(default)]
    metadata_url: Option<String>,
    #[serde(default)]
    metadata_sha256: Option<String>,
    source_sha: String,
    issued_at_ms: u64,
    #[serde(default)]
    release_notes: Option<String>,
    #[serde(default)]
    release_receipt: Option<ReleaseReceipt>,
    signature: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ChannelVerification {
    channel: String,
    manifest_url: String,
    key_id: Option<String>,
    algorithm: Option<String>,
    status: String,
    fallback: Option<String>,
    error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct UpdateCheck {
    status: &'static str,
    channel: UpdateChannel,
    current_version: String,
    latest_version: Option<String>,
    source_url: String,
    error: Option<String>,
    release_notes: Option<String>,
    release_tag: Option<String>,
    release_url: Option<String>,
    release_receipt: Option<ReleaseReceipt>,
    channel_verification: Option<ChannelVerification>,
    attempt_id: Option<String>,
    verification: Option<InstallReceipt>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct StartupUpdateState {
    version: String,
    last_attempt_at: u64,
    last_status: String,
    #[serde(default)]
    source_url: Option<String>,
    #[serde(default)]
    last_error: Option<String>,
    #[serde(default)]
    retry_after_at: Option<u64>,
    #[serde(default)]
    rollback_version: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct InstallVerification {
    #[serde(default)]
    manifest_sha256: Option<String>,
    #[serde(default)]
    manifest_checksum_verified: bool,
    #[serde(default)]
    signature_verified: bool,
    #[serde(default)]
    artifact_sha256: Option<String>,
    #[serde(default)]
    metadata_sha256: Option<String>,
    #[serde(default)]
    metadata_checksum_verified: bool,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct InstallReceipt {
    #[serde(default)]
    schema_version: String,
    #[serde(default)]
    version: String,
    #[serde(default)]
    platform: String,
    #[serde(default)]
    installed_at_ms: u64,
    #[serde(default)]
    verified: bool,
    #[serde(default)]
    verification: InstallVerification,
    #[serde(default)]
    release_metadata_asset: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct UpdateAttempt {
    #[serde(default)]
    attempt_id: String,
    #[serde(default)]
    operation: String,
    #[serde(default)]
    trigger: String,
    #[serde(default)]
    status: String,
    #[serde(default)]
    attempted_at_ms: u64,
    #[serde(default)]
    completed_at_ms: Option<u64>,
    #[serde(default)]
    from_version: Option<String>,
    #[serde(default)]
    to_version: Option<String>,
    #[serde(default)]
    source_url: Option<String>,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    release_notes: Option<String>,
    #[serde(default)]
    release_receipt: Option<ReleaseReceipt>,
    #[serde(default)]
    verification: Option<InstallReceipt>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct UpdateHistory {
    #[serde(default = "update_history_schema")]
    schema_version: String,
    #[serde(default)]
    attempts: Vec<UpdateAttempt>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct RetryStatus {
    window_ms: u64,
    last_attempt_at_ms: Option<u64>,
    next_attempt_at_ms: Option<u64>,
    retry_after_ms: Option<u64>,
    throttled: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct UpdateStatus {
    schema_version: &'static str,
    state: String,
    channel: UpdateChannel,
    active_version: Option<String>,
    current_version: Option<String>,
    latest_version: Option<String>,
    install_method: Option<String>,
    update_source: Option<String>,
    channel_verification: Option<ChannelVerification>,
    release_notes: Option<String>,
    release_receipt: Option<ReleaseReceipt>,
    last_attempt: Option<UpdateAttempt>,
    retry: RetryStatus,
    verification: Option<InstallReceipt>,
    history_path: Option<String>,
    check_error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct UpdateHistoryOutput {
    schema_version: &'static str,
    history_path: Option<String>,
    attempts: Vec<UpdateAttempt>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct RollbackOutcome {
    schema_version: &'static str,
    status: &'static str,
    from_version: String,
    active_version: String,
    attempt: Option<UpdateAttempt>,
    verification: Option<InstallReceipt>,
    history_error: Option<String>,
    launcher_warning: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
enum UpdateAction {
    #[default]
    Apply,
    Status,
    History,
    Rollback {
        version: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
enum UpdateChannel {
    #[default]
    Stable,
    Beta,
    Alpha,
}

impl UpdateChannel {
    fn parse(value: &str) -> Result<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "stable" => Ok(Self::Stable),
            "beta" => Ok(Self::Beta),
            "alpha" => Ok(Self::Alpha),
            other => {
                bail!(
                    "{}",
                    crate::localization::cli_locale().format(
                        "Unknown Deixic Code update channel: {0}; expected stable, beta, or alpha",
                        &[(other).to_string()]
                    )
                )
            }
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Stable => "stable",
            Self::Beta => "beta",
            Self::Alpha => "alpha",
        }
    }

    fn from_environment() -> Result<Self> {
        env::var("MAESTRO_UPDATE_CHANNEL").map_or(Ok(Self::Stable), |value| Self::parse(&value))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum InstallContext {
    Package {
        manager: String,
        package: String,
        prefix: Option<PathBuf>,
        launcher: PathBuf,
    },
    Release {
        install_dir: PathBuf,
        data_dir: PathBuf,
        launcher: PathBuf,
    },
}

#[derive(Debug, Default)]
struct UpdateArgs {
    action: UpdateAction,
    check_only: bool,
    json: bool,
    help: bool,
    channel: UpdateChannel,
}

fn parse_args(args: &[String]) -> Result<UpdateArgs> {
    let mut parsed = UpdateArgs::default();
    let mut channel_explicit = false;
    let mut index = 0;
    if let Some(first) = args.first().filter(|arg| !arg.starts_with('-')) {
        match first.as_str() {
            "status" => parsed.action = UpdateAction::Status,
            "history" => parsed.action = UpdateAction::History,
            "rollback" => {
                index = 1;
                let version = args.get(index).filter(|arg| !arg.starts_with('-')).cloned();
                if version.is_some() {
                    index += 1;
                }
                parsed.action = UpdateAction::Rollback { version };
            }
            other => bail!(
                "{}",
                crate::localization::cli_locale().format(
                    "Unknown deixic-code update subcommand: {0}",
                    &[(other).to_string()]
                )
            ),
        }
        if !matches!(parsed.action, UpdateAction::Rollback { .. }) {
            index = 1;
        }
    }
    while let Some(arg) = args.get(index) {
        match arg.as_str() {
            "--check" => parsed.check_only = true,
            "--json" => parsed.json = true,
            "--help" | "-h" => parsed.help = true,
            "--channel" => {
                index += 1;
                let value = args
                    .get(index)
                    .context("--channel requires stable, beta, or alpha")?;
                parsed.channel = UpdateChannel::parse(value)?;
                channel_explicit = true;
            }
            other => bail!(
                "{}",
                crate::localization::cli_locale().format(
                    "Unknown deixic-code update option: {0}",
                    &[(other).to_string()]
                )
            ),
        }
        index += 1;
    }
    if parsed.check_only && !matches!(parsed.action, UpdateAction::Apply) {
        bail!(
            "{}",
            crate::localization::cli_locale().format(
                "--check is only supported for the default update action",
                &[]
            )
        );
    }
    if !channel_explicit && matches!(parsed.action, UpdateAction::Apply | UpdateAction::Status) {
        parsed.channel = UpdateChannel::from_environment()?;
    }
    if !matches!(parsed.channel, UpdateChannel::Stable)
        && matches!(
            parsed.action,
            UpdateAction::History | UpdateAction::Rollback { .. }
        )
    {
        bail!(
            "{}",
            crate::localization::cli_locale().format(
                "--channel is only supported for update and update status",
                &[]
            )
        );
    }
    Ok(parsed)
}

fn update_history_schema() -> String {
    UPDATE_HISTORY_SCHEMA.to_owned()
}

fn configured_update_urls() -> Option<Vec<String>> {
    if let Ok(value) = env::var("MAESTRO_UPDATE_URLS") {
        let values = value
            .split([',', '\n'])
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
            .collect::<Vec<_>>();
        if !values.is_empty() {
            return Some(values);
        }
    }
    if let Ok(value) = env::var("MAESTRO_UPDATE_URL") {
        let value = value.trim();
        if !value.is_empty() {
            return Some(vec![value.to_owned()]);
        }
    }
    None
}

fn legacy_channel_manifest_url(channel: UpdateChannel) -> String {
    format!(
        "{LEGACY_CHANNEL_MANIFEST_BASE_URL}/{}/manifest.json",
        channel.as_str()
    )
}

fn github_channel_manifest_url(tag: &str) -> String {
    format!("{}/channel-manifest.json", github_release_url(tag))
}

fn github_release_url(tag: &str) -> String {
    format!("https://github.com/evalops/maestro/releases/download/{tag}")
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct GithubReleaseSelection {
    tag: String,
    release_url: String,
    manifest_url: String,
}

fn github_release_selection(tag: &str) -> GithubReleaseSelection {
    let release_url = github_release_url(tag);
    GithubReleaseSelection {
        tag: tag.to_owned(),
        manifest_url: github_channel_manifest_url(tag),
        release_url,
    }
}

fn tag_matches_channel(tag: &str, channel: UpdateChannel) -> bool {
    let tag = tag.trim().trim_start_matches('v');
    let Ok(version) = Version::parse(tag) else {
        return false;
    };
    version.to_string() == tag && channel_version_matches(&version, channel)
}

fn channel_version_matches(version: &Version, channel: UpdateChannel) -> bool {
    if !version.build.is_empty() {
        return false;
    }
    let prerelease_ordinal = |prefix: &str| {
        let Some(ordinal) = version
            .pre
            .as_str()
            .strip_prefix(prefix)
            .and_then(|value| value.strip_prefix('.'))
        else {
            return false;
        };
        let bytes = ordinal.as_bytes();
        !bytes.is_empty()
            && (b'1'..=b'9').contains(&bytes[0])
            && bytes[1..].iter().all(|byte| byte.is_ascii_digit())
    };
    match channel {
        UpdateChannel::Stable => version.pre.is_empty(),
        UpdateChannel::Beta => prerelease_ordinal("beta"),
        UpdateChannel::Alpha => prerelease_ordinal("alpha"),
    }
}

#[derive(Debug, Deserialize)]
struct GithubReleaseListItem {
    tag_name: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    assets: Vec<GithubReleaseAsset>,
}

#[derive(Debug, Deserialize)]
struct GithubReleaseAsset {
    name: String,
}

fn github_release_has_channel_manifest(release: &GithubReleaseListItem) -> bool {
    release
        .assets
        .iter()
        .any(|asset| asset.name == "channel-manifest.json")
}

fn github_release_is_eligible(release: &GithubReleaseListItem, channel: UpdateChannel) -> bool {
    !release.draft
        && github_release_has_channel_manifest(release)
        && match channel {
            UpdateChannel::Stable => !release.prerelease,
            UpdateChannel::Beta | UpdateChannel::Alpha => release.prerelease,
        }
}

async fn resolve_github_channel_manifest_url(
    client: &reqwest::Client,
    channel: UpdateChannel,
) -> Result<GithubReleaseSelection, String> {
    let mut releases = Vec::new();
    for page in 1..=GITHUB_RELEASES_MAX_PAGES {
        let response = client
            .get(GITHUB_RELEASES_API_URL)
            .query(&[("per_page", GITHUB_RELEASES_PAGE_SIZE), ("page", page)])
            .header(reqwest::header::ACCEPT, "application/vnd.github+json")
            .header(reqwest::header::USER_AGENT, "maestro-updater")
            .send()
            .await
            .map_err(|error| error.to_string())?;
        if !response.status().is_success() {
            return Err(format!(
                "GitHub releases list failed ({})",
                response.status()
            ));
        }
        let page_releases = response
            .json::<Vec<GithubReleaseListItem>>()
            .await
            .map_err(|error| format!("Invalid GitHub releases list: {error}"))?;
        let page_len = page_releases.len();
        releases.extend(page_releases);
        if page_len < GITHUB_RELEASES_PAGE_SIZE {
            break;
        }
    }
    let tag = releases
        .into_iter()
        .filter(|release| github_release_is_eligible(release, channel))
        .filter_map(|release| {
            if !tag_matches_channel(&release.tag_name, channel) {
                return None;
            }
            let version = Version::parse(release.tag_name.trim_start_matches('v')).ok()?;
            Some((version, release.tag_name))
        })
        .max_by(|left, right| left.0.cmp(&right.0))
        .map(|(_, tag)| tag);
    tag.map(|tag| github_release_selection(&tag))
        .ok_or_else(|| format!("No published {} GitHub release", channel.as_str()))
}

fn channel_from_manifest_url(url: &str) -> Option<UpdateChannel> {
    [
        UpdateChannel::Stable,
        UpdateChannel::Beta,
        UpdateChannel::Alpha,
    ]
    .into_iter()
    .find(|channel| url == legacy_channel_manifest_url(*channel))
}

fn channel_from_update_url(url: &str, requested: UpdateChannel) -> Option<UpdateChannel> {
    channel_from_manifest_url(url)
        .or_else(|| url.ends_with("/channel-manifest.json").then_some(requested))
}

fn legacy_channel_source(url: &str) -> bool {
    [
        UpdateChannel::Stable,
        UpdateChannel::Beta,
        UpdateChannel::Alpha,
    ]
    .into_iter()
    .any(|channel| url == legacy_channel_manifest_url(channel))
}

fn canonical_channel_value(value: serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Array(values) => {
            serde_json::Value::Array(values.into_iter().map(canonical_channel_value).collect())
        }
        serde_json::Value::Object(values) => {
            let mut canonical = serde_json::Map::new();
            let mut keys = values.into_iter().collect::<Vec<_>>();
            keys.sort_by(|left, right| left.0.cmp(&right.0));
            for (key, value) in keys {
                canonical.insert(key, canonical_channel_value(value));
            }
            serde_json::Value::Object(canonical)
        }
        value => value,
    }
}

fn canonical_channel_manifest_payload(manifest: &ChannelManifest) -> Result<Vec<u8>> {
    let mut value = serde_json::to_value(manifest).context("serialize channel manifest")?;
    if let serde_json::Value::Object(fields) = &mut value {
        fields.remove("signature");
    }
    serde_json::to_vec(&canonical_channel_value(value))
        .context("serialize canonical channel manifest payload")
}

fn trusted_channel_key(channel: UpdateChannel, key_id: &str) -> Result<VerifyingKey> {
    let (expected_key_id, encoded) = match channel {
        UpdateChannel::Stable => (STABLE_CHANNEL_KEY_ID, STABLE_CHANNEL_PUBLIC_KEY),
        UpdateChannel::Beta | UpdateChannel::Alpha => {
            (PRERELEASE_CHANNEL_KEY_ID, PRERELEASE_CHANNEL_PUBLIC_KEY)
        }
    };
    if key_id != expected_key_id {
        bail!(
            "{}",
            crate::localization::cli_locale().format(
                "untrusted {0} channel key id: {1}",
                &[(channel.as_str()).to_string(), (key_id).to_string()]
            )
        );
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .context("decode trusted release channel public key")?;
    let bytes: [u8; 32] = bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("trusted release channel public key must be 32 bytes"))?;
    let key = VerifyingKey::from_bytes(&bytes)
        .map_err(|_| anyhow::anyhow!("validate trusted release channel public key"))?;
    if key.is_weak() {
        bail!(
            "{}",
            crate::localization::cli_locale()
                .format("trusted release channel public key is weak", &[])
        );
    }
    Ok(key)
}

fn verify_channel_manifest(
    manifest: &ChannelManifest,
    expected_channel: UpdateChannel,
) -> Result<()> {
    if manifest.schema_version != CHANNEL_MANIFEST_SCHEMA {
        bail!(
            "{}",
            crate::localization::cli_locale()
                .format("unsupported release channel manifest schema", &[])
        );
    }
    if manifest.channel != expected_channel.as_str() {
        bail!(
            "{}",
            crate::localization::cli_locale()
                .format("release channel manifest channel mismatch", &[])
        );
    }
    let version = Version::parse(manifest.version.trim())
        .context("release channel manifest has an invalid version")?;
    if manifest.release_tag != format!("v{}", version) {
        bail!(
            "{}",
            crate::localization::cli_locale().format(
                "release channel manifest tag does not match its version",
                &[]
            )
        );
    }
    if !channel_version_matches(&version, expected_channel) {
        match expected_channel {
            UpdateChannel::Stable => bail!(
                "{}",
                crate::localization::cli_locale()
                    .format("stable channel requires a stable semver version", &[])
            ),
            UpdateChannel::Beta => bail!(
                "{}",
                crate::localization::cli_locale()
                    .format("beta channel requires a beta prerelease version", &[])
            ),
            UpdateChannel::Alpha => bail!(
                "{}",
                crate::localization::cli_locale()
                    .format("alpha channel requires an alpha prerelease version", &[])
            ),
        }
    }
    if !manifest.release_url.starts_with("https://") {
        bail!(
            "{}",
            crate::localization::cli_locale()
                .format("release channel manifest release URL must use HTTPS", &[])
        );
    }
    if manifest
        .metadata_url
        .as_deref()
        .is_some_and(|url| !url.starts_with("https://"))
    {
        bail!(
            "{}",
            crate::localization::cli_locale()
                .format("release channel manifest metadata URL must use HTTPS", &[])
        );
    }
    if !manifest
        .source_sha
        .bytes()
        .all(|byte| byte.is_ascii_hexdigit())
        || manifest.source_sha.len() != 40
    {
        bail!(
            "{}",
            crate::localization::cli_locale()
                .format("release channel manifest source SHA is invalid", &[])
        );
    }
    if let Some(digest) = manifest.metadata_sha256.as_deref() {
        if !digest.starts_with("sha256:")
            || digest.len() != "sha256:".len() + 64
            || !digest["sha256:".len()..]
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
        {
            bail!(
                "{}",
                crate::localization::cli_locale()
                    .format("release channel manifest metadata digest is invalid", &[])
            );
        }
    }
    let key = trusted_channel_key(expected_channel, &manifest.key_id)?;
    let signature = base64::engine::general_purpose::STANDARD
        .decode(&manifest.signature)
        .context("decode release channel manifest signature")?;
    let signature = Signature::from_slice(&signature)
        .map_err(|_| anyhow::anyhow!("release channel manifest signature must be 64 bytes"))?;
    let payload = canonical_channel_manifest_payload(manifest)?;
    key.verify(&payload, &signature)
        .map_err(|_| anyhow::anyhow!("release channel manifest signature mismatch"))?;
    Ok(())
}

fn verify_github_release_manifest_binding(
    manifest: &ChannelManifest,
    selected: &GithubReleaseSelection,
) -> Result<()> {
    let selected_version = selected.tag.trim_start_matches('v');
    if manifest.release_tag != selected.tag {
        bail!(
            "{}",
            crate::localization::cli_locale().format(
                "release channel manifest tag {0} does not match selected GitHub release {1}",
                &[(manifest.release_tag).clone(), (selected.tag).clone()]
            )
        );
    }
    if manifest.version.trim() != selected_version {
        bail!(
            "{}",
            crate::localization::cli_locale().format(
                "release channel manifest version {0} does not match selected GitHub release {1}",
                &[(manifest.version).clone(), (selected.tag).clone()]
            )
        );
    }
    if manifest.release_url.trim_end_matches('/') != selected.release_url {
        bail!(
            "{}",
            crate::localization::cli_locale().format(
                "release channel manifest URL {0} does not match selected GitHub release {1}",
                &[
                    (manifest.release_url).clone(),
                    (selected.release_url).clone()
                ]
            )
        );
    }
    Ok(())
}

fn channel_verification_failure(
    channel: UpdateChannel,
    manifest_url: &str,
    error: impl Into<String>,
) -> ChannelVerification {
    ChannelVerification {
        channel: channel.as_str().to_owned(),
        manifest_url: manifest_url.to_owned(),
        key_id: None,
        algorithm: Some("ed25519".to_owned()),
        status: "invalid".to_owned(),
        fallback: None,
        error: Some(error.into()),
    }
}

fn update_urls(package: &str, channel: UpdateChannel) -> Vec<String> {
    configured_update_urls().unwrap_or_else(|| {
        let npm_tag = match channel {
            UpdateChannel::Stable => "latest",
            UpdateChannel::Beta => "beta",
            UpdateChannel::Alpha => "alpha",
        };
        let mut urls = github_channel_update_urls(channel);
        urls.push(format!(
            "https://registry.npmjs.org/{}/{}",
            urlencoding::encode(package),
            npm_tag
        ));
        urls
    })
}

fn github_channel_update_urls(channel: UpdateChannel) -> Vec<String> {
    let mut urls = Vec::new();
    if channel == UpdateChannel::Stable {
        urls.push(GITHUB_STABLE_LATEST_MANIFEST_URL.to_owned());
    }
    urls.push(GITHUB_RELEASES_API_URL.to_owned());
    urls
}

fn trusted_startup_update_urls(context: &InstallContext, channel: UpdateChannel) -> Vec<String> {
    match context {
        InstallContext::Package { package, .. } => update_urls(package, channel),
        InstallContext::Release { .. } => github_channel_update_urls(channel),
    }
}

async fn check_for_update(
    current: &str,
    context: &InstallContext,
    channel: UpdateChannel,
) -> UpdateCheck {
    let urls = match context {
        InstallContext::Package { package, .. } => update_urls(package, channel),
        InstallContext::Release { .. } => configured_update_urls()
            .unwrap_or_else(|| trusted_startup_update_urls(context, channel)),
    };
    check_for_update_urls_with_timeout(current, urls, DEFAULT_CHECK_TIMEOUT, channel).await
}

#[cfg(test)]
async fn check_for_update_urls(current: &str, urls: Vec<String>) -> UpdateCheck {
    check_for_update_urls_with_timeout(current, urls, DEFAULT_CHECK_TIMEOUT, UpdateChannel::Stable)
        .await
}

async fn check_for_update_urls_with_timeout(
    current: &str,
    urls: Vec<String>,
    timeout: Duration,
    channel: UpdateChannel,
) -> UpdateCheck {
    let current_version = Version::parse(current.trim());
    let client = reqwest::Client::builder().timeout(timeout).build();
    let mut best: Option<(Version, UpdateCheck)> = None;
    let mut last_error = None;
    let mut last_url = String::new();
    let mut last_channel_verification: Option<ChannelVerification> = None;

    let Ok(client) = client else {
        return failed_check(current, "", "Failed to create update client", channel, None);
    };
    let Ok(current_semver) = current_version else {
        return failed_check(
            current,
            "",
            "Current Deixic Code version is not valid semver",
            channel,
            None,
        );
    };

    for url in urls {
        last_url.clone_from(&url);
        let mut selected_github_release = None;
        let url = if url == GITHUB_RELEASES_API_URL {
            match resolve_github_channel_manifest_url(&client, channel).await {
                Ok(selection) => {
                    last_url.clone_from(&selection.manifest_url);
                    selected_github_release = Some(selection.clone());
                    selection.manifest_url
                }
                Err(error) => {
                    last_error = Some(error);
                    continue;
                }
            }
        } else {
            url
        };
        let response = match client
            .get(&url)
            .header(reqwest::header::ACCEPT, "application/json")
            .send()
            .await
        {
            Ok(response) => response,
            Err(error) => {
                if let Some(channel) = channel_from_update_url(&url, channel) {
                    last_channel_verification = Some(channel_verification_failure(
                        channel,
                        &url,
                        error.to_string(),
                    ));
                }
                last_error = Some(error.to_string());
                continue;
            }
        };
        if !response.status().is_success() {
            let error = format!("Update check failed ({})", response.status());
            if let Some(channel) = channel_from_update_url(&url, channel) {
                last_channel_verification =
                    Some(channel_verification_failure(channel, &url, &error));
            }
            last_error = Some(error);
            continue;
        }
        let manifest_channel = channel_from_update_url(&url, channel);
        if let Some(manifest_channel) = manifest_channel {
            let manifest = match response.json::<ChannelManifest>().await {
                Ok(manifest) => manifest,
                Err(error) => {
                    let message = format!("Invalid release channel manifest: {error}");
                    last_channel_verification = Some(channel_verification_failure(
                        manifest_channel,
                        &url,
                        &message,
                    ));
                    last_error = Some(message);
                    continue;
                }
            };
            if let Err(error) = verify_channel_manifest(&manifest, manifest_channel) {
                let message = format!("Invalid release channel manifest: {error:#}");
                last_channel_verification = Some(channel_verification_failure(
                    manifest_channel,
                    &url,
                    &message,
                ));
                last_error = Some(message);
                continue;
            }
            if let Some(selected) = selected_github_release.as_ref() {
                if let Err(error) = verify_github_release_manifest_binding(&manifest, selected) {
                    let message = format!("Invalid selected GitHub release manifest: {error:#}");
                    last_channel_verification = Some(channel_verification_failure(
                        manifest_channel,
                        &url,
                        &message,
                    ));
                    last_error = Some(message);
                    continue;
                }
            }
            let latest = match Version::parse(manifest.version.trim()) {
                Ok(version) => version,
                Err(error) => {
                    let message = format!("Invalid release channel version: {error}");
                    last_channel_verification = Some(channel_verification_failure(
                        manifest_channel,
                        &url,
                        &message,
                    ));
                    last_error = Some(message);
                    continue;
                }
            };
            let legacy_source = legacy_channel_source(&url);
            return UpdateCheck {
                status: if latest > current_semver {
                    "available"
                } else {
                    "current"
                },
                channel,
                current_version: current.to_owned(),
                latest_version: Some(latest.to_string()),
                source_url: url.clone(),
                error: None,
                release_notes: manifest.release_notes,
                release_tag: Some(manifest.release_tag),
                release_url: Some(manifest.release_url),
                release_receipt: manifest.release_receipt,
                channel_verification: Some(ChannelVerification {
                    channel: manifest.channel,
                    manifest_url: url,
                    key_id: Some(manifest.key_id),
                    algorithm: Some("ed25519".to_owned()),
                    status: if legacy_source {
                        "legacyFallback".to_owned()
                    } else {
                        "verified".to_owned()
                    },
                    fallback: if legacy_source {
                        Some("legacyExplicit".to_owned())
                    } else {
                        (manifest_channel != channel).then(|| manifest_channel.as_str().to_owned())
                    },
                    error: None,
                }),
                attempt_id: None,
                verification: None,
            };
        }
        let metadata = match response.json::<VersionMetadata>().await {
            Ok(metadata) => metadata,
            Err(error) => {
                last_error = Some(format!("Invalid update metadata: {error}"));
                continue;
            }
        };
        let latest = match Version::parse(metadata.version.trim()) {
            Ok(version) => version,
            Err(error) => {
                last_error = Some(format!("Invalid update version: {error}"));
                continue;
            }
        };
        let status = if latest > current_semver {
            "available"
        } else {
            "current"
        };
        let check = UpdateCheck {
            status,
            channel,
            current_version: current.to_owned(),
            latest_version: Some(latest.to_string()),
            source_url: url,
            error: None,
            release_notes: metadata.release_notes.or(metadata.notes),
            release_tag: metadata.release_tag,
            release_url: metadata.release_url,
            release_receipt: metadata.receipt,
            channel_verification: last_channel_verification.take().map(|mut verification| {
                verification.status = "legacyFallback".to_owned();
                verification.fallback = Some("legacyMetadata".to_owned());
                verification
            }),
            attempt_id: None,
            verification: None,
        };
        if best.as_ref().is_none_or(|(version, _)| latest > *version) {
            best = Some((latest, check));
        }
    }

    best.map_or_else(
        || {
            failed_check(
                current,
                &last_url,
                last_error
                    .as_deref()
                    .unwrap_or("No update metadata sources configured"),
                channel,
                last_channel_verification,
            )
        },
        |(_, check)| check,
    )
}

fn failed_check(
    current: &str,
    source_url: &str,
    error: &str,
    channel: UpdateChannel,
    channel_verification: Option<ChannelVerification>,
) -> UpdateCheck {
    UpdateCheck {
        status: "failed",
        channel,
        current_version: current.to_owned(),
        latest_version: None,
        source_url: source_url.to_owned(),
        error: Some(error.to_owned()),
        release_notes: None,
        release_tag: None,
        release_url: None,
        release_receipt: None,
        channel_verification,
        attempt_id: None,
        verification: None,
    }
}

fn package_prefix(package_root: &Path, manager: &str) -> Option<PathBuf> {
    let normalized = package_root.to_string_lossy().replace('\\', "/");
    let marker = if manager == "bun" {
        "/install/global/node_modules/"
    } else {
        "/lib/node_modules/"
    };
    normalized
        .find(marker)
        .map(|index| {
            if manager == "bun" {
                PathBuf::from(format!("{}/install/global", &normalized[..index]))
            } else {
                PathBuf::from(&normalized[..index])
            }
        })
        .or_else(|| {
            (manager == "npm")
                .then(|| normalized.find("/node_modules/"))
                .flatten()
                .map(|index| PathBuf::from(&normalized[..index]))
        })
}

fn package_install_context_from(
    executable: &Path,
    package_root: &Path,
    package: String,
    manager_override: Option<&str>,
) -> Option<InstallContext> {
    let package_root = dunce::canonicalize(package_root).ok()?;
    let relative = executable.strip_prefix(&package_root).ok()?;
    let components = relative
        .components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>();
    if components.len() != 4
        || components[0] != "vendor"
        || components[1] != "maestro"
        || components[3]
            != if cfg!(windows) {
                "maestro.exe"
            } else {
                "maestro"
            }
    {
        return None;
    }
    let manager = match manager_override {
        Some("npm") => "npm",
        Some("bun") => "bun",
        _ if package_root
            .to_string_lossy()
            .replace('\\', "/")
            .contains("/.bun/install/global/") =>
        {
            "bun"
        }
        _ => "npm",
    }
    .to_owned();
    let prefix = package_prefix(&package_root, &manager);
    Some(InstallContext::Package {
        manager,
        package,
        prefix,
        launcher: package_root.join("bin/maestro"),
    })
}

fn package_install_context(executable: &Path) -> Option<InstallContext> {
    let package = env::var("MAESTRO_PACKAGE_NAME").ok()?;
    let package_root = PathBuf::from(env::var_os("MAESTRO_PACKAGE_ROOT")?);
    let manager_override = env::var("MAESTRO_UPDATE_PACKAGE_MANAGER").ok();
    package_install_context_from(
        executable,
        &package_root,
        package,
        manager_override.as_deref(),
    )
}

fn release_install_context(executable: &Path) -> Option<InstallContext> {
    let install_dir = PathBuf::from(env::var_os("MAESTRO_INSTALL_DIR")?);
    let data_dir = PathBuf::from(env::var_os("MAESTRO_DATA_DIR")?);
    let releases = data_dir.join("releases");
    let relative = executable.strip_prefix(&releases).ok()?;
    let components = relative
        .components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>();
    if components.len() != 4
        || components[2] != "bin"
        || components[3]
            != if cfg!(windows) {
                "maestro.exe"
            } else {
                "maestro"
            }
    {
        return None;
    }
    let launcher = install_dir.join(if cfg!(windows) {
        "maestro.exe"
    } else {
        "maestro"
    });
    launcher.is_file().then_some(InstallContext::Release {
        install_dir,
        data_dir,
        launcher,
    })
}

// Older native package launchers did not export installation metadata. Resolve
// only the two published packages in recognizable global package layouts; never
// turn an arbitrary executable or a local development checkout into an install.
fn legacy_package_install_context(executable: &Path) -> Option<InstallContext> {
    let executable = dunce::canonicalize(executable).ok()?;
    let root = executable.ancestors().nth(4)?;
    let normalized = root.to_string_lossy().replace('\\', "/");
    if !normalized.contains("/lib/node_modules/")
        && !normalized.contains("/.bun/install/global/node_modules/")
    {
        return None;
    }
    let metadata: serde_json::Value =
        serde_json::from_slice(&fs::read(root.join("package.json")).ok()?).ok()?;
    let name = metadata.get("name")?.as_str()?;
    if !matches!(name, "@evalops/maestro" | "@evalops/deixic-code")
        || !normalized.ends_with(&format!("/node_modules/{name}"))
        || metadata.pointer("/bin/maestro")?.as_str()? != "bin/maestro"
        || !root.join("bin/maestro").is_file()
    {
        return None;
    }
    package_install_context_from(&executable, root, name.to_owned(), None)
}

fn legacy_package_version(executable: &Path) -> Option<String> {
    let InstallContext::Package { launcher, .. } = legacy_package_install_context(executable)?
    else {
        return None;
    };
    let root = launcher.parent()?.parent()?;
    let metadata: serde_json::Value =
        serde_json::from_slice(&fs::read(root.join("package.json")).ok()?).ok()?;
    let version = metadata.get("version")?.as_str()?;
    Version::parse(version)
        .ok()
        .map(|version| version.to_string())
}

fn install_context() -> Option<InstallContext> {
    let executable = env::current_exe().ok()?;
    match env::var("MAESTRO_INSTALL_METHOD").ok().as_deref() {
        Some("package") => package_install_context(&executable),
        Some("release") => release_install_context(&executable),
        None => legacy_package_install_context(&executable),
        _ => None,
    }
}

fn run_with_timeout(command: &mut Command, label: &str) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        command.process_group(0);
    }
    let mut child = command
        .spawn()
        .with_context(|| format!("Failed to launch {label}"))?;
    let Some(status) = child.wait_timeout(INSTALL_TIMEOUT)? else {
        #[cfg(unix)]
        {
            if let Ok(process_group_id) = i32::try_from(child.id()) {
                unsafe {
                    let _ = libc::kill(-process_group_id, libc::SIGTERM);
                }
                if child.wait_timeout(INSTALL_CLEANUP_GRACE)?.is_none() {
                    unsafe {
                        let _ = libc::kill(-process_group_id, libc::SIGKILL);
                    }
                }
            } else {
                let _ = child.kill();
            }
        }
        #[cfg(not(unix))]
        let _ = child.kill();
        let _ = child.wait();
        bail!(
            "{}",
            crate::localization::cli_locale().format(
                "{0} timed out after {1} seconds",
                &[(label).to_string(), (INSTALL_TIMEOUT.as_secs()).to_string()]
            )
        );
    };
    if !status.success() {
        bail!(
            "{}",
            crate::localization::cli_locale().format(
                "{0} exited with status {1}",
                &[(label).to_string(), (status).to_string()]
            )
        );
    }
    Ok(())
}

fn install_package(
    manager: &str,
    package: &str,
    prefix: Option<&Path>,
    version: &str,
) -> Result<()> {
    let spec = format!("{package}@{version}");
    let mut command = Command::new(manager);
    command.args(["install", "-g", &spec]);
    sanitize_package_manager_env(&mut command);
    if manager == "npm" {
        if let Some(prefix) = prefix {
            command.env("NPM_CONFIG_PREFIX", prefix);
        }
    } else if let Some(prefix) = prefix.and_then(Path::parent).and_then(Path::parent) {
        command.env("BUN_INSTALL", prefix);
    }
    run_with_timeout(&mut command, manager)
}

fn install_release(
    install_dir: &Path,
    data_dir: &Path,
    version: &str,
    release_url: Option<&str>,
    channel: UpdateChannel,
    show_progress: bool,
) -> Result<()> {
    let temporary = tempfile::tempdir().context("Failed to create updater directory")?;
    let installer = temporary.path().join("install.sh");
    fs::write(&installer, EMBEDDED_INSTALLER).context("Failed to stage embedded installer")?;
    let mut command = Command::new("bash");
    command.arg(&installer);
    sanitize_release_installer_env(&mut command);
    command
        .env("MAESTRO_INSTALL_VERSION", version)
        .env("MAESTRO_INSTALL_CHANNEL", channel.as_str())
        .env("MAESTRO_INSTALL_DIR", install_dir)
        .env("MAESTRO_DATA_DIR", data_dir)
        .env("MAESTRO_REQUIRE_SIGNED_INSTALL", "1")
        .env("MAESTRO_SKIP_STARTUP_UPDATE", "1");
    if show_progress {
        command.env("MAESTRO_UPDATE_PROGRESS", "1");
    }
    if let Some(release_url) = release_url {
        command.env("MAESTRO_RELEASE_BASE_URL", release_url);
    }
    run_with_timeout(&mut command, "signed Deixic Code installer")
}

fn install(
    context: &InstallContext,
    version: &str,
    release_url: Option<&str>,
    channel: UpdateChannel,
    show_progress: bool,
) -> Result<()> {
    match context {
        InstallContext::Package {
            manager,
            package,
            prefix,
            ..
        } => install_package(manager, package, prefix.as_deref(), version),
        InstallContext::Release {
            install_dir,
            data_dir,
            ..
        } => install_release(
            install_dir,
            data_dir,
            version,
            release_url,
            channel,
            show_progress,
        ),
    }
}

fn launcher(context: &InstallContext) -> &Path {
    match context {
        InstallContext::Package { launcher, .. } | InstallContext::Release { launcher, .. } => {
            launcher
        }
    }
}

fn sanitize_package_manager_env(command: &mut Command) {
    for (key, _) in env::vars_os() {
        let key_text = key.to_string_lossy();
        if should_remove_package_manager_env(&key_text) {
            command.env_remove(key);
        }
    }
    command.env("MAESTRO_STARTUP_UPDATE_RETRY_MS", "0");
}

fn sanitize_release_installer_env(command: &mut Command) {
    for key in [
        "MAESTRO_ALLOW_UNSIGNED_INSTALL",
        "MAESTRO_RELEASE_BASE_URL",
        "MAESTRO_RELEASE_REPO",
        "MAESTRO_REQUIRE_SIGNED_INSTALL",
        "MAESTRO_INSTALL_VERSION",
        "MAESTRO_INSTALL_CHANNEL",
        "MAESTRO_UPDATE_CHANNEL",
        "MAESTRO_UPDATE_PROGRESS",
        "MAESTRO_VERSION",
        "MAESTRO_UPDATE_HISTORY",
    ] {
        command.env_remove(key);
    }
}

fn should_remove_package_manager_env(key: &str) -> bool {
    let lower = key.to_ascii_lowercase();
    let package_manager_setting = lower.starts_with("npm_config_")
        || lower.starts_with("bun_config_")
        || lower.starts_with("yarn_")
        || lower.starts_with("pnpm_");
    let allowed_prefix = lower == "npm_config_prefix";
    let blocked = matches!(
        key,
        "CI" | "NODE_ENV"
            | "NODE_OPTIONS"
            | "NPM_TOKEN"
            | "NODE_AUTH_TOKEN"
            | "MAESTRO_UPDATE_URL"
            | "MAESTRO_UPDATE_URLS"
            | "MAESTRO_UPDATE_HISTORY"
            | "MAESTRO_UPDATE_CHANNEL"
            | "MAESTRO_STARTUP_UPDATE_STATE"
            | "MAESTRO_SKIP_STARTUP_UPDATE"
            | "MAESTRO_STARTUP_UPDATE"
            | "MAESTRO_AUTO_UPDATE"
            | "MAESTRO_PACKAGE_NAME"
            | "MAESTRO_PACKAGE_ROOT"
            | "MAESTRO_INSTALL_METHOD"
            | "MAESTRO_INSTALL_DIR"
            | "MAESTRO_DATA_DIR"
    );
    blocked || (package_manager_setting && !allowed_prefix)
}

fn env_duration(name: &str, default: Duration, allow_zero: bool) -> Duration {
    env::var(name)
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| allow_zero || *value > 0)
        .map(Duration::from_millis)
        .unwrap_or(default)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

fn update_history_path(context: Option<&InstallContext>) -> Option<PathBuf> {
    env::var_os("MAESTRO_UPDATE_HISTORY")
        .map(PathBuf::from)
        .or_else(|| match context {
            Some(InstallContext::Release { data_dir, .. }) => {
                Some(data_dir.join(UPDATE_HISTORY_FILE))
            }
            _ => None,
        })
        .or_else(|| {
            crate::path_utils::maestro_home_dir().map(|home| home.join(UPDATE_HISTORY_FILE))
        })
}

fn startup_state_path_for(context: Option<&InstallContext>) -> Option<PathBuf> {
    env::var_os("MAESTRO_STARTUP_UPDATE_STATE")
        .map(PathBuf::from)
        .or_else(|| match context {
            Some(InstallContext::Release { data_dir, .. }) => {
                Some(data_dir.join("startup-update-state.json"))
            }
            _ => None,
        })
        .or_else(|| {
            crate::path_utils::maestro_home_dir().map(|home| home.join("startup-update-state.json"))
        })
}

fn load_update_history(path: Option<&Path>) -> Result<UpdateHistory> {
    let Some(path) = path else {
        return Ok(UpdateHistory {
            schema_version: update_history_schema(),
            attempts: Vec::new(),
        });
    };
    if !path.is_file() {
        return Ok(UpdateHistory {
            schema_version: update_history_schema(),
            attempts: Vec::new(),
        });
    }
    let bytes = fs::read(path)
        .with_context(|| format!("Failed to read update history {}", path.display()))?;
    let mut history: UpdateHistory = serde_json::from_slice(&bytes)
        .with_context(|| format!("Invalid update history {}", path.display()))?;
    history.schema_version = update_history_schema();
    if history.attempts.len() > MAX_UPDATE_HISTORY {
        let keep_from = history.attempts.len() - MAX_UPDATE_HISTORY;
        history.attempts.drain(..keep_from);
    }
    Ok(history)
}

fn persist_update_history(path: &Path, mut history: UpdateHistory) -> Result<()> {
    history.schema_version = update_history_schema();
    if history.attempts.len() > MAX_UPDATE_HISTORY {
        let keep_from = history.attempts.len() - MAX_UPDATE_HISTORY;
        history.attempts.drain(..keep_from);
    }
    let bytes = serde_json::to_vec_pretty(&history)?;
    crate::path_utils::atomic_private_write(path, &bytes)
        .with_context(|| format!("Failed to persist update history {}", path.display()))
}

fn new_update_attempt(
    operation: &str,
    trigger: &str,
    from_version: Option<&str>,
    to_version: Option<&str>,
    source_url: Option<&str>,
    release_notes: Option<&str>,
    release_receipt: Option<&ReleaseReceipt>,
) -> UpdateAttempt {
    let attempted_at_ms = now_ms();
    UpdateAttempt {
        attempt_id: format!(
            "{}-{}-{}",
            attempted_at_ms,
            std::process::id(),
            UPDATE_ATTEMPT_COUNTER.fetch_add(1, Ordering::Relaxed)
        ),
        operation: operation.to_owned(),
        trigger: trigger.to_owned(),
        status: "started".to_owned(),
        attempted_at_ms,
        completed_at_ms: None,
        from_version: from_version.map(str::to_owned),
        to_version: to_version.map(str::to_owned),
        source_url: source_url.map(str::to_owned),
        error: None,
        release_notes: release_notes.map(str::to_owned),
        release_receipt: release_receipt.cloned(),
        verification: None,
    }
}

fn begin_update_attempt(path: Option<&Path>, attempt: &UpdateAttempt) -> Result<()> {
    let Some(path) = path else {
        return Ok(());
    };
    let mut history = load_update_history(Some(path))?;
    history.attempts.push(attempt.clone());
    persist_update_history(path, history)
}

fn finish_update_attempt(
    path: Option<&Path>,
    attempt_id: &str,
    status: &str,
    error: Option<String>,
    verification: Option<InstallReceipt>,
) -> Result<Option<UpdateAttempt>> {
    let Some(path) = path else {
        return Ok(None);
    };
    let mut history = load_update_history(Some(path))?;
    let Some(attempt) = history
        .attempts
        .iter_mut()
        .find(|attempt| attempt.attempt_id == attempt_id)
    else {
        return Ok(None);
    };
    attempt.status = status.to_owned();
    attempt.completed_at_ms = Some(now_ms());
    attempt.error = error;
    attempt.verification = verification;
    let result = attempt.clone();
    persist_update_history(path, history)?;
    Ok(Some(result))
}

fn load_install_receipt(release_dir: &Path) -> Option<InstallReceipt> {
    let bytes = fs::read(release_dir.join(INSTALL_RECEIPT_FILE)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn load_release_metadata(release_dir: &Path) -> Option<VersionMetadata> {
    let bytes = fs::read(release_dir.join(RELEASE_METADATA_FILE)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn load_verified_release_metadata(
    release_dir: &Path,
    receipt: &InstallReceipt,
) -> Option<VersionMetadata> {
    let verification = &receipt.verification;
    if receipt.schema_version != INSTALL_RECEIPT_SCHEMA
        || !receipt.verified
        || !verification.manifest_checksum_verified
        || !verification.signature_verified
        || !verification.metadata_checksum_verified
    {
        return None;
    }
    let expected = verification.metadata_sha256.as_deref()?;
    let actual = sha256_file(&release_dir.join(RELEASE_METADATA_FILE))?;
    if expected != actual {
        return None;
    }
    let metadata = load_release_metadata(release_dir)?;
    (metadata.schema_version.as_deref() == Some(RELEASE_METADATA_SCHEMA)
        && metadata.version == receipt.version)
        .then_some(metadata)
}

fn sha256_file(path: &Path) -> Option<String> {
    let bytes = fs::read(path).ok()?;
    Some(format!("sha256:{:x}", Sha256::digest(bytes)))
}

fn native_platform() -> &'static str {
    match (env::consts::OS, env::consts::ARCH) {
        ("darwin", "x86_64") => "darwin-x64",
        ("darwin", "aarch64") => "darwin-arm64",
        ("linux", "x86_64") => "linux-x64",
        ("linux", "aarch64") => "linux-arm64",
        _ => "unsupported",
    }
}

fn release_dir_from_executable(executable: &Path, data_dir: &Path) -> Option<PathBuf> {
    let releases = data_dir.join("releases");
    let relative = executable.strip_prefix(&releases).ok()?;
    let components = relative
        .components()
        .map(|component| component.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    if components.len() == 4 && components[2] == "bin" {
        Some(releases.join(&components[0]).join(&components[1]))
    } else {
        None
    }
}

fn current_release_dir(context: &InstallContext) -> Option<PathBuf> {
    let InstallContext::Release { data_dir, .. } = context else {
        return None;
    };
    release_dir_from_executable(&env::current_exe().ok()?, data_dir)
}

fn current_release_receipt(context: &InstallContext) -> Option<InstallReceipt> {
    load_install_receipt(&current_release_dir(context)?)
}

#[derive(Debug, Clone)]
struct VerifiedRelease {
    version: Version,
    version_text: String,
    release_dir: PathBuf,
    receipt: InstallReceipt,
    metadata: Option<VersionMetadata>,
}

fn release_binary_path(release_dir: &Path) -> PathBuf {
    release_dir.join("bin").join(if cfg!(windows) {
        "maestro.exe"
    } else {
        "maestro"
    })
}

fn is_verified_release(release_dir: &Path, version: &Version, receipt: &InstallReceipt) -> bool {
    let verification = &receipt.verification;
    let metadata_verified = !verification.metadata_checksum_verified
        || load_verified_release_metadata(release_dir, receipt).is_some();
    receipt.schema_version == INSTALL_RECEIPT_SCHEMA
        && receipt.verified
        && receipt.version == version.to_string()
        && receipt.platform == native_platform()
        && verification.manifest_checksum_verified
        && verification.signature_verified
        && release_binary_path(release_dir).is_file()
        && verification
            .artifact_sha256
            .as_deref()
            .zip(sha256_file(&release_binary_path(release_dir)).as_deref())
            .is_some_and(|(expected, actual)| expected == actual)
        && metadata_verified
}

fn list_verified_releases(data_dir: &Path) -> Result<Vec<VerifiedRelease>> {
    let root = data_dir.join("releases");
    if !root.is_dir() {
        return Ok(Vec::new());
    }
    let mut releases = Vec::new();
    for version_entry in fs::read_dir(&root)? {
        let version_entry = version_entry?;
        let version_root = version_entry.path();
        if !version_root.is_dir() {
            continue;
        }
        let Some(version_text) = version_root.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let Ok(version) = Version::parse(version_text) else {
            continue;
        };
        let Ok(release_entries) = fs::read_dir(&version_root) else {
            continue;
        };
        for release_entry in release_entries.filter_map(Result::ok) {
            let release_dir = release_entry.path();
            if !release_dir.is_dir() {
                continue;
            }
            let Some(receipt) = load_install_receipt(&release_dir) else {
                continue;
            };
            if !is_verified_release(&release_dir, &version, &receipt) {
                continue;
            }
            let metadata = load_verified_release_metadata(&release_dir, &receipt);
            releases.push(VerifiedRelease {
                version: version.clone(),
                version_text: version_text.to_owned(),
                release_dir: release_dir.clone(),
                receipt,
                metadata,
            });
        }
    }
    releases.sort_by(|left, right| {
        left.version.cmp(&right.version).then_with(|| {
            left.receipt
                .installed_at_ms
                .cmp(&right.receipt.installed_at_ms)
        })
    });
    Ok(releases)
}

fn find_release_installation(data_dir: &Path, version: &str) -> Option<InstallReceipt> {
    let root = data_dir.join("releases").join(version);
    let mut candidates = fs::read_dir(root)
        .ok()?
        .filter_map(Result::ok)
        .filter_map(|entry| load_install_receipt(&entry.path()))
        .collect::<Vec<_>>();
    candidates.sort_by_key(|receipt| receipt.installed_at_ms);
    candidates.pop()
}

fn verify_retained_release(release: &VerifiedRelease) -> Result<()> {
    let output = Command::new(release_binary_path(&release.release_dir))
        .arg("--version")
        .env("MAESTRO_SKIP_STARTUP_UPDATE", "1")
        .output()
        .with_context(|| {
            format!(
                "Failed to verify retained Deixic Code {}",
                release.version_text
            )
        })?;
    if !output.status.success() {
        bail!(
            "{}",
            crate::localization::cli_locale().format(
                "Retained Deixic Code {0} failed its version check",
                std::slice::from_ref(&(release.version_text))
            )
        );
    }
    let reported = String::from_utf8_lossy(&output.stdout);
    if !reported_version_matches(&reported, &release.version_text) {
        bail!(
            "{}",
            crate::localization::cli_locale().format(
                "Retained Deixic Code reported the wrong version: expected {0}, got {1}",
                &[
                    (release.version_text).clone(),
                    (reported.trim()).to_string()
                ]
            )
        );
    }
    Ok(())
}

fn reported_version_matches(reported: &str, expected: &str) -> bool {
    let Ok(expected) = Version::parse(expected) else {
        return false;
    };
    reported
        .split_whitespace()
        .filter_map(|token| Version::parse(token).ok())
        .any(|version| version == expected)
}

fn select_rollback_release(
    data_dir: &Path,
    current: &str,
    requested: Option<&str>,
) -> Result<VerifiedRelease> {
    let current_version = Version::parse(current.trim())
        .with_context(|| format!("Current Deixic Code version is not valid semver: {current}"))?;
    let releases = list_verified_releases(data_dir)?;
    if let Some(requested) = requested {
        let requested_version = Version::parse(requested.trim())
            .with_context(|| format!("Rollback version is not valid semver: {requested}"))?;
        if requested_version >= current_version {
            bail!(
                "{}",
                crate::localization::cli_locale().format(
                    "Rollback target must be older than the active version {0}",
                    &[(current).to_string()]
                )
            );
        }
        return releases
            .into_iter()
            .find(|release| release.version == requested_version)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "Rollback target {requested} is not a retained, previously verified native release"
                )
            });
    }
    releases
        .into_iter()
        .filter(|release| release.version < current_version)
        .max_by(|left, right| left.version.cmp(&right.version))
        .ok_or_else(|| {
            anyhow::anyhow!(
                "No retained, previously verified native release is available for rollback"
            )
        })
}

fn shell_quote_path(path: &Path) -> String {
    let value = path.to_string_lossy();
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn launcher_contents(
    install_dir: &Path,
    data_dir: &Path,
    release_dir: &Path,
    version: &str,
) -> Vec<u8> {
    format!(
        "#!/usr/bin/env bash\nset -eu\nrelease_dir={}\ninstall_dir={}\ndata_dir={}\nrelease_version={}\nexport MAESTRO_INSTALL_METHOD=release\nexport MAESTRO_INSTALL_DIR=\"$install_dir\"\nexport MAESTRO_DATA_DIR=\"$data_dir\"\nexport MAESTRO_STARTUP_UPDATE_STATE=\"${{MAESTRO_STARTUP_UPDATE_STATE:-$data_dir/startup-update-state.json}}\"\nexport MAESTRO_VERSION=\"$release_version\"\nexec \"$release_dir/bin/maestro\" \"$@\"\n",
        shell_quote_path(release_dir),
        shell_quote_path(install_dir),
        shell_quote_path(data_dir),
        shell_quote_path(Path::new(version)),
    )
    .into_bytes()
}

#[derive(Debug, Default)]
struct AtomicWriteOutcome {
    durability_warning: Option<String>,
}

fn atomic_write_executable(path: &Path, contents: &[u8]) -> Result<AtomicWriteOutcome> {
    let parent = path
        .parent()
        .context("stable Deixic Code launcher has no parent directory")?;
    fs::create_dir_all(parent)?;
    let mut temporary = tempfile::Builder::new()
        .prefix(".maestro-update-")
        .tempfile_in(parent)?;
    temporary.write_all(contents)?;
    temporary.as_file().sync_all()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        temporary
            .as_file()
            .set_permissions(fs::Permissions::from_mode(0o755))?;
    }
    let temporary_path = temporary.into_temp_path();
    fs::rename(&temporary_path, path).with_context(|| {
        format!(
            "Failed to atomically repoint the Deixic Code launcher {}",
            path.display()
        )
    })?;
    #[cfg(unix)]
    let durability_warning = match OpenOptions::new().read(true).open(parent) {
        Ok(directory) => directory.sync_all().err().map(|error| {
            format!(
                "Launcher {} was replaced, but syncing its parent directory failed: {error}",
                path.display()
            )
        }),
        Err(error) => Some(format!(
            "Launcher {} was replaced, but opening its parent directory for syncing failed: {error}",
            path.display()
        )),
    };
    #[cfg(not(unix))]
    let durability_warning = None;
    Ok(AtomicWriteOutcome { durability_warning })
}

fn read_startup_state(path: &Path) -> Option<StartupUpdateState> {
    let contents = fs::read(path).ok()?;
    serde_json::from_slice(&contents).ok()
}

fn write_startup_state(path: &Path, state: &StartupUpdateState) -> Result<()> {
    crate::path_utils::atomic_private_write(path, &serde_json::to_vec_pretty(state)?)
}

fn restore_startup_state(path: &Path, previous: Option<&StartupUpdateState>) -> Result<()> {
    if let Some(previous) = previous {
        return write_startup_state(path, previous);
    }
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

struct StartupUpdateLock {
    _lock: FileLock<fs::File>,
}

fn try_acquire_startup_update_lock(state_path: &Path) -> io::Result<Option<StartupUpdateLock>> {
    let lock_path = state_path.with_extension("lock");
    if let Some(parent) = lock_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(lock_path)?;
    let mut lock = FileLock::new(file);
    {
        let guard = match lock.try_write() {
            Ok(guard) => guard,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(None),
            Err(error) => return Err(error),
        };
        // The operating-system lock is tied to the open file and is released
        // when `StartupUpdateLock` is dropped. Forgetting the borrowing guard
        // lets this function return ownership of that file-backed lock.
        std::mem::forget(guard);
    }
    Ok(Some(StartupUpdateLock { _lock: lock }))
}

fn acquire_update_lock(context: &InstallContext) -> Result<Option<StartupUpdateLock>> {
    let Some(state_path) = startup_state_path_for(Some(context)) else {
        return Ok(None);
    };
    let lock = try_acquire_startup_update_lock(&state_path).with_context(|| {
        format!(
            "Failed to lock Deixic Code update state {}",
            state_path.display()
        )
    })?;
    if lock.is_none() {
        bail!(
            "{}",
            crate::localization::cli_locale()
                .format("Another Deixic Code update is already in progress", &[])
        );
    }
    Ok(lock)
}

fn should_throttle_startup_update(
    state: Option<&StartupUpdateState>,
    version: &str,
    now_ms: u64,
    retry: Duration,
) -> bool {
    let Some(state) = state else {
        return false;
    };
    state.version == version && now_ms < startup_retry_deadline(state, retry)
}

fn is_startup_retryable(
    attempt: &UpdateAttempt,
    startup_state: Option<&StartupUpdateState>,
    latest_version: Option<&str>,
    check_status: Option<&str>,
) -> bool {
    check_status == Some("available")
        && attempt.trigger == "startup"
        && startup_state.is_some()
        && matches!(attempt.status.as_str(), "failed" | "started")
        && latest_version.is_none_or(|latest| attempt.to_version.as_deref() == Some(latest))
}

fn startup_attempt_from_state(state: &StartupUpdateState) -> UpdateAttempt {
    UpdateAttempt {
        attempt_id: format!("startup-state-{}", state.last_attempt_at),
        operation: "update".to_owned(),
        trigger: "startup".to_owned(),
        status: state.last_status.clone(),
        attempted_at_ms: state.last_attempt_at,
        completed_at_ms: (state.last_status == "updated").then_some(state.last_attempt_at),
        from_version: None,
        to_version: Some(state.version.clone()),
        source_url: state.source_url.clone(),
        error: state.last_error.clone(),
        release_notes: None,
        release_receipt: None,
        verification: None,
    }
}

fn startup_retry_deadline(state: &StartupUpdateState, retry: Duration) -> u64 {
    state.retry_after_at.unwrap_or_else(|| {
        state
            .last_attempt_at
            .saturating_add(retry.as_millis() as u64)
    })
}

fn rollback_suppresses_startup_update(
    state: Option<&StartupUpdateState>,
    latest_version: &str,
) -> bool {
    let Some(rollback_version) = state.and_then(|state| state.rollback_version.as_deref()) else {
        return false;
    };
    let Ok(latest) = Version::parse(latest_version) else {
        return false;
    };
    let Ok(rollback) = Version::parse(rollback_version) else {
        return false;
    };
    latest > rollback
}

fn clear_rollback_suppression(path: Option<&Path>) -> Result<()> {
    let Some(path) = path else {
        return Ok(());
    };
    let Some(mut state) = read_startup_state(path) else {
        return Ok(());
    };
    if state.rollback_version.take().is_some() {
        write_startup_state(path, &state)?;
    }
    Ok(())
}

fn persist_rollback_suppression(path: &Path, version: &str) -> Result<()> {
    let state = StartupUpdateState {
        version: version.to_owned(),
        last_attempt_at: now_ms(),
        last_status: "rolledBack".to_owned(),
        source_url: None,
        last_error: None,
        retry_after_at: None,
        rollback_version: Some(version.to_owned()),
    };
    write_startup_state(path, &state)
}

fn startup_update_mode() -> &'static str {
    let mode = env::var("MAESTRO_AUTO_UPDATE")
        .or_else(|_| env::var("MAESTRO_STARTUP_UPDATE"))
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    if matches!(mode.as_str(), "0" | "false" | "off" | "skip" | "disabled") {
        "off"
    } else if matches!(mode.as_str(), "check" | "notice" | "notify") {
        "check"
    } else {
        "apply"
    }
}

fn startup_update_enabled() -> bool {
    env::var_os("MAESTRO_SKIP_STARTUP_UPDATE").is_none()
        && env::var_os("CI").is_none()
        && env::var("NODE_ENV").ok().as_deref() != Some("test")
        && startup_update_mode() != "off"
        && std::io::stdin().is_terminal()
        && std::io::stdout().is_terminal()
}

/// Best-effort update of an installed interactive Maestro before the TUI starts.
///
/// Returns the restarted process exit code after a successful update. All check,
/// state, and install failures fail open so an unavailable update service can
/// never prevent Maestro from starting.
pub async fn run_startup_update(raw_args: &[std::ffi::OsString]) -> Option<i32> {
    if maestro_local_host::safety::vendor_network_disabled() {
        return None;
    }
    if !startup_update_enabled() {
        return None;
    }
    let context = install_context()?;
    let channel = UpdateChannel::from_environment().ok()?;
    let current = current_version();
    let urls = trusted_startup_update_urls(&context, channel);
    let total_timeout = env_duration(
        "MAESTRO_STARTUP_UPDATE_TIMEOUT_MS",
        DEFAULT_STARTUP_CHECK_TIMEOUT,
        false,
    );
    let source_count = u128::try_from(urls.len().max(1)).unwrap_or(1);
    let source_timeout = Duration::from_millis(
        u64::try_from((total_timeout.as_millis() / source_count).max(1)).unwrap_or(1),
    );
    let check = match tokio::time::timeout(
        total_timeout,
        check_for_update_urls_with_timeout(&current, urls, source_timeout, channel),
    )
    .await
    {
        Ok(check) if check.status != "failed" => check,
        _ => return None,
    };
    if check.status != "available" {
        return None;
    }
    let latest = check.latest_version.as_deref()?;
    if startup_update_mode() == "check" {
        eprintln!(
            "{}",
            crate::localization::cli_locale().format(
                "Deixic Code {0} is available (current {1}); run `deixic-code update`.",
                &[(latest).to_string(), (current).clone()]
            )
        );
        return None;
    }

    let state_path = startup_state_path_for(Some(&context))?;
    let update_lock = try_acquire_startup_update_lock(&state_path).ok()??;
    let now_ms = now_ms();
    let persisted_state = read_startup_state(&state_path);
    if rollback_suppresses_startup_update(persisted_state.as_ref(), latest) {
        return None;
    }
    let retry = env_duration(
        "MAESTRO_STARTUP_UPDATE_RETRY_MS",
        DEFAULT_STARTUP_RETRY,
        true,
    );
    if should_throttle_startup_update(persisted_state.as_ref(), latest, now_ms, retry) {
        return None;
    }
    let attempted = StartupUpdateState {
        version: latest.to_owned(),
        last_attempt_at: now_ms,
        last_status: "failed".to_owned(),
        source_url: Some(check.source_url.clone()),
        last_error: None,
        retry_after_at: Some(now_ms.saturating_add(retry.as_millis() as u64)),
        rollback_version: None,
    };
    if write_startup_state(&state_path, &attempted).is_err() {
        return None;
    }
    let history_path = update_history_path(Some(&context));
    let attempt = new_update_attempt(
        "update",
        "startup",
        Some(&current),
        Some(latest),
        Some(&check.source_url),
        check.release_notes.as_deref(),
        check.release_receipt.as_ref(),
    );
    if begin_update_attempt(history_path.as_deref(), &attempt).is_err() {
        let _ = restore_startup_state(&state_path, persisted_state.as_ref());
        return None;
    }
    eprintln!(
        "{}",
        crate::localization::cli_locale().format(
            "Updating Deixic Code from {0} to {1}...",
            &[(current).clone(), (latest).to_string()]
        )
    );
    if let Err(error) = install(
        &context,
        latest,
        check.release_url.as_deref(),
        channel,
        false,
    ) {
        let mut failed = attempted;
        failed.last_error = Some(format!("{error:#}"));
        let _ = write_startup_state(&state_path, &failed);
        let _ = finish_update_attempt(
            history_path.as_deref(),
            &attempt.attempt_id,
            "failed",
            Some(format!("{error:#}")),
            None,
        );
        eprintln!(
            "{}",
            crate::localization::cli_locale().format(
                "Deixic Code auto-update failed; continuing with {0}: {1}",
                &[(current).clone(), format!("{:#}", error)]
            )
        );
        return None;
    }
    let verification = match &context {
        InstallContext::Release { data_dir, .. } => find_release_installation(data_dir, latest),
        InstallContext::Package { .. } => None,
    };
    let _ = finish_update_attempt(
        history_path.as_deref(),
        &attempt.attempt_id,
        "updated",
        None,
        verification,
    );
    let completed = StartupUpdateState {
        last_status: "updated".to_owned(),
        last_error: None,
        ..attempted
    };
    let _ = write_startup_state(&state_path, &completed);
    eprintln!(
        "{}",
        crate::localization::cli_locale().format(
            "Updated Deixic Code to {0}; restarting.",
            &[(latest).to_string()]
        )
    );
    drop(update_lock);

    let mut restart = Command::new(launcher(&context));
    restart
        .args(raw_args.iter().skip(1))
        .env("MAESTRO_SKIP_STARTUP_UPDATE", "1");
    match restart.status() {
        Ok(status) => Some(status.code().unwrap_or(1)),
        Err(error) => {
            eprintln!(
                "{}",
                crate::localization::cli_locale().format(
                    "Deixic Code was updated, but automatic restart failed: {0}",
                    &[(error).to_string()]
                )
            );
            None
        }
    }
}

fn current_version() -> String {
    env::var("MAESTRO_VERSION")
        .ok()
        .or_else(|| legacy_package_version(&env::current_exe().ok()?))
        .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_owned())
}

fn install_method(context: Option<&InstallContext>) -> Option<String> {
    context.map(|context| match context {
        InstallContext::Package { .. } => "package".to_owned(),
        InstallContext::Release { .. } => "release".to_owned(),
    })
}

async fn build_update_status(
    context: Option<&InstallContext>,
    channel: UpdateChannel,
) -> Result<UpdateStatus> {
    let current = current_version();
    let history_path = update_history_path(context);
    let history = load_update_history(history_path.as_deref())?;
    let startup_state = startup_state_path_for(context).and_then(|path| read_startup_state(&path));
    let startup_attempt = startup_state.as_ref().map(startup_attempt_from_state);
    let last_attempt = history
        .attempts
        .last()
        .cloned()
        .or_else(|| startup_attempt.clone());
    let check = match context {
        Some(context) => Some(check_for_update(&current, context, channel).await),
        None => None,
    };
    let verification = context.and_then(current_release_receipt);
    let current_metadata = context
        .and_then(current_release_dir)
        .and_then(|release_dir| {
            verification
                .as_ref()
                .and_then(|receipt| load_verified_release_metadata(&release_dir, receipt))
        });
    let latest_version = check
        .as_ref()
        .and_then(|check| check.latest_version.clone());
    let last_retryable = startup_attempt.as_ref().filter(|attempt| {
        is_startup_retryable(
            attempt,
            startup_state.as_ref(),
            latest_version.as_deref(),
            check.as_ref().map(|check| check.status),
        )
    });
    let retry_window = env_duration(
        "MAESTRO_STARTUP_UPDATE_RETRY_MS",
        DEFAULT_STARTUP_RETRY,
        true,
    );
    let retry_window_ms = retry_window.as_millis() as u64;
    let (last_attempt_at_ms, next_attempt_at_ms, retry_after_ms, throttled) =
        if let Some(attempt) = last_retryable {
            let next = startup_state
                .as_ref()
                .map(|state| startup_retry_deadline(state, retry_window))
                .unwrap_or_else(|| attempt.attempted_at_ms.saturating_add(retry_window_ms));
            let remaining = next.saturating_sub(now_ms());
            (
                Some(attempt.attempted_at_ms),
                Some(next),
                Some(remaining),
                remaining > 0,
            )
        } else {
            (None, None, None, false)
        };
    let state = check
        .as_ref()
        .map(|check| check.status.to_owned())
        .unwrap_or_else(|| "unavailable".to_owned());
    let update_source = check
        .as_ref()
        .map(|check| check.source_url.clone())
        .filter(|source| !source.is_empty())
        .or_else(|| {
            last_attempt
                .as_ref()
                .and_then(|attempt| attempt.source_url.clone())
        });
    let release_notes = check
        .as_ref()
        .and_then(|check| check.release_notes.clone())
        .or_else(|| {
            current_metadata.as_ref().and_then(|metadata| {
                metadata
                    .release_notes
                    .clone()
                    .or_else(|| metadata.notes.clone())
            })
        });
    let release_receipt = check
        .as_ref()
        .and_then(|check| check.release_receipt.clone())
        .or_else(|| {
            current_metadata
                .as_ref()
                .and_then(|metadata| metadata.receipt.clone())
        });
    let channel_verification = check
        .as_ref()
        .and_then(|check| check.channel_verification.clone());
    Ok(UpdateStatus {
        schema_version: UPDATE_STATUS_SCHEMA,
        state,
        channel,
        active_version: Some(current.clone()),
        current_version: Some(current),
        latest_version,
        install_method: install_method(context),
        update_source,
        channel_verification,
        release_notes,
        release_receipt,
        last_attempt,
        retry: RetryStatus {
            window_ms: retry_window_ms,
            last_attempt_at_ms,
            next_attempt_at_ms,
            retry_after_ms,
            throttled,
        },
        verification,
        history_path: history_path.map(|path| path.display().to_string()),
        check_error: check.and_then(|check| check.error),
    })
}

async fn run_status(json: bool, channel: UpdateChannel) -> Result<i32> {
    let context = install_context();
    let status = build_update_status(context.as_ref(), channel).await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&status)?);
    } else {
        println!(
            "{}",
            crate::localization::cli_locale().format(
                "Deixic Code update status: {0}",
                std::slice::from_ref(&(status.state))
            )
        );
        println!("  channel: {}", status.channel.as_str());
        println!(
            "  active/current: {} / {}",
            status.active_version.as_deref().unwrap_or("unknown"),
            status.current_version.as_deref().unwrap_or("unknown")
        );
        println!(
            "  latest: {}",
            status.latest_version.as_deref().unwrap_or("unknown")
        );
        println!(
            "{}",
            crate::localization::cli_locale().format(
                "  install method: {0}",
                &[status
                    .install_method
                    .as_deref()
                    .unwrap_or("unknown")
                    .to_string()]
            )
        );
        println!(
            "{}",
            crate::localization::cli_locale().format(
                "  update source: {0}",
                &[status
                    .update_source
                    .as_deref()
                    .unwrap_or("unknown")
                    .to_string()]
            )
        );
        println!("  channel: {}", status.channel.as_str());
        if let Some(channel) = status.channel_verification.as_ref() {
            println!(
                "{}",
                crate::localization::cli_locale().format(
                    "  channel verification: {0} ({1})",
                    &[
                        (channel.status).clone(),
                        channel
                            .algorithm
                            .as_deref()
                            .unwrap_or("unknown")
                            .to_string()
                    ]
                )
            );
        } else {
            println!(
                "{}",
                crate::localization::cli_locale()
                    .format("  channel verification: unavailable", &[])
            );
        }
        if let Some(attempt) = status.last_attempt.as_ref() {
            println!(
                "{}",
                crate::localization::cli_locale().format(
                    "  last attempt: {0} {1} -> {2}",
                    &[
                        (attempt.status).clone(),
                        attempt
                            .from_version
                            .as_deref()
                            .unwrap_or("unknown")
                            .to_string(),
                        attempt
                            .to_version
                            .as_deref()
                            .unwrap_or("unknown")
                            .to_string()
                    ]
                )
            );
            if let Some(error) = attempt.error.as_deref() {
                println!(
                    "{}",
                    crate::localization::cli_locale()
                        .format("  last error: {0}", &[(error).to_string()])
                );
            }
        } else {
            println!(
                "{}",
                crate::localization::cli_locale().format("  last attempt: none", &[])
            );
        }
        println!(
            "  retry: {}ms{}",
            status.retry.window_ms,
            status
                .retry
                .retry_after_ms
                .map(|remaining| format!(", next in {remaining}ms"))
                .unwrap_or_default()
        );
        if let Some(receipt) = status.verification.as_ref() {
            println!(
                "  verification: {} ({})",
                if receipt.verified {
                    "verified"
                } else {
                    "not verified"
                },
                receipt.schema_version
            );
        } else {
            println!("  verification: unavailable");
        }
        if let Some(error) = status.check_error.as_deref() {
            println!(
                "{}",
                crate::localization::cli_locale()
                    .format("  check error: {0}", &[(error).to_string()])
            );
        }
    }
    Ok(0)
}

fn run_history(json: bool) -> Result<i32> {
    let context = install_context();
    let path = update_history_path(context.as_ref());
    let history = load_update_history(path.as_deref())?;
    if json {
        let output = UpdateHistoryOutput {
            schema_version: UPDATE_HISTORY_SCHEMA,
            history_path: path.map(|path| path.display().to_string()),
            attempts: history.attempts,
        };
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else if history.attempts.is_empty() {
        println!(
            "{}",
            crate::localization::cli_locale()
                .format("No Deixic Code update attempts recorded.", &[])
        );
    } else {
        for attempt in history.attempts.iter().rev() {
            println!(
                "{} {} {} -> {}",
                attempt.status,
                attempt.attempted_at_ms,
                attempt.from_version.as_deref().unwrap_or("unknown"),
                attempt.to_version.as_deref().unwrap_or("unknown")
            );
            if let Some(error) = attempt.error.as_deref() {
                println!("  error: {error}");
            }
        }
    }
    Ok(0)
}

async fn run_rollback(requested: Option<String>, json: bool) -> Result<i32> {
    let context = install_context().context(
        "deixic-code update rollback requires a native release installation; package-manager rollback is not supported",
    )?;
    let InstallContext::Release {
        install_dir,
        data_dir,
        launcher,
    } = &context
    else {
        bail!("{}", crate::localization::cli_locale().format("package-manager rollback is not supported; rollback requires a retained, previously verified native release", &[]));
    };
    let current = current_version();
    let _update_lock = acquire_update_lock(&context)?;
    let release = select_rollback_release(data_dir, &current, requested.as_deref())?;
    let startup_state_path = startup_state_path_for(Some(&context))
        .context("native release rollback requires persistent startup state")?;
    let previous_startup_state = read_startup_state(&startup_state_path);
    let history_path = update_history_path(Some(&context));
    let attempt = new_update_attempt(
        "rollback",
        "manual",
        Some(&current),
        Some(&release.version_text),
        None,
        release.metadata.as_ref().and_then(|metadata| {
            metadata
                .release_notes
                .as_deref()
                .or(metadata.notes.as_deref())
        }),
        release
            .metadata
            .as_ref()
            .and_then(|metadata| metadata.receipt.as_ref()),
    );
    begin_update_attempt(history_path.as_deref(), &attempt)?;
    if let Err(error) = verify_retained_release(&release) {
        let _ = finish_update_attempt(
            history_path.as_deref(),
            &attempt.attempt_id,
            "failed",
            Some(format!("{error:#}")),
            Some(release.receipt.clone()),
        );
        return Err(error);
    }
    if let Err(error) = persist_rollback_suppression(&startup_state_path, &release.version_text) {
        let _ = finish_update_attempt(
            history_path.as_deref(),
            &attempt.attempt_id,
            "failed",
            Some(format!("{error:#}")),
            Some(release.receipt.clone()),
        );
        return Err(error);
    }
    let contents = launcher_contents(
        install_dir,
        data_dir,
        &release.release_dir,
        &release.version_text,
    );
    let launcher_write = match atomic_write_executable(launcher, &contents) {
        Ok(outcome) => outcome,
        Err(error) => {
            let error =
                match restore_startup_state(&startup_state_path, previous_startup_state.as_ref()) {
                    Ok(()) => error,
                    Err(restore_error) => error.context(format!(
                        "also failed to restore startup state {}: {restore_error:#}",
                        startup_state_path.display()
                    )),
                };
            let _ = finish_update_attempt(
                history_path.as_deref(),
                &attempt.attempt_id,
                "failed",
                Some(format!("{error:#}")),
                Some(release.receipt.clone()),
            );
            return Err(error);
        }
    };
    let (completed, history_error) = match finish_update_attempt(
        history_path.as_deref(),
        &attempt.attempt_id,
        "rolledBack",
        None,
        Some(release.receipt.clone()),
    ) {
        Ok(completed) => (completed, None),
        Err(error) => (None, Some(format!("{error:#}"))),
    };
    let outcome = RollbackOutcome {
        schema_version: UPDATE_STATUS_SCHEMA,
        status: "rolledBack",
        from_version: current,
        active_version: release.version_text,
        attempt: completed,
        verification: Some(release.receipt),
        history_error: history_error.clone(),
        launcher_warning: launcher_write.durability_warning.clone(),
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&outcome)?);
    } else {
        println!(
            "{}",
            crate::localization::cli_locale().format(
                "Rolled Deixic Code back to {0}.",
                std::slice::from_ref(&(outcome.active_version))
            )
        );
        if let Some(error) = history_error {
            eprintln!(
                "{}",
                crate::localization::cli_locale().format(
                    "Deixic Code rollback succeeded, but update history persistence failed: {0}",
                    std::slice::from_ref(&(error))
                )
            );
        }
        if let Some(warning) = outcome.launcher_warning {
            eprintln!(
                "{}",
                crate::localization::cli_locale().format(
                    "Deixic Code rollback completed with a launcher durability warning: {0}",
                    std::slice::from_ref(&(warning))
                )
            );
        }
    }
    Ok(0)
}

fn print_help() {
    println!("{}", crate::localization::cli_locale().format("Usage: deixic-code update [status|history|rollback [version]] [--channel stable|beta|alpha] [--json]\n\nCommands:\n  status    Show current/latest versions, receipt, retry, and last attempt\n  history   Show the bounded persisted update-attempt history\n  rollback  Repoint a native release launcher to a retained verified release\n\nOptions:\n  --channel Select stable, beta, or alpha for this update (default: stable)\n  --check   Check for the newest version without installing it (legacy apply mode)\n  --json    Print the machine-readable lifecycle contract\n  --help    Show this help", &[]));
}

fn write_update_header(output: &mut impl Write, current: &str) -> io::Result<()> {
    writeln!(output, "Deixic Code Update")?;
    writeln!(output)?;
    writeln!(output, "Current version: {current}")?;
    writeln!(output, "Checking for updates...")
}

fn write_update_target(output: &mut impl Write, current: &str, latest: &str) -> io::Result<()> {
    writeln!(output)?;
    writeln!(output, "Updating: {current} → {latest}")
}

fn write_update_success(output: &mut impl Write, latest: &str) -> io::Result<()> {
    writeln!(output, "✓ Successfully updated to version {latest}!")
}

pub async fn run_update(args: &[String]) -> Result<i32> {
    maestro_local_host::safety::require_vendor_network()?;
    let parsed = match parse_args(args) {
        Ok(parsed) => parsed,
        Err(error) => {
            eprintln!("{error}");
            return Ok(1);
        }
    };
    if parsed.help {
        print_help();
        return Ok(0);
    }

    match parsed.action.clone() {
        UpdateAction::Status => return run_status(parsed.json, parsed.channel).await,
        UpdateAction::History => return run_history(parsed.json),
        UpdateAction::Rollback { version } => return run_rollback(version, parsed.json).await,
        UpdateAction::Apply => {}
    }

    let current = current_version();
    let context = install_context().context(
        "deixic-code update is available for signed release and global npm/Bun installations",
    )?;
    let _update_lock = acquire_update_lock(&context)?;
    let show_progress = !parsed.json && !parsed.check_only;
    if show_progress {
        let mut output = io::stdout().lock();
        write_update_header(&mut output, &current)?;
        output.flush()?;
    }
    let check = check_for_update(&current, &context, parsed.channel).await;
    if parsed.check_only {
        if parsed.json {
            println!("{}", serde_json::to_string_pretty(&check)?);
        } else if check.status == "available" {
            println!(
                "{}",
                crate::localization::cli_locale().format(
                    "Deixic Code {0} is available (current {1}).",
                    &[
                        check
                            .latest_version
                            .as_deref()
                            .unwrap_or("update")
                            .to_string(),
                        (current).clone()
                    ]
                )
            );
        } else if check.status == "current" {
            println!(
                "{}",
                crate::localization::cli_locale().format(
                    "Deixic Code is up to date ({0}).",
                    std::slice::from_ref(&(current))
                )
            );
        } else {
            eprintln!(
                "{}",
                crate::localization::cli_locale().format(
                    "Deixic Code update check failed: {0}",
                    &[check
                        .error
                        .as_deref()
                        .unwrap_or("unknown error")
                        .to_string()]
                )
            );
        }
        return Ok(i32::from(check.status == "failed"));
    }
    if check.status == "failed" {
        let history_path = update_history_path(Some(&context));
        let attempt = new_update_attempt(
            "update",
            "manual",
            Some(&current),
            None,
            (!check.source_url.is_empty()).then_some(check.source_url.as_str()),
            None,
            None,
        );
        begin_update_attempt(history_path.as_deref(), &attempt)?;
        let error_text = check
            .error
            .clone()
            .unwrap_or_else(|| "unknown error".to_owned());
        let _ = finish_update_attempt(
            history_path.as_deref(),
            &attempt.attempt_id,
            "failed",
            Some(error_text.clone()),
            None,
        );
        if parsed.json {
            let mut outcome = check;
            outcome.attempt_id = Some(attempt.attempt_id);
            println!("{}", serde_json::to_string_pretty(&outcome)?);
        } else {
            eprintln!(
                "{}",
                crate::localization::cli_locale().format(
                    "Deixic Code update failed: {0}",
                    std::slice::from_ref(&(error_text))
                )
            );
        }
        return Ok(1);
    }
    if check.status == "current" {
        if parsed.json {
            println!("{}", serde_json::to_string_pretty(&check)?);
        } else {
            println!(
                "{}",
                crate::localization::cli_locale().format(
                    "Deixic Code is up to date ({0}).",
                    std::slice::from_ref(&(current))
                )
            );
        }
        return Ok(0);
    }
    let latest = check
        .latest_version
        .as_deref()
        .context("Update metadata missing latest version")?;
    if show_progress {
        let mut output = io::stdout().lock();
        write_update_target(&mut output, &current, latest)?;
        output.flush()?;
    }
    clear_rollback_suppression(startup_state_path_for(Some(&context)).as_deref())?;
    let history_path = update_history_path(Some(&context));
    let attempt = new_update_attempt(
        "update",
        "manual",
        Some(&current),
        Some(latest),
        Some(&check.source_url),
        check.release_notes.as_deref(),
        check.release_receipt.as_ref(),
    );
    begin_update_attempt(history_path.as_deref(), &attempt)?;
    match install(
        &context,
        latest,
        check.release_url.as_deref(),
        parsed.channel,
        show_progress,
    ) {
        Ok(()) => {
            let verification = match &context {
                InstallContext::Release { data_dir, .. } => {
                    find_release_installation(data_dir, latest)
                }
                InstallContext::Package { .. } => None,
            };
            let history_error = finish_update_attempt(
                history_path.as_deref(),
                &attempt.attempt_id,
                "updated",
                None,
                verification.clone(),
            )
            .err()
            .map(|error| format!("{error:#}"));
            if parsed.json {
                let mut outcome = check.clone();
                outcome.status = "updated";
                outcome.attempt_id = Some(attempt.attempt_id);
                outcome.verification = verification;
                outcome.error = history_error.as_ref().map(|error| {
                    format!("Update installed, but history persistence failed: {error}")
                });
                println!("{}", serde_json::to_string_pretty(&outcome)?);
            } else {
                let mut output = io::stdout().lock();
                write_update_success(&mut output, latest)?;
                if let Some(error) = history_error {
                    eprintln!("{}", crate::localization::cli_locale().format("Deixic Code updated successfully, but update history persistence failed: {0}", std::slice::from_ref(&(error))));
                }
            }
            Ok(0)
        }
        Err(error) => {
            let error_text = format!("{error:#}");
            let _ = finish_update_attempt(
                history_path.as_deref(),
                &attempt.attempt_id,
                "failed",
                Some(error_text.clone()),
                None,
            );
            if parsed.json {
                let mut outcome = check;
                outcome.status = "failed";
                outcome.error = Some(error_text);
                outcome.attempt_id = Some(attempt.attempt_id);
                println!("{}", serde_json::to_string_pretty(&outcome)?);
            } else {
                eprintln!(
                    "{}",
                    crate::localization::cli_locale()
                        .format("Deixic Code update failed: {0}", &[format!("{:#}", error)])
                );
            }
            Ok(1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;

    #[test]
    fn parses_update_options() {
        let args = vec![
            "--check".to_owned(),
            "--json".to_owned(),
            "--channel".to_owned(),
            "beta".to_owned(),
        ];
        let parsed = parse_args(&args).expect("parse update args");
        assert!(parsed.check_only);
        assert!(parsed.json);
        assert_eq!(parsed.channel, UpdateChannel::Beta);
    }

    #[test]
    fn renders_the_manual_update_progress_contract() {
        let mut output = Vec::new();
        write_update_header(&mut output, "0.197.0").expect("write update header");
        write_update_target(&mut output, "0.197.0", "0.212.0").expect("write update target");
        write_update_success(&mut output, "0.212.0").expect("write update success");

        assert_eq!(
            String::from_utf8(output).expect("progress output is utf-8"),
            "Deixic Code Update\n\nCurrent version: 0.197.0\nChecking for updates...\n\nUpdating: 0.197.0 → 0.212.0\n✓ Successfully updated to version 0.212.0!\n"
        );
    }

    #[test]
    fn verifies_signed_stable_channel_manifest_and_rejects_tampering() {
        let manifest = ChannelManifest {
            schema_version: CHANNEL_MANIFEST_SCHEMA.to_owned(),
            channel: "stable".to_owned(),
            key_id: STABLE_CHANNEL_KEY_ID.to_owned(),
            version: "1.2.3".to_owned(),
            release_tag: "v1.2.3".to_owned(),
            release_url: "https://github.com/evalops/maestro/releases/download/v1.2.3".to_owned(),
            metadata_url: Some(
                "https://github.com/evalops/maestro/releases/download/v1.2.3/release-metadata.json"
                    .to_owned(),
            ),
            metadata_sha256: None,
            source_sha: "a".repeat(40),
            issued_at_ms: 1,
            release_notes: None,
            release_receipt: None,
            signature: "0PaVbvGiUaH3DTgqHgfl6JvvC8VlzmRgM7cDWIXkLJwG4aXb6rTXn2ZfVVwylQVLoJX53cCSIhtM18TcYycdBw=="
                .to_owned(),
        };
        verify_channel_manifest(&manifest, UpdateChannel::Stable)
            .expect("fixture signature verifies");

        let mut tampered = manifest;
        tampered.release_url.push_str("/tampered");
        assert!(verify_channel_manifest(&tampered, UpdateChannel::Stable).is_err());
    }

    #[test]
    fn channel_policy_rejects_mismatched_release_versions() {
        let stable = ChannelManifest {
            schema_version: CHANNEL_MANIFEST_SCHEMA.to_owned(),
            channel: "stable".to_owned(),
            key_id: STABLE_CHANNEL_KEY_ID.to_owned(),
            version: "1.2.3-rc.1".to_owned(),
            release_tag: "v1.2.3-rc.1".to_owned(),
            release_url: "https://github.com/evalops/maestro/releases/download/v1.2.3-rc.1"
                .to_owned(),
            metadata_url: None,
            metadata_sha256: None,
            source_sha: "a".repeat(40),
            issued_at_ms: 1,
            release_notes: None,
            release_receipt: None,
            signature: String::new(),
        };
        assert!(verify_channel_manifest(&stable, UpdateChannel::Stable).is_err());

        let beta = ChannelManifest {
            channel: "beta".to_owned(),
            key_id: PRERELEASE_CHANNEL_KEY_ID.to_owned(),
            version: "1.2.3".to_owned(),
            release_tag: "v1.2.3".to_owned(),
            ..stable
        };
        assert!(verify_channel_manifest(&beta, UpdateChannel::Beta).is_err());
    }

    #[test]
    fn channel_policy_rejects_versions_the_installer_cannot_accept() {
        for (channel, version) in [
            (UpdateChannel::Beta, "1.2.3-beta.foo"),
            (UpdateChannel::Beta, "1.2.3-beta.0"),
            (UpdateChannel::Alpha, "1.2.3-alpha.foo"),
            (UpdateChannel::Alpha, "1.2.3-alpha.0"),
            (UpdateChannel::Stable, "1.2.3+build"),
        ] {
            assert!(
                !tag_matches_channel(&format!("v{version}"), channel),
                "{channel:?} must reject {version}"
            );
        }
    }

    #[test]
    fn parses_lifecycle_subcommands() {
        assert_eq!(
            parse_args(&["status".to_owned(), "--json".to_owned()])
                .expect("status args")
                .action,
            UpdateAction::Status
        );
        assert_eq!(
            parse_args(&["history".to_owned()])
                .expect("history args")
                .action,
            UpdateAction::History
        );
        assert_eq!(
            parse_args(&[
                "rollback".to_owned(),
                "0.9.0".to_owned(),
                "--json".to_owned()
            ])
            .expect("rollback args")
            .action,
            UpdateAction::Rollback {
                version: Some("0.9.0".to_owned())
            }
        );
        assert_eq!(
            parse_args(&["rollback".to_owned(), "--json".to_owned()])
                .expect("default rollback args")
                .action,
            UpdateAction::Rollback { version: None }
        );
        assert!(parse_args(&["rollback".to_owned(), "--check".to_owned()]).is_err());
    }

    #[test]
    fn rejects_unknown_update_options() {
        let error = parse_args(&["--wat".to_owned()]).expect_err("unknown option");
        assert!(
            error
                .to_string()
                .contains("Unknown deixic-code update option")
        );
        assert!(parse_args(&["--channel".to_owned(), "nightly".to_owned()]).is_err());
        assert!(
            parse_args(&[
                "history".to_owned(),
                "--channel".to_owned(),
                "alpha".to_owned()
            ])
            .is_err()
        );
    }

    #[test]
    fn strips_untrusted_package_manager_environment() {
        for key in [
            "NODE_OPTIONS",
            "NPM_TOKEN",
            "NODE_AUTH_TOKEN",
            "MAESTRO_UPDATE_URL",
            "npm_config_userconfig",
            "NPM_CONFIG_REGISTRY",
            "BUN_CONFIG_REGISTRY",
            "YARN_REGISTRY",
            "PNPM_HOME",
        ] {
            assert!(should_remove_package_manager_env(key), "kept {key}");
        }
        assert!(!should_remove_package_manager_env("PATH"));
        assert!(!should_remove_package_manager_env("NPM_CONFIG_PREFIX"));
        assert!(!should_remove_package_manager_env("npm_config_prefix"));
    }

    #[test]
    fn preserves_the_original_global_install_prefix() {
        assert_eq!(
            package_prefix(
                Path::new("/opt/npm/lib/node_modules/@evalops/maestro"),
                "npm"
            ),
            Some(PathBuf::from("/opt/npm"))
        );
        assert_eq!(
            package_prefix(
                Path::new("/Users/me/.bun/install/global/node_modules/@evalops/maestro"),
                "bun"
            ),
            Some(PathBuf::from("/Users/me/.bun/install/global"))
        );
    }

    #[test]
    fn throttles_any_recent_attempt_for_the_same_version() {
        let mut state = StartupUpdateState {
            version: "0.11.0".to_owned(),
            last_attempt_at: 1_000,
            last_status: "failed".to_owned(),
            source_url: None,
            last_error: None,
            retry_after_at: None,
            rollback_version: None,
        };
        assert!(should_throttle_startup_update(
            Some(&state),
            "0.11.0",
            2_000,
            Duration::from_secs(2)
        ));
        state.last_status = "updated".to_owned();
        assert!(should_throttle_startup_update(
            Some(&state),
            "0.11.0",
            2_000,
            Duration::from_secs(2)
        ));
        assert!(!should_throttle_startup_update(
            Some(&state),
            "0.11.1",
            2_000,
            Duration::from_secs(2)
        ));
        assert!(!should_throttle_startup_update(
            Some(&state),
            "0.11.0",
            3_000,
            Duration::from_secs(2)
        ));
    }

    #[test]
    fn uses_the_persisted_retry_deadline_for_enforcement() {
        let state = StartupUpdateState {
            version: "0.11.0".to_owned(),
            last_attempt_at: 1_000,
            last_status: "failed".to_owned(),
            source_url: None,
            last_error: None,
            retry_after_at: Some(5_000),
            rollback_version: None,
        };
        assert_eq!(startup_retry_deadline(&state, Duration::ZERO), 5_000);
        assert!(should_throttle_startup_update(
            Some(&state),
            "0.11.0",
            4_999,
            Duration::ZERO
        ));
        assert!(!should_throttle_startup_update(
            Some(&state),
            "0.11.0",
            5_000,
            Duration::ZERO
        ));
    }

    #[test]
    fn retained_version_check_requires_an_exact_semver_token() {
        assert!(reported_version_matches("maestro 0.10.6\n", "0.10.6"));
        assert!(!reported_version_matches("maestro 0.10.65\n", "0.10.6"));
        assert!(!reported_version_matches("maestro 1.0.6\n", "0.10.6"));
    }

    #[test]
    fn unsigned_install_receipt_never_surfaces_release_metadata() {
        let temporary = tempfile::tempdir().expect("temp directory");
        let release_dir = temporary.path().join("release");
        fs::create_dir_all(&release_dir).expect("create release directory");
        let metadata = VersionMetadata {
            version: "0.10.6".to_owned(),
            schema_version: Some(RELEASE_METADATA_SCHEMA.to_owned()),
            notes: None,
            release_notes: Some("unsigned notes".to_owned()),
            release_tag: None,
            release_url: None,
            receipt: None,
        };
        let metadata_path = release_dir.join(RELEASE_METADATA_FILE);
        fs::write(
            &metadata_path,
            serde_json::to_vec(&metadata).expect("serialize metadata"),
        )
        .expect("write metadata");
        let receipt = InstallReceipt {
            schema_version: INSTALL_RECEIPT_SCHEMA.to_owned(),
            version: "0.10.6".to_owned(),
            verified: false,
            verification: InstallVerification {
                manifest_checksum_verified: false,
                signature_verified: false,
                metadata_sha256: sha256_file(&metadata_path),
                metadata_checksum_verified: true,
                ..InstallVerification::default()
            },
            ..InstallReceipt::default()
        };

        assert!(load_verified_release_metadata(&release_dir, &receipt).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn unreadable_retained_version_directory_is_skipped() {
        use std::os::unix::fs::PermissionsExt;

        let temporary = tempfile::tempdir().expect("temp directory");
        let version_dir = temporary.path().join("releases/0.10.6");
        fs::create_dir_all(&version_dir).expect("create version directory");
        let mut permissions = fs::metadata(&version_dir)
            .expect("version metadata")
            .permissions();
        permissions.set_mode(0o0);
        fs::set_permissions(&version_dir, permissions).expect("make version directory unreadable");

        let read_denied = fs::read_dir(&version_dir).is_err();
        let result = list_verified_releases(temporary.path());

        let mut restore = fs::metadata(&version_dir)
            .expect("version metadata after read")
            .permissions();
        restore.set_mode(0o700);
        fs::set_permissions(&version_dir, restore).expect("restore version permissions");

        if read_denied {
            assert!(
                result.is_ok(),
                "unreadable version directory must be skipped"
            );
        }
    }

    #[test]
    fn rollback_suppresses_newer_startup_updates_until_manual_update() {
        let temporary = tempfile::tempdir().expect("temp directory");
        let path = temporary.path().join("startup-update-state.json");
        let state = StartupUpdateState {
            version: "0.9.0".to_owned(),
            last_attempt_at: 1_000,
            last_status: "rolledBack".to_owned(),
            source_url: None,
            last_error: None,
            retry_after_at: None,
            rollback_version: Some("0.9.0".to_owned()),
        };
        write_startup_state(&path, &state).expect("persist rollback suppression");
        let persisted = read_startup_state(&path).expect("read rollback suppression");
        assert!(rollback_suppresses_startup_update(
            Some(&persisted),
            "1.0.0"
        ));
        assert!(!rollback_suppresses_startup_update(
            Some(&persisted),
            "0.9.0"
        ));
        clear_rollback_suppression(Some(&path)).expect("clear rollback suppression");
        let cleared = read_startup_state(&path).expect("read cleared suppression");
        assert!(!rollback_suppresses_startup_update(Some(&cleared), "1.0.0"));
    }

    #[test]
    fn restores_startup_state_when_launcher_replacement_fails() {
        let temporary = tempfile::tempdir().expect("temp directory");
        let state_path = temporary.path().join("startup-update-state.json");
        let previous = StartupUpdateState {
            version: "1.0.0".to_owned(),
            last_attempt_at: 1_000,
            last_status: "updated".to_owned(),
            source_url: Some("https://updates.example.test".to_owned()),
            last_error: None,
            retry_after_at: None,
            rollback_version: None,
        };
        write_startup_state(&state_path, &previous).expect("persist previous state");

        let launcher = temporary.path().join("maestro");
        fs::create_dir(&launcher).expect("create conflicting launcher directory");
        persist_rollback_suppression(&state_path, "0.9.0").expect("persist rollback state");
        assert!(atomic_write_executable(&launcher, b"new launcher").is_err());

        restore_startup_state(&state_path, Some(&previous)).expect("restore previous state");
        let restored = read_startup_state(&state_path).expect("read restored state");
        assert_eq!(
            serde_json::to_value(restored).expect("serialize restored state"),
            serde_json::to_value(previous).expect("serialize previous state")
        );
    }

    #[test]
    fn github_release_tags_bind_to_channels() {
        assert!(tag_matches_channel("v0.10.68", UpdateChannel::Stable));
        assert!(!tag_matches_channel(
            "v0.10.67-beta.2",
            UpdateChannel::Stable
        ));
        assert!(tag_matches_channel("v0.10.67-beta.2", UpdateChannel::Beta));
        assert!(!tag_matches_channel(
            "v0.10.67-beta.foo",
            UpdateChannel::Beta
        ));
        assert!(!tag_matches_channel("v0.10.67-beta.0", UpdateChannel::Beta));
        assert!(!tag_matches_channel(
            "v0.10.67-alpha.1",
            UpdateChannel::Beta
        ));
        assert!(tag_matches_channel(
            "v0.10.68-alpha.3",
            UpdateChannel::Alpha
        ));
        assert_eq!(
            github_channel_manifest_url("v0.10.67-beta.2"),
            "https://github.com/evalops/maestro/releases/download/v0.10.67-beta.2/channel-manifest.json"
        );
    }

    #[test]
    fn github_release_selection_binds_manifest_identity() {
        let selected = github_release_selection("v1.2.3");
        assert_eq!(
            selected.manifest_url,
            "https://github.com/evalops/maestro/releases/download/v1.2.3/channel-manifest.json"
        );
        assert_eq!(
            selected.release_url,
            "https://github.com/evalops/maestro/releases/download/v1.2.3"
        );

        let manifest = ChannelManifest {
            schema_version: CHANNEL_MANIFEST_SCHEMA.to_owned(),
            channel: "stable".to_owned(),
            key_id: STABLE_CHANNEL_KEY_ID.to_owned(),
            version: "1.2.3".to_owned(),
            release_tag: "v1.2.3".to_owned(),
            release_url: selected.release_url.clone(),
            metadata_url: None,
            metadata_sha256: None,
            source_sha: "a".repeat(40),
            issued_at_ms: 1,
            release_notes: None,
            release_receipt: None,
            signature: String::new(),
        };
        verify_github_release_manifest_binding(&manifest, &selected)
            .expect("selected manifest identity should verify");

        let mut older = manifest;
        older.version = "1.2.2".to_owned();
        older.release_tag = "v1.2.2".to_owned();
        older.release_url = github_release_url("v1.2.2");
        let error = verify_github_release_manifest_binding(&older, &selected)
            .expect_err("an older signed manifest must not satisfy a newer selection");
        assert!(
            error
                .to_string()
                .contains("does not match selected GitHub release")
        );
    }

    #[test]
    fn github_release_prerelease_flags_bind_to_channels() {
        let stable = GithubReleaseListItem {
            tag_name: "v0.10.68".to_owned(),
            draft: false,
            prerelease: false,
            assets: vec![GithubReleaseAsset {
                name: "channel-manifest.json".to_owned(),
            }],
        };
        assert!(github_release_is_eligible(&stable, UpdateChannel::Stable));
        assert!(!github_release_is_eligible(&stable, UpdateChannel::Beta));

        let beta = GithubReleaseListItem {
            tag_name: "v0.10.67-beta.2".to_owned(),
            draft: false,
            prerelease: true,
            assets: vec![GithubReleaseAsset {
                name: "channel-manifest.json".to_owned(),
            }],
        };
        assert!(!github_release_is_eligible(&beta, UpdateChannel::Stable));
        assert!(github_release_is_eligible(&beta, UpdateChannel::Beta));

        let incomplete_beta = GithubReleaseListItem {
            assets: Vec::new(),
            ..beta
        };
        assert!(!github_release_is_eligible(
            &incomplete_beta,
            UpdateChannel::Beta
        ));
    }

    #[test]
    fn startup_sources_prefer_signed_release_channels() {
        let release = InstallContext::Release {
            install_dir: PathBuf::from("/opt/bin"),
            data_dir: PathBuf::from("/opt/share/maestro"),
            launcher: PathBuf::from("/opt/bin/maestro"),
        };
        assert_eq!(
            trusted_startup_update_urls(&release, UpdateChannel::Stable),
            vec![
                GITHUB_STABLE_LATEST_MANIFEST_URL.to_owned(),
                GITHUB_RELEASES_API_URL.to_owned()
            ]
        );
        assert_eq!(
            trusted_startup_update_urls(&release, UpdateChannel::Beta),
            vec![GITHUB_RELEASES_API_URL.to_owned()]
        );
        assert_eq!(
            trusted_startup_update_urls(&release, UpdateChannel::Alpha),
            vec![GITHUB_RELEASES_API_URL.to_owned()]
        );

        let package = InstallContext::Package {
            manager: "npm".to_owned(),
            package: "@evalops/maestro".to_owned(),
            prefix: None,
            launcher: PathBuf::from("/opt/bin/maestro"),
        };
        assert_eq!(
            trusted_startup_update_urls(&package, UpdateChannel::Stable),
            vec![
                GITHUB_STABLE_LATEST_MANIFEST_URL.to_owned(),
                GITHUB_RELEASES_API_URL.to_owned(),
                "https://registry.npmjs.org/%40evalops%2Fmaestro/latest".to_owned(),
            ]
        );
        assert_eq!(
            trusted_startup_update_urls(&package, UpdateChannel::Alpha),
            vec![
                GITHUB_RELEASES_API_URL.to_owned(),
                "https://registry.npmjs.org/%40evalops%2Fmaestro/alpha".to_owned(),
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn package_context_accepts_a_symlinked_package_root() {
        use std::os::unix::fs::symlink;

        let temporary = tempfile::tempdir().expect("temp directory");
        let package_root = temporary.path().join("package");
        let executable = package_root.join("vendor/maestro/test-target/maestro");
        fs::create_dir_all(executable.parent().expect("executable parent"))
            .expect("create package tree");
        fs::write(&executable, b"binary").expect("write executable");
        let executable = dunce::canonicalize(&executable).expect("canonical executable");
        let alias = temporary.path().join("package-alias");
        symlink(&package_root, &alias).expect("symlink package root");

        let context = package_install_context_from(
            &executable,
            &alias,
            "@evalops/maestro".to_owned(),
            Some("npm"),
        )
        .expect("package install context");
        assert_eq!(
            context,
            InstallContext::Package {
                manager: "npm".to_owned(),
                package: "@evalops/maestro".to_owned(),
                prefix: None,
                launcher: dunce::canonicalize(&package_root)
                    .expect("canonical package root")
                    .join("bin/maestro"),
            }
        );
    }

    #[test]
    fn legacy_package_context_recovers_global_launchers_without_environment() {
        let temporary = tempfile::tempdir().unwrap();
        for (layout, name, manager) in [
            ("lib/node_modules", "@evalops/maestro", "npm"),
            ("lib/node_modules", "@evalops/deixic-code", "npm"),
            (
                ".bun/install/global/node_modules",
                "@evalops/maestro",
                "bun",
            ),
        ] {
            let root = temporary.path().join(layout).join(name);
            let executable = root.join("vendor/maestro/linux-x64/maestro");
            fs::create_dir_all(executable.parent().unwrap()).unwrap();
            fs::create_dir_all(root.join("bin")).unwrap();
            fs::write(&executable, b"native binary").unwrap();
            fs::write(root.join("bin/maestro"), b"legacy launcher").unwrap();
            let metadata = serde_json::json!({"name": name, "version": "0.10.65", "bin": {"maestro": "bin/maestro"}});
            fs::write(root.join("package.json"), metadata.to_string()).unwrap();
            assert_eq!(
                legacy_package_version(&executable).as_deref(),
                Some("0.10.65")
            );
            let context = legacy_package_install_context(&executable).unwrap();
            assert!(
                matches!(context, InstallContext::Package { manager: actual, package, prefix: Some(_), .. } if actual == manager && package == name)
            );
            // Identity, layout, and launcher are all required; partial metadata
            // must not redirect an update to a different global package.
            fs::write(
                root.join("package.json"),
                r#"{"name":"unrelated","bin":{"maestro":"bin/maestro"}}"#,
            )
            .unwrap();
            assert!(legacy_package_install_context(&executable).is_none());
            fs::write(root.join("package.json"), metadata.to_string()).unwrap();
            fs::remove_file(root.join("bin/maestro")).unwrap();
            assert!(legacy_package_install_context(&executable).is_none());
        }
        assert!(legacy_package_install_context(Path::new("/tmp/maestro")).is_none());
        let local = temporary
            .path()
            .join("node_modules/@evalops/maestro/vendor/maestro/linux-x64/maestro");
        fs::create_dir_all(local.parent().unwrap()).unwrap();
        fs::write(&local, b"local binary").unwrap();
        assert!(legacy_package_install_context(&local).is_none());
    }

    #[test]
    fn startup_update_lock_is_nonblocking_and_released_on_drop() {
        let temporary = tempfile::tempdir().expect("temp directory");
        let state_path = temporary.path().join("startup-update-state.json");
        let first = try_acquire_startup_update_lock(&state_path)
            .expect("acquire first lock")
            .expect("first lock available");
        assert!(
            try_acquire_startup_update_lock(&state_path)
                .expect("try second lock")
                .is_none()
        );
        drop(first);
        // A concurrent test can fork while the first lock's file descriptor
        // is open. The child inherits that descriptor until it execs, so
        // reacquisition may briefly report `None` even after `first` drops.
        // The lock must still become available promptly once that inherited
        // descriptor closes.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let _reacquired = loop {
            match try_acquire_startup_update_lock(&state_path).expect("reacquire lock") {
                Some(lock) => break lock,
                None if std::time::Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(5));
                }
                None => {
                    panic!("startup update lock stayed held after the inherited descriptor closed")
                }
            }
        };
    }

    #[test]
    fn update_history_is_bounded_and_keeps_the_newest_attempts() {
        let temporary = tempfile::tempdir().expect("temp directory");
        let path = temporary.path().join(UPDATE_HISTORY_FILE);
        for index in 0..40 {
            let mut attempt = new_update_attempt(
                "update",
                "manual",
                Some("0.1.0"),
                Some("0.2.0"),
                None,
                None,
                None,
            );
            attempt.attempt_id = format!("attempt-{index}");
            begin_update_attempt(Some(&path), &attempt).expect("persist attempt");
        }
        let history = load_update_history(Some(&path)).expect("load bounded history");
        assert_eq!(history.attempts.len(), MAX_UPDATE_HISTORY);
        assert_eq!(history.attempts.first().unwrap().attempt_id, "attempt-8");
        assert_eq!(history.attempts.last().unwrap().attempt_id, "attempt-39");
    }

    #[test]
    fn manual_failures_do_not_report_startup_retry_throttling() {
        let mut attempt = new_update_attempt(
            "update",
            "manual",
            Some("0.1.0"),
            Some("0.2.0"),
            None,
            None,
            None,
        );
        attempt.status = "failed".to_owned();
        let startup_state = StartupUpdateState {
            version: "0.2.0".to_owned(),
            last_attempt_at: attempt.attempted_at_ms,
            last_status: "failed".to_owned(),
            source_url: None,
            last_error: Some("fixture failure".to_owned()),
            retry_after_at: Some(attempt.attempted_at_ms + 1_000),
            rollback_version: None,
        };
        let startup_attempt = startup_attempt_from_state(&startup_state);
        assert!(is_startup_retryable(
            &startup_attempt,
            Some(&startup_state),
            Some("0.2.0"),
            Some("available")
        ));
        assert!(!is_startup_retryable(
            &attempt,
            Some(&startup_state),
            Some("0.2.0"),
            Some("available")
        ));
        attempt.trigger = "startup".to_owned();
        assert!(is_startup_retryable(
            &attempt,
            Some(&startup_state),
            Some("0.2.0"),
            Some("available")
        ));
        assert!(!is_startup_retryable(
            &startup_attempt,
            Some(&startup_state),
            Some("0.2.0"),
            Some("current")
        ));
    }

    #[test]
    fn restores_startup_state_when_history_initialization_fails() {
        let temporary = tempfile::tempdir().expect("temp directory");
        let state_path = temporary.path().join("startup-update-state.json");
        let previous = StartupUpdateState {
            version: "1.0.0".to_owned(),
            last_attempt_at: 1_000,
            last_status: "failed".to_owned(),
            source_url: Some("https://updates.example.test".to_owned()),
            last_error: Some("previous failure".to_owned()),
            retry_after_at: Some(2_000),
            rollback_version: None,
        };
        write_startup_state(&state_path, &previous).expect("persist previous state");

        let attempted = StartupUpdateState {
            version: "1.1.0".to_owned(),
            last_attempt_at: 3_000,
            last_status: "failed".to_owned(),
            source_url: Some("https://updates.example.test".to_owned()),
            last_error: None,
            retry_after_at: Some(4_000),
            rollback_version: None,
        };
        write_startup_state(&state_path, &attempted).expect("persist attempted state");

        let history_path = temporary.path().join("history-directory");
        fs::create_dir(&history_path).expect("create invalid history target");
        let attempt = new_update_attempt(
            "update",
            "startup",
            Some("1.0.0"),
            Some("1.1.0"),
            Some("https://updates.example.test"),
            None,
            None,
        );
        assert!(begin_update_attempt(Some(&history_path), &attempt).is_err());
        restore_startup_state(&state_path, Some(&previous)).expect("restore previous state");

        assert_eq!(
            serde_json::to_value(read_startup_state(&state_path).expect("read restored state"))
                .expect("serialize restored state"),
            serde_json::to_value(previous).expect("serialize previous state")
        );
    }

    #[cfg(unix)]
    #[test]
    fn rollback_requires_verified_receipt_and_preserves_release_assets() {
        use std::os::unix::fs::PermissionsExt;

        let temporary = tempfile::tempdir().expect("temp directory");
        let data_dir = temporary.path().join("data");
        let release_dir = data_dir.join("releases/0.9.0/native.fixture");
        let binary = release_dir.join("bin/maestro");
        fs::create_dir_all(binary.parent().expect("binary parent")).expect("create binary dir");
        fs::write(&binary, b"#!/bin/sh\nprintf 'maestro 0.9.0\\n'\n").expect("write binary");
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o755)).expect("chmod binary");
        let metadata = VersionMetadata {
            version: "0.9.0".to_owned(),
            schema_version: Some("evalops.maestro.release-metadata.v1".to_owned()),
            notes: None,
            release_notes: Some("rollback fixture".to_owned()),
            release_tag: Some("v0.9.0".to_owned()),
            release_url: None,
            receipt: Some(ReleaseReceipt {
                schema_version: "evalops.maestro.release-receipt.v1".to_owned(),
                source_sha: Some("a".repeat(40)),
                artifacts: Vec::new(),
            }),
        };
        let metadata_path = release_dir.join(RELEASE_METADATA_FILE);
        let metadata_bytes = serde_json::to_vec(&metadata).expect("serialize metadata");
        fs::write(&metadata_path, &metadata_bytes).expect("write metadata");
        let mut receipt = InstallReceipt {
            schema_version: INSTALL_RECEIPT_SCHEMA.to_owned(),
            version: "0.9.0".to_owned(),
            platform: native_platform().to_owned(),
            installed_at_ms: 9,
            verified: true,
            verification: InstallVerification {
                manifest_sha256: Some("sha256:manifest".to_owned()),
                manifest_checksum_verified: true,
                signature_verified: true,
                artifact_sha256: sha256_file(&binary),
                metadata_sha256: sha256_file(&metadata_path),
                metadata_checksum_verified: true,
            },
            release_metadata_asset: Some(RELEASE_METADATA_FILE.to_owned()),
        };
        let receipt_path = release_dir.join(INSTALL_RECEIPT_FILE);
        fs::write(
            &receipt_path,
            serde_json::to_vec(&receipt).expect("serialize receipt"),
        )
        .expect("write receipt");

        assert!(load_verified_release_metadata(&release_dir, &receipt).is_some());
        let mut foreign_metadata = metadata.clone();
        foreign_metadata.version = "0.8.0".to_owned();
        fs::write(
            &metadata_path,
            serde_json::to_vec(&foreign_metadata).expect("serialize foreign metadata"),
        )
        .expect("write foreign metadata");
        assert!(load_verified_release_metadata(&release_dir, &receipt).is_none());
        assert!(
            list_verified_releases(&data_dir)
                .expect("reject foreign metadata")
                .is_empty()
        );
        fs::write(&metadata_path, &metadata_bytes).expect("restore metadata");

        let releases = list_verified_releases(&data_dir).expect("list verified releases");
        assert_eq!(releases.len(), 1);
        let selected = select_rollback_release(&data_dir, "1.0.0", None).expect("select rollback");
        verify_retained_release(&selected).expect("verify retained binary");

        let launcher = temporary.path().join("bin/maestro");
        fs::create_dir_all(launcher.parent().expect("launcher parent"))
            .expect("create launcher dir");
        fs::write(&launcher, b"old launcher").expect("write old launcher");
        atomic_write_executable(
            &launcher,
            &launcher_contents(
                launcher.parent().expect("launcher parent"),
                &data_dir,
                &selected.release_dir,
                &selected.version_text,
            ),
        )
        .expect("atomically repoint launcher");
        let launcher_text = fs::read_to_string(&launcher).expect("read launcher");
        assert!(launcher_text.contains("MAESTRO_STARTUP_UPDATE_STATE"));
        assert!(launcher_text.contains(&selected.release_dir.display().to_string()));

        fs::remove_file(&metadata_path).expect("remove optional metadata fixture");
        receipt.verification.metadata_sha256 = None;
        receipt.verification.metadata_checksum_verified = false;
        receipt.release_metadata_asset = None;
        fs::write(
            &receipt_path,
            serde_json::to_vec(&receipt).expect("serialize metadata-free receipt"),
        )
        .expect("write metadata-free receipt");
        assert_eq!(
            list_verified_releases(&data_dir)
                .expect("list metadata-free receipt")
                .len(),
            1
        );

        receipt.verified = false;
        fs::write(
            &receipt_path,
            serde_json::to_vec(&receipt).expect("serialize invalid receipt"),
        )
        .expect("write invalid receipt");
        assert!(
            list_verified_releases(&data_dir)
                .expect("list invalid receipt")
                .is_empty()
        );
    }

    #[test]
    fn embedded_release_updater_keeps_signature_verification() {
        assert!(EMBEDDED_INSTALLER.contains("verify_blob_signature"));
        assert!(EMBEDDED_INSTALLER.contains("SHA256SUMS.cosign.bundle"));
        assert!(EMBEDDED_INSTALLER.contains("${asset}.cosign.bundle"));
        assert!(EMBEDDED_INSTALLER.contains("maestro-internal/.github/workflows/release"));
        assert!(EMBEDDED_INSTALLER.contains("maestro/.github/workflows/release"));
        assert!(EMBEDDED_INSTALLER.contains("mono/.github/workflows/maestro-release"));
    }

    #[tokio::test]
    async fn checks_update_metadata_and_compares_semver() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
        let address = listener.local_addr().expect("server address");
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept request");
            let mut request = [0_u8; 1024];
            let _ = stream.read(&mut request).expect("read request");
            let body = r#"{"version":"0.11.0","notes":"native updater"}"#;
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .expect("write response");
        });
        let check =
            check_for_update_urls("0.10.52", vec![format!("http://{address}/version.json")]).await;
        server.join().expect("join server");
        assert_eq!(check.status, "available");
        assert_eq!(check.latest_version.as_deref(), Some("0.11.0"));
        assert_eq!(check.release_notes.as_deref(), Some("native updater"));
    }

    #[tokio::test]
    async fn stable_update_uses_latest_manifest_before_rate_limited_api() {
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };
        use std::time::Instant;

        let latest_listener = TcpListener::bind("127.0.0.1:0").expect("bind latest server");
        let latest_address = latest_listener.local_addr().expect("latest server address");
        let api_listener = TcpListener::bind("127.0.0.1:0").expect("bind api server");
        api_listener
            .set_nonblocking(true)
            .expect("make api server nonblocking");
        let api_address = api_listener.local_addr().expect("api server address");
        let api_requested = Arc::new(AtomicBool::new(false));
        let api_requested_for_server = Arc::clone(&api_requested);
        let api_server = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_millis(250);
            while Instant::now() < deadline {
                match api_listener.accept() {
                    Ok((mut stream, _)) => {
                        api_requested_for_server.store(true, Ordering::SeqCst);
                        write!(
                            stream,
                            "HTTP/1.1 429 Too Many Requests\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        )
                        .expect("write rate-limit response");
                        return;
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("accept api request: {error}"),
                }
            }
        });

        let manifest = ChannelManifest {
            schema_version: CHANNEL_MANIFEST_SCHEMA.to_owned(),
            channel: "stable".to_owned(),
            key_id: STABLE_CHANNEL_KEY_ID.to_owned(),
            version: "1.2.3".to_owned(),
            release_tag: "v1.2.3".to_owned(),
            release_url: "https://github.com/evalops/maestro/releases/download/v1.2.3"
                .to_owned(),
            metadata_url: Some(
                "https://github.com/evalops/maestro/releases/download/v1.2.3/release-metadata.json"
                    .to_owned(),
            ),
            metadata_sha256: None,
            source_sha: "a".repeat(40),
            issued_at_ms: 1,
            release_notes: None,
            release_receipt: None,
            signature: "0PaVbvGiUaH3DTgqHgfl6JvvC8VlzmRgM7cDWIXkLJwG4aXb6rTXn2ZfVVwylQVLoJX53cCSIhtM18TcYycdBw=="
                .to_owned(),
        };
        let body = serde_json::to_string(&manifest).expect("serialize signed manifest");
        let latest_server = thread::spawn(move || {
            let (mut stream, _) = latest_listener.accept().expect("accept latest request");
            let mut request = [0_u8; 1024];
            let length = stream.read(&mut request).expect("read latest request");
            let request = String::from_utf8_lossy(&request[..length]);
            assert!(
                request.contains("GET /releases/latest/download/channel-manifest.json HTTP/1.1")
            );
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .expect("write latest manifest");
        });

        let latest_url =
            format!("http://{latest_address}/releases/latest/download/channel-manifest.json");
        let api_url = format!("http://{api_address}/releases");
        let check = check_for_update_urls_with_timeout(
            "1.0.0",
            vec![latest_url.clone(), api_url],
            Duration::from_secs(1),
            UpdateChannel::Stable,
        )
        .await;
        latest_server.join().expect("join latest server");
        api_server.join().expect("join api server");

        assert_eq!(check.status, "available");
        assert_eq!(check.latest_version.as_deref(), Some("1.2.3"));
        assert_eq!(check.source_url, latest_url);
        assert_eq!(
            check
                .channel_verification
                .as_ref()
                .map(|verification| verification.status.as_str()),
            Some("verified")
        );
        assert!(
            !api_requested.load(Ordering::SeqCst),
            "stable update should not request the rate-limited Releases API after latest manifest succeeds"
        );
    }

    #[tokio::test]
    async fn failed_channel_checks_preserve_verification_context() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
        let address = listener.local_addr().expect("server address");
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept request");
            let mut request = [0_u8; 1024];
            let _ = stream.read(&mut request).expect("read request");
            write!(
                stream,
                "HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            )
            .expect("write response");
        });

        let url = format!("http://{address}/channel-manifest.json");
        let check = check_for_update_urls_with_timeout(
            "0.10.52",
            vec![url.clone()],
            Duration::from_secs(1),
            UpdateChannel::Beta,
        )
        .await;
        server.join().expect("join server");

        let verification = check
            .channel_verification
            .expect("channel failure should remain visible");
        assert_eq!(verification.status, "invalid");
        assert_eq!(verification.manifest_url, url);
        assert!(
            verification
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("403")
        );
    }
}

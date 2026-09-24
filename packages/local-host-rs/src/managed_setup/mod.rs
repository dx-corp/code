//! Organization-owned client configuration fetched from the Deixic platform.
//!
//! An administrator authors one document per organization (or per workspace) in
//! Deixic: prompt rules, expected skills, an MCP server policy, and a sandbox
//! policy document. This module fetches that document at session start, caches
//! it under the Maestro home directory, and hands the parts to the code that
//! enforces them.
//!
//! A previously fetched tenant policy remains enforceable when a refresh
//! cannot complete rather than reverting to "no policy". The cache records the
//! tenant selector that requested the document, because an organization-wide
//! document may be returned as the effective policy for a workspace but must
//! not become another workspace's offline fallback. The failure rule is strict
//! because this document also carries an MCP allowlist:
//!
//! - Fetch succeeds: use the fetched document and rewrite the cache.
//! - Fetch fails and a cache exists: use the cached document.
//! - Fetch fails, no cache, and the session is bound to a platform workspace:
//!   fail closed. MCP runs in an empty allowlist (every server refused) and the
//!   session shows a notice. It never falls back to an open policy.
//! - The session is not platform-bound at all: there is no administrator, so
//!   the policy is absent and nothing is enforced.
//!
//! Platform owns the policy; Maestro owns the agent loop. This module does not
//! interpret the sandbox policy body: it hands the text to
//! [`crate::sandbox_policy::parse_policy_toml`]. That parser is the in-tree
//! structured sandbox-policy document, not [`crate::sandbox::SandboxPolicy`]
//! and not the private `crate::safety::policy::NetworkPolicy`.

use std::path::PathBuf;
use std::time::Duration;

use prost::Message;
use serde::{Deserialize, Serialize};

mod wire;

use crate::credential_mode::PlatformSession;
use crate::path_utils;
use crate::sandbox_policy::{SandboxPolicyDocument, TeamPolicyProvider, parse_policy_toml};

/// Connect RPC path for the managed setup read.
const GET_MANAGED_SETUP_PATH: &str = "/deixicpublic.v1.DeixicPublicService/GetClientSetup";

/// Environment variables that name the Deixic platform base URL, in priority
/// order. The managed-setup-specific name wins, followed by the shared
/// platform name used by the other Platform clients, then the older EvalOps
/// name kept for compatibility.
const BASE_URL_ENV_VARS: &[&str] = &[
    "MAESTRO_MANAGED_SETUP_URL",
    "MAESTRO_PLATFORM_BASE_URL",
    "MAESTRO_EVALOPS_BASE_URL",
];

/// First-party Platform origin used when no deployment-specific override is set.
const DEFAULT_PLATFORM_BASE_URL: &str = "https://app.deixic.com";

/// Cache file name under the Maestro home directory.
pub const CACHE_FILE_NAME: &str = "managed-setup.json";

/// How long a cached document is treated as fresh enough to skip a fetch.
pub const DEFAULT_CACHE_TTL: Duration = Duration::from_mins(15);

const FETCH_TIMEOUT: Duration = Duration::from_secs(10);

/// Which tenant level authored a rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum RuleScope {
    /// The organization-wide document.
    #[default]
    #[serde(rename = "RULE_SCOPE_ORGANIZATION", alias = "RULE_SCOPE_UNSPECIFIED")]
    Organization,
    /// A workspace override.
    #[serde(rename = "RULE_SCOPE_WORKSPACE")]
    Workspace,
}

/// How a client treats an MCP server the policy does not name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum McpPolicyMode {
    /// Unset. Treated as [`Self::Allowlist`] by [`McpPolicy::decide`] so an
    /// unreadable or truncated policy cannot silently open every server.
    #[default]
    #[serde(rename = "MCP_POLICY_MODE_UNSPECIFIED")]
    Unspecified,
    /// Every server is permitted.
    #[serde(rename = "MCP_POLICY_MODE_OPEN")]
    Open,
    /// Only the listed servers are permitted.
    #[serde(rename = "MCP_POLICY_MODE_ALLOWLIST")]
    Allowlist,
    /// Every server except the listed ones is permitted.
    #[serde(rename = "MCP_POLICY_MODE_DENYLIST")]
    Denylist,
}

/// One operator-authored instruction block for the system prompt.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ManagedRule {
    pub id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default, alias = "body_markdown")]
    pub body_markdown: String,
    #[serde(default)]
    pub scope: RuleScope,
}

/// A skill the organization expects to be available locally.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ManagedSkillRef {
    pub id: String,
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub required: bool,
}

/// One MCP server named by the policy.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpServerRef {
    #[serde(default)]
    pub name: String,
    #[serde(default, alias = "url_pattern")]
    pub url_pattern: String,
    #[serde(default)]
    pub transport: String,
}

/// The organization decision about MCP server connections.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpPolicy {
    #[serde(default)]
    pub mode: McpPolicyMode,
    #[serde(default)]
    pub servers: Vec<McpServerRef>,
}

/// Why a server connection was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpDecision {
    /// The policy permits this server.
    Allowed,
    /// The policy is an allowlist and does not name this server.
    RefusedNotAllowlisted,
    /// The policy is a denylist and names this server.
    RefusedDenylisted,
}

impl McpPolicy {
    /// An empty allowlist: every server is refused. This is the fail-closed
    /// policy used when no document could be obtained.
    #[must_use]
    pub fn deny_all() -> Self {
        Self {
            mode: McpPolicyMode::Allowlist,
            servers: Vec::new(),
        }
    }

    /// Decide one server by name, optional URL, and transport.
    ///
    /// An unspecified mode is treated as an allowlist, so a document that lost
    /// its mode cannot widen access.
    #[must_use]
    pub fn decide(&self, name: &str, url: Option<&str>, transport: &str) -> McpDecision {
        let listed = self
            .servers
            .iter()
            .any(|server| server_matches(server, name, url, transport));
        match self.mode {
            McpPolicyMode::Open => McpDecision::Allowed,
            McpPolicyMode::Denylist => {
                if listed {
                    McpDecision::RefusedDenylisted
                } else {
                    McpDecision::Allowed
                }
            }
            McpPolicyMode::Allowlist | McpPolicyMode::Unspecified => {
                if listed {
                    McpDecision::Allowed
                } else {
                    McpDecision::RefusedNotAllowlisted
                }
            }
        }
    }
}

/// Match a policy entry against a server. Every populated selector is
/// conjunctive so a user-controlled configuration cannot reuse an allowed name
/// with a different endpoint or transport.
fn server_matches(server: &McpServerRef, name: &str, url: Option<&str>, transport: &str) -> bool {
    let entry_name = server.name.trim();
    let pattern = server.url_pattern.trim();
    let entry_transport = server.transport.trim();
    let has_selector = !entry_name.is_empty() || !pattern.is_empty() || !entry_transport.is_empty();
    has_selector
        && (entry_name.is_empty() || entry_name.eq_ignore_ascii_case(name.trim()))
        && (pattern.is_empty()
            || url
                .map(str::trim)
                .filter(|url| !url.is_empty())
                .is_some_and(|url| url_pattern_matches(pattern, url)))
        && (entry_transport.is_empty() || entry_transport.eq_ignore_ascii_case(transport.trim()))
}

/// Match an MCP URL without allowing a wildcard to cross URL component
/// boundaries. In particular, an authority wildcard must never consume the
/// real host and continue matching text from the path or query.
fn url_pattern_matches(pattern: &str, value: &str) -> bool {
    let Ok(url) = url::Url::parse(value) else {
        return false;
    };
    if url.host().is_none() || !url.username().is_empty() || url.password().is_some() {
        return false;
    }

    if pattern == "*" {
        return true;
    }

    let Some((scheme_pattern, remainder)) = pattern.split_once("://") else {
        // Preserve the existing scheme-less host glob form (for example,
        // `*.example.com`) while still matching it only against the host.
        return !pattern.contains(['/', '?', '#'])
            && url
                .host_str()
                .is_some_and(|host| glob_matches(pattern, host));
    };
    if scheme_pattern.is_empty()
        || scheme_pattern.contains('*')
        || !scheme_pattern.eq_ignore_ascii_case(url.scheme())
    {
        return false;
    }

    let authority_end = remainder.find(['/', '?', '#']).unwrap_or(remainder.len());
    let authority_pattern = &remainder[..authority_end];
    if authority_pattern.is_empty() || authority_pattern.contains('@') {
        return false;
    }
    let Some(authority) = normalized_url_authority(&url) else {
        return false;
    };
    if !glob_matches(authority_pattern, &authority) {
        return false;
    }

    let suffix_pattern = &remainder[authority_end..];
    let suffix_pattern = if suffix_pattern.is_empty() {
        "/".to_string()
    } else if suffix_pattern.starts_with(['?', '#']) {
        format!("/{suffix_pattern}")
    } else {
        suffix_pattern.to_string()
    };
    glob_matches(&suffix_pattern, &normalized_url_suffix(&url))
}

fn normalized_url_authority(url: &url::Url) -> Option<String> {
    let mut authority = match url.host()? {
        url::Host::Ipv6(address) => format!("[{address}]"),
        host => host.to_string(),
    };
    if let Some(port) = url.port() {
        authority.push(':');
        authority.push_str(&port.to_string());
    }
    Some(authority)
}

fn normalized_url_suffix(url: &url::Url) -> String {
    let mut suffix = url.path().to_string();
    if let Some(query) = url.query() {
        suffix.push('?');
        suffix.push_str(query);
    }
    if let Some(fragment) = url.fragment() {
        suffix.push('#');
        suffix.push_str(fragment);
    }
    suffix
}

/// A deliberately small glob: `*` matches any run of characters. The policy is
/// authored by an administrator and this helper is applied to one parsed URL
/// component at a time, so a full regex engine would be a second policy
/// language to reason about.
fn glob_matches(pattern: &str, value: &str) -> bool {
    let mut segments = pattern.split('*');
    let Some(first) = segments.next() else {
        return false;
    };
    if !value.starts_with(first) {
        return false;
    }
    let mut rest = &value[first.len()..];
    let segments: Vec<&str> = segments.collect();
    let Some((last, middle)) = segments.split_last() else {
        // No `*` in the pattern: it must match exactly.
        return rest.is_empty();
    };
    for segment in middle {
        if segment.is_empty() {
            continue;
        }
        match rest.find(segment) {
            Some(index) => rest = &rest[index + segment.len()..],
            None => return false,
        }
    }
    if last.is_empty() {
        return true;
    }
    rest.len() >= last.len() && rest.ends_with(last)
}

/// The document a client applies.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ManagedSetup {
    /// Platform issuance time, retained in the local policy cache.
    #[serde(default, alias = "issued_at")]
    pub issued_at: Option<ManagedSetupTimestamp>,
    /// Monotonic per tenant. `0` means the tenant has no stored document.
    #[serde(default, deserialize_with = "deserialize_u64_flexible")]
    pub version: u64,
    #[serde(default, alias = "organization_id")]
    pub organization_id: String,
    #[serde(default, alias = "workspace_id")]
    pub workspace_id: String,
    #[serde(default)]
    pub rules: Vec<ManagedRule>,
    #[serde(default)]
    pub skills: Vec<ManagedSkillRef>,
    #[serde(default)]
    pub mcp: McpPolicy,
    #[serde(default, alias = "sandbox_policy_toml")]
    pub sandbox_policy_toml: String,
}

/// Exact protobuf timestamp components from the policy owner.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManagedSetupTimestamp {
    pub seconds: i64,
    pub nanos: i32,
}

/// Proto3 JSON encodes `uint64` as a string. Accept either form.
fn deserialize_u64_flexible<'de, D>(deserializer: D) -> Result<u64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error;
    match serde_json::Value::deserialize(deserializer)? {
        serde_json::Value::Null => Ok(0),
        serde_json::Value::Number(number) => Ok(number.as_u64().unwrap_or(0)),
        serde_json::Value::String(text) => text.trim().parse::<u64>().map_err(D::Error::custom),
        other => Err(D::Error::custom(format!(
            "managed setup version must be a number or string, got {other}"
        ))),
    }
}

/// The on-disk cache entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CachedManagedSetup {
    /// Cache schema version, independent of the document's own version.
    pub schema_version: u32,
    /// Organization selected by the session that fetched this document.
    pub tenant_organization_id: String,
    /// Workspace selected by that session, or empty for an organization-only
    /// session. This is distinct from `setup.workspace_id`, which can be empty
    /// when the platform falls back to an organization-wide document.
    pub tenant_workspace_id: String,
    /// Unix seconds when this document was fetched.
    pub fetched_at: i64,
    /// The document's platform version, duplicated for cheap inspection.
    pub version: u64,
    pub setup: ManagedSetup,
}

const CACHE_SCHEMA_VERSION: u32 = 2;

/// Where a client's effective document came from. This is what the session
/// notice reports, so an operator can tell "the admin set no policy" apart from
/// "the platform was unreachable".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManagedSetupOrigin {
    /// The session is not bound to a platform workspace: no administrator.
    Unmanaged,
    /// Fetched from the platform during this session.
    Fetched,
    /// The platform was unreachable; the cached document is in force.
    Cache,
    /// The platform was unreachable and there is no cache. MCP is refused.
    FailedClosed,
}

/// Errors that stop a fetch. Every one of them leads to the cache or to the
/// fail-closed policy; none of them opens the session up.
#[derive(Debug, thiserror::Error)]
pub enum ManagedSetupError {
    /// No platform base URL is configured.
    #[error("no Deixic platform base URL is configured")]
    NotConfigured,
    /// The request could not be made or did not return 200.
    #[error("managed setup request failed: {0}")]
    Request(String),
    /// The response body was not the expected document.
    #[error("managed setup response could not be decoded: {0}")]
    Decode(String),
    /// The response belongs to a different tenant selector.
    #[error("managed setup response tenant does not match the requested tenant")]
    TenantMismatch,
}

/// The client: one resolved document plus the notices to show the operator.
#[derive(Debug, Clone)]
pub struct ManagedSetupClient {
    setup: ManagedSetup,
    origin: ManagedSetupOrigin,
    notices: Vec<String>,
    fetched_at: Option<i64>,
    served_from_cache: bool,
}

impl Default for ManagedSetupClient {
    fn default() -> Self {
        Self::unmanaged()
    }
}

impl ManagedSetupClient {
    /// A client for a session with no platform binding: nothing is enforced.
    #[must_use]
    pub fn unmanaged() -> Self {
        Self {
            setup: ManagedSetup {
                mcp: McpPolicy {
                    mode: McpPolicyMode::Open,
                    servers: Vec::new(),
                },
                ..ManagedSetup::default()
            },
            origin: ManagedSetupOrigin::Unmanaged,
            notices: Vec::new(),
            fetched_at: None,
            served_from_cache: false,
        }
    }

    /// Resolve startup policy for an explicitly selected local runtime.
    ///
    /// Local inference does not need live Identity, but a stored tenant
    /// selector must not silently become unmanaged authority. Keep managed
    /// MCP access fail-closed without turning normal airplane-mode startup
    /// into an error banner; the TUI identifies the local route in its chrome.
    #[must_use]
    pub fn offline_local(session: Option<&PlatformSession>) -> Self {
        let Some(session) = session else {
            return Self::unmanaged();
        };
        Self {
            setup: ManagedSetup {
                organization_id: session.organization_id.clone(),
                workspace_id: session.workspace_id.clone().unwrap_or_default(),
                mcp: McpPolicy::deny_all(),
                ..ManagedSetup::default()
            },
            origin: ManagedSetupOrigin::FailedClosed,
            notices: Vec::new(),
            fetched_at: None,
            served_from_cache: false,
        }
    }

    /// Resolve the document for a session start.
    ///
    /// `session` is `None` when the process is in BYOK mode. `fetch` is the
    /// network call, injected so the resolution rule can be tested without a
    /// server. `cache_path` is where the cached document lives.
    pub fn resolve_with<F>(
        session: Option<&PlatformSession>,
        cache_path: Option<&std::path::Path>,
        now_unix: i64,
        cache_ttl: Duration,
        fetch: F,
    ) -> Self
    where
        F: FnOnce(&PlatformSession) -> Result<ManagedSetup, ManagedSetupError>,
    {
        if crate::safety::vendor_network_disabled() {
            return Self::offline_local(session);
        }
        let Some(session) = session else {
            return Self::unmanaged();
        };

        let cached = cache_path
            .and_then(read_cache)
            .filter(|cached| cache_matches_session(cached, session));
        if let Some(cached) = cached.as_ref() {
            let age = now_unix.saturating_sub(cached.fetched_at);
            if age >= 0 && (age as u64) < cache_ttl.as_secs() {
                return Self {
                    setup: cached.setup.clone(),
                    origin: ManagedSetupOrigin::Fetched,
                    notices: Vec::new(),
                    fetched_at: Some(cached.fetched_at),
                    served_from_cache: true,
                };
            }
        }

        match fetch(session) {
            Ok(setup) if setup_matches_session(&setup, session) => {
                if let Some(path) = cache_path {
                    let _ = write_cache(path, session, &setup, now_unix);
                }
                Self {
                    setup,
                    origin: ManagedSetupOrigin::Fetched,
                    notices: Vec::new(),
                    fetched_at: Some(now_unix),
                    served_from_cache: false,
                }
            }
            Ok(_) | Err(ManagedSetupError::TenantMismatch) => {
                Self::fetch_failed_closed(session, cached, ManagedSetupError::TenantMismatch)
            }
            Err(error) => Self::fetch_failed_closed(session, cached, error),
        }
    }

    fn fetch_failed_closed(
        session: &PlatformSession,
        cached: Option<CachedManagedSetup>,
        error: ManagedSetupError,
    ) -> Self {
        match cached {
            Some(cached) => Self {
                notices: vec![format!(
                    "Deixic managed setup could not be refreshed ({error}); \
                         enforcing the cached policy, version {}.",
                    cached.version
                )],
                fetched_at: Some(cached.fetched_at),
                served_from_cache: true,
                setup: cached.setup,
                origin: ManagedSetupOrigin::Cache,
            },
            None => Self {
                setup: ManagedSetup {
                    organization_id: session.organization_id.clone(),
                    workspace_id: session.workspace_id.clone().unwrap_or_default(),
                    mcp: McpPolicy::deny_all(),
                    ..ManagedSetup::default()
                },
                fetched_at: None,
                served_from_cache: false,
                origin: ManagedSetupOrigin::FailedClosed,
                notices: vec![format!(
                    "Deixic managed setup is unavailable ({error}) and no cached policy \
                         exists. MCP servers are refused for this session until the platform \
                         is reachable."
                )],
            },
        }
    }

    /// Age of the accepted document; absent when no document was available or the clock moved backward.
    pub fn age_seconds(&self, now_unix: i64) -> Option<u64> {
        self.fetched_at
            .and_then(|at| u64::try_from(now_unix.saturating_sub(at)).ok())
    }

    /// Whether this resolution used the tenant-bound disk cache, including a fresh cache hit.
    pub fn served_from_cache(&self) -> bool {
        self.served_from_cache
    }

    /// Resolve using the real platform client and the default cache path.
    pub fn resolve(session: Option<&PlatformSession>) -> Self {
        Self::resolve_with(
            session,
            default_cache_path().as_deref(),
            now_unix(),
            DEFAULT_CACHE_TTL,
            fetch_managed_setup,
        )
    }

    #[must_use]
    pub fn setup(&self) -> &ManagedSetup {
        &self.setup
    }

    #[must_use]
    pub fn origin(&self) -> ManagedSetupOrigin {
        self.origin
    }

    #[must_use]
    pub fn mcp_policy(&self) -> &McpPolicy {
        &self.setup.mcp
    }

    /// The document version, for error messages that must name the policy that
    /// refused an action.
    #[must_use]
    pub fn version(&self) -> u64 {
        self.setup.version
    }

    /// Notices to surface at session start.
    #[must_use]
    pub fn notices(&self) -> &[String] {
        &self.notices
    }

    /// True when an administrator's policy is in force.
    #[must_use]
    pub fn is_managed(&self) -> bool {
        !matches!(self.origin, ManagedSetupOrigin::Unmanaged)
    }

    /// Restrict the native executor's baseline with the merged user,
    /// repository, and team policy. The merge can only remove authority: a
    /// partial network allowlist maps to no network access because the native
    /// policy accepts only an all-or-none network grant.
    pub fn native_sandbox_policy(
        &self,
        workspace_root: &std::path::Path,
        baseline: Option<crate::sandbox::SandboxPolicy>,
    ) -> Result<Option<crate::sandbox::SandboxPolicy>, crate::sandbox_policy::PolicyLoadError> {
        let effective = crate::sandbox_policy::resolve_effective_policy(workspace_root, self)?;
        if effective.is_empty() {
            return Ok(baseline);
        }
        if effective.additional_read_paths.is_some() {
            // The native sandbox currently grants either global reads or no
            // writes. Until it can express path-scoped reads, choose the
            // stricter representable policy instead of silently widening.
            return Ok(Some(crate::sandbox::SandboxPolicy::ReadOnly));
        }
        let network_access = effective
            .network
            .as_ref()
            .is_some_and(crate::sandbox_policy::NetworkPolicy::permits_everything);
        let writable_roots =
            effective
                .additional_write_paths
                .unwrap_or_else(|| match baseline.as_ref() {
                    Some(crate::sandbox::SandboxPolicy::WorkspaceWrite {
                        writable_roots, ..
                    }) => writable_roots.clone(),
                    _ => Vec::new(),
                });
        let managed = crate::sandbox::SandboxPolicy::WorkspaceWrite {
            writable_roots,
            network_access,
            exclude_tmpdir_env_var: false,
            exclude_slash_tmp: false,
        };
        Ok(restrict_native_sandbox_policy(baseline, managed))
    }

    /// The typed system-prompt block for the organization's rules, or `None`
    /// when there are no rules.
    #[must_use]
    pub fn rules_prompt_section(&self) -> Option<String> {
        rules_prompt_section(&self.setup)
    }

    /// Required skills the local catalog does not have.
    #[must_use]
    pub fn missing_required_skills<'a, I>(&self, installed: I) -> Vec<&ManagedSkillRef>
    where
        I: IntoIterator<Item = &'a str>,
    {
        let installed: Vec<String> = installed
            .into_iter()
            .map(|id| id.trim().to_ascii_lowercase())
            .collect();
        self.setup
            .skills
            .iter()
            .filter(|skill| skill.required)
            .filter(|skill| {
                let id = skill.id.trim().to_ascii_lowercase();
                !installed.iter().any(|candidate| candidate == &id)
            })
            .collect()
    }

    /// One notice naming every required skill that is absent, or `None`.
    ///
    /// Installing a missing skill is out of scope for this module: the client
    /// reports the gap and the operator installs it.
    #[must_use]
    pub fn missing_required_skills_notice<'a, I>(&self, installed: I) -> Option<String>
    where
        I: IntoIterator<Item = &'a str>,
    {
        let missing = self.missing_required_skills(installed);
        if missing.is_empty() {
            return None;
        }
        let names = missing
            .iter()
            .map(|skill| {
                if skill.source.trim().is_empty() {
                    skill.id.clone()
                } else {
                    format!("{} (from {})", skill.id, skill.source.trim())
                }
            })
            .collect::<Vec<_>>()
            .join(", ");
        Some(format!(
            "Deixic managed setup version {} requires {} skill(s) that are not installed: {}. \
             Install them before relying on the organization's procedures; Maestro does not \
             install managed skills.",
            self.setup.version,
            missing.len(),
            names
        ))
    }
}

/// The sandbox policy half: the administrator's document, parsed by the
/// sandbox policy parser that owns that schema.
impl TeamPolicyProvider for ManagedSetupClient {
    fn team_policy(&self) -> Option<SandboxPolicyDocument> {
        let text = self.setup.sandbox_policy_toml.trim();
        if text.is_empty() {
            return None;
        }
        // A policy the client cannot parse is not an excuse to run unpoliced,
        // but this trait cannot report an error. Returning `None` here would
        // silently drop the administrator's opinion, so the parse failure is
        // surfaced as a fully restrictive document instead.
        match parse_policy_toml(text) {
            Ok(document) => Some(document),
            Err(_) => Some(unparseable_team_policy()),
        }
    }
}

/// The document used when the administrator's sandbox policy cannot be parsed:
/// deny all network access and grant no extra paths.
fn unparseable_team_policy() -> SandboxPolicyDocument {
    SandboxPolicyDocument {
        network: Some(crate::sandbox_policy::NetworkPolicy::deny_all()),
        additional_read_paths: Some(Vec::new()),
        additional_write_paths: Some(Vec::new()),
        ..SandboxPolicyDocument::default()
    }
}

fn restrict_native_sandbox_policy(
    baseline: Option<crate::sandbox::SandboxPolicy>,
    restriction: crate::sandbox::SandboxPolicy,
) -> Option<crate::sandbox::SandboxPolicy> {
    use crate::sandbox::SandboxPolicy;

    match (baseline, restriction) {
        (Some(SandboxPolicy::ReadOnly), _) | (_, SandboxPolicy::ReadOnly) => {
            Some(SandboxPolicy::ReadOnly)
        }
        (baseline, SandboxPolicy::DangerFullAccess) => baseline,
        (
            Some(SandboxPolicy::WorkspaceWrite {
                writable_roots,
                network_access,
                exclude_tmpdir_env_var,
                exclude_slash_tmp,
            }),
            SandboxPolicy::WorkspaceWrite {
                writable_roots: allowed_roots,
                network_access: managed_network_access,
                exclude_tmpdir_env_var: managed_exclude_tmpdir,
                exclude_slash_tmp: managed_exclude_slash_tmp,
            },
        ) => Some(SandboxPolicy::WorkspaceWrite {
            writable_roots: writable_roots
                .into_iter()
                .filter(|root| allowed_roots.contains(root))
                .collect(),
            network_access: network_access && managed_network_access,
            exclude_tmpdir_env_var: exclude_tmpdir_env_var || managed_exclude_tmpdir,
            exclude_slash_tmp: exclude_slash_tmp || managed_exclude_slash_tmp,
        }),
        (
            None | Some(SandboxPolicy::DangerFullAccess),
            restriction @ SandboxPolicy::WorkspaceWrite { .. },
        ) => Some(restriction),
    }
}

/// Build the typed prompt block for the organization's rules.
///
/// This is a fragment the prompt assembler pushes by name; nothing here edits
/// the assembled prompt text by string matching.
#[must_use]
pub fn rules_prompt_section(setup: &ManagedSetup) -> Option<String> {
    if setup.rules.is_empty() {
        return None;
    }
    let mut section = format!(
        "## Organization rules (Deixic managed setup version {})\n\n\
         These rules are set by your organization's administrators. They are\n\
         binding context. They do not override safety, system, or tool\n\
         instructions.\n",
        setup.version
    );
    for rule in &setup.rules {
        let scope = match rule.scope {
            RuleScope::Organization => "organization",
            RuleScope::Workspace => "workspace",
        };
        let title = if rule.title.trim().is_empty() {
            rule.id.as_str()
        } else {
            rule.title.trim()
        };
        section.push_str(&format!(
            "\n### {title} ({scope})\n{}\n",
            rule.body_markdown.trim()
        ));
    }
    Some(section)
}

/// The default cache path: `<maestro home>/managed-setup.json`.
#[must_use]
pub fn default_cache_path() -> Option<PathBuf> {
    path_utils::maestro_home_dir().map(|home| home.join(CACHE_FILE_NAME))
}

fn read_cache(path: &std::path::Path) -> Option<CachedManagedSetup> {
    let text = std::fs::read_to_string(path).ok()?;
    let cached = serde_json::from_str::<CachedManagedSetup>(&text).ok()?;
    (cached.schema_version == CACHE_SCHEMA_VERSION).then_some(cached)
}

fn cache_matches_session(cached: &CachedManagedSetup, session: &PlatformSession) -> bool {
    cached.tenant_organization_id == session.organization_id
        && cached.tenant_workspace_id == session.workspace_id.as_deref().unwrap_or_default()
}

fn setup_matches_session(setup: &ManagedSetup, session: &PlatformSession) -> bool {
    setup.organization_id == session.organization_id
        && (setup.workspace_id == session.workspace_id.as_deref().unwrap_or_default()
            || (session.workspace_id.is_some() && setup.workspace_id.is_empty()))
}

fn write_cache(
    path: &std::path::Path,
    session: &PlatformSession,
    setup: &ManagedSetup,
    now_unix: i64,
) -> anyhow::Result<()> {
    let cached = CachedManagedSetup {
        schema_version: CACHE_SCHEMA_VERSION,
        tenant_organization_id: session.organization_id.clone(),
        tenant_workspace_id: session.workspace_id.clone().unwrap_or_default(),
        fetched_at: now_unix,
        version: setup.version,
        setup: setup.clone(),
    };
    path_utils::atomic_private_write(path, &serde_json::to_vec_pretty(&cached)?)
}

fn now_unix() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or_default()
}

/// The configured Deixic platform base URL, or the first-party default.
#[must_use]
pub fn platform_base_url() -> Option<String> {
    BASE_URL_ENV_VARS
        .iter()
        .find_map(|name| {
            std::env::var(name)
                .ok()
                .map(|value| value.trim().trim_end_matches('/').to_owned())
                .filter(|value| !value.is_empty())
        })
        .or_else(|| Some(DEFAULT_PLATFORM_BASE_URL.to_owned()))
}

/// Fetch the document over Connect protobuf using the session's hosted bearer
/// token. The workspace the session is bound to selects the document; an
/// organization-only session reads the organization document.
pub fn fetch_managed_setup(session: &PlatformSession) -> Result<ManagedSetup, ManagedSetupError> {
    crate::safety::require_vendor_network()
        .map_err(|error| ManagedSetupError::Request(error.to_string()))?;
    let base_url = platform_base_url().ok_or(ManagedSetupError::NotConfigured)?;
    fetch_managed_setup_from(session, &base_url)
}

fn fetch_managed_setup_from(
    session: &PlatformSession,
    base_url: &str,
) -> Result<ManagedSetup, ManagedSetupError> {
    let client = reqwest::blocking::Client::builder()
        .timeout(FETCH_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|error| ManagedSetupError::Request(error.to_string()))?;
    let body = wire::GetManagedSetupRequest {
        scope: Some(crate::public_protocol::Scope {
            organization_id: session.organization_id.clone(),
            workspace_id: session.workspace_id.clone().unwrap_or_default(),
        }),
    }
    .encode_to_vec();
    let request = client
        .post(format!("{base_url}{GET_MANAGED_SETUP_PATH}"))
        .bearer_auth(&session.access_token)
        .header("x-organization-id", &session.organization_id)
        .header("connect-protocol-version", "1")
        .header("accept", "application/proto")
        .header("content-type", "application/proto")
        .body(body);
    let request = match session.workspace_id.as_deref().filter(|id| !id.is_empty()) {
        Some(workspace_id) => request.header("x-workspace-id", workspace_id),
        None => request,
    };
    let response = request
        .send()
        .map_err(|error| ManagedSetupError::Request(error.to_string()))?;
    let status = response.status();
    let bytes = response
        .bytes()
        .map_err(|error| ManagedSetupError::Request(error.to_string()))?;
    if !status.is_success() {
        return Err(ManagedSetupError::Request(format!(
            "HTTP {}: {}",
            status.as_u16(),
            bounded(&String::from_utf8_lossy(&bytes))
        )));
    }
    let setup = wire::ManagedSetup::decode(bytes.as_ref())
        .map_err(|error| ManagedSetupError::Decode(error.to_string()))?
        .try_into_domain()?;
    if !setup_matches_session(&setup, session) {
        return Err(ManagedSetupError::TenantMismatch);
    }
    Ok(setup)
}

fn bounded(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return "no response body".to_owned();
    }
    if trimmed.chars().count() > 240 {
        let clipped: String = trimmed.chars().take(240).collect();
        format!("{clipped}…")
    } else {
        trimmed.to_owned()
    }
}

#[cfg(test)]
mod tests;

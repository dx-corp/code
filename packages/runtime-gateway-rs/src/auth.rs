use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use hmac::{Hmac, KeyInit, Mac};
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header};
use serde_json::Value;
use sha2::Sha256;
use std::env;
use std::time::Duration;

use crate::http::{RequestHead, json_response};
use crate::{Config, now_millis, trimmed_env};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum AuthSource {
    IdentityJwt,
    StaticGatewayKey,
    TrustedProxy,
    #[default]
    LoopbackDev,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct AuthContext {
    pub(crate) subject: Option<String>,
    pub(crate) organization_id: Option<String>,
    pub(crate) workspace_id: Option<String>,
    pub(crate) scopes: Vec<String>,
    pub(crate) source: AuthSource,
    pub(crate) unrestricted: bool,
}

impl AuthContext {
    pub(crate) fn actor_label(&self) -> String {
        let source = match self.source {
            AuthSource::IdentityJwt => "identity-jwt",
            AuthSource::StaticGatewayKey => "api-key",
            AuthSource::TrustedProxy => "trusted-proxy",
            AuthSource::LoopbackDev => "loopback-dev",
        };
        match (
            self.subject.as_deref(),
            self.organization_id.as_deref(),
            self.workspace_id.as_deref(),
        ) {
            (Some(subject), Some(org), Some(workspace)) => format!(
                "{source}:{subject}:org={org}:ws={workspace}:scopes={}",
                self.scopes.join("+")
            ),
            (Some(subject), Some(org), None) => format!(
                "{source}:{subject}:org={org}:scopes={}",
                self.scopes.join("+")
            ),
            (Some(subject), None, _) => {
                format!("{source}:{subject}:scopes={}", self.scopes.join("+"))
            }
            (None, _, _) if self.unrestricted => source.to_owned(),
            (None, _, _) => source.to_owned(),
        }
    }
}

const RUNTIME_WRITE_SCOPE: &str = "maestro:write";
// Elevated role required to mutate process-global control surfaces: the managed
// enterprise safety policy (`/api/admin/*`) and MCP server configuration
// (`POST /api/mcp`), both of which write state that every subsequent agent turn
// on the host consumes. Write scope alone is insufficient for these routes.
const RUNTIME_ADMIN_SCOPE: &str = "maestro:admin";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RuntimeCapability {
    Read,
    TenantRead,
    Write,
    TenantWrite,
}

impl RuntimeCapability {
    fn for_request(head: &RequestHead) -> Self {
        let write = runtime_request_is_mutating(head);
        if runtime_tenant_resource_path(&head.path) {
            if write {
                Self::TenantWrite
            } else {
                Self::TenantRead
            }
        } else if write {
            Self::Write
        } else {
            Self::Read
        }
    }

    fn requires_tenant(self) -> bool {
        matches!(self, Self::TenantRead | Self::TenantWrite)
    }
}

fn runtime_request_is_mutating(head: &RequestHead) -> bool {
    // A2A task subscriptions use POST for an SSE read stream; they do not
    // mutate task state and therefore must retain tenant-read semantics.
    if head.method == "POST" && a2a_task_id_from_subscribe_path(&head.path).is_some() {
        return false;
    }
    matches!(head.method.as_str(), "POST" | "PUT" | "PATCH" | "DELETE")
        || is_chat_websocket_request(head)
}

impl AuthContext {
    fn has_tenant_binding(&self) -> bool {
        self.subject
            .as_deref()
            .is_some_and(|value| !value.is_empty())
            && self
                .organization_id
                .as_deref()
                .is_some_and(|value| !value.is_empty())
            && self
                .workspace_id
                .as_deref()
                .is_some_and(|value| !value.is_empty())
    }

    fn permits(&self, capability: RuntimeCapability, require_tenant: bool) -> bool {
        match capability {
            RuntimeCapability::Read => true,
            RuntimeCapability::TenantRead => {
                self.unrestricted || !require_tenant || self.has_tenant_binding()
            }
            RuntimeCapability::Write => self.has_write_scope(),
            RuntimeCapability::TenantWrite => {
                (self.unrestricted || !require_tenant || self.has_tenant_binding())
                    && self.has_write_scope()
            }
        }
    }

    fn has_write_scope(&self) -> bool {
        self.unrestricted
            || self.scopes.iter().any(|scope| {
                scope == RUNTIME_WRITE_SCOPE
                    || matches!(scope.as_str(), "*" | "runtime:write" | "console:write")
            })
    }

    // Whether the principal carries the elevated administrator role. Loopback
    // development keeps the unrestricted static key privileged; every network
    // principal must present an explicit admin scope.
    pub(crate) fn has_admin_scope(&self) -> bool {
        self.unrestricted
            || self.scopes.iter().any(|scope| {
                scope == RUNTIME_ADMIN_SCOPE || matches!(scope.as_str(), "*" | "console:admin")
            })
    }
}

fn is_chat_websocket_request(head: &RequestHead) -> bool {
    head.method == "GET" && head.path == "/api/chat/ws"
}

fn runtime_tenant_resource_path(path: &str) -> bool {
    path.starts_with("/api/chat")
        || path.starts_with("/api/sessions")
        || matches!(
            path,
            "/message:send" | "/message:stream" | "/extendedAgentCard" | "/tasks"
        )
        || path.starts_with("/tasks/")
        // Pending-request resume forwards an approval decision into a blocked
        // agent turn; it must be bound to the caller's tenant. Per-session
        // ownership is additionally enforced in the resume handler.
        || path.starts_with("/api/pending-requests/")
        // The extended API (MCP config, automations, workspace configs, traces,
        // admin/enterprise-policy, ...) is one process-global store. Binding the
        // whole surface to a tenant principal closes the "any write scope, no
        // tenant" reachability. `/api/mcp` and `/api/admin/*` additionally
        // require the administrator role (see `authorize_extended`).
        || crate::extended::extended_api_matches_path(path)
}

fn configured_api_key_scopes() -> Vec<String> {
    env::var("MAESTRO_WEB_API_KEY_SCOPES")
        .ok()
        .map(|value| {
            value
                .split(|character: char| character == ',' || character.is_ascii_whitespace())
                .map(str::trim)
                .filter(|scope| !scope.is_empty())
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

// A static key is intentionally privileged only for loopback development. A
// network-facing key remains useful for read-only health and console access,
// while operators grant mutation access explicitly with MAESTRO_WEB_API_KEY_SCOPES.
fn static_key_is_unrestricted(config: &Config) -> bool {
    config.listen_host_is_loopback() && !strict_jwt_profile()
}

pub(crate) fn authorized_context(
    head: &RequestHead,
    config: &Config,
) -> Result<AuthContext, Vec<u8>> {
    let auth = auth_context(head, config)
        .ok_or_else(|| json_response(401, &serde_json::json!({ "error": "Unauthorized" })))?;
    let capability = RuntimeCapability::for_request(head);
    // Loopback development keeps subject-only legacy cookies usable; every
    // network-facing or hosted deployment requires a tenant-bound principal.
    let require_tenant =
        capability.requires_tenant() && (!config.listen_host_is_loopback() || strict_jwt_profile());
    if auth.permits(capability, require_tenant) {
        Ok(auth)
    } else if require_tenant && capability.requires_tenant() && !auth.has_tenant_binding() {
        Err(json_response(
            403,
            &serde_json::json!({
                "error": "Forbidden: tenant-bound principal required",
                "requiredClaims": ["sub", "organization_id", "workspace_id"],
            }),
        ))
    } else {
        Err(json_response(
            403,
            &serde_json::json!({
                "error": "Forbidden: insufficient scope",
                "requiredScope": RUNTIME_WRITE_SCOPE,
            }),
        ))
    }
}

pub(crate) fn authorize(head: &RequestHead, config: &Config) -> Result<(), Vec<u8>> {
    authorized_context(head, config).map(|_| ())
}

// Paths on the extended API that mutate process-global control state and
// therefore require the administrator role in addition to tenant-bound write
// authorization: the managed enterprise safety policy (`/api/admin/*`, which
// gates tool approval for every session) and MCP server configuration
// (`POST /api/mcp`, which persists an executable command spawned by every
// subsequent agent turn).
fn extended_path_requires_admin_role(head: &RequestHead) -> bool {
    let path = head.path.as_str();
    path.starts_with("/api/admin/")
        || (path == "/api/mcp"
            && matches!(head.method.as_str(), "POST" | "PUT" | "PATCH" | "DELETE"))
}

// Whether tenant/role invariants are enforced for this deployment. Loopback
// development stays permissive; every network-facing bind or strict-JWT profile
// requires tenant-bound (and, where applicable, role-bound) principals. Callers
// that add their own per-resource authorization (for example the pending-request
// resume ownership check) gate it on this so local development is not disrupted.
pub(crate) fn tenant_enforcement_active(config: &Config) -> bool {
    !config.listen_host_is_loopback() || strict_jwt_profile()
}

// Authorization for the extended API surface. Layers the administrator-role
// requirement for admin/MCP-config mutations on top of the tenant-bound write
// authorization applied to every extended path. Role enforcement is relaxed on
// loopback development (mirroring tenant relaxation) so local operators keep
// access; every network-facing or strict-profile deployment requires the role.
pub(crate) fn authorize_extended(
    head: &RequestHead,
    config: &Config,
) -> Result<AuthContext, Vec<u8>> {
    let auth = authorized_context(head, config)?;
    let enforce_role = tenant_enforcement_active(config);
    if enforce_role && extended_path_requires_admin_role(head) && !auth.has_admin_scope() {
        return Err(json_response(
            403,
            &serde_json::json!({
                "error": "Forbidden: administrator role required",
                "requiredScope": RUNTIME_ADMIN_SCOPE,
            }),
        ));
    }
    Ok(auth)
}

pub(crate) fn auth_context(head: &RequestHead, config: &Config) -> Option<AuthContext> {
    let bearer = head
        .headers
        .get("authorization")
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::trim);
    let header_key = head
        .headers
        .get("x-maestro-api-key")
        .or_else(|| head.headers.get("x-composer-api-key"))
        .map(String::as_str);

    if header_auth_matches(config, bearer, header_key) {
        return Some(AuthContext {
            source: AuthSource::StaticGatewayKey,
            scopes: configured_api_key_scopes(),
            unrestricted: static_key_is_unrestricted(config),
            ..AuthContext::default()
        });
    }

    if let Some(principal) = bearer.and_then(|token| {
        if jwt_requires_invariants(config) {
            bearer_token_principal_for_config(token, config)
        } else {
            bearer_token_principal(token)
        }
    }) {
        return Some(principal);
    }

    if let Some(context) = trusted_proxy_auth_context(head) {
        return Some(context);
    }

    if !config.require_key && !auth_is_configured(config) {
        return Some(AuthContext {
            source: AuthSource::LoopbackDev,
            unrestricted: true,
            ..AuthContext::default()
        });
    }

    None
}

pub(crate) fn header_auth_matches(
    config: &Config,
    bearer: Option<&str>,
    header_key: Option<&str>,
) -> bool {
    config
        .api_key
        .as_deref()
        .map(|expected| {
            let matches = |provided: Option<&str>| {
                provided.is_some_and(|provided| {
                    constant_time_eq(provided.as_bytes(), expected.as_bytes())
                })
            };
            matches(bearer) || matches(header_key)
        })
        .unwrap_or(false)
}

pub(crate) fn trusted_proxy_auth_identity(head: &RequestHead) -> Option<(String, Vec<String>)> {
    let expected_token = trimmed_env("MAESTRO_WEB_TRUST_PROXY_AUTH_TOKEN")?;
    let provided_token = head
        .headers
        .get("x-maestro-proxy-auth")
        .or_else(|| head.headers.get("x-composer-proxy-auth"))
        .map(String::as_str)?;
    if !constant_time_eq(provided_token.as_bytes(), expected_token.as_bytes()) {
        return None;
    }
    let subject = [
        "x-auth-request-email",
        "x-forwarded-email",
        "x-auth-request-user",
    ]
    .iter()
    .find_map(|name| {
        head.headers
            .get(*name)
            .and_then(|value| nonempty_str(value).map(str::to_string))
    })?;
    let scopes = [
        "x-auth-request-scope",
        "x-auth-request-scopes",
        "x-forwarded-scope",
        "x-forwarded-scopes",
    ]
    .iter()
    .find_map(|name| head.headers.get(*name).map(|value| parse_scopes(value)))
    .unwrap_or_default();
    Some((subject, scopes))
}

pub(crate) fn trusted_proxy_auth_context(head: &RequestHead) -> Option<AuthContext> {
    let (subject, scopes) = trusted_proxy_auth_identity(head)?;
    let organization_id = ["x-organization-id", "x-evalops-organization-id"]
        .iter()
        .find_map(|name| {
            head.headers
                .get(*name)
                .and_then(|value| nonempty_str(value).map(str::to_owned))
        });
    let workspace_id = ["x-evalops-workspace-id", "x-workspace-id"]
        .iter()
        .find_map(|name| {
            head.headers
                .get(*name)
                .and_then(|value| nonempty_str(value).map(str::to_owned))
        });
    Some(AuthContext {
        subject: Some(subject),
        organization_id,
        workspace_id,
        scopes,
        source: AuthSource::TrustedProxy,
        unrestricted: false,
    })
}

fn parse_scopes(value: &str) -> Vec<String> {
    value
        .split(|character: char| character == ',' || character.is_ascii_whitespace())
        .map(str::trim)
        .filter(|scope| !scope.is_empty())
        .map(str::to_owned)
        .collect()
}

pub(crate) fn nonempty_str(value: &str) -> Option<&str> {
    let value = value.trim();
    (!value.is_empty()).then_some(value)
}

pub(crate) fn validate_csrf(head: &RequestHead, config: &Config) -> Result<(), Vec<u8>> {
    if !csrf_applies(head) || !config.require_csrf {
        return Ok(());
    }
    let Some(expected) = config.csrf_token.as_deref() else {
        return Err(json_response(
            403,
            &serde_json::json!({
                "error": "MAESTRO_WEB_CSRF_TOKEN is required for state-changing requests"
            }),
        ));
    };
    let provided = head
        .headers
        .get("x-composer-csrf")
        .or_else(|| head.headers.get("x-maestro-csrf"))
        .or_else(|| head.headers.get("x-csrf-token"))
        .or_else(|| head.headers.get("x-xsrf-token"))
        .map(String::as_str);
    if provided
        .map(|value| constant_time_eq(value.as_bytes(), expected.as_bytes()))
        .unwrap_or(false)
    {
        Ok(())
    } else {
        Err(json_response(
            403,
            &serde_json::json!({ "error": "Forbidden: invalid CSRF token" }),
        ))
    }
}

pub(crate) fn csrf_applies(head: &RequestHead) -> bool {
    if matches!(head.method.as_str(), "GET" | "HEAD" | "OPTIONS") {
        return false;
    }

    head.path.starts_with("/api/") || csrf_applies_to_a2a_path(&head.path)
}

fn csrf_applies_to_a2a_path(path: &str) -> bool {
    matches!(path, "/message:send" | "/message:stream")
        || a2a_task_id_from_cancel_path(path).is_some()
        || a2a_task_id_from_subscribe_path(path).is_some()
        || a2a_push_notification_config_path(path).is_some()
}

pub(crate) fn a2a_task_id_from_cancel_path(path: &str) -> Option<&str> {
    let id = path.strip_prefix("/tasks/")?.strip_suffix(":cancel")?;
    (!id.is_empty() && !id.contains('/') && !id.contains(':')).then_some(id)
}

pub(crate) fn a2a_task_id_from_subscribe_path(path: &str) -> Option<&str> {
    let id = path
        .strip_prefix("/tasks/")?
        .strip_suffix(":subscribe")
        .or_else(|| path.strip_prefix("/tasks/")?.strip_suffix("/subscribe"))?;
    (!id.is_empty() && !id.contains('/') && !id.contains(':')).then_some(id)
}

fn a2a_push_notification_config_path(path: &str) -> Option<()> {
    let rest = path.strip_prefix("/tasks/")?;
    let (task_id, suffix) = rest.split_once("/pushNotificationConfigs")?;
    if task_id.is_empty() || task_id.contains('/') || task_id.contains(':') {
        return None;
    }
    if suffix.is_empty() {
        return Some(());
    }
    let config_id = suffix.strip_prefix('/')?;
    (!config_id.is_empty() && !config_id.contains('/') && !config_id.contains(':')).then_some(())
}

pub(crate) fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let diff = left
        .iter()
        .zip(right.iter())
        .fold(0_u8, |diff, (left, right)| diff | (left ^ right));
    diff == 0
}

pub(crate) fn auth_is_configured(config: &Config) -> bool {
    config.api_key.is_some()
        || env::var("MAESTRO_WEB_TRUST_PROXY_AUTH_TOKEN")
            .map(|value| !value.trim().is_empty())
            .unwrap_or(false)
        || env::var("MAESTRO_JWT_SECRET")
            .map(|value| !value.trim().is_empty())
            .unwrap_or(false)
        || env::var("MAESTRO_JWT_JWKS_URL")
            .map(|value| !value.trim().is_empty())
            .unwrap_or(false)
        || env::var("MAESTRO_JWT_JWKS")
            .map(|value| !value.trim().is_empty())
            .unwrap_or(false)
}

pub(crate) fn prod_profile() -> bool {
    matches!(
        env::var("MAESTRO_PROFILE")
            .or_else(|_| env::var("MAESTRO_WEB_PROFILE"))
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
            .as_str(),
        "prod" | "production" | "secure" | "hardened"
    )
}

pub(crate) fn bearer_token_principal(token: &str) -> Option<AuthContext> {
    jwt_principal(token)
}

pub(crate) fn jwt_principal(token: &str) -> Option<AuthContext> {
    jwt_principal_with_requirements(token, strict_jwt_profile())
}

fn bearer_token_principal_for_config(token: &str, config: &Config) -> Option<AuthContext> {
    jwt_principal_with_requirements(token, jwt_requires_invariants(config))
}

fn jwt_principal_with_requirements(token: &str, require_invariants: bool) -> Option<AuthContext> {
    let algorithm = env::var("MAESTRO_JWT_ALG")
        .ok()
        .map(|alg| alg.trim().to_string())
        .filter(|alg| !alg.is_empty());
    let algorithm = if require_invariants {
        algorithm?
    } else {
        algorithm.unwrap_or_else(|| "HS256".to_string())
    };
    match algorithm.as_str() {
        "HS256" => hs256_jwt_principal_with_requirements(token, require_invariants),
        "RS256" => {
            jwks_jwt_principal_with_requirements(token, Algorithm::RS256, require_invariants)
        }
        "RS384" => {
            jwks_jwt_principal_with_requirements(token, Algorithm::RS384, require_invariants)
        }
        "RS512" => {
            jwks_jwt_principal_with_requirements(token, Algorithm::RS512, require_invariants)
        }
        _ => None,
    }
}

fn hs256_jwt_principal_with_requirements(
    token: &str,
    require_invariants: bool,
) -> Option<AuthContext> {
    let configured_algorithm = env::var("MAESTRO_JWT_ALG")
        .ok()
        .map(|alg| alg.trim().to_string())
        .filter(|alg| !alg.is_empty());
    if require_invariants {
        if configured_algorithm.as_deref() != Some("HS256") {
            return None;
        }
    } else if configured_algorithm
        .as_deref()
        .is_some_and(|algorithm| algorithm != "HS256")
    {
        return None;
    }
    let Ok(secret) = env::var("MAESTRO_JWT_SECRET") else {
        return None;
    };
    let secret = secret.trim();
    if secret.is_empty() {
        return None;
    }
    let mut parts = token.split('.');
    let (Some(header), Some(payload), Some(signature), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return None;
    };
    let Ok(header_value) = URL_SAFE_NO_PAD
        .decode(header)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .ok_or(())
    else {
        return None;
    };
    if header_value.get("alg").and_then(Value::as_str) != Some("HS256") {
        return None;
    }
    let signed = format!("{header}.{payload}");
    let expected = hmac_sha256_base64url(secret.as_bytes(), signed.as_bytes());
    if !constant_time_eq(signature.as_bytes(), expected.as_bytes()) {
        return None;
    }
    let Ok(payload_value) = URL_SAFE_NO_PAD
        .decode(payload)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .ok_or(())
    else {
        return None;
    };
    let subject = payload_value
        .get("sub")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|sub| !sub.is_empty())
        .map(str::to_string)?;
    let now_secs = now_millis() / 1000;
    let exp = payload_value.get("exp").and_then(Value::as_u64);
    if require_invariants && exp.is_none() {
        return None;
    }
    if exp.is_some_and(|value| value <= now_secs) {
        return None;
    }
    if require_invariants
        && payload_value
            .get("exp")
            .is_some_and(|value| !value.is_number())
    {
        return None;
    }
    if payload_value
        .get("nbf")
        .and_then(Value::as_u64)
        .is_some_and(|nbf| nbf > now_secs)
        || (require_invariants
            && payload_value
                .get("nbf")
                .is_some_and(|nbf| nbf.as_u64().is_none()))
    {
        return None;
    }
    if !jwt_claim_requirements_match(&payload_value, require_invariants) {
        return None;
    }
    Some(principal_from_claims(payload_value, subject))
}

#[cfg(test)]
pub(crate) fn jwks_jwt_principal(token: &str, algorithm: Algorithm) -> Option<AuthContext> {
    jwks_jwt_principal_with_requirements(token, algorithm, strict_jwt_profile())
}

fn jwks_jwt_principal_with_requirements(
    token: &str,
    algorithm: Algorithm,
    require_invariants: bool,
) -> Option<AuthContext> {
    let Ok(header) = decode_header(token) else {
        return None;
    };
    if header.alg != algorithm {
        return None;
    }
    let jwks = load_jwks()?;
    let key = jwks
        .keys
        .iter()
        .find(|key| {
            header
                .kid
                .as_deref()
                .map(|kid| key.common.key_id.as_deref() == Some(kid))
                .unwrap_or(true)
        })
        .and_then(|key| DecodingKey::from_jwk(key).ok());
    let key = key?;
    let mut validation = Validation::new(algorithm);
    let audience = env::var("MAESTRO_JWT_AUD")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    if let Some(audience) = audience {
        validation.set_audience(&[audience]);
    } else if !require_invariants {
        validation.validate_aud = false;
    } else {
        return None;
    }
    let issuer = env::var("MAESTRO_JWT_ISS")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    if let Some(issuer) = issuer {
        validation.set_issuer(&[issuer]);
    } else if require_invariants {
        return None;
    }
    if require_invariants {
        validation.set_required_spec_claims(&["exp", "aud", "iss", "sub"]);
    }
    let Ok(data) = decode::<Value>(token, &key, &validation) else {
        return None;
    };
    let subject = data
        .claims
        .get("sub")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|sub| !sub.is_empty())
        .map(str::to_string)?;
    Some(principal_from_claims(data.claims, subject))
}

fn jwt_claim_requirements_match(claims: &Value, require_invariants: bool) -> bool {
    let audience = env::var("MAESTRO_JWT_AUD")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    let issuer = env::var("MAESTRO_JWT_ISS")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    if require_invariants && (audience.is_none() || issuer.is_none()) {
        return false;
    }
    if let Some(audience) = audience {
        if !jwt_claim_matches(claims, "aud", &audience) {
            return false;
        }
    }
    if let Some(issuer) = issuer {
        if !jwt_claim_matches(claims, "iss", &issuer) {
            return false;
        }
    }
    true
}

// Hosted/secure profiles require an explicit algorithm and the issuer,
// audience, and expiration invariants that remote identity providers issue.
// Loopback development keeps the historical optional-claim compatibility.
fn strict_jwt_profile() -> bool {
    prod_profile() || hosted_runner_profile()
}

pub(crate) fn hosted_runner_profile() -> bool {
    // Share the crate-wide strict env_bool parser so this JWT strict profile and
    // the platform auto-registration switch (a2a_platform_registration.rs) agree
    // on what MAESTRO_HOSTED_RUNNER_MODE / MAESTRO_HOSTED_RUNNER mean: only the
    // affirmative tokens 1/true/yes/on enable them, and an unknown value is off.
    ["MAESTRO_HOSTED_RUNNER_MODE", "MAESTRO_HOSTED_RUNNER"]
        .iter()
        .any(|name| crate::env_bool(name) == Some(true))
        || env::var("MAESTRO_RUNNER_KIND")
            .ok()
            .is_some_and(|value| value.trim().eq_ignore_ascii_case("hosted"))
        || env::var("MAESTRO_PROFILE")
            .ok()
            .is_some_and(|value| value.trim().eq_ignore_ascii_case("hosted-runner"))
}

fn jwt_requires_invariants(config: &Config) -> bool {
    !config.listen_host_is_loopback() || strict_jwt_profile()
}

fn principal_from_claims(claims: Value, subject: String) -> AuthContext {
    let organization_id = claims
        .get("organization_id")
        .or_else(|| claims.get("org_id"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    let workspace_id = claims
        .get("workspace_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    let scopes = claims
        .get("scopes")
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .or_else(|| {
            claims.get("scope").and_then(Value::as_str).map(|scope| {
                scope
                    .split_whitespace()
                    .map(str::to_owned)
                    .collect::<Vec<_>>()
            })
        })
        .unwrap_or_default();
    AuthContext {
        subject: Some(subject),
        organization_id,
        workspace_id,
        scopes,
        source: AuthSource::IdentityJwt,
        unrestricted: false,
    }
}

pub(crate) fn load_jwks() -> Option<jsonwebtoken::jwk::JwkSet> {
    if let Ok(raw) = env::var("MAESTRO_JWT_JWKS") {
        return serde_json::from_str(raw.trim()).ok();
    }
    let url = env::var("MAESTRO_JWT_JWKS_URL").ok()?;
    let url = url.trim();
    if url.is_empty() {
        return None;
    }
    // `reqwest::blocking` panics when used directly on a Tokio runtime thread.
    let url = url.to_string();
    if tokio::runtime::Handle::try_current().is_ok() {
        return std::thread::spawn(move || fetch_jwks_from_url(&url))
            .join()
            .ok()
            .flatten();
    }
    fetch_jwks_from_url(&url)
}

pub(crate) fn fetch_jwks_from_url(url: &str) -> Option<jsonwebtoken::jwk::JwkSet> {
    reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .ok()?
        .get(url)
        .header("accept", "application/json")
        .send()
        .ok()?
        .error_for_status()
        .ok()?
        .json()
        .ok()
}

pub(crate) fn jwt_claim_matches(payload: &Value, claim: &str, expected: &str) -> bool {
    if expected.is_empty() {
        return true;
    }
    match payload.get(claim) {
        Some(Value::String(value)) => value == expected,
        Some(Value::Array(values)) => values.iter().any(|value| value.as_str() == Some(expected)),
        _ => false,
    }
}

pub(crate) fn hmac_sha256_base64url(secret: &[u8], payload: &[u8]) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret).expect("HMAC accepts arbitrary key sizes");
    mac.update(payload);
    URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes())
}

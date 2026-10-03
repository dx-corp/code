//! Signed, per-turn authorization for managed LLM Gateway inference, and the
//! published price contract for the prepaid managed default model.
//!
//! This crate deliberately contains only the capability envelope, its
//! cryptographic verification, and the versioned price constants that the
//! Gateway enforces and the console reports. Prompt bodies, credentials,
//! provider responses, replay state, and audit records belong to the Gateway
//! outcome ledger.

use std::collections::{BTreeMap, BTreeSet};

use aws_lc_rs::signature::{ED25519, Ed25519KeyPair, UnparsedPublicKey};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use thiserror::Error;

pub const AUTHORIZATION_SCHEMA_VERSION: u32 = 1;

/// Complete provider coordinates selected for a turn, shared by dispatchers
/// and executors so neither can substitute its startup credential or model.
/// Credential names identify stored credentials; this type never holds keys.
#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ManagedInferenceProviderBinding {
    pub model: String,
    pub provider: String,
    pub provider_environment: String,
    #[serde(default)]
    pub credential_name: String,
    #[serde(default)]
    pub team_id: String,
}

impl ManagedInferenceProviderBinding {
    /// Whether these exact coordinates select a reviewed prepaid route.
    pub fn is_prepaid(&self) -> bool {
        prepaid_model_credential_name(&self.provider, &self.model)
            == Some(self.credential_name.as_str())
            && self.provider_environment == DEFAULT_MANAGED_ENVIRONMENT
    }
}

/// Published per-token price for the prepaid managed default model.
///
/// Amounts are USD micros (1 USD = 1_000_000 micros) per one million tokens,
/// so a rate of $1.40 per million tokens is `1_400_000`. Integers keep the
/// contract exact for both the Gateway's reservation bound and the console's
/// customer-facing display. A rate change requires a new `pricing_version`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PrepaidModelPricing {
    pub provider: &'static str,
    pub model: &'static str,
    /// Customer-facing name; `model` is the provider's identifier.
    pub display_name: &'static str,
    pub pricing_version: &'static str,
    pub input_micros_per_million_tokens: i64,
    pub cached_input_micros_per_million_tokens: i64,
    pub output_micros_per_million_tokens: i64,
}

impl PrepaidModelPricing {
    fn usd(micros: i64) -> f64 {
        micros as f64 / 1_000_000.0
    }

    pub fn input_usd_per_million(&self) -> f64 {
        Self::usd(self.input_micros_per_million_tokens)
    }

    pub fn cached_input_usd_per_million(&self) -> f64 {
        Self::usd(self.cached_input_micros_per_million_tokens)
    }

    pub fn output_usd_per_million(&self) -> f64 {
        Self::usd(self.output_micros_per_million_tokens)
    }
}

/// Public coordinate of the shared managed route; never a provider secret.
pub const DEFAULT_MANAGED_CREDENTIAL_NAME: &str = "deixic-llm-gateway-glm53";
pub const DEFAULT_MANAGED_ENVIRONMENT: &str = "production";

/// Default price contract; existing deployments retain this default.
pub const PREPAID_DEFAULT_PRICING: PrepaidModelPricing = PrepaidModelPricing {
    provider: "fireworks",
    model: "accounts/fireworks/models/glm-5p3",
    display_name: "GLM-5.3",
    pricing_version: "fireworks-glm-5p3-2026-09-04",
    input_micros_per_million_tokens: 1_400_000,
    cached_input_micros_per_million_tokens: 260_000,
    output_micros_per_million_tokens: 4_400_000,
};
/// Reviewed text/tool price contracts for managed routes.
/// Rates are microdollars per million tokens. September 13 additions were checked
/// against Fireworks model pages and Google standard pricing; Gemini promotional
/// rates apply through December 2026 and require a new price version thereafter.
pub const PREPAID_MODEL_PRICING: &[PrepaidModelPricing] = &[
    PREPAID_DEFAULT_PRICING,
    PrepaidModelPricing {
        provider: "fireworks",
        model: "accounts/fireworks/models/glm-5p3-flash",
        display_name: "GLM-5.3 Flash",
        pricing_version: "fireworks-glm-5p3-flash-2026-09-08",
        input_micros_per_million_tokens: 150_000,
        cached_input_micros_per_million_tokens: 30_000,
        output_micros_per_million_tokens: 500_000,
    },
    PrepaidModelPricing {
        provider: "fireworks",
        model: "accounts/fireworks/models/deepseek-v4-flash-0731",
        display_name: "DeepSeek V4 Flash 0731",
        pricing_version: "fireworks-deepseek-v4-flash-0731-2026-09-08",
        input_micros_per_million_tokens: 220_000,
        cached_input_micros_per_million_tokens: 7_000,
        output_micros_per_million_tokens: 660_000,
    },
    PrepaidModelPricing {
        provider: "fireworks",
        model: "accounts/fireworks/models/kimi-k3",
        display_name: "Kimi K3",
        pricing_version: "fireworks-kimi-k3-2026-09-08",
        input_micros_per_million_tokens: 3_000_000,
        cached_input_micros_per_million_tokens: 300_000,
        output_micros_per_million_tokens: 15_000_000,
    },
    PrepaidModelPricing {
        provider: "fireworks",
        model: "accounts/fireworks/models/deepseek-v4p1-flash",
        display_name: "DeepSeek V4.1 Flash",
        pricing_version: "fireworks-deepseek-v4p1-flash-2026-09-13",
        input_micros_per_million_tokens: 220_000,
        cached_input_micros_per_million_tokens: 7_000,
        output_micros_per_million_tokens: 660_000,
    },
    PrepaidModelPricing {
        provider: "fireworks",
        model: "accounts/fireworks/models/qwen3p8-max",
        display_name: "Qwen 3.8 Max",
        pricing_version: "fireworks-qwen3p8-max-2026-09-13",
        input_micros_per_million_tokens: 2_000_000,
        cached_input_micros_per_million_tokens: 250_000,
        output_micros_per_million_tokens: 6_000_000,
    },
    PrepaidModelPricing {
        provider: "google",
        model: "gemini-3.8-flash",
        display_name: "Gemini 3.8 Flash",
        pricing_version: "google-gemini-3.8-flash-2026-09-13",
        input_micros_per_million_tokens: 750_000,
        cached_input_micros_per_million_tokens: 75_000,
        output_micros_per_million_tokens: 3_750_000,
    },
];

/// Exact provider/model lookup; unknown routes never inherit the default price.
pub fn prepaid_model_pricing(provider: &str, model: &str) -> Option<&'static PrepaidModelPricing> {
    PREPAID_MODEL_PRICING
        .iter()
        .find(|price| price.provider == provider && price.model == model)
}

/// Canonical shared credential reference for a reviewed managed model.
/// Provider coordinates remain separate, and this never contains a secret.
pub fn prepaid_model_credential_name(provider: &str, model: &str) -> Option<&'static str> {
    let price = prepaid_model_pricing(provider, model)?;
    match price.provider {
        "fireworks" => Some(DEFAULT_MANAGED_CREDENTIAL_NAME),
        "google" => Some("deixic-llm-gateway-gemini38"),
        _ => None,
    }
}

pub const AUTHORIZATION_AUDIENCE: &str = "evalops.llm-gateway";
pub const MAX_AUTHORIZATION_LIFETIME_MS: i64 = 15 * 60 * 1_000;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ManagedInferenceProviderCandidate {
    pub provider: String,
    pub environment: String,
    pub credential_name: String,
    pub team_id: String,
    pub model: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ManagedInferenceRouting {
    Ordered,
    RoundRobin,
    Adaptive,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ManagedInferenceBudgetOrigin {
    Explicit,
    RoutePolicy,
    ProviderDefault,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ManagedInferenceOutputTokenBudget {
    pub value: Option<u64>,
    pub origin: ManagedInferenceBudgetOrigin,
    pub origin_reference: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ManagedInferenceScope {
    pub organization_id: String,
    pub workspace_id: String,
    pub session_id: String,
    pub thread_id: String,
    pub run_id: String,
    pub turn_id: String,
    pub lineage_id: String,
    pub endpoint: String,
    pub model: String,
    pub provider_candidates: Vec<ManagedInferenceProviderCandidate>,
    pub routing: ManagedInferenceRouting,
    pub output_token_budget: ManagedInferenceOutputTokenBudget,
}

/// Reference to Billing's immutable Work allowance. The signature binds the
/// reference to the same tenant and turn as model execution.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ManagedInferenceSpendAuthority {
    pub authority_id: String,
    pub work_id: String,
    pub price_book_version: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ManagedInferenceAuthorizationClaims {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spend_authority: Option<ManagedInferenceSpendAuthority>,
    pub schema_version: u32,
    pub authorization_id: String,
    pub key_id: String,
    pub audience: String,
    pub issued_at_ms: i64,
    pub not_before_ms: i64,
    pub expires_at_ms: i64,
    pub grant_epoch: u64,
    pub organization_id: String,
    pub workspace_id: String,
    pub session_id: String,
    pub thread_id: String,
    pub run_id: String,
    pub turn_id: String,
    pub lineage_id: String,
    pub runtime_generation: u64,
    pub endpoint: String,
    pub model: String,
    pub provider_candidates: Vec<ManagedInferenceProviderCandidate>,
    pub routing: ManagedInferenceRouting,
    pub output_token_budget: ManagedInferenceOutputTokenBudget,
    pub scope_digest: String,
}

impl ManagedInferenceAuthorizationClaims {
    fn scope(&self) -> ManagedInferenceScope {
        ManagedInferenceScope {
            organization_id: self.organization_id.clone(),
            workspace_id: self.workspace_id.clone(),
            session_id: self.session_id.clone(),
            thread_id: self.thread_id.clone(),
            run_id: self.run_id.clone(),
            turn_id: self.turn_id.clone(),
            lineage_id: self.lineage_id.clone(),
            endpoint: self.endpoint.clone(),
            model: self.model.clone(),
            provider_candidates: self.provider_candidates.clone(),
            routing: self.routing,
            output_token_budget: self.output_token_budget.clone(),
        }
    }

    fn validate(&self) -> Result<(), AuthorizationVerificationError> {
        if self.schema_version != AUTHORIZATION_SCHEMA_VERSION {
            return Err(AuthorizationVerificationError::UnsupportedSchemaVersion(
                self.schema_version,
            ));
        }
        for (field, value) in [
            ("authorization_id", &self.authorization_id),
            ("key_id", &self.key_id),
            ("audience", &self.audience),
            ("organization_id", &self.organization_id),
            ("workspace_id", &self.workspace_id),
            ("session_id", &self.session_id),
            ("thread_id", &self.thread_id),
            ("run_id", &self.run_id),
            ("turn_id", &self.turn_id),
            ("lineage_id", &self.lineage_id),
            ("endpoint", &self.endpoint),
            ("model", &self.model),
            ("scope_digest", &self.scope_digest),
        ] {
            if value.is_empty() || value.len() > 512 || value.chars().any(char::is_control) {
                return Err(AuthorizationVerificationError::InvalidClaim(field));
            }
        }
        if let Some(authority) = &self.spend_authority {
            for value in [
                &authority.authority_id,
                &authority.work_id,
                &authority.price_book_version,
            ] {
                if value.trim().is_empty()
                    || value.len() > 512
                    || value.chars().any(char::is_control)
                {
                    return Err(AuthorizationVerificationError::InvalidClaim(
                        "spend_authority",
                    ));
                }
            }
        }
        if self.issued_at_ms > self.expires_at_ms
            || self.not_before_ms > self.expires_at_ms
            || self.expires_at_ms - self.issued_at_ms > MAX_AUTHORIZATION_LIFETIME_MS
        {
            return Err(AuthorizationVerificationError::InvalidLifetime);
        }
        if self.provider_candidates.is_empty() {
            return Err(AuthorizationVerificationError::NoProviderCandidates);
        }
        let mut seen = BTreeSet::new();
        for candidate in &self.provider_candidates {
            for value in [
                &candidate.provider,
                &candidate.environment,
                &candidate.credential_name,
                &candidate.team_id,
                &candidate.model,
            ] {
                if value.len() > 512 || value.chars().any(char::is_control) {
                    return Err(AuthorizationVerificationError::InvalidClaim(
                        "provider_candidate",
                    ));
                }
            }
            if candidate.provider.is_empty()
                || candidate.environment.is_empty()
                || candidate.model.is_empty()
            {
                return Err(AuthorizationVerificationError::InvalidClaim(
                    "provider_candidate",
                ));
            }
            let identity = format!(
                "{}\u{1f}{}\u{1f}{}\u{1f}{}\u{1f}{}",
                candidate.provider,
                candidate.environment,
                candidate.credential_name,
                candidate.team_id,
                candidate.model
            );
            if !seen.insert(identity) {
                return Err(AuthorizationVerificationError::DuplicateProviderCandidate);
            }
        }
        if self.output_token_budget.origin_reference.len() > 512
            || self
                .output_token_budget
                .origin_reference
                .chars()
                .any(char::is_control)
        {
            return Err(AuthorizationVerificationError::InvalidClaim(
                "output_token_budget.origin_reference",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ManagedInferenceAuthorization {
    pub claims: ManagedInferenceAuthorizationClaims,
    pub signature: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ManagedInferenceKeySet {
    keys: BTreeMap<String, Vec<u8>>,
}

impl ManagedInferenceKeySet {
    pub fn from_raw(keys: BTreeMap<String, Vec<u8>>) -> Result<Self, KeySetError> {
        if keys.is_empty() {
            return Err(KeySetError::Empty);
        }
        for (key_id, key) in &keys {
            if key_id.is_empty() || key_id.len() > 128 || key.len() != 32 {
                return Err(KeySetError::InvalidKey(key_id.clone()));
            }
        }
        Ok(Self { keys })
    }

    pub fn public_keys_base64(&self) -> BTreeMap<String, String> {
        self.keys
            .iter()
            .map(|(key_id, key)| (key_id.clone(), URL_SAFE_NO_PAD.encode(key)))
            .collect()
    }
}

#[derive(Clone, Debug)]
pub struct AuthorizationVerificationContext {
    pub expected_scope: ManagedInferenceScope,
    pub expected_audience: String,
    pub current_grant_epoch: u64,
    pub now_ms: i64,
    pub clock_skew_ms: i64,
    pub keys: ManagedInferenceKeySet,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifiedManagedInference {
    pub authorization_id: String,
    pub authorization_hash: String,
    pub claims: ManagedInferenceAuthorizationClaims,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum KeySetError {
    #[error("managed inference public-key set is empty")]
    Empty,
    #[error("managed inference public key is invalid: {0}")]
    InvalidKey(String),
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum AuthorizationVerificationError {
    #[error("managed inference authorization serialization failed")]
    Serialization,
    #[error("managed inference authorization has an unsupported schema version: {0}")]
    UnsupportedSchemaVersion(u32),
    #[error("managed inference authorization claim is invalid: {0}")]
    InvalidClaim(&'static str),
    #[error("managed inference authorization lifetime is invalid")]
    InvalidLifetime,
    #[error("managed inference authorization has no provider candidates")]
    NoProviderCandidates,
    #[error("managed inference authorization has a duplicate provider candidate")]
    DuplicateProviderCandidate,
    #[error("managed inference authorization audience is invalid")]
    AudienceMismatch,
    #[error("managed inference authorization is not yet valid")]
    NotYetValid,
    #[error("managed inference authorization has expired")]
    Expired,
    #[error("managed inference authorization grant epoch is stale")]
    GrantEpochMismatch,
    #[error("managed inference authorization scope mismatch: {field}")]
    ScopeMismatch { field: &'static str },
    #[error("managed inference authorization key is unknown")]
    UnknownKey,
    #[error("managed inference authorization signature is malformed")]
    MalformedSignature,
    #[error("managed inference authorization signature is invalid")]
    InvalidSignature,
}

fn canonical_json<T: Serialize>(value: &T) -> Result<Vec<u8>, AuthorizationVerificationError> {
    serde_json::to_vec(value).map_err(|_| AuthorizationVerificationError::Serialization)
}

fn digest(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex_encode(&hasher.finalize())
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(char::from(b"0123456789abcdef"[(byte >> 4) as usize]));
        output.push(char::from(b"0123456789abcdef"[(byte & 0x0f) as usize]));
    }
    output
}

pub fn scope_digest(
    scope: &ManagedInferenceScope,
) -> Result<String, AuthorizationVerificationError> {
    Ok(digest(&canonical_json(scope)?))
}

fn authorization_hash(
    authorization: &ManagedInferenceAuthorization,
) -> Result<String, AuthorizationVerificationError> {
    Ok(digest(&canonical_json(authorization)?))
}

pub fn sign_authorization(
    claims: &ManagedInferenceAuthorizationClaims,
    private_key_pkcs8: &[u8],
) -> Result<ManagedInferenceAuthorization, AuthorizationVerificationError> {
    claims.validate()?;
    if claims.scope_digest != scope_digest(&claims.scope())? {
        return Err(AuthorizationVerificationError::InvalidClaim("scope_digest"));
    }
    let key = Ed25519KeyPair::from_pkcs8(private_key_pkcs8)
        .map_err(|_| AuthorizationVerificationError::InvalidSignature)?;
    let message = canonical_json(claims)?;
    let signature = key.sign(&message);
    Ok(ManagedInferenceAuthorization {
        claims: claims.clone(),
        signature: URL_SAFE_NO_PAD.encode(signature.as_ref()),
    })
}

pub fn verify_authorization(
    authorization: &ManagedInferenceAuthorization,
    context: &AuthorizationVerificationContext,
) -> Result<VerifiedManagedInference, AuthorizationVerificationError> {
    authorization.claims.validate()?;
    if authorization.claims.audience != context.expected_audience {
        return Err(AuthorizationVerificationError::AudienceMismatch);
    }
    let now_with_skew = context
        .now_ms
        .checked_add(context.clock_skew_ms.max(0))
        .unwrap_or(i64::MAX);
    let now_without_skew = context
        .now_ms
        .checked_sub(context.clock_skew_ms.max(0))
        .unwrap_or(i64::MIN);
    if authorization.claims.not_before_ms > now_with_skew {
        return Err(AuthorizationVerificationError::NotYetValid);
    }
    if authorization.claims.expires_at_ms < now_without_skew {
        return Err(AuthorizationVerificationError::Expired);
    }
    if authorization.claims.grant_epoch != context.current_grant_epoch {
        return Err(AuthorizationVerificationError::GrantEpochMismatch);
    }
    let actual_scope = authorization.claims.scope();
    if let Some(error) = scope_mismatch(&actual_scope, &context.expected_scope) {
        return Err(error);
    }
    if authorization.claims.scope_digest != scope_digest(&actual_scope)? {
        return Err(AuthorizationVerificationError::InvalidClaim("scope_digest"));
    }
    let signature = URL_SAFE_NO_PAD
        .decode(&authorization.signature)
        .map_err(|_| AuthorizationVerificationError::MalformedSignature)?;
    if signature.len() != 64 {
        return Err(AuthorizationVerificationError::MalformedSignature);
    }
    let key = context
        .keys
        .keys
        .get(&authorization.claims.key_id)
        .ok_or(AuthorizationVerificationError::UnknownKey)?;
    let public_key = UnparsedPublicKey::new(&ED25519, key.as_slice());
    public_key
        .verify(&canonical_json(&authorization.claims)?, &signature)
        .map_err(|_| AuthorizationVerificationError::InvalidSignature)?;
    Ok(VerifiedManagedInference {
        authorization_id: authorization.claims.authorization_id.clone(),
        authorization_hash: authorization_hash(authorization)?,
        claims: authorization.claims.clone(),
    })
}

fn scope_mismatch(
    actual: &ManagedInferenceScope,
    expected: &ManagedInferenceScope,
) -> Option<AuthorizationVerificationError> {
    if actual.organization_id != expected.organization_id {
        return Some(AuthorizationVerificationError::ScopeMismatch {
            field: "organization_id",
        });
    }
    if actual.workspace_id != expected.workspace_id {
        return Some(AuthorizationVerificationError::ScopeMismatch {
            field: "workspace_id",
        });
    }
    if !expected.session_id.is_empty() && actual.session_id != expected.session_id {
        return Some(AuthorizationVerificationError::ScopeMismatch {
            field: "session_id",
        });
    }
    if !expected.thread_id.is_empty() && actual.thread_id != expected.thread_id {
        return Some(AuthorizationVerificationError::ScopeMismatch { field: "thread_id" });
    }
    if !expected.run_id.is_empty() && actual.run_id != expected.run_id {
        return Some(AuthorizationVerificationError::ScopeMismatch { field: "run_id" });
    }
    if !expected.turn_id.is_empty() && actual.turn_id != expected.turn_id {
        return Some(AuthorizationVerificationError::ScopeMismatch { field: "turn_id" });
    }
    if actual.lineage_id != expected.lineage_id {
        return Some(AuthorizationVerificationError::ScopeMismatch {
            field: "lineage_id",
        });
    }
    if actual.endpoint != expected.endpoint {
        return Some(AuthorizationVerificationError::ScopeMismatch { field: "endpoint" });
    }
    if actual.model != expected.model {
        return Some(AuthorizationVerificationError::ScopeMismatch { field: "model" });
    }
    if actual.provider_candidates != expected.provider_candidates {
        return Some(AuthorizationVerificationError::ScopeMismatch {
            field: "provider_candidates",
        });
    }
    if actual.routing != expected.routing {
        return Some(AuthorizationVerificationError::ScopeMismatch { field: "routing" });
    }
    if actual.output_token_budget != expected.output_token_budget {
        return Some(AuthorizationVerificationError::ScopeMismatch {
            field: "output_token_budget",
        });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use aws_lc_rs::{
        rand::SystemRandom,
        signature::{Ed25519KeyPair, KeyPair as _},
    };
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use std::collections::BTreeMap;

    #[test]
    fn provider_binding_preserves_explicit_route_and_legacy_defaults() {
        let input = serde_json::json!({
            "model": "model-a", "provider": "provider-a", "provider_environment": "production"
        });
        let binding: ManagedInferenceProviderBinding =
            serde_json::from_value(input.clone()).unwrap();
        assert_eq!(binding.model, "model-a");
        assert_eq!(binding.provider, "provider-a");
        assert!(binding.credential_name.is_empty());
        assert!(binding.team_id.is_empty());
        let mut unknown = input.clone();
        unknown["unexpected"] = true.into();
        assert!(serde_json::from_value::<ManagedInferenceProviderBinding>(unknown).is_err());
        let mut missing = input;
        missing
            .as_object_mut()
            .unwrap()
            .remove("provider_environment");
        assert!(serde_json::from_value::<ManagedInferenceProviderBinding>(missing).is_err());
    }

    fn fixture_claims() -> ManagedInferenceAuthorizationClaims {
        ManagedInferenceAuthorizationClaims {
            spend_authority: None,
            schema_version: AUTHORIZATION_SCHEMA_VERSION,
            authorization_id: "auth-1".into(),
            key_id: "maestro-2026-08".into(),
            audience: AUTHORIZATION_AUDIENCE.into(),
            issued_at_ms: 1_700_000_000_000,
            not_before_ms: 1_700_000_000_000,
            expires_at_ms: 1_700_000_060_000,
            grant_epoch: 7,
            organization_id: "org-1".into(),
            workspace_id: "ws-1".into(),
            session_id: "session-1".into(),
            thread_id: "thread-1".into(),
            run_id: "run-1".into(),
            turn_id: "turn-1".into(),
            lineage_id: "lineage-1".into(),
            runtime_generation: 42,
            endpoint: "responses".into(),
            model: "gpt-5.6".into(),
            provider_candidates: vec![
                ManagedInferenceProviderCandidate {
                    provider: "openai".into(),
                    environment: "production".into(),
                    credential_name: "maestro".into(),
                    team_id: "team-1".into(),
                    model: "gpt-5.6".into(),
                },
                ManagedInferenceProviderCandidate {
                    provider: "anthropic".into(),
                    environment: "production".into(),
                    credential_name: "maestro".into(),
                    team_id: "team-1".into(),
                    model: "claude-sonnet".into(),
                },
            ],
            routing: ManagedInferenceRouting::Ordered,
            output_token_budget: ManagedInferenceOutputTokenBudget {
                value: Some(4096),
                origin: ManagedInferenceBudgetOrigin::RoutePolicy,
                origin_reference: "policy-1".into(),
            },
            scope_digest: String::new(),
        }
    }

    fn signed_fixture() -> (ManagedInferenceAuthorization, Vec<u8>) {
        let document = Ed25519KeyPair::generate_pkcs8(&SystemRandom::new()).unwrap();
        let pair = Ed25519KeyPair::from_pkcs8(document.as_ref()).unwrap();
        let mut claims = fixture_claims();
        claims.scope_digest = scope_digest(&claims.scope()).unwrap();
        let authorization = sign_authorization(&claims, document.as_ref()).unwrap();
        (authorization, pair.public_key().as_ref().to_vec())
    }

    #[test]
    fn prepaid_model_prices_are_unique_and_unknown_routes_have_no_default_fallback() {
        let mut models = std::collections::HashSet::new();
        let mut versions = std::collections::HashSet::new();
        assert_eq!(PREPAID_MODEL_PRICING.len(), 7);
        for price in PREPAID_MODEL_PRICING {
            assert!(models.insert(price.model));
            assert!(versions.insert(price.pricing_version));
            assert_eq!(
                prepaid_model_pricing(price.provider, price.model),
                Some(price)
            );
            assert!(prepaid_model_credential_name(price.provider, price.model).is_some());
            assert!(price.input_micros_per_million_tokens > 0);
            assert!(price.cached_input_micros_per_million_tokens > 0);
            assert!(price.output_micros_per_million_tokens > 0);
            assert!(prepaid_model_pricing("openai", price.model).is_none());
        }
        assert!(prepaid_model_pricing("fireworks", "accounts/fireworks/models/unknown").is_none());
        assert_eq!(
            prepaid_model_credential_name("google", "gemini-3.8-flash"),
            Some("deixic-llm-gateway-gemini38")
        );
        assert!(prepaid_model_credential_name("fireworks", "gemini-3.8-flash").is_none());
        assert!(prepaid_model_credential_name("google", "unknown").is_none());
    }

    #[test]
    fn prepaid_bindings_require_reviewed_model_and_exact_coordinates() {
        for price in PREPAID_MODEL_PRICING {
            let mut binding = ManagedInferenceProviderBinding {
                provider: price.provider.into(),
                model: price.model.into(),
                provider_environment: DEFAULT_MANAGED_ENVIRONMENT.into(),
                credential_name: prepaid_model_credential_name(price.provider, price.model)
                    .unwrap()
                    .into(),
                team_id: String::new(),
            };
            assert!(binding.is_prepaid());
            binding.provider_environment = "staging".into();
            assert!(!binding.is_prepaid());
            binding.provider_environment = DEFAULT_MANAGED_ENVIRONMENT.into();
            binding.credential_name.push_str("-custom");
            assert!(!binding.is_prepaid());
            binding.credential_name = prepaid_model_credential_name(price.provider, price.model)
                .unwrap()
                .into();
            binding.model = "unknown".into();
            assert!(!binding.is_prepaid());
            binding.model = price.model.into();
            binding.provider = "unknown".into();
            assert!(!binding.is_prepaid());
        }
    }

    #[test]
    fn work_authority_is_signature_bound_and_cannot_be_stripped_or_replaced() {
        let document = Ed25519KeyPair::generate_pkcs8(&SystemRandom::new()).unwrap();
        let pair = Ed25519KeyPair::from_pkcs8(document.as_ref()).unwrap();
        let mut claims = fixture_claims();
        claims.scope_digest = scope_digest(&claims.scope()).unwrap();
        assert!(
            !serde_json::to_value(&claims)
                .unwrap()
                .as_object()
                .unwrap()
                .contains_key("spend_authority")
        );
        claims.spend_authority = Some(ManagedInferenceSpendAuthority {
            authority_id: "budget-1".into(),
            work_id: "work-1".into(),
            price_book_version: "price-1".into(),
        });
        let signed = sign_authorization(&claims, document.as_ref()).unwrap();
        let keys = BTreeMap::from([(
            "maestro-2026-08".into(),
            pair.public_key().as_ref().to_vec(),
        )]);
        let context = verification_context(&signed, &keys);
        assert!(verify_authorization(&signed, &context).is_ok());
        for field in 0..4 {
            let mut altered = signed.clone();
            match field {
                0 => altered.claims.spend_authority = None,
                1 => {
                    altered
                        .claims
                        .spend_authority
                        .as_mut()
                        .unwrap()
                        .authority_id = "budget-2".into()
                }
                2 => altered.claims.spend_authority.as_mut().unwrap().work_id = "work-2".into(),
                _ => {
                    altered
                        .claims
                        .spend_authority
                        .as_mut()
                        .unwrap()
                        .price_book_version = "price-2".into()
                }
            }
            assert!(verify_authorization(&altered, &context).is_err());
        }
        claims.spend_authority.as_mut().unwrap().work_id = " ".into();
        assert!(sign_authorization(&claims, document.as_ref()).is_err());
    }

    #[test]
    fn scope_digest_is_stable_for_the_declared_candidate_order() {
        let mut claims = fixture_claims();
        let first = scope_digest(&claims.scope()).unwrap();
        let second = scope_digest(&claims.scope()).unwrap();
        assert_eq!(first, second);
        claims.provider_candidates.swap(0, 1);
        assert_ne!(first, scope_digest(&claims.scope()).unwrap());
    }

    #[test]
    fn scope_digest_changes_when_model_provider_or_budget_changes() {
        let claims = fixture_claims();
        let original = scope_digest(&claims.scope()).unwrap();

        let mut model = claims.clone();
        model.model = "gpt-5.7".into();
        assert_ne!(original, scope_digest(&model.scope()).unwrap());

        let mut provider = claims.clone();
        provider.provider_candidates[0].provider = "azure-openai".into();
        assert_ne!(original, scope_digest(&provider.scope()).unwrap());

        let mut budget = claims;
        budget.output_token_budget.value = Some(8192);
        assert_ne!(original, scope_digest(&budget.scope()).unwrap());
    }

    #[test]
    fn authorization_verifies_with_fixed_ed25519_vector() {
        let (authorization, public_key) = signed_fixture();
        let mut keys = BTreeMap::new();
        keys.insert("maestro-2026-08".into(), public_key);
        let context = AuthorizationVerificationContext {
            expected_scope: authorization.claims.scope(),
            expected_audience: AUTHORIZATION_AUDIENCE.into(),
            current_grant_epoch: 7,
            now_ms: 1_700_000_030_000,
            clock_skew_ms: 60_000,
            keys: ManagedInferenceKeySet::from_raw(keys).unwrap(),
        };
        let verified = verify_authorization(&authorization, &context).unwrap();
        assert_eq!(verified.authorization_id, "auth-1");
        assert_eq!(
            verified.authorization_hash,
            authorization_hash(&authorization).unwrap()
        );
    }

    #[test]
    fn authorization_rejects_wrong_scope_and_duplicate_candidates() {
        let (authorization, public_key) = signed_fixture();
        let mut keys = BTreeMap::new();
        keys.insert("maestro-2026-08".into(), public_key);

        let wrong_scope = authorization.clone();
        let mut expected = wrong_scope.claims.scope();
        expected.workspace_id = "ws-other".into();
        let context = AuthorizationVerificationContext {
            expected_scope: expected,
            ..verification_context(&wrong_scope, &keys)
        };
        assert!(matches!(
            verify_authorization(&wrong_scope, &context),
            Err(AuthorizationVerificationError::ScopeMismatch { .. })
        ));

        let mut duplicate = authorization;
        duplicate
            .claims
            .provider_candidates
            .push(duplicate.claims.provider_candidates[0].clone());
        duplicate.claims.scope_digest = scope_digest(&duplicate.claims.scope()).unwrap();
        let document = Ed25519KeyPair::generate_pkcs8(&SystemRandom::new()).unwrap();
        let pair = Ed25519KeyPair::from_pkcs8(document.as_ref()).unwrap();
        let claims_bytes = serde_json::to_vec(&duplicate.claims).unwrap();
        duplicate.signature = URL_SAFE_NO_PAD.encode(pair.sign(&claims_bytes).as_ref());
        let mut duplicate_keys = BTreeMap::new();
        duplicate_keys.insert(
            "maestro-2026-08".into(),
            pair.public_key().as_ref().to_vec(),
        );
        assert!(matches!(
            verify_authorization(
                &duplicate,
                &verification_context(&duplicate, &duplicate_keys)
            ),
            Err(AuthorizationVerificationError::DuplicateProviderCandidate)
        ));
    }

    #[test]
    fn authorization_rejects_non_base64url_signature_and_secret_bearing_fields() {
        let (mut authorization, public_key) = signed_fixture();
        authorization.signature = "not base64!".into();
        let mut keys = BTreeMap::new();
        keys.insert("maestro-2026-08".into(), public_key);
        assert!(matches!(
            verify_authorization(&authorization, &verification_context(&authorization, &keys)),
            Err(AuthorizationVerificationError::MalformedSignature)
        ));

        let secret_json = serde_json::json!({
            "schema_version": 1,
            "authorization_id": "auth-1",
            "key_id": "key-1",
            "audience": AUTHORIZATION_AUDIENCE,
            "issued_at_ms": 1,
            "not_before_ms": 1,
            "expires_at_ms": 2,
            "grant_epoch": 1,
            "organization_id": "org-1",
            "workspace_id": "ws-1",
            "session_id": "session-1",
            "thread_id": "thread-1",
            "run_id": "run-1",
            "turn_id": "turn-1",
            "lineage_id": "lineage-1",
            "runtime_generation": 1,
            "endpoint": "responses",
            "model": "gpt-5.6",
            "provider_candidates": [],
            "routing": "ordered",
            "output_token_budget": {"value": 1, "origin": "explicit", "origin_reference": "test"},
            "scope_digest": "digest",
            "api_key": "sk-secret"
        });
        assert!(
            serde_json::from_value::<ManagedInferenceAuthorizationClaims>(secret_json).is_err()
        );
    }

    #[test]
    fn authorization_rejects_control_characters_in_budget_origin_reference() {
        let mut claims = fixture_claims();
        claims.output_token_budget.origin_reference = "policy\nsecret".into();
        claims.scope_digest = scope_digest(&claims.scope()).unwrap();
        assert!(matches!(
            sign_authorization(
                &claims,
                Ed25519KeyPair::generate_pkcs8(&SystemRandom::new())
                    .unwrap()
                    .as_ref(),
            ),
            Err(AuthorizationVerificationError::InvalidClaim(
                "output_token_budget.origin_reference"
            ))
        ));
    }

    fn verification_context(
        authorization: &ManagedInferenceAuthorization,
        keys: &BTreeMap<String, Vec<u8>>,
    ) -> AuthorizationVerificationContext {
        AuthorizationVerificationContext {
            expected_scope: authorization.claims.scope(),
            expected_audience: AUTHORIZATION_AUDIENCE.into(),
            current_grant_epoch: 7,
            now_ms: 1_700_000_030_000,
            clock_skew_ms: 60_000,
            keys: ManagedInferenceKeySet::from_raw(keys.clone()).unwrap(),
        }
    }
}

#[cfg(test)]
mod pricing_tests {
    use super::PREPAID_DEFAULT_PRICING;

    #[test]
    fn published_prices_convert_exactly_to_the_gateway_contract() {
        assert_eq!(PREPAID_DEFAULT_PRICING.input_usd_per_million(), 1.4);
        assert_eq!(PREPAID_DEFAULT_PRICING.cached_input_usd_per_million(), 0.26);
        assert_eq!(PREPAID_DEFAULT_PRICING.output_usd_per_million(), 4.4);
        assert_eq!(
            PREPAID_DEFAULT_PRICING.pricing_version,
            "fireworks-glm-5p3-2026-09-04"
        );
    }
}

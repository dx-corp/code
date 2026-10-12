//! Typed, non-authoritative Platform controller identity bound during headless hello.
//!
//! This contract is correlation and compatibility evidence only. It does not
//! grant tools, connections, credentials, or execution authority; governed
//! effects continue to require the signed [`super::messages::GovernedToolGrant`].

use std::fmt::Write as _;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use thiserror::Error;

use super::messages::ControllerBindingHello;

/// Version of the optional Platform-to-Maestro controller binding handshake.
pub const CONTROLLER_BINDING_VERSION: &str = "evalops.maestro.controller-binding.v1";
/// Schema version for the secret-free controller context inside the binding.
pub const CONTROLLER_CONTEXT_SCHEMA_VERSION: &str = "evalops.maestro.controller-context.v1";
const PLATFORM_CONTROLLER_ID: &str = "evalops.platform";

/// Lifetime selected by Platform before the Maestro process is admitted.
#[derive(Debug, Clone, Copy, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ControllerLifetimeProfile {
    /// One bounded operating turn.
    Ephemeral,
    /// One durable hosted thread generation.
    Resident,
}

/// Secret-free Platform scope and request correlation for one headless child.
#[derive(Debug, Clone, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ControllerContext {
    pub schema_version: String,
    pub controller_id: String,
    pub organization_id: String,
    pub workspace_id: String,
    pub thread_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    pub lifetime_profile: ControllerLifetimeProfile,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_generation: Option<u64>,
}

/// Runtime identity values that a managed process may require the hello to match.
#[derive(Debug, Clone, Default, Eq, PartialEq)]
pub struct ControllerScopeExpectation {
    pub organization_id: Option<String>,
    pub workspace_id: Option<String>,
    pub thread_id: Option<String>,
    pub channel_id: Option<String>,
    pub request_id: Option<String>,
}

impl ControllerScopeExpectation {
    /// Reads only secret-free identity variables populated by the Platform adapter.
    #[must_use]
    pub fn from_evalops_environment() -> Self {
        Self {
            organization_id: std::env::var("MAESTRO_EVALOPS_ORG_ID").ok(),
            workspace_id: std::env::var("MAESTRO_EVALOPS_WORKSPACE_ID").ok(),
            thread_id: std::env::var("MAESTRO_EVALOPS_THREAD_ID").ok(),
            channel_id: std::env::var("MAESTRO_EVALOPS_CHANNEL_ID").ok(),
            request_id: std::env::var("MAESTRO_EVALOPS_REQUEST_ID").ok(),
        }
    }
}

/// Echoed proof that Maestro parsed and accepted the exact binding payload.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct ControllerBindingReceipt {
    pub binding_version: String,
    pub binding_sha256: String,
    pub controller_context: ControllerContext,
}

/// Strict controller-binding validation failures.
#[derive(Debug, Error, Eq, PartialEq)]
pub enum ControllerBindingError {
    #[error("controller binding fields must be supplied together")]
    Incomplete,
    #[error("unsupported controller binding version")]
    UnsupportedBindingVersion,
    #[error("unsupported controller context schema")]
    UnsupportedContextSchema,
    #[error("controller context field is empty: {0}")]
    EmptyField(&'static str),
    #[error("controller context is not owned by EvalOps Platform")]
    InvalidController,
    #[error("controller lifetime profile and runtime generation disagree")]
    InvalidLifetime,
    #[error("controller context does not match managed runtime field: {0}")]
    ScopeMismatch(&'static str),
    #[error("capability manifest is not an object")]
    InvalidManifest,
    #[error("capability manifest field is missing or invalid: {0}")]
    InvalidManifestField(&'static str),
    #[error("capability manifest protocol does not match the negotiated protocol")]
    ManifestProtocolMismatch,
    #[error("controller binding JSON is invalid")]
    InvalidJson,
    #[error("controller binding digest could not be encoded")]
    DigestEncoding,
}

#[derive(Debug, Deserialize)]
struct ControllerHelloExtension {
    #[serde(default)]
    controller_binding_version: Option<String>,
    #[serde(default)]
    controller_context: Option<ControllerContext>,
    #[serde(default)]
    capability_manifest: Option<Value>,
}

/// Parses and validates the optional controller extension carried by `hello`.
pub fn controller_binding_from_hello_json(
    raw: &str,
    negotiated_protocol_version: &str,
    expected: &ControllerScopeExpectation,
) -> Result<Option<ControllerBindingReceipt>, ControllerBindingError> {
    let extension: ControllerHelloExtension =
        serde_json::from_str(raw).map_err(|_| ControllerBindingError::InvalidJson)?;
    let supplied = extension.controller_binding_version.is_some()
        || extension.controller_context.is_some()
        || extension.capability_manifest.is_some();
    if !supplied {
        return Ok(None);
    }
    let binding_version = extension
        .controller_binding_version
        .ok_or(ControllerBindingError::Incomplete)?;
    let context = extension
        .controller_context
        .ok_or(ControllerBindingError::Incomplete)?;
    let manifest = extension
        .capability_manifest
        .ok_or(ControllerBindingError::Incomplete)?;

    validate_context(&binding_version, &context, expected)?;
    validate_manifest(&manifest, negotiated_protocol_version)?;
    let binding_sha256 = controller_binding_sha256(&binding_version, &context, &manifest)?;
    Ok(Some(ControllerBindingReceipt {
        binding_version,
        binding_sha256,
        controller_context: context,
    }))
}

pub fn controller_binding_from_hello_extension(
    extension: Option<&ControllerBindingHello>,
    negotiated_protocol_version: &str,
    expected: &ControllerScopeExpectation,
) -> Result<Option<ControllerBindingReceipt>, ControllerBindingError> {
    let Some(extension) = extension else {
        return Ok(None);
    };
    let context = serde_json::from_value::<ControllerContext>(extension.controller_context.clone())
        .map_err(|_| ControllerBindingError::InvalidJson)?;
    validate_context(&extension.controller_binding_version, &context, expected)?;
    validate_manifest(&extension.capability_manifest, negotiated_protocol_version)?;
    let binding_sha256 = controller_binding_sha256(
        &extension.controller_binding_version,
        &context,
        &extension.capability_manifest,
    )?;
    Ok(Some(ControllerBindingReceipt {
        binding_version: extension.controller_binding_version.clone(),
        binding_sha256,
        controller_context: context,
    }))
}

fn validate_context(
    binding_version: &str,
    context: &ControllerContext,
    expected: &ControllerScopeExpectation,
) -> Result<(), ControllerBindingError> {
    if binding_version != CONTROLLER_BINDING_VERSION {
        return Err(ControllerBindingError::UnsupportedBindingVersion);
    }
    if context.schema_version != CONTROLLER_CONTEXT_SCHEMA_VERSION {
        return Err(ControllerBindingError::UnsupportedContextSchema);
    }
    if context.controller_id != PLATFORM_CONTROLLER_ID {
        return Err(ControllerBindingError::InvalidController);
    }
    for (field, value) in [
        ("organization_id", context.organization_id.as_str()),
        ("workspace_id", context.workspace_id.as_str()),
        ("thread_id", context.thread_id.as_str()),
    ] {
        if value.trim().is_empty() {
            return Err(ControllerBindingError::EmptyField(field));
        }
    }
    for (field, value) in [
        ("channel_id", context.channel_id.as_deref()),
        ("request_id", context.request_id.as_deref()),
    ] {
        if value.is_some_and(|value| value.trim().is_empty()) {
            return Err(ControllerBindingError::EmptyField(field));
        }
    }
    match (context.lifetime_profile, context.runtime_generation) {
        (ControllerLifetimeProfile::Ephemeral, None) => {}
        (ControllerLifetimeProfile::Resident, Some(generation)) if generation > 0 => {}
        _ => return Err(ControllerBindingError::InvalidLifetime),
    }
    require_expected(
        expected.organization_id.as_deref(),
        &context.organization_id,
        "organization_id",
    )?;
    require_expected(
        expected.workspace_id.as_deref(),
        &context.workspace_id,
        "workspace_id",
    )?;
    require_expected(
        expected.thread_id.as_deref(),
        &context.thread_id,
        "thread_id",
    )?;
    require_expected_optional(
        expected.channel_id.as_deref(),
        context.channel_id.as_deref(),
        "channel_id",
    )?;
    require_expected_optional(
        expected.request_id.as_deref(),
        context.request_id.as_deref(),
        "request_id",
    )?;
    Ok(())
}

fn require_expected(
    expected: Option<&str>,
    actual: &str,
    field: &'static str,
) -> Result<(), ControllerBindingError> {
    if expected.is_some_and(|expected| expected != actual) {
        return Err(ControllerBindingError::ScopeMismatch(field));
    }
    Ok(())
}

fn require_expected_optional(
    expected: Option<&str>,
    actual: Option<&str>,
    field: &'static str,
) -> Result<(), ControllerBindingError> {
    if expected.is_some_and(|expected| Some(expected) != actual) {
        return Err(ControllerBindingError::ScopeMismatch(field));
    }
    Ok(())
}

fn validate_manifest(
    manifest: &Value,
    negotiated_protocol_version: &str,
) -> Result<(), ControllerBindingError> {
    let object = manifest
        .as_object()
        .ok_or(ControllerBindingError::InvalidManifest)?;
    let required_string = |field: &'static str| {
        object
            .get(field)
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .ok_or(ControllerBindingError::InvalidManifestField(field))
    };
    required_string("schema_version")?;
    if required_string("engine_kind")? != "maestro" {
        return Err(ControllerBindingError::InvalidManifestField("engine_kind"));
    }
    if required_string("protocol_version")? != negotiated_protocol_version {
        return Err(ControllerBindingError::ManifestProtocolMismatch);
    }
    Ok(())
}

/// Computes the cross-repository canonical binding digest.
pub fn controller_binding_sha256(
    binding_version: &str,
    context: &ControllerContext,
    capability_manifest: &Value,
) -> Result<String, ControllerBindingError> {
    let value = serde_json::json!({
        "binding_version": binding_version,
        "capability_manifest": capability_manifest,
        "controller_context": context,
    });
    let mut canonical = String::new();
    write_canonical_json(&value, &mut canonical)?;
    let digest = Sha256::digest(canonical.as_bytes());
    let mut encoded = String::with_capacity("sha256:".len() + digest.len() * 2);
    encoded.push_str("sha256:");
    for byte in digest {
        write!(&mut encoded, "{byte:02x}").map_err(|_| ControllerBindingError::DigestEncoding)?;
    }
    Ok(encoded)
}

fn write_canonical_json(value: &Value, output: &mut String) -> Result<(), ControllerBindingError> {
    match value {
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {
            output.push_str(
                &serde_json::to_string(value)
                    .map_err(|_| ControllerBindingError::DigestEncoding)?,
            );
        }
        Value::Array(items) => {
            output.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    output.push(',');
                }
                write_canonical_json(item, output)?;
            }
            output.push(']');
        }
        Value::Object(object) => {
            output.push('{');
            let mut keys = object.keys().collect::<Vec<_>>();
            keys.sort_unstable();
            for (index, key) in keys.into_iter().enumerate() {
                if index > 0 {
                    output.push(',');
                }
                output.push_str(
                    &serde_json::to_string(key)
                        .map_err(|_| ControllerBindingError::DigestEncoding)?,
                );
                output.push(':');
                write_canonical_json(&object[key], output)?;
            }
            output.push('}');
        }
    }
    Ok(())
}

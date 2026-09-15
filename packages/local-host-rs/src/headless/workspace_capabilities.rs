//! Prompt-only workspace capability activation for hosted resident Maestro.
//!
//! This projects Platform-authorized workspace instructions into the model
//! prompt. It deliberately does not create tools, credentials, approvals, or
//! any other executable authority.

use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use thiserror::Error;

use super::controller_binding::{
    CONTROLLER_BINDING_VERSION, ControllerBindingReceipt, ControllerContext,
    ControllerLifetimeProfile,
};

const PROMPT_CAPABILITY_SCHEMA_VERSION: &str = "evalops.maestro.workspace-prompt-capability-set.v1";

/// One selected workspace capability projected only as model context.
#[derive(Debug, Clone, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspacePromptCapability {
    pub qualified_id: String,
    pub name: String,
    pub scope: String,
    pub revision_digest: String,
    pub body_digest: String,
    pub trigger_patterns: Vec<String>,
    pub user_invocable: bool,
    /// This protocol slice supports only entries whose verified body is pinned
    /// into prompt context. On-demand matching is intentionally rejected.
    pub pinned_prompt_only: bool,
    pub title: String,
    pub description: String,
    pub instructions: Vec<String>,
    pub body: String,
    pub entry_digest: String,
}

/// Complete-replacement request for a resident workspace prompt capability set.
#[derive(Debug, Clone, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ApplyWorkspaceCapabilitySet {
    pub organization_id: String,
    pub workspace_id: String,
    pub runner_session_id: String,
    pub runtime_generation: u64,
    pub activation_generation: u64,
    pub workspace_snapshot_digest: String,
    pub workspace_skill_set_digest: String,
    pub capability_set_digest: String,
    pub workspace_instructions: Vec<String>,
    pub admitted_catalog: Vec<WorkspacePromptCapability>,
    pub admission_receipt_id: String,
}

/// Secret-free receipt proving the exact projected prompt set was accepted.
#[derive(Debug, Clone, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct WorkspaceCapabilitySetApplied {
    pub schema_version: String,
    pub organization_id: String,
    pub workspace_id: String,
    pub runner_session_id: String,
    pub runtime_generation: u64,
    pub activation_generation: u64,
    pub effective_catalog_digest: String,
    pub accepted_entry_digests: Vec<String>,
    pub rejected_entries: Vec<String>,
    pub replay_cursor: String,
    pub applied_at: u64,
    pub controller_binding_sha256: String,
    pub provider_prompt_sha256: String,
    pub staged_for_next_turn: bool,
    pub idempotent: bool,
}

/// A runtime receipt may become reconnect authority only for the complete
/// admitted set, and only if it cannot roll back a newer accepted generation.
pub(crate) fn accept_workspace_capability_receipt(
    pending: &mut HashMap<String, ApplyWorkspaceCapabilitySet>,
    last_accepted: &mut Option<ApplyWorkspaceCapabilitySet>,
    receipt: &WorkspaceCapabilitySetApplied,
) -> bool {
    let Some(request) = pending.get(&receipt.replay_cursor) else {
        return false;
    };
    let matches = receipt.schema_version == PROMPT_CAPABILITY_SCHEMA_VERSION
        && receipt.replay_cursor
            == format!(
                "{}:{}",
                request.activation_generation, request.capability_set_digest
            )
        && receipt.organization_id == request.organization_id
        && receipt.workspace_id == request.workspace_id
        && receipt.runner_session_id == request.runner_session_id
        && receipt.runtime_generation == request.runtime_generation
        && receipt.activation_generation == request.activation_generation
        && receipt.effective_catalog_digest == request.capability_set_digest
        && receipt.accepted_entry_digests
            == request
                .admitted_catalog
                .iter()
                .map(|entry| entry.entry_digest.as_str())
                .collect::<Vec<_>>()
        && receipt.rejected_entries.is_empty()
        && last_accepted.as_ref().is_none_or(|accepted| {
            request.activation_generation > accepted.activation_generation
                || (request.activation_generation == accepted.activation_generation
                    && request.capability_set_digest == accepted.capability_set_digest)
        });
    if !matches {
        return false;
    }
    let accepted = pending
        .remove(&receipt.replay_cursor)
        .expect("matched pending capability set");
    pending.retain(|_, request| request.activation_generation > accepted.activation_generation);
    *last_accepted = Some(accepted);
    true
}

#[derive(Debug, Error, Eq, PartialEq)]
pub(crate) enum WorkspaceCapabilityError {
    #[error(
        "workspace prompt capabilities require an accepted Platform resident controller binding"
    )]
    MissingResidentBinding,
    #[error("workspace prompt capabilities do not match the accepted controller scope")]
    ScopeMismatch,
    #[error("workspace prompt capabilities use an invalid generation")]
    InvalidGeneration,
    #[error("workspace prompt capability set digest does not match its recomputed canonical value")]
    DigestMismatch,
    #[error("workspace prompt capability catalog must be sorted, unique, and selected")]
    InvalidCatalog,
    #[error("workspace prompt capability values must be non-empty and bounded")]
    InvalidContent,
    #[error("workspace prompt capability generation cannot move backwards or change in place")]
    GenerationConflict,
    #[error("workspace prompt capability JSON could not be canonicalized")]
    Canonicalization,
}

#[derive(Debug, Clone)]
pub(crate) struct AcceptedCapabilitySet {
    request: ApplyWorkspaceCapabilitySet,
    effective_catalog_digest: String,
    accepted_entry_digests: Vec<String>,
    prompt: String,
    provider_prompt_sha256: String,
    applied_at: u64,
}

#[derive(Debug, Clone)]
pub(crate) enum PreparedWorkspaceCapabilitySet {
    New(AcceptedCapabilitySet),
    Idempotent(AcceptedCapabilitySet),
}

impl PreparedWorkspaceCapabilitySet {
    #[must_use]
    pub(crate) fn prompt(&self) -> &str {
        match self {
            Self::New(set) | Self::Idempotent(set) => &set.prompt,
        }
    }

    /// Bind the admission receipt to the complete instructions the runtime will send.
    pub(crate) fn bind_provider_prompt(&mut self, prompt: &str) {
        let (Self::New(set) | Self::Idempotent(set)) = self;
        set.provider_prompt_sha256 = sha256_prefixed(prompt.as_bytes());
    }

    #[must_use]
    pub(crate) fn is_idempotent(&self) -> bool {
        matches!(self, Self::Idempotent(_))
    }
}

/// In-memory resident state. Platform remains the durable owner of the set.
#[derive(Debug, Clone)]
pub(crate) struct WorkspaceCapabilityActivation {
    base_prompt: String,
    current: Option<AcceptedCapabilitySet>,
    staged: Option<AcceptedCapabilitySet>,
}

impl WorkspaceCapabilityActivation {
    #[must_use]
    pub(crate) fn new(base_prompt: String) -> Self {
        Self {
            base_prompt,
            current: None,
            staged: None,
        }
    }

    pub(crate) fn set_base_prompt(&mut self, base_prompt: String) {
        self.base_prompt = base_prompt;
        if let Some(current) = self.current.as_mut() {
            current.prompt = project_prompt(&self.base_prompt, &current.request);
            current.provider_prompt_sha256 = provider_prompt_sha256(&current.prompt);
        }
        if let Some(staged) = self.staged.as_mut() {
            staged.prompt = project_prompt(&self.base_prompt, &staged.request);
            staged.provider_prompt_sha256 = provider_prompt_sha256(&staged.prompt);
        }
    }

    #[must_use]
    pub(crate) fn base_prompt(&self) -> &str {
        &self.base_prompt
    }

    #[must_use]
    pub(crate) fn current_prompt(&self) -> &str {
        self.current
            .as_ref()
            .map_or(self.base_prompt.as_str(), |set| set.prompt.as_str())
    }

    #[must_use]
    pub(crate) fn prompt_for_next_turn(&self) -> &str {
        self.staged
            .as_ref()
            .map_or_else(|| self.current_prompt(), |set| set.prompt.as_str())
    }

    #[must_use]
    pub(crate) fn has_staged_set(&self) -> bool {
        self.staged.is_some()
    }

    #[cfg(test)]
    pub(crate) fn apply(
        &mut self,
        request: ApplyWorkspaceCapabilitySet,
        binding: &ControllerBindingReceipt,
        context: &ControllerContext,
        runner_session_id: &str,
        turn_active: bool,
    ) -> Result<WorkspaceCapabilitySetApplied, WorkspaceCapabilityError> {
        let prepared = self.prepare(request, binding, context, runner_session_id)?;
        Ok(self.commit(prepared, binding, turn_active))
    }

    pub(crate) fn prepare(
        &self,
        request: ApplyWorkspaceCapabilitySet,
        binding: &ControllerBindingReceipt,
        context: &ControllerContext,
        runner_session_id: &str,
    ) -> Result<PreparedWorkspaceCapabilitySet, WorkspaceCapabilityError> {
        validate_binding(binding, context, runner_session_id, &request)?;
        let accepted_entry_digests = validate_content(&request)?;
        let effective_catalog_digest = capability_set_sha256(&request, &accepted_entry_digests)?;
        if request.capability_set_digest != effective_catalog_digest {
            return Err(WorkspaceCapabilityError::DigestMismatch);
        }
        let prompt = project_prompt(&self.base_prompt, &request);
        let candidate = AcceptedCapabilitySet {
            request,
            effective_catalog_digest,
            accepted_entry_digests,
            provider_prompt_sha256: provider_prompt_sha256(&prompt),
            prompt,
            applied_at: now_unix_ms(),
        };
        let existing = self.staged.as_ref().or(self.current.as_ref());
        if let Some(existing) = existing {
            if candidate.request.activation_generation < existing.request.activation_generation
                || (candidate.request.activation_generation
                    == existing.request.activation_generation
                    && candidate.effective_catalog_digest != existing.effective_catalog_digest)
            {
                return Err(WorkspaceCapabilityError::GenerationConflict);
            }
            if candidate.request.activation_generation == existing.request.activation_generation {
                return Ok(PreparedWorkspaceCapabilitySet::Idempotent(existing.clone()));
            }
        }
        Ok(PreparedWorkspaceCapabilitySet::New(candidate))
    }

    pub(crate) fn commit(
        &mut self,
        prepared: PreparedWorkspaceCapabilitySet,
        binding: &ControllerBindingReceipt,
        turn_active: bool,
    ) -> WorkspaceCapabilitySetApplied {
        if let PreparedWorkspaceCapabilitySet::Idempotent(existing) = prepared {
            let staged_for_next_turn = self.staged.as_ref().is_some_and(|staged| {
                staged.effective_catalog_digest == existing.effective_catalog_digest
            });
            return receipt(&existing, binding, staged_for_next_turn, true);
        }
        let PreparedWorkspaceCapabilitySet::New(candidate) = prepared else {
            unreachable!("idempotent capability set returned above")
        };
        if turn_active {
            self.staged = Some(candidate);
            receipt(
                self.staged.as_ref().expect("staged set"),
                binding,
                true,
                false,
            )
        } else {
            self.current = Some(candidate);
            self.staged = None;
            receipt(
                self.current.as_ref().expect("current set"),
                binding,
                false,
                false,
            )
        }
    }

    /// Promotes a set staged while a native turn was active at the next boundary.
    pub(crate) fn activate_staged_for_next_turn(&mut self) {
        if let Some(staged) = self.staged.take() {
            self.current = Some(staged);
        }
    }
}

fn validate_binding(
    binding: &ControllerBindingReceipt,
    context: &ControllerContext,
    runner_session_id: &str,
    request: &ApplyWorkspaceCapabilitySet,
) -> Result<(), WorkspaceCapabilityError> {
    if binding.binding_version != CONTROLLER_BINDING_VERSION
        || binding.controller_context != *context
        || context.lifetime_profile != ControllerLifetimeProfile::Resident
        || context.runtime_generation != Some(request.runtime_generation)
    {
        return Err(WorkspaceCapabilityError::MissingResidentBinding);
    }
    if request.runtime_generation == 0 || request.activation_generation == 0 {
        return Err(WorkspaceCapabilityError::InvalidGeneration);
    }
    if request.organization_id != context.organization_id
        || request.workspace_id != context.workspace_id
        || request.runner_session_id != runner_session_id
    {
        return Err(WorkspaceCapabilityError::ScopeMismatch);
    }
    if request.runner_session_id.trim().is_empty()
        || request.admission_receipt_id.trim().is_empty()
        || !is_sha256_digest(&request.workspace_snapshot_digest)
        || !is_sha256_digest(&request.workspace_skill_set_digest)
        || !is_sha256_digest(&request.capability_set_digest)
    {
        return Err(WorkspaceCapabilityError::InvalidContent);
    }
    Ok(())
}

fn validate_content(
    request: &ApplyWorkspaceCapabilitySet,
) -> Result<Vec<String>, WorkspaceCapabilityError> {
    if request.workspace_instructions.is_empty()
        || request.workspace_instructions.len() > 64
        || request
            .workspace_instructions
            .iter()
            .any(|value| value.trim().is_empty() || value.len() > 4096)
        || request.admitted_catalog.len() > 64
    {
        return Err(WorkspaceCapabilityError::InvalidContent);
    }
    let mut previous: Option<&str> = None;
    let mut digests = Vec::with_capacity(request.admitted_catalog.len());
    let mut names = std::collections::BTreeSet::new();
    for capability in &request.admitted_catalog {
        if capability.qualified_id.trim().is_empty()
            || capability.name.trim().is_empty()
            || capability.scope.trim().is_empty()
            || !is_sha256_digest(&capability.revision_digest)
            || !is_sha256_digest(&capability.body_digest)
            || capability.body_digest != sha256_prefixed(capability.body.as_bytes())
            || capability.trigger_patterns.is_empty()
            || capability.trigger_patterns.len() > 32
            || capability
                .trigger_patterns
                .iter()
                .any(|value| value.trim().is_empty() || value.len() > 256)
            || !capability.pinned_prompt_only
            || capability.title.trim().is_empty()
            || capability.description.trim().is_empty()
            || capability.instructions.is_empty()
            || capability.instructions.len() > 32
            || capability
                .instructions
                .iter()
                .any(|value| value.trim().is_empty() || value.len() > 4096)
            || capability.body.trim().is_empty()
            || capability.body.len() > 64 * 1024
            || !is_safe_prompt_capability_id(&capability.qualified_id)
            || !names.insert(capability.name.clone())
            || previous.is_some_and(|prior| prior >= capability.qualified_id.as_str())
        {
            return Err(WorkspaceCapabilityError::InvalidCatalog);
        }
        let digest = capability_item_sha256(capability)?;
        if capability.entry_digest != digest {
            return Err(WorkspaceCapabilityError::DigestMismatch);
        }
        digests.push(digest);
        previous = Some(&capability.qualified_id);
    }
    Ok(digests)
}

fn is_safe_prompt_capability_id(value: &str) -> bool {
    !value.starts_with("native.")
        && !value.starts_with("tool.")
        && !value.starts_with("client_")
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

fn capability_set_sha256(
    request: &ApplyWorkspaceCapabilitySet,
    capability_digests: &[String],
) -> Result<String, WorkspaceCapabilityError> {
    let catalog = request
        .admitted_catalog
        .iter()
        .zip(capability_digests)
        .map(|(capability, capability_sha256)| {
            serde_json::json!({
                "body": capability.body,
                "entry_digest": capability_sha256,
                "description": capability.description,
                "instructions": capability.instructions,
                "name": capability.name,
                "pinned_prompt_only": capability.pinned_prompt_only,
                "qualified_id": capability.qualified_id,
                "revision_digest": capability.revision_digest,
                "body_digest": capability.body_digest,
                "scope": capability.scope,
                "trigger_patterns": capability.trigger_patterns,
                "user_invocable": capability.user_invocable,
                "title": capability.title,
            })
        })
        .collect::<Vec<_>>();
    let value = serde_json::json!({
        "activation_generation": request.activation_generation,
        "admission_receipt_id": request.admission_receipt_id,
        "catalog": catalog,
        "runner_session_id": request.runner_session_id,
        "workspace_instructions": request.workspace_instructions,
        "organization_id": request.organization_id,
        "runtime_generation": request.runtime_generation,
        "schema_version": PROMPT_CAPABILITY_SCHEMA_VERSION,
        "workspace_skill_set_digest": request.workspace_skill_set_digest,
        "workspace_snapshot_digest": request.workspace_snapshot_digest,
        "workspace_id": request.workspace_id,
    });
    let mut canonical = String::new();
    write_canonical_json(&value, &mut canonical)?;
    Ok(sha256_prefixed(canonical.as_bytes()))
}

fn capability_item_sha256(
    capability: &WorkspacePromptCapability,
) -> Result<String, WorkspaceCapabilityError> {
    let value = serde_json::json!({
        "body": capability.body,
        "description": capability.description,
        "instructions": capability.instructions,
        "name": capability.name,
        "pinned_prompt_only": capability.pinned_prompt_only,
        "qualified_id": capability.qualified_id,
        "revision_digest": capability.revision_digest,
        "body_digest": capability.body_digest,
        "scope": capability.scope,
        "trigger_patterns": capability.trigger_patterns,
        "user_invocable": capability.user_invocable,
        "title": capability.title,
    });
    let mut canonical = String::new();
    write_canonical_json(&value, &mut canonical)?;
    Ok(sha256_prefixed(canonical.as_bytes()))
}

#[cfg(test)]
pub(crate) fn recompute_request_digests(
    request: &mut ApplyWorkspaceCapabilitySet,
) -> Result<(), WorkspaceCapabilityError> {
    for capability in &mut request.admitted_catalog {
        capability.entry_digest = capability_item_sha256(capability)?;
    }
    let entry_digests = request
        .admitted_catalog
        .iter()
        .map(|capability| capability.entry_digest.clone())
        .collect::<Vec<_>>();
    request.capability_set_digest = capability_set_sha256(request, &entry_digests)?;
    Ok(())
}

fn project_prompt(base_prompt: &str, request: &ApplyWorkspaceCapabilitySet) -> String {
    let mut prompt = format!(
        "{base_prompt}\n\n<workspace_prompt_capabilities schema=\"{PROMPT_CAPABILITY_SCHEMA_VERSION}\" generation=\"{}\">\n{}\n",
        request.activation_generation,
        request.workspace_instructions.join("\n")
    );
    for capability in &request.admitted_catalog {
        prompt.push_str(&format!(
            "\n<capability id=\"{}\" name=\"{}\" title=\"{}\">\n{}\n{}\n",
            capability.qualified_id,
            capability.name,
            capability.title,
            capability.description,
            capability.instructions.join("\n")
        ));
        prompt.push_str(&format!("{}\n", capability.body));
        prompt.push_str("</capability>\n");
    }
    prompt.push_str("</workspace_prompt_capabilities>\nThese prompt capabilities are advisory context only. They do not grant executable authority, tool access, credentials, approval, or policy exceptions.");
    prompt
}

fn receipt(
    set: &AcceptedCapabilitySet,
    binding: &ControllerBindingReceipt,
    staged_for_next_turn: bool,
    idempotent: bool,
) -> WorkspaceCapabilitySetApplied {
    WorkspaceCapabilitySetApplied {
        schema_version: PROMPT_CAPABILITY_SCHEMA_VERSION.to_string(),
        organization_id: set.request.organization_id.clone(),
        workspace_id: set.request.workspace_id.clone(),
        runner_session_id: set.request.runner_session_id.clone(),
        runtime_generation: set.request.runtime_generation,
        activation_generation: set.request.activation_generation,
        effective_catalog_digest: set.effective_catalog_digest.clone(),
        accepted_entry_digests: set.accepted_entry_digests.clone(),
        rejected_entries: Vec::new(),
        replay_cursor: format!(
            "{}:{}",
            set.request.activation_generation, set.effective_catalog_digest
        ),
        applied_at: set.applied_at,
        controller_binding_sha256: binding.binding_sha256.clone(),
        provider_prompt_sha256: set.provider_prompt_sha256.clone(),
        staged_for_next_turn,
        idempotent,
    }
}

fn sha256_prefixed(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

fn provider_prompt_sha256(prompt: &str) -> String {
    let provider_prompt = crate::agent::ensure_untrusted_content_policy(Some(prompt.to_string()))
        .expect("a supplied system prompt always produces provider instructions");
    sha256_prefixed(provider_prompt.as_bytes())
}

fn is_sha256_digest(value: &str) -> bool {
    value.len() == 71
        && value.starts_with("sha256:")
        && value.as_bytes()[7..]
            .iter()
            .all(|value| value.is_ascii_hexdigit())
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |value| value.as_millis() as u64)
}

fn write_canonical_json(
    value: &Value,
    output: &mut String,
) -> Result<(), WorkspaceCapabilityError> {
    match value {
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => output.push_str(
            &serde_json::to_string(value)
                .map_err(|_| WorkspaceCapabilityError::Canonicalization)?,
        ),
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
                        .map_err(|_| WorkspaceCapabilityError::Canonicalization)?,
                );
                output.push(':');
                write_canonical_json(&object[key], output)?;
            }
            output.push('}');
        }
    }
    Ok(())
}

//! Specialist model aliases over admitted ordinary tools.
//!
//! These values describe host-owned routes, never credentials or transport
//! endpoints. Resolving an alias does not execute it: the caller must send the
//! returned name and unchanged arguments through the ordinary nested-call
//! bridge, where the call budget, policy, journal and cancellation still apply.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::Tool;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ModelOperation {
    Classify,
    GenerateImages,
}

impl ModelOperation {
    pub fn function_name(self) -> &'static str {
        match self {
            Self::Classify => "classify",
            Self::GenerateImages => "generateImages",
        }
    }
}

/// Exact references declared by the host for one admitted tool. The host
/// remains responsible for verifying that these references match its executor.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelBinding {
    pub owner: String,
    pub provider: String,
    pub model: String,
}

/// Selection never authorizes a route; every field must match admitted metadata.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelSelector {
    #[serde(default)]
    pub owner: Option<String>,
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ModelAlias {
    pub operation: ModelOperation,
    pub tool: String,
    pub binding: ModelBinding,
    pub input_schema: Value,
    pub output_schema: Option<Value>,
}

/// An ordinary tool request, without a separate model transport or authority.
#[derive(Clone, Debug, PartialEq)]
pub struct ModelCall {
    pub name: String,
    pub args: Value,
}

fn valid_reference(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && !value.contains("://")
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b'/' | b':')
        })
}

/// Build aliases only from the catalog already admitted by the host. An
/// incomplete declaration fails closed rather than fabricating availability.
pub fn admitted_models(tools: &[Tool]) -> Result<Vec<ModelAlias>, String> {
    let mut aliases = Vec::new();
    let mut names = std::collections::BTreeSet::new();
    for tool in tools {
        let Some(operation) = tool.model_operation else {
            continue;
        };
        let binding = tool
            .model_binding
            .as_ref()
            .ok_or_else(|| format!("model tool {} has no owner/model binding", tool.name))?;
        if !valid_reference(&binding.owner)
            || !valid_reference(&binding.provider)
            || !valid_reference(&binding.model)
            || tool.name.is_empty()
            || !names.insert(tool.name.as_str())
        {
            return Err(format!(
                "invalid or duplicate admitted model tool {}",
                tool.name
            ));
        }
        aliases.push(ModelAlias {
            operation,
            tool: tool.name.clone(),
            binding: binding.clone(),
            input_schema: tool.schema.clone(),
            output_schema: tool.output_schema.clone(),
        });
    }
    Ok(aliases)
}

/// Resolve one alias to the exact admitted ordinary tool. No selection is
/// inferred from names, descriptions, model output, confidence or catalog order.
/// Arguments remain in the owner's schema; this function never injects routing,
/// credential, tenant, approval or cost fields into them.
pub fn resolve_model_call(
    tools: &[Tool],
    operation: ModelOperation,
    selector: &ModelSelector,
    args: Value,
) -> Result<ModelCall, String> {
    let aliases = admitted_models(tools)?;
    let matches = |alias: &&ModelAlias| {
        alias.operation == operation
            && selector
                .owner
                .as_ref()
                .is_none_or(|value| value == &alias.binding.owner)
            && selector
                .provider
                .as_ref()
                .is_none_or(|value| value == &alias.binding.provider)
            && selector
                .model
                .as_ref()
                .is_none_or(|value| value == &alias.binding.model)
    };
    let mut matching = aliases.iter().filter(matches);
    let selected = matching.next().ok_or_else(|| {
        format!(
            "models.{} is unavailable for the requested admitted model",
            operation.function_name()
        )
    })?;
    if matching.next().is_some() {
        return Err(format!(
            "models.{} is ambiguous; select an exact admitted owner/provider/model",
            operation.function_name()
        ));
    }
    Ok(ModelCall {
        name: selected.tool.clone(),
        args,
    })
}

#[cfg(test)]
#[path = "models_tests.rs"]
mod tests;

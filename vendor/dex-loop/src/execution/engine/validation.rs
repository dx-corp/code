//! Arguments checked against a tool's schema before a call runs: the
//! compiled validator, the corrections a schema rejection names, the argument
//! layer's refusal added to them, and the interpretations a failed call
//! reports.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use sha2::{Digest, Sha256};

use super::*;
use crate::Output;

/// Compiled argument schemas, keyed by the schema document. Arguments and
/// caller authority are checked on every call and are not stored. A full
/// cache returns a fresh validator without evicting an older schema.
const COMPILED_ARGUMENT_SCHEMAS: usize = 4096;

pub(super) fn argument_schema_cache()
-> &'static Mutex<HashMap<[u8; 32], Arc<jsonschema::Validator>>> {
    static CACHE: OnceLock<Mutex<HashMap<[u8; 32], Arc<jsonschema::Validator>>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(super) fn compiled_validator(
    schema: &serde_json::Value,
) -> Result<Arc<jsonschema::Validator>, String> {
    let bytes =
        serde_json::to_vec(schema).map_err(|error| format!("invalid tool schema: {error}"))?;
    let key: [u8; 32] = Sha256::digest(bytes).into();
    {
        let cache = argument_schema_cache()
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(validator) = cache.get(&key) {
            return Ok(Arc::clone(validator));
        }
    }
    let compiled = Arc::new(
        jsonschema::validator_for(schema)
            .map_err(|error| format!("invalid tool schema: {error}"))?,
    );
    let mut cache = argument_schema_cache()
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    if let Some(validator) = cache.get(&key) {
        return Ok(Arc::clone(validator));
    }
    if cache.len() < COMPILED_ARGUMENT_SCHEMAS {
        cache.insert(key, Arc::clone(&compiled));
    }
    Ok(compiled)
}

/// Validate boolean and object JSON schemas; malformed schemas fail closed.
pub fn validate_args(spec: &ToolSpec, args: &serde_json::Value) -> Result<(), String> {
    let validator = compiled_validator(&spec.schema)?;
    let corrections: Vec<String> = validator
        .iter_errors(args)
        .take(MAX_ARGUMENT_CORRECTIONS)
        .map(|error| argument_correction(&error))
        .collect();
    if corrections.is_empty() {
        return Ok(());
    }
    Err(format!("invalid arguments: {}", corrections.join("; ")))
}

/// How much of one schema location a log line repeats.
const MAX_RULE_CHARS: usize = 96;

/// The rules of the tool's schema that `args` broke, for the rejection log:
/// each as its location in the schema (`/properties/kind/enum`,
/// `/additionalProperties`), and for a missing field the field the schema
/// requires (`/required:action`). Every part comes from the tool's own
/// schema; nothing the model sent is repeated, so the line stays free of
/// arguments.
pub(super) fn broken_rules(spec: &ToolSpec, args: &serde_json::Value) -> String {
    let Ok(validator) = compiled_validator(&spec.schema) else {
        return "schema_invalid".into();
    };
    validator
        .iter_errors(args)
        .take(MAX_ARGUMENT_CORRECTIONS)
        .map(|error| {
            let mut rule: String = error
                .schema_path()
                .as_str()
                .chars()
                .take(MAX_RULE_CHARS)
                .collect();
            if let jsonschema::error::ValidationErrorKind::Required { property } = error.kind()
                && let Some(name) = property.as_str()
            {
                rule.push(':');
                rule.extend(name.chars().take(MAX_RULE_CHARS));
            }
            rule
        })
        .collect::<Vec<_>>()
        .join(",")
}

/// The argument layer's refusal of `args` (`crate::arguments`): the nearest
/// fields or values and a valid call built from the tool's schema, added to
/// a schema rejection so the model can correct the call in one step.
pub(super) fn arguments_refusal(spec: &ToolSpec, args: &serde_json::Value) -> Option<String> {
    let args = args.as_object()?;
    let refused = crate::arguments::normalize(&spec.schema, args).err()?;
    Some(format!(
        "{refused} A valid call built from its schema: {}",
        serde_json::Value::Object(refused.example.clone())
    ))
}

/// A failed call's message names what the engine interpreted in its
/// arguments, so the model corrects from what actually ran. A successful
/// output carries them from the tool host (`interpreted` in its result).
pub(super) fn with_interpreted(call: &ProposedCall, mut result: ToolResult) -> ToolResult {
    if call.interpreted.is_empty() || result.outcome != Outcome::Failed {
        return result;
    }
    if let Output::Text(text) = &mut result.output {
        text.push_str(&format!(
            " (This call ran as interpreted: {}.)",
            call.interpreted.join("; ")
        ));
    }
    result
}

/// Corrections returned for one call; the model fixes these and sees any others next.
const MAX_ARGUMENT_CORRECTIONS: usize = 5;
/// How much of a sent value a correction repeats.
const MAX_SENT_VALUE_CHARS: usize = 200;

fn json_type_name(value: &serde_json::Value) -> &'static str {
    match value {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "boolean",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
    }
}

/// What the model must change: the field by its path, then for a wrong type the
/// expected type and what it sent. Never coerces: the model sends the value again.
fn argument_correction(error: &jsonschema::ValidationError<'_>) -> String {
    let path = error
        .instance_path()
        .as_str()
        .trim_start_matches('/')
        .replace('/', ".");
    let field = if path.is_empty() {
        "arguments"
    } else {
        path.as_str()
    };
    if let jsonschema::error::ValidationErrorKind::Type { kind } = error.kind() {
        let sent = error.instance().to_string();
        let sent = if sent.chars().count() > MAX_SENT_VALUE_CHARS {
            format!(
                "{}...",
                sent.chars().take(MAX_SENT_VALUE_CHARS).collect::<String>()
            )
        } else {
            sent
        };
        let expected = match kind {
            jsonschema::error::TypeKind::Single(kind) => kind.to_string(),
            jsonschema::error::TypeKind::Multiple(kinds) => kinds
                .iter()
                .map(|kind| kind.to_string())
                .collect::<Vec<_>>()
                .join(" or "),
        };
        return format!(
            "{field}: expected {expected}, got {} {sent}",
            json_type_name(error.instance()),
        );
    }
    format!("{field}: {error}")
}

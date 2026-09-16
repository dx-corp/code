use serde_json::{Map, Value};
use std::fmt;

use crate::{Tool, ToolSchemaEnforcement};

const UNSUPPORTED_STRICT_SCHEMA_KEYS: &[&str] = &[
    "$ref",
    "$defs",
    "definitions",
    "allOf",
    "oneOf",
    "patternProperties",
    "dependentSchemas",
    "dependencies",
    "unevaluatedProperties",
    "propertyNames",
    "contains",
    "prefixItems",
    "not",
    "if",
    "then",
    "else",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StrictSchemaError(String);

impl StrictSchemaError {
    fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl fmt::Display for StrictSchemaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for StrictSchemaError {}

fn is_structured_schema(value: &Value) -> bool {
    let Some(object) = value.as_object() else {
        return false;
    };
    match object.get("type") {
        Some(Value::String(kind)) if matches!(kind.as_str(), "object" | "array") => true,
        Some(Value::Array(kinds))
            if kinds.iter().any(|kind| {
                kind.as_str()
                    .is_some_and(|kind| matches!(kind, "object" | "array"))
            }) =>
        {
            true
        }
        _ => object.contains_key("properties") || object.contains_key("items"),
    }
}

fn schema_allows_null(value: &Value) -> bool {
    let Some(object) = value.as_object() else {
        return false;
    };
    if object.get("type").is_some_and(|kind| {
        kind == "null"
            || kind
                .as_array()
                .is_some_and(|kinds| kinds.iter().any(|kind| kind == "null"))
    }) {
        return true;
    }
    if object.get("const").is_some_and(Value::is_null)
        || object
            .get("enum")
            .and_then(Value::as_array)
            .is_some_and(|values| values.iter().any(Value::is_null))
    {
        return true;
    }
    object
        .get("anyOf")
        .and_then(Value::as_array)
        .is_some_and(|variants| variants.iter().any(schema_allows_null))
}

fn make_node_strict(value: &mut Value) -> Result<(), StrictSchemaError> {
    let object = value
        .as_object_mut()
        .ok_or_else(|| StrictSchemaError::new("boolean schemas are unsupported"))?;

    if let Some(key) = UNSUPPORTED_STRICT_SCHEMA_KEYS
        .iter()
        .find(|key| object.contains_key(**key))
    {
        return Err(StrictSchemaError::new(format!(
            "{key} schemas are unsupported"
        )));
    }

    if let Some(any_of) = object.get_mut("anyOf") {
        let variants = any_of
            .as_array_mut()
            .filter(|variants| !variants.is_empty())
            .ok_or_else(|| StrictSchemaError::new("anyOf must contain at least one schema"))?;
        for variant in variants {
            if is_structured_schema(variant) {
                return Err(StrictSchemaError::new(
                    "object and array unions are unsupported",
                ));
            }
            make_node_strict(variant)?;
        }
    }

    if let Some(items) = object.get_mut("items") {
        if items.is_array() {
            return Err(StrictSchemaError::new("tuple schemas are unsupported"));
        }
        make_node_strict(items)?;
    }

    let is_object = object.get("type").and_then(Value::as_str) == Some("object");
    if object.contains_key("properties") && !is_object {
        return Err(StrictSchemaError::new("properties require type object"));
    }
    if !is_object {
        return Ok(());
    }

    if object
        .get("additionalProperties")
        .is_some_and(|value| value != &Value::Bool(false))
    {
        return Err(StrictSchemaError::new(
            "schema-valued or true additionalProperties is unsupported",
        ));
    }

    let mut properties = match object.remove("properties") {
        Some(Value::Object(properties)) => properties,
        Some(_) => {
            return Err(StrictSchemaError::new(
                "object properties must be a schema map",
            ));
        }
        None => Map::new(),
    };
    let property_names = properties.keys().cloned().collect::<Vec<_>>();
    let required = match object.get("required") {
        Some(Value::Array(required)) => required
            .iter()
            .map(|name| {
                name.as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| StrictSchemaError::new("object required must be a string array"))
            })
            .collect::<Result<Vec<_>, _>>()?,
        Some(_) => {
            return Err(StrictSchemaError::new(
                "object required must be a string array",
            ));
        }
        None => Vec::new(),
    };
    if required.iter().any(|name| !properties.contains_key(name)) {
        return Err(StrictSchemaError::new(
            "required contains an unknown property",
        ));
    }

    for (name, property) in &mut properties {
        let was_required = required.iter().any(|required| required == name);
        let allowed_null = schema_allows_null(property);
        make_node_strict(property)?;
        if !was_required && !allowed_null {
            let original = std::mem::take(property);
            *property = serde_json::json!({
                "anyOf": [original, {"type": "null"}]
            });
        }
    }

    object.insert("properties".to_string(), Value::Object(properties));
    object.insert(
        "required".to_string(),
        Value::Array(property_names.into_iter().map(Value::String).collect()),
    );
    object.insert("additionalProperties".to_string(), Value::Bool(false));
    Ok(())
}

/// Convert a tool schema to the strict subset supported by constrained sampling.
pub(crate) fn strict_json_schema(schema: &Value) -> Result<Value, StrictSchemaError> {
    let mut strict = schema.clone();
    make_node_strict(&mut strict)?;
    if strict.get("type").and_then(Value::as_str) != Some("object") {
        return Err(StrictSchemaError::new("root schema must have type object"));
    }
    Ok(strict)
}

/// Resolve the strict schema to send for one tool and provider capability.
pub(crate) fn resolve_strict_json_schema(
    tool: &Tool,
    supports_strict: bool,
) -> Result<Option<Value>, StrictSchemaError> {
    match (tool.schema_enforcement, supports_strict) {
        (ToolSchemaEnforcement::Off, _) | (ToolSchemaEnforcement::Prefer, false) => Ok(None),
        (ToolSchemaEnforcement::Require, false) => Err(StrictSchemaError::new(format!(
            "tool {:?} requires JSON-schema constrained sampling, but strict tools are unsupported",
            tool.name
        ))),
        (ToolSchemaEnforcement::Prefer, true) => match strict_json_schema(&tool.input_schema) {
            Ok(schema) => Ok(Some(schema)),
            Err(error) => {
                tracing::debug!(
                    tool = %tool.name,
                    reason = %error,
                    "strict tool schema is unsupported; using the original schema"
                );
                Ok(None)
            }
        },
        (ToolSchemaEnforcement::Require, true) => strict_json_schema(&tool.input_schema)
            .map(Some)
            .map_err(|error| {
                StrictSchemaError::new(format!(
                    "tool {:?} requires JSON-schema constrained sampling, but {error}",
                    tool.name
                ))
            }),
    }
}

#[cfg(test)]
mod tests {
    use super::{resolve_strict_json_schema, strict_json_schema};
    use crate::{Tool, ToolSchemaEnforcement};
    use serde_json::json;

    #[test]
    fn constrained_sampling_converts_optional_properties_without_mutating_input() {
        let schema = json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "limit": {"type": "integer"}
            },
            "required": ["path"]
        });
        let original = schema.clone();

        assert_eq!(
            strict_json_schema(&schema).expect("compatible schema"),
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "limit": {"anyOf": [{"type": "integer"}, {"type": "null"}]}
                },
                "required": ["limit", "path"],
                "additionalProperties": false
            })
        );
        assert_eq!(schema, original, "conversion must not mutate caller input");
    }

    #[test]
    fn constrained_sampling_rejects_unsupported_schema_shapes() {
        let cases = [
            ("boolean root", json!(true)),
            ("non-object root", json!({"type": "string"})),
            (
                "reference",
                json!({"type": "object", "properties": {"x": {"$ref": "#/$defs/x"}}}),
            ),
            (
                "definition",
                json!({"type": "object", "$defs": {"x": {"type": "string"}}}),
            ),
            (
                "oneOf",
                json!({"type": "object", "properties": {"x": {"oneOf": [{"type": "string"}]}}}),
            ),
            (
                "structured anyOf",
                json!({"type": "object", "properties": {"x": {"anyOf": [{"type": "object"}, {"type": "null"}]}}}),
            ),
            (
                "tuple items",
                json!({"type": "object", "properties": {"x": {"type": "array", "items": [{"type": "string"}]}}}),
            ),
            (
                "unknown required",
                json!({"type": "object", "properties": {}, "required": ["missing"]}),
            ),
            (
                "open additional properties",
                json!({"type": "object", "properties": {}, "additionalProperties": true}),
            ),
        ];

        for (name, schema) in cases {
            assert!(
                strict_json_schema(&schema).is_err(),
                "{name} must be rejected"
            );
        }
    }

    #[test]
    fn constrained_sampling_recurses_into_arrays_and_scalar_unions() {
        let schema = json!({
            "type": "object",
            "properties": {
                "values": {
                    "type": "array",
                    "items": {
                        "anyOf": [
                            {"type": "string"},
                            {"type": "null"}
                        ]
                    }
                }
            },
            "required": ["values"]
        });

        let strict = strict_json_schema(&schema).expect("compatible array schema");
        assert_eq!(strict["additionalProperties"], false);
        assert_eq!(
            strict["properties"]["values"]["items"],
            schema["properties"]["values"]["items"]
        );
    }

    #[test]
    fn constrained_sampling_respects_provider_support_and_required_mode() {
        let preferred =
            Tool::new("read", "Read").with_schema_enforcement(ToolSchemaEnforcement::Prefer);
        assert_eq!(resolve_strict_json_schema(&preferred, false).unwrap(), None);

        let required =
            Tool::new("write", "Write").with_schema_enforcement(ToolSchemaEnforcement::Require);
        let error = resolve_strict_json_schema(&required, false)
            .expect_err("required strict sampling must fail when unsupported");
        assert!(error.to_string().contains("write"));
        assert!(error.to_string().contains("unsupported"));
    }
}

//! Model-facing declarations describe admitted tools; they confer no authority.
use crate::Tool;
use serde_json::Value;

const MAX_DECLARATION_BYTES: usize = 16_000;

pub(crate) fn identifier(name: &str) -> String {
    let mut id: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '$' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if id.as_bytes().first().is_none_or(u8::is_ascii_digit) {
        id.insert(0, '_');
    }
    id
}

pub fn namespace(tool: &Tool) -> Option<String> {
    tool.namespace
        .clone()
        .or_else(|| tool.name.rsplit_once("__").map(|(ns, _)| ns.to_owned()))
        .or_else(|| tool.name.split_once('.').map(|(ns, _)| ns.to_owned()))
}

pub fn describe_tool(tool: &Tool) -> String {
    format!("declare const tools: {{\n{}\n}};", declaration_member(tool))
}

fn declaration_member(tool: &Tool) -> String {
    let input = render_type(&tool.schema);
    let output = tool
        .output_schema
        .as_ref()
        .map(render_type)
        .unwrap_or_else(|| "unknown".into());
    let description = tool
        .description
        .replace("*/", "* /")
        .replace(['\r', '\n'], " ");
    format!(
        "/** {description} */\n{}(args: {input}): Promise<{output}>;",
        serde_json::to_string(&identifier(&tool.name)).expect("string serialization")
    )
}

/// Keep a stable bounded prompt; omitted tools remain discoverable only inside
/// the catalog this host admitted. Never introduce a new tool through search.
pub fn declaration_description(tools: &[Tool], token_budget: usize) -> String {
    let max_bytes = token_budget.min(16_384).saturating_mul(4);
    let prefix = "declare const tools: {\n";
    let suffix = "\n};";
    if max_bytes < prefix.len() + suffix.len() {
        return String::new();
    }
    let mut remaining = max_bytes - prefix.len() - suffix.len();
    let mut groups = std::collections::BTreeMap::<String, Vec<(String, String)>>::new();
    for tool in tools {
        groups
            .entry(namespace(tool).unwrap_or_default())
            .or_default()
            .push((tool.name.clone(), declaration_member(tool)));
    }
    for group in groups.values_mut() {
        group.sort_by(|a, b| a.1.len().cmp(&b.1.len()).then(a.0.cmp(&b.0)));
        group.reverse();
    }
    let mut selected = Vec::new();
    loop {
        let mut progress = false;
        for group in groups.values_mut() {
            if let Some((_, text)) = group.pop() {
                let cost = text.len().saturating_add(2);
                if cost <= remaining {
                    remaining -= cost;
                    selected.push(text);
                }
                progress = true;
            }
        }
        if !progress {
            break;
        }
    }
    if selected.is_empty() {
        String::new()
    } else {
        format!("{prefix}{}{suffix}", selected.join("\n\n"))
    }
}

pub(crate) fn lookup<'a>(tools: &'a [Tool], name: &str) -> Option<&'a Tool> {
    tools
        .iter()
        .find(|tool| tool.name == name || identifier(&tool.name) == name)
}

pub(crate) fn search(
    tools: &[Tool],
    query: &str,
    limit: usize,
    requested_namespace: Option<&str>,
) -> Vec<Value> {
    let words: Vec<String> = query
        .split(|c: char| !c.is_alphanumeric())
        .filter(|s| !s.is_empty())
        .map(str::to_lowercase)
        .collect();
    let mut matches: Vec<_> = tools
        .iter()
        .filter(|tool| {
            requested_namespace.is_none_or(|wanted| namespace(tool).as_deref() == Some(wanted))
        })
        .filter_map(|tool| {
            let name = tool.name.to_lowercase();
            let description = tool.description.to_lowercase();
            let score: usize = words
                .iter()
                .map(|word| {
                    usize::from(name.contains(word)) * 6
                        + usize::from(description.contains(word)) * 2
                })
                .sum();
            (score > 0).then_some((score, tool))
        })
        .collect();
    matches.sort_by(|(a, ta), (b, tb)| b.cmp(a).then(ta.name.cmp(&tb.name)));
    matches.into_iter().take(limit).map(|(_,tool)|serde_json::json!({"name":identifier(&tool.name),"description":tool.description,"namespace":namespace(tool)})).collect()
}

pub fn render_type(schema: &Value) -> String {
    let mut expansions = 0;
    let rendered = render(schema, schema, 0, &mut expansions);
    if rendered.len() > MAX_DECLARATION_BYTES {
        "unknown".into()
    } else {
        rendered
    }
}
fn render(schema: &Value, root: &Value, depth: usize, expansions: &mut usize) -> String {
    if depth >= 12 || *expansions >= 32 {
        return "unknown".into();
    }
    if let Some(reference) = schema.get("$ref").and_then(Value::as_str) {
        *expansions += 1;
        return reference
            .strip_prefix('#')
            .and_then(|p| root.pointer(p))
            .map(|value| render(value, root, depth + 1, expansions))
            .unwrap_or_else(|| "unknown".into());
    }
    if let Some(value) = schema.get("const") {
        return literal(value);
    }
    if let Some(values) = schema.get("enum").and_then(Value::as_array) {
        if values.is_empty() || values.len() > 64 {
            return "unknown".into();
        }
        return values.iter().map(literal).collect::<Vec<_>>().join(" | ");
    }
    for (key, join) in [("anyOf", " | "), ("oneOf", " | "), ("allOf", " & ")] {
        if let Some(values) = schema.get(key).and_then(Value::as_array) {
            if values.is_empty() || values.len() > 32 {
                return "unknown".into();
            }
            return format!(
                "({})",
                values
                    .iter()
                    .map(|v| render(v, root, depth + 1, expansions))
                    .collect::<Vec<_>>()
                    .join(join)
            );
        }
    }
    if let Some(types) = schema.get("type").and_then(Value::as_array) {
        return types
            .iter()
            .map(|t| {
                let mut value = schema.clone();
                value["type"] = t.clone();
                render(&value, root, depth + 1, expansions)
            })
            .collect::<Vec<_>>()
            .join(" | ");
    }
    match schema.get("type").and_then(Value::as_str) {
        Some("null") => "null".into(),
        Some("boolean") => "boolean".into(),
        Some("integer" | "number") => "number".into(),
        Some("string") => "string".into(),
        Some("array") => format!(
            "Array<{}>",
            schema
                .get("items")
                .map(|v| render(v, root, depth + 1, expansions))
                .unwrap_or_else(|| "unknown".into())
        ),
        Some("object") => {
            let Some(properties) = schema.get("properties").and_then(Value::as_object) else {
                return "Record<string, unknown>".into();
            };
            if properties.len() > 64 {
                return "Record<string, unknown>".into();
            }
            let required = schema.get("required").and_then(Value::as_array);
            let mut fields: Vec<String> = properties
                .iter()
                .map(|(key, value)| {
                    format!(
                        "{}{}: {};",
                        serde_json::to_string(key).expect("string serialization"),
                        if required.is_some_and(|r| r.iter().any(|v| v.as_str() == Some(key))) {
                            ""
                        } else {
                            "?"
                        },
                        render(value, root, depth + 1, expansions)
                    )
                })
                .collect();
            if schema.get("additionalProperties") != Some(&Value::Bool(false)) {
                fields.push("[key: string]: unknown;".into());
            }
            format!("{{ {} }}", fields.join(" "))
        }
        _ => "unknown".into(),
    }
}
fn literal(value: &Value) -> String {
    match value {
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => value.to_string(),
        _ => "unknown".into(),
    }
}

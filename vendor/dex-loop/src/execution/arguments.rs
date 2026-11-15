//! Arguments checked against the JSON Schema their tool or read declares,
//! before the call runs. One layer for every catalog read and operation Dex
//! reaches through its six tools; the schema is the only input, so nothing
//! here is written per tool, read, field or enum.
//!
//! - **Obvious variants are accepted and recorded.** An enum value in another
//!   case or without its type prefix (`failing`, `FAILING`,
//!   `framework_coverage_state_failing` for
//!   `FRAMEWORK_COVERAGE_STATE_FAILING`), a field named in camelCase or in the
//!   other number (`controlIds`, `state` for `states`), one value where a list
//!   is declared, a number sent as text, and an ISO 8601 date or date-time
//!   that is not in RFC 3339 form. Each becomes an [`Interpretation`] the
//!   caller returns with the result, so nothing changes silently.
//! - **Nothing else is guessed.** An unknown field, an unknown or ambiguous
//!   value and a scalar where an object is declared are each a [`Problem`]
//!   naming the nearest valid fields or values (edit distance), with the
//!   declared descriptions, and [`ArgumentsInvalid`] carries a corrected
//!   example built from the schema. Unknown fields are never dropped.
//! - **One unambiguous correction may be retried.** When every problem has
//!   exactly one candidate, [`ArgumentsInvalid::corrected`] is the call with
//!   each candidate in place, recorded as [`Rule::Suggested`]; the caller runs
//!   it once and otherwise returns the refusal to the model.
//!
//! Fields are checked only where the schema closes its object
//! (`additionalProperties: false`); an open object keeps what it was sent.
//! This layer does not replace validation: the caller still validates the
//! normalized arguments against the same schema.
//!
//! The coercion rule is model-checked with Z3 over every enum and field set
//! the page reads declare (`docs/formal/param_coercion`); change the model
//! with `resolve`, `shared_prefix`, `same_field` or `plural`.
//!
//! Edit distance is plain Levenshtein, written here rather than taken from
//! `strsim`: a direct registry dependency needs a crate_universe repin of
//! `bazel/crates.lock`, for twenty lines of code.

use std::fmt;

use serde_json::{Map, Value, json};

/// Proto enums' zero value: declared, never a value a caller means.
const PLACEHOLDER_SUFFIX: &str = "_UNSPECIFIED";
/// Values or fields listed in one refusal.
const MAX_LISTED: usize = 24;
/// How much of a description or sent value a refusal repeats.
const MAX_DESCRIPTION_CHARS: usize = 140;
const MAX_SENT_CHARS: usize = 80;

/// Which variant an [`Interpretation`] accepted.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Rule {
    /// An enum value in another case or without its type prefix.
    EnumName,
    /// A field named in camelCase, kebab-case or the other number.
    FieldName,
    /// One value where the schema declares a list.
    OneAsList,
    /// A number sent as text.
    NumberText,
    /// An ISO 8601 date or date-time rewritten in RFC 3339 form.
    DateTime,
    /// Not an obvious variant: a value's single nearest candidate, used
    /// only by [`ArgumentsInvalid::corrected`].
    Suggested,
    /// Not an obvious variant: an unknown field's single nearest field,
    /// used only by [`ArgumentsInvalid::corrected`].
    SuggestedField,
    /// A field the routing wrapper offers for another route, which the
    /// tool this call routes to does not declare: left out ([`route`]).
    Dropped,
}

impl Rule {
    /// A stable label for logs and metrics.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::EnumName => "enum_name",
            Self::FieldName => "field_name",
            Self::OneAsList => "one_as_list",
            Self::NumberText => "number_text",
            Self::DateTime => "date_time",
            Self::Suggested => "suggested",
            Self::SuggestedField => "suggested_field",
            Self::Dropped => "dropped",
        }
    }
}

/// One accepted variant: what was sent at `path` and what is used instead.
#[derive(Clone, Debug, PartialEq)]
pub struct Interpretation {
    /// Field path, `a.b` for nested fields and `a[1]` for list items.
    pub path: String,
    pub sent: Value,
    pub used: Value,
    pub rule: Rule,
}

impl fmt::Display for Interpretation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.rule {
            Rule::FieldName => write!(
                f,
                "interpreted field {} as {}",
                short(&self.sent),
                short(&self.used)
            ),
            Rule::Dropped => write!(
                f,
                "left out {} {}: the tool this call routes to does not take it",
                self.path,
                short(&self.sent)
            ),
            Rule::SuggestedField => write!(
                f,
                "corrected field {} to {}",
                short(&self.sent),
                short(&self.used)
            ),
            Rule::Suggested => write!(
                f,
                "corrected {} {} to {}",
                self.path,
                short(&self.sent),
                short(&self.used)
            ),
            _ => write!(
                f,
                "interpreted {} {} as {}",
                self.path,
                short(&self.sent),
                short(&self.used)
            ),
        }
    }
}

/// Arguments the schema accepts, with every variant that was interpreted.
#[derive(Clone, Debug, PartialEq)]
pub struct Normalized {
    pub args: Map<String, Value>,
    pub interpreted: Vec<Interpretation>,
}

/// A declared field, for a refusal to list.
#[derive(Clone, Debug, PartialEq)]
pub struct Field {
    pub name: String,
    pub description: Option<String>,
}

/// Why one argument was refused.
#[derive(Clone, Debug, PartialEq)]
pub enum Problem {
    /// `sent` is not a field of the object at `path`.
    UnknownField {
        path: String,
        sent: String,
        /// The nearest declared fields; more than one is ambiguous.
        candidates: Vec<Field>,
        /// Every declared field of the object.
        fields: Vec<String>,
    },
    /// `sent` is not one of the field's values.
    UnknownValue {
        path: String,
        sent: Value,
        /// The values `sent` most likely means; more than one is ambiguous.
        candidates: Vec<String>,
        accepted: Vec<String>,
        description: Option<String>,
    },
    /// `sent` is not of the declared type.
    WrongType {
        path: String,
        sent: Value,
        expected: String,
    },
}

impl Problem {
    /// Whether the problem has exactly one candidate.
    fn single(&self) -> bool {
        match self {
            Self::UnknownField { candidates, .. } => candidates.len() == 1,
            Self::UnknownValue { candidates, .. } => candidates.len() == 1,
            Self::WrongType { .. } => false,
        }
    }
}

impl fmt::Display for Problem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownField {
                path,
                sent,
                candidates,
                fields,
            } => {
                write!(f, "{path} is not a field here.")?;
                if sent != path {
                    write!(f, " (sent as {sent})")?;
                }
                match candidates.as_slice() {
                    [] => write!(f, " Its fields are: {}.", listed(fields)),
                    [one] => write!(f, " Did you mean {}?", field_line(one)),
                    many => write!(
                        f,
                        " It could mean {}; name the one you mean.",
                        many.iter().map(field_line).collect::<Vec<_>>().join(" or ")
                    ),
                }
            }
            Self::UnknownValue {
                path,
                sent,
                candidates,
                accepted,
                description,
            } => {
                write!(f, "{path} does not accept {}.", short(sent))?;
                match candidates.as_slice() {
                    [] => {}
                    [one] => write!(f, " Did you mean {one}?")?,
                    many => write!(
                        f,
                        " It could mean {}; name the one you mean.",
                        many.join(" or ")
                    )?,
                }
                write!(
                    f,
                    " It takes only these exact values: {}.",
                    listed(accepted)
                )?;
                if let Some(description) = description {
                    write!(f, " ({description})")?;
                }
                Ok(())
            }
            Self::WrongType {
                path,
                sent,
                expected,
            } => write!(f, "{path} takes {expected}, not {}.", kind(sent)),
        }
    }
}

/// Arguments the schema refuses: each problem, and the example call built
/// from the schema.
#[derive(Clone, Debug, PartialEq)]
pub struct ArgumentsInvalid {
    pub problems: Vec<Problem>,
    /// The arguments with every variant interpreted, each problem replaced
    /// by its nearest candidate (or a value the schema declares) and unknown
    /// fields without a candidate left out. The schema accepts it.
    pub example: Map<String, Value>,
    interpreted: Vec<Interpretation>,
    suggested: Vec<Interpretation>,
}

impl ArgumentsInvalid {
    /// The call to retry once: `Some` only when every problem has exactly
    /// one candidate, with each correction recorded as [`Rule::Suggested`].
    #[must_use]
    pub fn corrected(&self) -> Option<Normalized> {
        if !self.problems.iter().all(Problem::single) {
            return None;
        }
        let mut interpreted = self.interpreted.clone();
        interpreted.extend(self.suggested.iter().cloned());
        Some(Normalized {
            args: self.example.clone(),
            interpreted,
        })
    }
}

impl fmt::Display for ArgumentsInvalid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let problems: Vec<String> = self.problems.iter().map(ToString::to_string).collect();
        f.write_str(&problems.join(" "))
    }
}

impl std::error::Error for ArgumentsInvalid {}

/// `args` checked against `schema`, an object JSON Schema: the arguments with
/// obvious variants accepted, or why they are refused.
pub fn normalize(
    schema: &Value,
    args: &Map<String, Value>,
) -> Result<Normalized, ArgumentsInvalid> {
    let mut walk = Walk::default();
    let args = walk.object(schema, args, "");
    if walk.problems.is_empty() {
        return Ok(Normalized {
            args,
            interpreted: walk.interpreted,
        });
    }
    Err(ArgumentsInvalid {
        problems: walk.problems,
        example: args,
        interpreted: walk.interpreted,
        suggested: walk.suggested,
    })
}

/// `args` of a call a wrapper routed to `target`. A wrapper declares the
/// union of its routes' fields (`offered`), so a call may carry fields only
/// another route takes: each field `offered` declares that a closed `target`
/// does not declare, in any name variant, is left out and recorded as
/// [`Rule::Dropped`]. The rest is [`normalize`]d against `target`; a field
/// neither schema declares is refused or corrected there, never dropped.
pub fn route(
    target: &Value,
    offered: &Value,
    args: &Map<String, Value>,
) -> Result<Normalized, ArgumentsInvalid> {
    let declared = target.get("properties").and_then(Value::as_object);
    let closed = target.get("additionalProperties") == Some(&Value::Bool(false));
    let offered = offered.get("properties").and_then(Value::as_object);
    let mut kept = Map::new();
    let mut dropped = Vec::new();
    for (field, value) in args {
        let other_route = closed
            && offered.is_some_and(|offered| offered.contains_key(field))
            && declared.is_some_and(|declared| {
                !declared.contains_key(field)
                    && !declared.keys().any(|name| same_field(field, name))
            });
        if other_route {
            dropped.push(Interpretation {
                path: field.clone(),
                sent: value.clone(),
                used: Value::Null,
                rule: Rule::Dropped,
            });
        } else {
            kept.insert(field.clone(), value.clone());
        }
    }
    let prepend = |interpreted: &mut Vec<Interpretation>| {
        interpreted.splice(0..0, dropped.iter().cloned());
    };
    let mut result = normalize(target, &kept);
    match &mut result {
        Ok(normalized) => prepend(&mut normalized.interpreted),
        Err(refused) => prepend(&mut refused.interpreted),
    }
    result
}

#[derive(Default)]
struct Walk {
    interpreted: Vec<Interpretation>,
    suggested: Vec<Interpretation>,
    problems: Vec<Problem>,
}

impl Walk {
    fn object(
        &mut self,
        schema: &Value,
        args: &Map<String, Value>,
        prefix: &str,
    ) -> Map<String, Value> {
        let Some(properties) = schema.get("properties").and_then(Value::as_object) else {
            return args.clone();
        };
        let closed = schema.get("additionalProperties") == Some(&Value::Bool(false));
        let mut out = Map::new();
        for (sent, value) in args {
            let name = if properties.contains_key(sent) {
                sent.clone()
            } else if !closed {
                out.insert(sent.clone(), value.clone());
                continue;
            } else {
                match self.field(properties, args, sent, prefix) {
                    Some(name) => name,
                    None => continue,
                }
            };
            let path = join(prefix, &name);
            if out.contains_key(&name) {
                // Two names sent for one field: which one is meant is the
                // caller's call.
                self.problems.push(Problem::UnknownField {
                    path: join(prefix, sent),
                    sent: sent.clone(),
                    candidates: Vec::new(),
                    fields: properties.keys().cloned().collect(),
                });
                continue;
            }
            let Some(field) = properties.get(&name) else {
                continue;
            };
            let value = self.value(field, value, &path);
            out.insert(name, value);
        }
        out
    }

    /// The declared field `sent` names, or `None` with the problem recorded.
    fn field(
        &mut self,
        properties: &Map<String, Value>,
        args: &Map<String, Value>,
        sent: &str,
        prefix: &str,
    ) -> Option<String> {
        let path = join(prefix, sent);
        let variants: Vec<&String> = properties
            .keys()
            .filter(|name| same_field(sent, name))
            .collect();
        if let [one] = variants.as_slice()
            && !args.contains_key(one.as_str())
        {
            self.interpreted.push(Interpretation {
                path: path.clone(),
                sent: json!(sent),
                used: json!(one),
                rule: Rule::FieldName,
            });
            return Some((*one).clone());
        }
        let names: Vec<&String> = if variants.is_empty() {
            nearest(&snake(sent), properties.keys().map(String::as_str))
                .into_iter()
                .filter_map(|name| properties.get_key_value(name).map(|(key, _)| key))
                .collect()
        } else {
            variants
        };
        let candidates: Vec<Field> = names
            .into_iter()
            .filter(|name| !args.contains_key(name.as_str()))
            .map(|name| Field {
                name: name.clone(),
                description: properties.get(name).and_then(description),
            })
            .collect();
        let chosen = match candidates.as_slice() {
            [one] => {
                self.suggested.push(Interpretation {
                    path: path.clone(),
                    sent: json!(sent),
                    used: json!(one.name),
                    rule: Rule::SuggestedField,
                });
                Some(one.name.clone())
            }
            _ => None,
        };
        self.problems.push(Problem::UnknownField {
            path,
            sent: sent.to_owned(),
            candidates,
            fields: properties.keys().cloned().collect(),
        });
        chosen
    }

    fn value(&mut self, schema: &Value, value: &Value, path: &str) -> Value {
        if let Some(accepted) = enum_values(schema) {
            return self.enum_value(schema, &accepted, value, path);
        }
        if has_type(schema, "array") {
            let (items, one) = match value {
                Value::Array(items) => (items.clone(), false),
                Value::Null => return Value::Null,
                one => (vec![one.clone()], true),
            };
            let items: Vec<Value> = items
                .iter()
                .enumerate()
                .map(|(index, item)| match schema.get("items") {
                    Some(items) => self.value(items, item, &format!("{path}[{index}]")),
                    None => item.clone(),
                })
                .collect();
            let list = Value::Array(items);
            if one {
                self.interpreted.push(Interpretation {
                    path: path.to_owned(),
                    sent: value.clone(),
                    used: list.clone(),
                    rule: Rule::OneAsList,
                });
            }
            return list;
        }
        if let Some(fields) = schema.get("properties").and_then(Value::as_object) {
            return match value {
                Value::Object(object) => Value::Object(self.object(schema, object, path)),
                Value::Null => Value::Null,
                other => {
                    let names: Vec<String> = fields.keys().cloned().collect();
                    self.problems.push(Problem::WrongType {
                        path: path.to_owned(),
                        sent: other.clone(),
                        expected: format!("an object with these fields: {}", listed(&names)),
                    });
                    json!({})
                }
            };
        }
        if let Value::String(text) = value {
            if has_type(schema, "integer") {
                return match text.trim().parse::<i64>() {
                    Ok(number) => self.interpret(path, value, json!(number), Rule::NumberText),
                    Err(_) => self.wrong_type(path, value, "an integer", json!(1)),
                };
            }
            if has_type(schema, "number") {
                let parsed = text
                    .trim()
                    .parse::<f64>()
                    .ok()
                    .and_then(serde_json::Number::from_f64);
                return match parsed {
                    Some(number) => {
                        self.interpret(path, value, Value::Number(number), Rule::NumberText)
                    }
                    None => self.wrong_type(path, value, "a number", json!(1)),
                };
            }
            if schema.get("format").and_then(Value::as_str) == Some("date-time") {
                return match rfc3339(text) {
                    Some(normal) if normal == *text => value.clone(),
                    Some(normal) => self.interpret(path, value, json!(normal), Rule::DateTime),
                    None => self.wrong_type(
                        path,
                        value,
                        "an RFC 3339 date-time such as 2026-01-31T00:00:00Z",
                        json!("1970-01-01T00:00:00Z"),
                    ),
                };
            }
        }
        value.clone()
    }

    fn enum_value(
        &mut self,
        schema: &Value,
        accepted: &[String],
        value: &Value,
        path: &str,
    ) -> Value {
        let text = match value {
            Value::String(text) => text,
            // Proto JSON also takes an enum by its number; validation checks it.
            Value::Number(_) | Value::Null => return value.clone(),
            other => {
                self.unknown_value(schema, accepted, other, path, Vec::new());
                return accepted.first().map_or(Value::Null, |first| json!(first));
            }
        };
        if accepted.iter().any(|value| value == text) {
            return value.clone();
        }
        match resolve(text, accepted) {
            Resolution::Exact(found) => self.interpret(path, value, json!(found), Rule::EnumName),
            Resolution::Near(candidates) => {
                let example = candidates
                    .first()
                    .or(accepted.first())
                    .map_or(Value::Null, |first| json!(first));
                if let [one] = candidates.as_slice() {
                    self.suggested.push(Interpretation {
                        path: path.to_owned(),
                        sent: value.clone(),
                        used: json!(one),
                        rule: Rule::Suggested,
                    });
                }
                self.unknown_value(schema, accepted, value, path, candidates);
                example
            }
        }
    }

    fn unknown_value(
        &mut self,
        schema: &Value,
        accepted: &[String],
        sent: &Value,
        path: &str,
        candidates: Vec<String>,
    ) {
        self.problems.push(Problem::UnknownValue {
            path: path.to_owned(),
            sent: sent.clone(),
            candidates,
            accepted: accepted.to_vec(),
            description: description(schema),
        });
    }

    fn interpret(&mut self, path: &str, sent: &Value, used: Value, rule: Rule) -> Value {
        self.interpreted.push(Interpretation {
            path: path.to_owned(),
            sent: sent.clone(),
            used: used.clone(),
            rule,
        });
        used
    }

    fn wrong_type(&mut self, path: &str, sent: &Value, expected: &str, example: Value) -> Value {
        self.problems.push(Problem::WrongType {
            path: path.to_owned(),
            sent: sent.clone(),
            expected: expected.to_owned(),
        });
        example
    }
}

enum Resolution {
    /// The value an obvious variant names.
    Exact(String),
    /// The values it most likely means: none, one, or ambiguous.
    Near(Vec<String>),
}

/// The accepted value `text` names. Exact: the same words in any case,
/// with or without the values' shared type prefix. Near: a value of another
/// enum of the same kind (`CONTROL_TEST_STATE_FAILING` for
/// `FRAMEWORK_COVERAGE_STATE_FAILING`: the sent words from the prefix's last
/// word on), else the values at the least edit distance within a quarter of
/// the value's length.
fn resolve(text: &str, accepted: &[String]) -> Resolution {
    let key = words(text);
    let prefix = shared_prefix(accepted);
    let exact: Vec<&String> = accepted
        .iter()
        .filter(|value| {
            let value = words(value);
            value == key || (!prefix.is_empty() && value == format!("{prefix}{key}"))
        })
        .collect();
    if let [one] = exact.as_slice() {
        return Resolution::Exact((*one).clone());
    }
    if !exact.is_empty() {
        return Resolution::Near(exact.into_iter().cloned().collect());
    }
    if let Some(kind) = prefix
        .trim_end_matches('_')
        .rsplit('_')
        .next()
        .filter(|kind| !kind.is_empty())
    {
        let parts: Vec<&str> = key.split('_').collect();
        if let Some(start) = parts.iter().rposition(|part| *part == kind) {
            let tail = format!("{}{}", &prefix, parts[start + 1..].join("_"));
            let renamed: Vec<String> = accepted
                .iter()
                .filter(|value| words(value) == tail)
                .cloned()
                .collect();
            if !renamed.is_empty() {
                return Resolution::Near(renamed);
            }
        }
    }
    let bare = key.strip_prefix(prefix.as_str()).unwrap_or(&key).to_owned();
    let stripped: Vec<String> = accepted
        .iter()
        .map(|value| {
            let value = words(value);
            value
                .strip_prefix(prefix.as_str())
                .unwrap_or(&value)
                .to_owned()
        })
        .collect();
    let near = nearest(&bare, stripped.iter().map(String::as_str));
    Resolution::Near(
        accepted
            .iter()
            .zip(&stripped)
            .filter(|(_, stripped)| near.contains(&stripped.as_str()))
            .map(|(value, _)| value.clone())
            .collect(),
    )
}

/// `text` as upper-case words joined by `_`.
fn words(text: &str) -> String {
    text.trim()
        .split(|c: char| c == '_' || c == '-' || c.is_whitespace())
        .filter(|part| !part.is_empty())
        .map(str::to_uppercase)
        .collect::<Vec<_>>()
        .join("_")
}

/// The `_`-separated words every value starts with, with the trailing `_`,
/// when at least two values share them and each has words after them.
fn shared_prefix(accepted: &[String]) -> String {
    let split: Vec<Vec<String>> = accepted
        .iter()
        .map(|value| words(value).split('_').map(str::to_owned).collect())
        .collect();
    let (Some(first), true) = (split.first(), split.len() >= 2) else {
        return String::new();
    };
    let mut shared = 0;
    while shared < first.len()
        && split
            .iter()
            .all(|parts| parts.len() > shared + 1 && parts[shared] == first[shared])
    {
        shared += 1;
    }
    if shared == 0 {
        return String::new();
    }
    format!("{}_", first[..shared].join("_"))
}

/// The candidates at the least edit distance from `sent`, when that
/// distance is within a quarter of the candidate's length (at least one).
fn nearest<'a>(sent: &str, candidates: impl Iterator<Item = &'a str>) -> Vec<&'a str> {
    let scored: Vec<(usize, &str)> = candidates
        .map(|candidate| (levenshtein(sent, candidate), candidate))
        .filter(|(distance, candidate)| *distance <= (candidate.chars().count() / 4).max(1))
        .collect();
    let Some(best) = scored.iter().map(|(distance, _)| *distance).min() else {
        return Vec::new();
    };
    scored
        .into_iter()
        .filter(|(distance, _)| *distance == best)
        .map(|(_, candidate)| candidate)
        .collect()
}

/// Character edits (insert, delete, substitute) between `a` and `b`.
fn levenshtein(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut diagonal = row[0];
        row[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let above = row[j + 1];
            row[j + 1] = if ca == *cb {
                diagonal
            } else {
                1 + diagonal.min(above).min(row[j])
            };
            diagonal = above;
        }
    }
    row[b.len()]
}

/// `name` in snake_case: `controlIds` and `control-ids` are `control_ids`.
fn snake(name: &str) -> String {
    let mut out = String::with_capacity(name.len() + 4);
    for c in name.trim().chars() {
        if c == '-' || c.is_whitespace() {
            out.push('_');
        } else if c.is_uppercase() {
            if !out.is_empty() && !out.ends_with('_') {
                out.push('_');
            }
            out.extend(c.to_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

/// Whether `sent` names the declared field `name` in another case or number.
fn same_field(sent: &str, name: &str) -> bool {
    let sent = snake(sent);
    sent == name || plural(&sent) == name || plural(name) == sent
}

fn plural(word: &str) -> String {
    if let Some(stem) = word.strip_suffix('y')
        && !stem.ends_with(['a', 'e', 'i', 'o', 'u'])
    {
        return format!("{stem}ies");
    }
    if word.ends_with('s') || word.ends_with('x') || word.ends_with("sh") || word.ends_with("ch") {
        return format!("{word}es");
    }
    format!("{word}s")
}

/// The declared values of an enum schema (or of its single `allOf` member),
/// without the proto zero value.
fn enum_values(schema: &Value) -> Option<Vec<String>> {
    if let Some(values) = schema.get("enum").and_then(Value::as_array) {
        let values: Vec<String> = values
            .iter()
            .filter_map(Value::as_str)
            .filter(|value| !value.ends_with(PLACEHOLDER_SUFFIX))
            .map(str::to_owned)
            .collect();
        return (!values.is_empty()).then_some(values);
    }
    match schema
        .get("allOf")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
    {
        Some([one]) => enum_values(one),
        _ => None,
    }
}

fn has_type(schema: &Value, name: &str) -> bool {
    match schema.get("type") {
        Some(Value::String(found)) => found == name,
        Some(Value::Array(types)) => types.iter().any(|found| found.as_str() == Some(name)),
        _ => false,
    }
}

/// A schema's description: its first line, bounded.
fn description(schema: &Value) -> Option<String> {
    let text = schema.get("description")?.as_str()?.lines().next()?.trim();
    if text.is_empty() {
        return None;
    }
    Some(bounded(text, MAX_DESCRIPTION_CHARS))
}

fn field_line(field: &Field) -> String {
    match &field.description {
        Some(description) => format!("{} ({description})", field.name),
        None => field.name.clone(),
    }
}

fn listed(values: &[String]) -> String {
    let mut listed = values
        .iter()
        .take(MAX_LISTED)
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join(", ");
    if values.len() > MAX_LISTED {
        listed.push_str(", ...");
    }
    listed
}

fn join(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.to_owned()
    } else {
        format!("{prefix}.{name}")
    }
}

fn kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "a list",
        Value::Object(_) => "an object",
    }
}

fn short(value: &Value) -> String {
    bounded(&value.to_string(), MAX_SENT_CHARS)
}

fn bounded(text: &str, max: usize) -> String {
    if text.chars().count() > max {
        format!("{}...", text.chars().take(max).collect::<String>())
    } else {
        text.to_owned()
    }
}

/// An ISO 8601 calendar date or date-time in RFC 3339 form: a date alone is
/// midnight UTC, a space or lower-case `t` separates the time, seconds
/// default to `00`, a missing offset is UTC and `+hhmm` gains its colon.
/// `None` when `text` is not such a date.
fn rfc3339(text: &str) -> Option<String> {
    let text = text.trim();
    if !text.is_ascii() {
        return None;
    }
    let number = |from: usize, to: usize| -> Option<u32> {
        let part = text.get(from..to)?;
        part.bytes()
            .all(|byte| byte.is_ascii_digit())
            .then(|| part.parse().ok())
            .flatten()
    };
    let at = |index: usize| text.as_bytes().get(index).copied();
    let (year, month, day) = (number(0, 4)?, number(5, 7)?, number(8, 10)?);
    if at(4) != Some(b'-') || at(7) != Some(b'-') {
        return None;
    }
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let days = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => return None,
    };
    if day == 0 || day > days {
        return None;
    }
    let date = &text[..10];
    if text.len() == 10 {
        return Some(format!("{date}T00:00:00Z"));
    }
    if !matches!(at(10), Some(b'T' | b't' | b' ')) {
        return None;
    }
    let (hour, minute) = (number(11, 13)?, number(14, 16)?);
    if at(13) != Some(b':') || hour > 23 || minute > 59 {
        return None;
    }
    let mut index = 16;
    let mut second = 0;
    let mut fraction = "";
    if at(16) == Some(b':') {
        second = number(17, 19)?;
        if second > 59 {
            return None;
        }
        index = 19;
        if at(19) == Some(b'.') {
            let digits = text[20..].bytes().take_while(u8::is_ascii_digit).count();
            if !(1..=9).contains(&digits) {
                return None;
            }
            fraction = &text[19..20 + digits];
            index = 20 + digits;
        }
    }
    let zone = match &text[index..] {
        "" | "Z" | "z" => "Z".to_owned(),
        offset => {
            let sign = offset
                .get(..1)
                .filter(|sign| *sign == "+" || *sign == "-")?;
            let field = |from: usize, to: usize| -> Option<u32> {
                let part = offset.get(from..to)?;
                part.bytes()
                    .all(|byte| byte.is_ascii_digit())
                    .then(|| part.parse().ok())
                    .flatten()
            };
            // `+hh`, `+hhmm` or `+hh:mm`.
            let (hours, minutes) = match offset.len() {
                3 => (field(1, 3)?, 0),
                5 => (field(1, 3)?, field(3, 5)?),
                6 if offset.as_bytes()[3] == b':' => (field(1, 3)?, field(4, 6)?),
                _ => return None,
            };
            if hours > 23 || minutes > 59 {
                return None;
            }
            format!("{sign}{hours:02}:{minutes:02}")
        }
    };
    Some(format!(
        "{date}T{hour:02}:{minute:02}:{second:02}{fraction}{zone}"
    ))
}

#[cfg(test)]
#[path = "arguments_tests.rs"]
mod tests;

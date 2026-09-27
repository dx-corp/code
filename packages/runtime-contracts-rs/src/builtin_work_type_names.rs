//! Closed presentation contract for one authorized built-in Work Type search.
//!
//! This module neither authorizes nor executes the owner read. The hosted
//! runtime must pass the result of the admitted `work_type.builtins` tool into
//! [`BuiltinWorkTypeNamesResult::from_owner_result`].

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashSet;

const MAX_OWNER_RESULT_BYTES: usize = 256 * 1024;
const MAX_NAME_INDEX_BYTES: usize = 16 * 1024;
const MAX_CATALOG_COUNT: usize = 4096;

/// The only model-proposed inputs for the first-page names read.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BuiltinWorkTypeNamesProposal {
    /// Optional name or description search; never an owner or scope selector.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
    /// Number of names to present from the owner's first page (1–8).
    pub limit: u8,
}

impl BuiltinWorkTypeNamesProposal {
    /// Rejects empty, oversized, or control-bearing model input.
    pub fn validate(&self) -> Result<(), BuiltinWorkTypeNamesError> {
        if !(1..=8).contains(&self.limit) {
            return Err(BuiltinWorkTypeNamesError::InvalidProposal(
                "limit must be 1..=8",
            ));
        }
        if let Some(query) = &self.query {
            validate_text(query, 200).map_err(BuiltinWorkTypeNamesError::InvalidProposal)?;
            if query.trim().is_empty() {
                return Err(BuiltinWorkTypeNamesError::InvalidProposal(
                    "query must contain a search term",
                ));
            }
        }
        Ok(())
    }

    /// Owner tool arguments. Presentation limit is deliberately omitted: the
    /// owner always returns its bounded first page and controls pagination.
    pub fn tool_arguments(&self) -> Result<Value, BuiltinWorkTypeNamesError> {
        self.validate()?;
        Ok(match &self.query {
            Some(query) => json!({"query": query}),
            None => json!({}),
        })
    }
}

/// Validation failure at the model proposal or owner-result compatibility edge.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BuiltinWorkTypeNamesError {
    /// The model proposal is outside the closed read contract.
    InvalidProposal(&'static str),
    /// The tool output is inconsistent, malformed, or too large to present.
    InvalidOwnerResult(&'static str),
}

impl std::fmt::Display for BuiltinWorkTypeNamesError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidProposal(reason) => {
                write!(formatter, "invalid built-in names proposal: {reason}")
            }
            Self::InvalidOwnerResult(reason) => {
                write!(formatter, "invalid built-in names owner result: {reason}")
            }
        }
    }
}

impl std::error::Error for BuiltinWorkTypeNamesError {}

#[derive(Clone, Debug, Eq, PartialEq)]
struct BuiltinWorkTypeName {
    blueprint_id: String,
    name: String,
}

/// Validated, presentation-only projection of one owner catalog page.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BuiltinWorkTypeNamesResult {
    names: Vec<BuiltinWorkTypeName>,
    matched_count: usize,
    catalog_revision: String,
}

impl BuiltinWorkTypeNamesResult {
    /// Accepts only a consistent first-page `project_builtin_catalog` result.
    /// Schema bodies, descriptions, and inspection arguments are discarded.
    pub fn from_owner_result(
        proposal: &BuiltinWorkTypeNamesProposal,
        owner: &Value,
    ) -> Result<Self, BuiltinWorkTypeNamesError> {
        proposal.validate()?;
        let invalid = BuiltinWorkTypeNamesError::InvalidOwnerResult;
        let bytes = serde_json::to_vec(owner).map_err(|_| invalid("result is not JSON"))?;
        if bytes.len() > MAX_OWNER_RESULT_BYTES {
            return Err(invalid("result exceeds owner output bound"));
        }
        let object = owner
            .as_object()
            .ok_or(invalid("result must be an object"))?;
        let revision = required_text(object.get("catalogRevision"), 64)?;
        if revision.len() != 64
            || !revision
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        {
            return Err(invalid(
                "catalog revision must be a lowercase SHA-256 digest",
            ));
        }
        let catalog_count = required_count(object.get("catalogCount"))?;
        let matched_count = required_count(object.get("matchedCount"))?;
        let returned_count = required_count(object.get("returnedCount"))?;
        let offset = required_count(object.get("offset"))?;
        if catalog_count > MAX_CATALOG_COUNT
            || matched_count > catalog_count
            || offset != 0
            || returned_count != matched_count.min(8)
        {
            return Err(invalid("first-page catalog counts are inconsistent"));
        }
        // Query-less discovery covers the entire catalog. A filtered search
        // can only narrow it, never create more matches.
        if proposal.query.is_none() && matched_count != catalog_count {
            return Err(invalid("unfiltered match count differs from catalog count"));
        }
        let has_more = object
            .get("hasMore")
            .and_then(Value::as_bool)
            .ok_or(invalid("missing hasMore"))?;
        let complete = object
            .get("complete")
            .and_then(Value::as_bool)
            .ok_or(invalid("missing complete"))?;
        if has_more != (returned_count < matched_count) || complete == has_more {
            return Err(invalid("page completion flags are inconsistent"));
        }
        validate_next_arguments(
            object.get("nextArguments"),
            revision,
            returned_count,
            proposal,
            has_more,
        )?;

        let blueprints = object
            .get("blueprints")
            .and_then(Value::as_array)
            .ok_or(invalid("missing blueprints"))?;
        if blueprints.len() != returned_count {
            return Err(invalid("returned count differs from blueprint page"));
        }
        let mut seen = HashSet::new();
        let mut page = Vec::with_capacity(returned_count);
        for blueprint in blueprints {
            let blueprint = blueprint
                .as_object()
                .ok_or(invalid("invalid blueprint summary"))?;
            let id = required_text(blueprint.get("blueprintId"), 200)?;
            let name = required_text(blueprint.get("name"), 200)?;
            if !seen.insert(id.to_owned()) {
                return Err(invalid("duplicate blueprint identity"));
            }
            let version = blueprint
                .get("version")
                .ok_or(invalid("missing blueprint version"))?;
            let version = version.as_u64().or_else(|| version.as_str()?.parse().ok());
            if !version.is_some_and(|version| (1..=i32::MAX as u64).contains(&version)) {
                return Err(invalid("invalid blueprint version"));
            }
            let digest = required_text(blueprint.get("contentDigest"), 64)?;
            if digest.len() != 64
                || !digest
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
            {
                return Err(invalid("invalid blueprint digest"));
            }
            page.push(BuiltinWorkTypeName {
                blueprint_id: id.to_owned(),
                name: name.to_owned(),
            });
        }
        if !page
            .windows(2)
            .all(|pair| pair[0].blueprint_id < pair[1].blueprint_id)
        {
            return Err(invalid("blueprint page is not sorted by identity"));
        }

        let index_complete = object
            .get("nameIndexComplete")
            .and_then(Value::as_bool)
            .ok_or(invalid("missing nameIndexComplete"))?;
        match (index_complete, object.get("nameIndex")) {
            (true, Some(Value::Array(index))) if proposal.query.is_none() => {
                if index.len() != catalog_count
                    || serde_json::to_vec(index)
                        .map_err(|_| invalid("invalid name index"))?
                        .len()
                        > MAX_NAME_INDEX_BYTES
                {
                    return Err(invalid("name index count or size is inconsistent"));
                }
                let mut previous_id: Option<&str> = None;
                for (position, entry) in index.iter().enumerate() {
                    let entry = entry
                        .as_object()
                        .ok_or(invalid("invalid name index entry"))?;
                    let id = required_text(entry.get("blueprintId"), 200)?;
                    let name = required_text(entry.get("name"), 200)?;
                    if previous_id.is_some_and(|previous| previous >= id) {
                        return Err(invalid("name index is not sorted by identity"));
                    }
                    previous_id = Some(id);
                    if page.get(position).is_some_and(|blueprint| {
                        id != blueprint.blueprint_id || name != blueprint.name
                    }) {
                        return Err(invalid("name index conflicts with blueprint page"));
                    }
                }
            }
            (false, Some(Value::Null)) => {}
            _ => return Err(invalid("name index shape is inconsistent")),
        }

        Ok(Self {
            names: page.into_iter().take(usize::from(proposal.limit)).collect(),
            matched_count,
            catalog_revision: revision.to_owned(),
        })
    }

    /// Version of the owner catalog that supplied the validated names.
    pub fn catalog_revision(&self) -> &str {
        &self.catalog_revision
    }

    /// Number of matches reported by the owner catalog for this search.
    pub fn matched_count(&self) -> usize {
        self.matched_count
    }

    /// Renders only validated names, with Markdown syntax in names escaped.
    #[must_use]
    pub fn render_text(&self) -> String {
        if self.matched_count == 0 {
            return "No built-in work types match this search.".to_owned();
        }
        let mut lines: Vec<String> = self
            .names
            .iter()
            .map(|item| format!("- {}", escape_markdown(&item.name)))
            .collect();
        let remaining = self.matched_count - self.names.len();
        if remaining > 0 {
            lines.push(format!(
                "{remaining} more match this search. Search by name to narrow the list."
            ));
        }
        lines.join("\n")
    }
}

fn required_count(value: Option<&Value>) -> Result<usize, BuiltinWorkTypeNamesError> {
    value
        .and_then(Value::as_u64)
        .and_then(|count| usize::try_from(count).ok())
        .ok_or(BuiltinWorkTypeNamesError::InvalidOwnerResult(
            "missing or invalid count",
        ))
}

fn required_text(
    value: Option<&Value>,
    max_bytes: usize,
) -> Result<&str, BuiltinWorkTypeNamesError> {
    let text =
        value
            .and_then(Value::as_str)
            .ok_or(BuiltinWorkTypeNamesError::InvalidOwnerResult(
                "missing text field",
            ))?;
    validate_text(text, max_bytes).map_err(BuiltinWorkTypeNamesError::InvalidOwnerResult)?;
    Ok(text)
}

fn validate_text(text: &str, max_bytes: usize) -> Result<(), &'static str> {
    if text.is_empty() || text.len() > max_bytes || text.chars().any(char::is_control) {
        return Err("text is empty, oversized, or contains a control character");
    }
    Ok(())
}

fn validate_next_arguments(
    value: Option<&Value>,
    revision: &str,
    returned_count: usize,
    proposal: &BuiltinWorkTypeNamesProposal,
    has_more: bool,
) -> Result<(), BuiltinWorkTypeNamesError> {
    let invalid = BuiltinWorkTypeNamesError::InvalidOwnerResult;
    if !has_more {
        return if matches!(value, Some(Value::Null)) {
            Ok(())
        } else {
            Err(invalid("unexpected next page"))
        };
    }
    let next = value
        .and_then(Value::as_object)
        .ok_or(invalid("missing next page"))?;
    let cursor = next
        .get("cursor")
        .and_then(Value::as_str)
        .ok_or(invalid("missing cursor"))?;
    if cursor != format!("{revision}:{returned_count}") {
        return Err(invalid("cursor does not match this catalog page"));
    }
    let expected_query = proposal
        .query
        .as_ref()
        .map(|query| query.trim().to_lowercase())
        .filter(|query| !query.is_empty());
    match (expected_query.as_deref(), next.get("query")) {
        (None, None) => {}
        (Some(expected), Some(Value::String(actual))) if actual == expected => {}
        _ => return Err(invalid("next page query differs from the proposal")),
    }
    if next.len() != if expected_query.is_some() { 2 } else { 1 } {
        return Err(invalid("next page contains unexpected fields"));
    }
    Ok(())
}

fn escape_markdown(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for character in text.chars() {
        if matches!(
            character,
            '\\' | '`'
                | '*'
                | '_'
                | '{'
                | '}'
                | '['
                | ']'
                | '<'
                | '>'
                | '('
                | ')'
                | '#'
                | '+'
                | '-'
                | '.'
                | '!'
                | '|'
        ) {
            escaped.push('\\');
        }
        escaped.push(character);
    }
    escaped
}

#[cfg(test)]
mod tests {
    use super::*;

    fn digest() -> String {
        "a".repeat(64)
    }

    fn proposal(limit: u8) -> BuiltinWorkTypeNamesProposal {
        BuiltinWorkTypeNamesProposal { query: None, limit }
    }

    fn owner(count: usize) -> Value {
        let blueprints: Vec<_> = (0..count.min(8)).map(|index| json!({
            "blueprintId": format!("type-{index:02}"), "name": format!("Type {index}"),
            "version": "1", "contentDigest": digest(), "description":"large owner description ignored",
            "objectTypes":[], "inspectArguments":{"blueprintId":format!("type-{index:02}")}
        })).collect();
        let index: Vec<_> = (0..count).map(|index| json!({"blueprintId":format!("type-{index:02}"),"name":format!("Type {index}")})).collect();
        let more = count > 8;
        json!({
            "blueprints":blueprints,"catalogCount":count,"catalogRevision":digest(),
            "nameIndexComplete":true,"nameIndex":index,
            "matchedCount":count,"returnedCount":count.min(8),"offset":0,
            "hasMore":more,"complete":!more,
            "nextArguments":if more { json!({"cursor":format!("{}:8", digest())}) } else {Value::Null}
        })
    }

    #[test]
    fn proposal_is_closed_and_emits_only_owner_search_arguments() {
        assert_eq!(proposal(3).tool_arguments().unwrap(), json!({}));
        let search = BuiltinWorkTypeNamesProposal {
            query: Some(" Setup ".into()),
            limit: 2,
        };
        assert_eq!(search.tool_arguments().unwrap(), json!({"query":" Setup "}));
        assert!(proposal(0).validate().is_err());
        assert!(proposal(9).validate().is_err());
        assert!(
            BuiltinWorkTypeNamesProposal {
                query: Some("  ".into()),
                limit: 1
            }
            .validate()
            .is_err()
        );
        assert!(
            BuiltinWorkTypeNamesProposal {
                query: Some("x".repeat(201)),
                limit: 1
            }
            .validate()
            .is_err()
        );
        assert!(
            serde_json::from_value::<BuiltinWorkTypeNamesProposal>(
                json!({"limit":2,"workspaceId":"ws-other"})
            )
            .is_err()
        );
        assert!(
            serde_json::from_value::<BuiltinWorkTypeNamesProposal>(
                json!({"query":"a","limit":2,"cursor":"forged"})
            )
            .is_err()
        );
    }

    #[test]
    fn valid_owner_page_renders_only_requested_names_and_true_remaining_count() {
        let value = owner(19);
        let result = BuiltinWorkTypeNamesResult::from_owner_result(&proposal(3), &value).unwrap();
        assert_eq!(result.catalog_revision(), digest());
        assert_eq!(result.matched_count(), 19);
        assert_eq!(
            result.render_text(),
            "- Type 0\n- Type 1\n- Type 2\n16 more match this search. Search by name to narrow the list."
        );
        assert!(!result.render_text().contains("large owner description"));
        let empty = BuiltinWorkTypeNamesResult::from_owner_result(&proposal(3), &owner(0)).unwrap();
        assert_eq!(
            empty.render_text(),
            "No built-in work types match this search."
        );
    }

    #[test]
    fn markdown_in_owner_name_is_escaped_without_rejecting_punctuation() {
        let mut value = owner(1);
        value["blueprints"][0]["name"] = "[A]*_`<B>|!".into();
        value["nameIndex"][0]["name"] = value["blueprints"][0]["name"].clone();
        let result = BuiltinWorkTypeNamesResult::from_owner_result(&proposal(1), &value).unwrap();
        assert_eq!(result.render_text(), "- \\[A\\]\\*\\_\\`\\<B\\>\\|\\!");
    }

    #[test]
    fn filtered_first_page_requires_the_same_query_in_its_next_page() {
        let search = BuiltinWorkTypeNamesProposal {
            query: Some("  Setup  ".into()),
            limit: 2,
        };
        let mut value = owner(9);
        value["catalogCount"] = 19.into();
        value["nameIndexComplete"] = false.into();
        value["nameIndex"] = Value::Null;
        value["nextArguments"]["query"] = "setup".into();
        let result = BuiltinWorkTypeNamesResult::from_owner_result(&search, &value).unwrap();
        assert_eq!(
            result.render_text(),
            "- Type 0\n- Type 1\n7 more match this search. Search by name to narrow the list."
        );
        value["nextArguments"]["query"] = "different".into();
        assert!(BuiltinWorkTypeNamesResult::from_owner_result(&search, &value).is_err());
    }

    #[test]
    fn malformed_or_conflicting_owner_results_fail_closed() {
        let base = owner(9);
        for (key, value) in [
            ("offset", json!(8)),
            ("matchedCount", json!(7)),
            ("returnedCount", json!(7)),
            ("hasMore", json!(false)),
            ("complete", json!(true)),
            ("catalogRevision", json!("forged")),
            ("nameIndexComplete", json!(false)),
            ("nextArguments", json!({"cursor":"forged:8"})),
        ] {
            let mut changed = base.clone();
            changed[key] = value;
            assert!(
                BuiltinWorkTypeNamesResult::from_owner_result(&proposal(2), &changed).is_err(),
                "accepted {key}"
            );
        }
        let mut body = base.clone();
        body["blueprints"][0]["name"] = "line\nbreak".into();
        assert!(BuiltinWorkTypeNamesResult::from_owner_result(&proposal(2), &body).is_err());
        let mut duplicated = base;
        duplicated["blueprints"][1]["blueprintId"] =
            duplicated["blueprints"][0]["blueprintId"].clone();
        assert!(BuiltinWorkTypeNamesResult::from_owner_result(&proposal(2), &duplicated).is_err());

        let mut index_conflict = owner(9);
        index_conflict["nameIndex"][8]["name"] = "line\nbreak".into();
        assert!(
            BuiltinWorkTypeNamesResult::from_owner_result(&proposal(2), &index_conflict).is_err()
        );

        let mut oversized = owner(1);
        oversized["blueprints"][0]["description"] = "x".repeat(MAX_OWNER_RESULT_BYTES).into();
        assert!(BuiltinWorkTypeNamesResult::from_owner_result(&proposal(1), &oversized).is_err());
    }
}

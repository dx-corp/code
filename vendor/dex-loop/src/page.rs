//! What the person was looking at when they asked Dex from a Deixic page.
//!
//! A typed locator, never data and never authority: the page's route and id,
//! the record it shows, its filters, the ids of the rows it lists, and the
//! owner reads that render it. Dex reads the data itself through
//! `dex.workspace_read`, which checks the person's live access on every call,
//! so a forged locator can name only reads the person may already make.
//!
//! The authenticated host validates the snapshot before admitting the turn,
//! and it is logged on the turn's `UserMessage`, so every step, resume, and
//! replica of the turn sees the same page.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// One page snapshot. Every field is bounded by [`PageContext::validate`].
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PageContext {
    /// The page record id, such as `page.test-detail`.
    pub page: String,
    /// The path the person is on, without query or fragment.
    route: String,
    /// The record the page shows, when it shows one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    record: Option<PageRecord>,
    /// Filters applied on the page, by name.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    filters: Vec<PageFilter>,
    /// The rows the page lists, by id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    visible: Option<PageCollection>,
    /// The owner reads that render the page, with their parameters.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reads: Vec<PageRead>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PageRecord {
    kind: String,
    id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PageFilter {
    name: String,
    value: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PageCollection {
    kind: String,
    ids: Vec<String>,
    /// How many rows the page says exist, when it knows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    total: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PageRead {
    /// A `dex.workspace_read` read name, such as `GetControlTestDetail`.
    read: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub params: BTreeMap<String, String>,
}

impl PageContext {
    /// Maximum serialized snapshot size, shared by admission and runtime ingress.
    const MAX_JSON_BYTES: usize = 16 * 1024;
    const MAX_IDS: usize = 50;
    const MAX_FILTERS: usize = 12;
    const MAX_READS: usize = 8;
    const MAX_PARAMS: usize = 8;
    const MAX_TEXT: usize = 256;

    /// Refuses a snapshot whose fields are empty, oversized, or not plain
    /// identifiers. The host calls this before admitting the turn.
    pub fn validate(&self) -> Result<(), &'static str> {
        let text = |value: &str| {
            !value.is_empty()
                && value.len() <= Self::MAX_TEXT
                && value.trim() == value
                && !value.chars().any(char::is_control)
        };
        let name = |value: &str| {
            text(value)
                && value
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
        };
        if !name(&self.page) {
            return Err("page must be a page id");
        }
        if !text(&self.route) || !self.route.starts_with('/') || self.route.contains(['?', '#']) {
            return Err("route must be a path");
        }
        if let Some(record) = &self.record
            && !(name(&record.kind) && text(&record.id))
        {
            return Err("record must name a kind and an id");
        }
        if self.filters.len() > Self::MAX_FILTERS
            || self
                .filters
                .iter()
                .any(|filter| !name(&filter.name) || !text(&filter.value))
        {
            return Err("filters must be at most 12 named values");
        }
        if let Some(visible) = &self.visible
            && (!name(&visible.kind)
                || visible.ids.len() > Self::MAX_IDS
                || visible.ids.iter().any(|id| !text(id)))
        {
            return Err("visible must name a kind and at most 50 ids");
        }
        if self.reads.len() > Self::MAX_READS
            || self.reads.iter().any(|read| {
                !name(&read.read)
                    || read.params.len() > Self::MAX_PARAMS
                    || read
                        .params
                        .iter()
                        .any(|(field, value)| !name(field) || !text(value))
            })
        {
            return Err("reads must be at most 8 named reads with at most 8 parameters each");
        }
        if serde_json::to_vec(self).map_or(true, |bytes| bytes.len() > Self::MAX_JSON_BYTES) {
            return Err("page context exceeds 16 KiB");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn test_page() -> PageContext {
        serde_json::from_value(json!({
            "page": "page.test-detail",
            "route": "/tests/test-github-branch",
            "record": {"kind": "control_test", "id": "test-github-branch"},
            "filters": [{"name": "result", "value": "failing"}],
            "visible": {"kind": "resource", "ids": ["repo-1", "repo-2"], "total": 2},
            "reads": [{"read": "GetControlTestDetail", "params": {"test_object_id": "test-github-branch"}}]
        }))
        .expect("page context")
    }

    #[test]
    fn a_page_snapshot_is_a_bounded_locator() {
        let page = test_page();
        assert_eq!(page.validate(), Ok(()));
        let back: PageContext =
            serde_json::from_str(&serde_json::to_string(&page).unwrap()).unwrap();
        assert_eq!(back, page);
    }

    #[test]
    fn page_snapshot_retains_its_declared_wire_fields() {
        assert_eq!(
            serde_json::to_value(test_page()).unwrap(),
            json!({
                "page": "page.test-detail",
                "route": "/tests/test-github-branch",
                "record": {"kind": "control_test", "id": "test-github-branch"},
                "filters": [{"name": "result", "value": "failing"}],
                "visible": {"kind": "resource", "ids": ["repo-1", "repo-2"], "total": 2},
                "reads": [{"read": "GetControlTestDetail", "params": {"test_object_id": "test-github-branch"}}]
            })
        );
    }

    #[test]
    fn data_tenants_and_oversized_or_malformed_fields_are_refused() {
        for forged in [
            json!({"page": "p", "route": "/x", "organization_id": "org-other"}),
            json!({"page": "p", "route": "/x", "record": {"kind": "test", "id": "t", "title": "Forged"}}),
            json!({"page": "p", "route": "/x", "rows": [{"status": "passing"}]}),
        ] {
            assert!(serde_json::from_value::<PageContext>(forged).is_err());
        }
        let mut page = test_page();
        page.route = "/tests/t?tab=x".into();
        assert!(page.validate().is_err());
        let mut page = test_page();
        page.visible.as_mut().unwrap().ids = (0..51).map(|i| format!("r{i}")).collect();
        assert!(page.validate().is_err());
        let mut page = test_page();
        page.reads[0].read = "/deixic.v1.DeixicService/GetControlTestDetail".into();
        assert!(page.validate().is_err());
        let mut page = test_page();
        page.page = String::new();
        assert!(page.validate().is_err());
        let mut page = test_page();
        page.filters[0].value = "a\nb".into();
        assert!(page.validate().is_err());
    }
}

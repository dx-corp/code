//! Deterministic, content-safe reporting for the assembled system prompt.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use maestro_context::token_counting::{self, CountConfidence};

pub(super) const PROMPT_AUDIT_SCHEMA_VERSION: u32 = 2;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct PromptAuditTool {
    pub(super) name: String,
    pub(super) requires_approval: bool,
    pub(super) description_sha256: String,
    pub(super) schema_sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct PromptAuditToolSurface {
    pub(super) scope: String,
    pub(super) completeness: String,
    pub(super) excluded_scopes: Vec<String>,
    pub(super) count: usize,
    pub(super) definitions_sha256: String,
    pub(super) definitions: Vec<PromptAuditTool>,
}

#[derive(Clone, PartialEq, Eq)]
pub(super) struct PromptFragment {
    name: String,
    source: String,
    content: String,
    rendered: bool,
    provenance: maestro_ai::ContentProvenance,
}

impl PromptFragment {
    pub(super) fn new(
        name: impl Into<String>,
        source: impl Into<String>,
        content: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            source: source.into(),
            content: content.into(),
            rendered: true,
            provenance: maestro_ai::ContentProvenance::new(
                maestro_ai::ContentSource::Unknown,
                None,
            ),
        }
    }

    pub(super) fn with_provenance(mut self, source: maestro_ai::ContentSource) -> Self {
        self.provenance = maestro_ai::ContentProvenance::new(source, Some(self.source.clone()));
        self
    }

    pub(super) fn audit_only(name: impl Into<String>, source: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            source: source.into(),
            content: String::new(),
            rendered: false,
            provenance: maestro_ai::ContentProvenance::new(
                maestro_ai::ContentSource::Unknown,
                None,
            ),
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
pub(super) struct PromptAssembly {
    fragments: Vec<PromptFragment>,
}

impl PromptAssembly {
    pub(super) fn new(fragments: Vec<PromptFragment>) -> Self {
        Self { fragments }
    }

    /// Matches render byte-for-byte when text parts are joined with two newlines.
    pub(super) fn content_parts(&self) -> Vec<maestro_ai::ContentBlock> {
        self.fragments
            .iter()
            .filter(|fragment| fragment.rendered)
            .map(|fragment| maestro_ai::ContentBlock::AttributedText {
                text: fragment.content.clone(),
                provenance: fragment.provenance.clone(),
            })
            .collect()
    }

    pub(super) fn render(&self) -> String {
        self.fragments
            .iter()
            .filter(|fragment| fragment.rendered)
            .map(|fragment| fragment.content.as_str())
            .collect::<Vec<_>>()
            .join("\n\n")
    }

    pub(super) fn audit(
        &self,
        model: Option<&str>,
        active_skill_ids: impl IntoIterator<Item = String>,
    ) -> PromptAuditReport {
        let tokenizer_model = model.filter(|value| !value.trim().is_empty());
        let method =
            match token_counting::count_tokens_with_metadata("", tokenizer_model).confidence {
                CountConfidence::Measured => PromptAuditTokenCountMethod::Measured,
                CountConfidence::Estimated => PromptAuditTokenCountMethod::Estimated,
            };
        let prompt = self.render();
        let redacted_prompt = redact(&prompt);
        let mut active_skill_ids = active_skill_ids.into_iter().collect::<Vec<_>>();
        active_skill_ids.sort();
        active_skill_ids.dedup();

        let fragments = self
            .fragments
            .iter()
            .enumerate()
            .map(|(order, fragment)| PromptAuditFragment {
                name: fragment.name.clone(),
                order,
                source: fragment.source.clone(),
                byte_count: fragment.content.len() as u64,
                token_count: token_counting::count_tokens(&fragment.content, tokenizer_model),
                redacted_sha256: content_sha256(&redact(&fragment.content)),
                exact_sha256: content_sha256(&fragment.content),
            })
            .collect::<Vec<_>>();

        PromptAuditReport {
            schema_version: PROMPT_AUDIT_SCHEMA_VERSION,
            model: tokenizer_model
                .unwrap_or(maestro_ui::localization::tr("unknown"))
                .to_string(),
            token_count_method: method,
            active_skill_ids,
            total_byte_count: prompt.len() as u64,
            total_token_count: token_counting::count_tokens(&prompt, tokenizer_model),
            prompt_sha256: content_sha256(&redacted_prompt),
            exact_prompt_sha256: content_sha256(&prompt),
            findings: findings(&fragments),
            fragments,
            effective: None,
            tools: PromptAuditToolSurface {
                scope: "registered_builtin".to_string(),
                completeness: "partial".to_string(),
                excluded_scopes: vec![
                    "runtime_tool_filtering".to_string(),
                    "mcp".to_string(),
                    "client_external".to_string(),
                ],
                count: 0,
                definitions_sha256: content_sha256("[]"),
                definitions: Vec::new(),
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum PromptAuditTokenCountMethod {
    Measured,
    Estimated,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct PromptAuditFragment {
    pub(super) name: String,
    pub(super) order: usize,
    pub(super) source: String,
    pub(super) byte_count: u64,
    pub(super) token_count: u64,
    pub(super) redacted_sha256: String,
    #[serde(skip)]
    exact_sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct PromptAuditEffectiveSurface {
    pub(super) prompt_revision: u64,
    pub(super) prompt_sha256: String,
    pub(super) prompt_matches_desired: bool,
    pub(super) tools: PromptAuditToolSurface,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum PromptAuditFindingKind {
    DuplicateFragment,
    MissingProvenance,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct PromptAuditFinding {
    pub(super) kind: PromptAuditFindingKind,
    pub(super) fragments: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(super) missing_fields: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct PromptAuditReport {
    pub(super) schema_version: u32,
    pub(super) model: String,
    pub(super) token_count_method: PromptAuditTokenCountMethod,
    pub(super) active_skill_ids: Vec<String>,
    pub(super) fragments: Vec<PromptAuditFragment>,
    pub(super) total_byte_count: u64,
    pub(super) total_token_count: u64,
    pub(super) prompt_sha256: String,
    #[serde(skip)]
    exact_prompt_sha256: String,
    pub(super) findings: Vec<PromptAuditFinding>,
    pub(super) tools: PromptAuditToolSurface,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) effective: Option<PromptAuditEffectiveSurface>,
}

impl PromptAuditReport {
    pub(super) fn with_registered_tools(
        mut self,
        tools: impl IntoIterator<Item = (String, bool, String, serde_json::Value)>,
    ) -> Self {
        let mut definitions = tools
            .into_iter()
            .map(
                |(name, requires_approval, description, schema)| PromptAuditTool {
                    name,
                    requires_approval,
                    description_sha256: content_sha256(&redact(&description)),
                    schema_sha256: content_sha256(
                        &serde_json::to_string(&schema).unwrap_or_else(|_| "null".to_string()),
                    ),
                },
            )
            .collect::<Vec<_>>();
        definitions.sort_by(|left, right| left.name.cmp(&right.name));
        let serialized = serde_json::to_string(&definitions).unwrap_or_else(|_| "[]".to_string());
        self.tools = PromptAuditToolSurface {
            scope: "executor_registered".to_string(),
            completeness: "partial".to_string(),
            excluded_scopes: vec![
                "runtime_tool_filtering".to_string(),
                "mcp".to_string(),
                "client_external".to_string(),
            ],
            count: definitions.len(),
            definitions_sha256: content_sha256(&serialized),
            definitions,
        };
        self
    }

    pub(super) fn with_effective_runtime(
        mut self,
        prompt_revision: u64,
        system_prompt: Option<&str>,
        tools: impl IntoIterator<Item = (String, bool, String, serde_json::Value)>,
    ) -> Self {
        let system_prompt = system_prompt.unwrap_or_default();
        let prompt_sha256 = content_sha256(&redact(system_prompt));
        let mut effective_report = self.clone().with_registered_tools(tools);
        effective_report.tools.scope = "runner_effective".to_string();
        effective_report.tools.completeness = "complete".to_string();
        effective_report.tools.excluded_scopes.clear();
        self.effective = Some(PromptAuditEffectiveSurface {
            prompt_revision,
            prompt_matches_desired: content_sha256(system_prompt) == self.exact_prompt_sha256,
            prompt_sha256,
            tools: effective_report.tools,
        });
        self
    }

    pub(super) fn render_markdown(&self) -> String {
        let mut report = maestro_ui::localization::format(
            "## Prompt Audit\n\n- Schema: `evalops.maestro.prompt_audit.v{0}`\n- Model: `{1}`\n- Prompt: {2} bytes, {3} tokens ({4})\n- Redacted prompt SHA-256: `{5}`\n- Registered tools: {6} (`{7}`; {8} surface; excludes {9})\n- Active skills: {10}\n\n### Fragments\n",
            &[
                (self.schema_version).to_string(),
                (self.model).clone(),
                (self.total_byte_count).to_string(),
                (self.total_token_count).to_string(),
                format!("{:?}", self.token_count_method),
                (self.prompt_sha256).clone(),
                (self.tools.count).to_string(),
                (self.tools.definitions_sha256).clone(),
                (self.tools.completeness).clone(),
                (self.tools.excluded_scopes.join(", ")).clone(),
                (if self.active_skill_ids.is_empty() {
                    "none".to_string()
                } else {
                    self.active_skill_ids.join(", ")
                })
                .clone(),
            ],
        );
        if let Some(effective) = &self.effective {
            report.push_str(&maestro_ui::localization::format("- Runner-effective prompt: revision {0}, `{1}` ({2})\n- Runner-effective tools: {3} (`{4}`)\n\n", &[(effective.prompt_revision).to_string(), (effective.prompt_sha256).clone(), (if effective.prompt_matches_desired { "matches desired" } else { "drifted" }).to_string(), (effective.tools.count).to_string(), (effective.tools.definitions_sha256).clone()]));
        }
        for fragment in &self.fragments {
            report.push_str(&format!(
                "{}. `{}` — {} bytes, {} tokens, `{}`; source `{}`\n",
                fragment.order + 1,
                fragment.name,
                fragment.byte_count,
                fragment.token_count,
                fragment.redacted_sha256,
                fragment.source
            ));
        }
        report.push_str("\n### Findings\n");
        if self.findings.is_empty() {
            report.push_str(maestro_ui::localization::tr("No deterministic findings.\n"));
        } else {
            for finding in &self.findings {
                report.push_str(&format!(
                    "- `{:?}`: {}\n",
                    finding.kind,
                    finding.fragments.join(", ")
                ));
            }
        }
        report
    }
}

fn redact(content: &str) -> String {
    match crate::agent::credential_store::redact_credentials_in_json(&serde_json::Value::String(
        content.to_string(),
    )) {
        serde_json::Value::String(redacted) => redacted,
        _ => "[REDACTED]".to_string(),
    }
}

fn content_sha256(content: &str) -> String {
    format!("{:x}", Sha256::digest(content.as_bytes()))
}

fn findings(fragments: &[PromptAuditFragment]) -> Vec<PromptAuditFinding> {
    let mut findings = Vec::new();

    for fragment in fragments {
        let mut missing_fields = Vec::new();
        if fragment.name.trim().is_empty() {
            missing_fields.push("name".to_string());
        }
        if fragment.source.trim().is_empty() {
            missing_fields.push("source".to_string());
        }
        if !missing_fields.is_empty() {
            findings.push(PromptAuditFinding {
                kind: PromptAuditFindingKind::MissingProvenance,
                fragments: vec![fragment_label(fragment)],
                missing_fields,
            });
        }
    }

    let mut hashes = BTreeMap::<&str, Vec<&PromptAuditFragment>>::new();
    for fragment in fragments {
        hashes
            .entry(&fragment.exact_sha256)
            .or_default()
            .push(fragment);
    }
    for duplicates in hashes.into_values().filter(|group| group.len() > 1) {
        findings.push(PromptAuditFinding {
            kind: PromptAuditFindingKind::DuplicateFragment,
            fragments: duplicates
                .into_iter()
                .map(fragment_label)
                .collect::<Vec<_>>(),
            missing_fields: Vec::new(),
        });
    }

    findings.sort_by(|left, right| {
        finding_rank(&left.kind)
            .cmp(&finding_rank(&right.kind))
            .then_with(|| left.fragments.cmp(&right.fragments))
    });
    findings
}

fn fragment_label(fragment: &PromptAuditFragment) -> String {
    if fragment.name.trim().is_empty() {
        format!("fragment[{}]", fragment.order)
    } else {
        fragment.name.clone()
    }
}

fn finding_rank(kind: &PromptAuditFindingKind) -> u8 {
    match kind {
        PromptAuditFindingKind::MissingProvenance => 0,
        PromptAuditFindingKind::DuplicateFragment => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::App;

    #[test]
    fn report_is_deterministic_and_omits_fragment_content() {
        let assembly = PromptAssembly::new(vec![
            PromptFragment::new("base", "maestro.base", "SECRET_BASE_PROMPT"),
            PromptFragment::new("skills", "skills.loader", "SECRET_SKILL_PROMPT"),
        ]);

        let first = assembly.audit(
            Some("claude-sonnet-4-5"),
            ["zeta".to_string(), "alpha".to_string(), "alpha".to_string()],
        );
        let second = assembly.audit(
            Some("claude-sonnet-4-5"),
            ["alpha".to_string(), "zeta".to_string()],
        );
        let json = serde_json::to_string(&first).expect("report serializes");

        assert_eq!(first, second);
        assert_eq!(
            first.active_skill_ids,
            vec!["alpha".to_string(), "zeta".to_string()]
        );
        assert!(!json.contains("SECRET_BASE_PROMPT"));
        assert!(!json.contains("SECRET_SKILL_PROMPT"));
        assert_eq!(first.fragments[0].order, 0);
        assert_eq!(first.fragments[1].order, 1);
    }

    #[test]
    fn report_hashes_redacted_content_and_safe_tool_metadata() {
        let secret = "sk-proj-abcdefghijklmnopqrstuvwxyz123456";
        let report = PromptAssembly::new(vec![PromptFragment::new("base", "maestro.base", secret)])
            .audit(Some("test-model"), Vec::new())
            .with_registered_tools([(
                "read".to_string(),
                false,
                "Read a file".to_string(),
                serde_json::json!({"type": "object"}),
            )]);
        let json = serde_json::to_string(&report).expect("report serializes");

        assert!(!json.contains(secret));
        assert_eq!(report.prompt_sha256, content_sha256(&redact(secret)));
        assert_eq!(report.tools.count, 1);
        assert_eq!(report.tools.definitions[0].name, "read");
        assert_eq!(report.tools.scope, "executor_registered");
        assert_eq!(
            report.tools.excluded_scopes,
            vec!["runtime_tool_filtering", "mcp", "client_external"]
        );
    }

    #[test]
    fn different_secrets_do_not_become_false_duplicate_fragments() {
        let report = PromptAssembly::new(vec![
            PromptFragment::new("first", "test", "password=alpha-secret"),
            PromptFragment::new("second", "test", "password=beta-secret"),
        ])
        .audit(Some("test-model"), Vec::new());

        assert_eq!(
            report.fragments[0].redacted_sha256, report.fragments[1].redacted_sha256,
            "the fixture must exercise a redaction collision"
        );
        assert!(
            report
                .findings
                .iter()
                .all(|finding| finding.kind != PromptAuditFindingKind::DuplicateFragment)
        );
    }

    #[test]
    fn effective_runtime_surface_reports_prompt_drift_and_complete_tools() {
        let report = PromptAssembly::new(vec![PromptFragment::new("base", "test", "desired")])
            .audit(Some("test-model"), Vec::new())
            .with_effective_runtime(
                7,
                Some("applied"),
                [(
                    "mcp__repo__read".to_string(),
                    false,
                    "Read".to_string(),
                    serde_json::json!({}),
                )],
            );
        let effective = report.effective.expect("effective runtime surface");
        assert_eq!(effective.prompt_revision, 7);
        assert!(!effective.prompt_matches_desired);
        assert_eq!(effective.tools.completeness, "complete");
        assert!(effective.tools.excluded_scopes.is_empty());
        assert_eq!(effective.tools.definitions[0].name, "mcp__repo__read");
    }

    #[test]
    fn effective_prompt_match_does_not_hide_credential_only_drift() {
        let report = PromptAssembly::new(vec![PromptFragment::new(
            "base",
            "test",
            "password=desired-secret",
        )])
        .audit(Some("test-model"), Vec::new())
        .with_effective_runtime(1, Some("password=applied-secret"), []);

        assert!(!report.effective.unwrap().prompt_matches_desired);
    }

    #[test]
    fn report_flags_only_duplicate_content_and_missing_provenance() {
        let report = PromptAssembly::new(vec![
            PromptFragment::new("base", "maestro.base", "same"),
            PromptFragment::new("skills", "skills.loader", "same"),
            PromptFragment::new("", "", "different"),
        ])
        .audit(None, Vec::new());

        assert_eq!(report.findings.len(), 2);
        assert_eq!(
            report.findings[0],
            PromptAuditFinding {
                kind: PromptAuditFindingKind::MissingProvenance,
                fragments: vec!["fragment[2]".to_string()],
                missing_fields: vec!["name".to_string(), "source".to_string()],
            }
        );
        assert_eq!(
            report.findings[1],
            PromptAuditFinding {
                kind: PromptAuditFindingKind::DuplicateFragment,
                fragments: vec!["base".to_string(), "skills".to_string()],
                missing_fields: Vec::new(),
            }
        );
    }

    #[test]
    fn provenance_assembly_preserves_legacy_prompt_bytes() {
        let skills = Some("skills".to_string());
        let harness = Some("harness".to_string());
        let rlm = Some("rlm".to_string());
        let mailbox = Some("mailbox".to_string());
        let active = "active skill prompt";

        let actual = App::build_system_prompt_with_context(
            "/tmp",
            2026,
            skills.clone(),
            harness.clone(),
            rlm.clone(),
            mailbox.clone(),
            active,
        );

        let mut legacy_sections = vec![App::build_base_system_prompt("/tmp")];
        legacy_sections.extend([skills, harness, rlm, mailbox].into_iter().flatten());
        legacy_sections.push(format!(
            "When using websearch/codesearch for up-to-date information, include the current year (2026) in the query unless the user specifies a different year or a historical range.\n\n{active}"
        ));
        assert_eq!(actual, legacy_sections.join("\n\n"));
    }
}

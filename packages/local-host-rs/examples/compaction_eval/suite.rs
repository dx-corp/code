use super::host::{agent::selective_summary, ai::Message};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashSet;

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Suite {
    pub schema: String,
    pub cases: Vec<Case>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Case {
    pub id: String,
    pub family: String,
    /// Seeded historical messages, not a claimed live execution trace.
    pub history: Vec<Message>,
    pub question: String,
    /// Grader-only answer: never included in a provider request.
    pub expected: Value,
}

impl Suite {
    pub fn validate(&self, context_window: u64, max_tokens: u32) -> Result<()> {
        ensure!(
            self.schema == "maestro.compaction-eval-suite.v1",
            "unsupported suite schema"
        );
        ensure!(
            !self.cases.is_empty() && self.cases.len() <= 100,
            "suite needs 1..100 cases"
        );
        let mut seen = HashSet::new();
        for case in &self.cases {
            ensure!(
                !case.id.is_empty()
                    && case
                        .id
                        .bytes()
                        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'),
                "unsafe case id"
            );
            ensure!(seen.insert(&case.id), "duplicate case id");
            ensure!(
                !case.family.trim().is_empty() && !case.question.trim().is_empty(),
                "missing family or question"
            );
            ensure!(
                case.expected.as_object().is_some_and(|o| !o.is_empty()),
                "expected answer must be a nonempty object"
            );
            let preview = selective_summary::preview(&case.history)?;
            ensure!(!preview.turns.is_empty(), "history needs a user turn");
            // Validate complete tool exchanges before any provider call.
            selective_summary::validate_groups(&case.history)?;
            let bytes = serde_json::to_vec(&case.history)?.len() + case.question.len();
            // Conservative byte ceiling leaves room for standing runtime instructions.
            ensure!(
                bytes as u64 + u64::from(max_tokens) + 4096 < context_window / 2,
                "seeded case may trigger automatic compaction: {}",
                case.id
            );
        }
        Ok(())
    }
}

pub fn grade(answer: &str, expected: &Value) -> bool {
    // Exact typed fields, including missing/null/false distinctions. No prose judge.
    serde_json::from_str::<Value>(answer.trim()).is_ok_and(|actual| actual == *expected)
}

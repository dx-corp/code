//! Instruction treatment on the existing native behavioral evaluation path.
use super::{hash, report, suite};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashSet};

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct InstructionSuite {
    pub schema: String,
    pub repetitions: u32,
    pub shared_system_prompt: String,
    /// Exact AGENTS.md fragments, supplied as instructions to the native agent.
    pub control_agents_md: String,
    pub candidate_agents_md: String,
    pub cohort: suite::Suite,
}

impl InstructionSuite {
    pub fn validate(&self, context_window: u64, max_tokens: u32) -> Result<()> {
        ensure!(
            self.schema == "maestro.instruction-eval-suite.v1",
            "unsupported instruction suite"
        );
        ensure!(
            (1..=100).contains(&self.repetitions),
            "repetitions must be in 1..100"
        );
        ensure!(
            !self.shared_system_prompt.trim().is_empty(),
            "missing shared system prompt"
        );
        ensure!(
            self.control_agents_md != self.candidate_agents_md,
            "unchanged instruction treatment"
        );
        let instruction_bytes = self.shared_system_prompt.len()
            + self
                .control_agents_md
                .len()
                .max(self.candidate_agents_md.len());
        ensure!(
            instruction_bytes < 64 * 1024,
            "instruction treatment exceeds byte budget"
        );
        self.cohort.validate(context_window, max_tokens)?;
        for case in &self.cohort.cases {
            let bytes =
                serde_json::to_vec(&case.history)?.len() + case.question.len() + instruction_bytes;
            ensure!(
                bytes as u64 + u64::from(max_tokens) + 4096 < context_window / 2,
                "instruction case may trigger automatic compaction: {}",
                case.id
            );
        }
        Ok(())
    }

    pub fn system_prompt(&self, candidate: bool) -> String {
        let instructions = if candidate {
            &self.candidate_agents_md
        } else {
            &self.control_agents_md
        };
        format!(
            "{}\n\n<workspace_instructions source=\"AGENTS.md\">\n{}\n</workspace_instructions>",
            self.shared_system_prompt, instructions
        )
    }

    pub fn plan(&self, protocol_sha256: &str) -> Vec<Slot> {
        let mut slots = Vec::new();
        for case in &self.cohort.cases {
            for repetition in 1..=self.repetitions {
                let arms = if repetition % 2 == 1 {
                    [false, true]
                } else {
                    [true, false]
                };
                for candidate in arms {
                    slots.push(Slot {
                        protocol_sha256: protocol_sha256.into(),
                        case_id: case.id.clone(),
                        repetition,
                        candidate,
                        instruction_sha256: hash(self.system_prompt(candidate).as_bytes()),
                    });
                }
            }
        }
        slots
    }
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct Slot {
    pub protocol_sha256: String,
    pub case_id: String,
    pub repetition: u32,
    pub candidate: bool,
    pub instruction_sha256: String,
}

#[derive(Serialize)]
pub struct AssignedTrial {
    pub slot: Slot,
    pub trial: report::Trial,
}

#[derive(Serialize)]
pub struct Arm {
    pub assigned: usize,
    pub observed: usize,
    pub verified: usize,
    pub success_rate: Option<f64>,
    pub elapsed_seconds_observed: Option<f64>,
    pub retries_started_observed: Option<usize>,
    // Null is absence/incompleteness, never an assumed zero. The shared
    // measurement still retains its response and usage observation counts.
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cache_read_tokens: Option<u64>,
    pub cache_write_tokens: Option<u64>,
    pub user_interventions: Option<usize>,
    pub provider_reported_cost_usd: Option<f64>,
    pub cost_per_verified_task_usd: Option<f64>,
}

fn arm(rows: &[report::Trial], assigned: usize, valid: bool) -> Arm {
    let summary = report::arm(&rows.iter().collect::<Vec<_>>());
    Arm {
        assigned,
        observed: summary.attempts,
        verified: summary.verified,
        success_rate: valid.then_some(summary.success_rate),
        elapsed_seconds_observed: (!rows.is_empty()).then_some(summary.elapsed_seconds),
        retries_started_observed: (!rows.is_empty()).then_some(summary.retries_started),
        input_tokens: (valid && summary.usage_complete).then_some(summary.input_tokens_observed),
        output_tokens: (valid && summary.usage_complete).then_some(summary.output_tokens_observed),
        cache_read_tokens: (valid && summary.usage_complete)
            .then_some(summary.cache_read_tokens_observed),
        cache_write_tokens: (valid && summary.usage_complete)
            .then_some(summary.cache_write_tokens_observed),
        // This tool-free, unattended runner has no intervention observer.
        user_interventions: None,
        provider_reported_cost_usd: valid
            .then_some(summary.provider_reported_cost_usd)
            .flatten(),
        cost_per_verified_task_usd: valid
            .then_some(summary.cost_per_verified_task_usd)
            .flatten(),
    }
}

#[derive(Serialize)]
pub struct Report {
    pub schema: &'static str,
    pub protocol_sha256: String,
    pub comparison_valid: bool,
    pub expected_pairs: usize,
    pub complete_pairs: usize,
    pub blockers: Vec<String>,
    pub control: Arm,
    pub candidate: Arm,
    pub candidate_only: usize,
    pub control_only: usize,
    pub difference_percentage_points: Option<f64>,
    pub limitations: Vec<&'static str>,
    pub promotion_allowed: bool,
}

/// Regrade actual answer artifacts. `verified` from a stored result never grants
/// success; complete exact slots and runtime outcomes own the denominator.
pub fn paired(suite: &InstructionSuite, protocol_sha256: &str, rows: &[AssignedTrial]) -> Report {
    let plan = suite.plan(protocol_sha256);
    let mut blockers = Vec::new();
    let mut control = Vec::new();
    let mut candidate = Vec::new();
    let mut wins = 0;
    let mut losses = 0;
    let mut complete_pairs = 0;
    let mut outcomes = BTreeSet::new();
    let mut varied = false;
    // Plan validity is checked before execution; callers inspecting partial
    // results still get a fail-closed report if the contract is empty.
    if plan.is_empty() {
        blockers.push("empty paired cohort".into());
    }
    for row in rows {
        if !plan.contains(&row.slot) {
            blockers.push("unexpected or mismatched assigned slot".into());
        }
    }
    for slots in plan.chunks_exact(2) {
        let mut pair = Vec::new();
        for slot in slots {
            let matches = rows
                .iter()
                .filter(|row| {
                    row.slot.case_id == slot.case_id
                        && row.slot.repetition == slot.repetition
                        && row.slot.candidate == slot.candidate
                })
                .collect::<Vec<_>>();
            let [row] = matches.as_slice() else {
                blockers.push(format!(
                    "{} repetition {} candidate {}: expected one observation, found {}",
                    slot.case_id,
                    slot.repetition,
                    slot.candidate,
                    matches.len()
                ));
                continue;
            };
            let trial = &row.trial;
            let runtime_valid = row.slot == *slot
                && trial.case_id == slot.case_id
                && !trial.compacted
                && !trial.forced_compaction_applied
                && trial.terminal
                && trial.failure.is_none()
                && trial.elapsed_seconds.is_finite()
                && trial.elapsed_seconds >= 0.0
                && trial.measurement.response_count > 0;
            let case = suite
                .cohort
                .cases
                .iter()
                .find(|case| case.id == slot.case_id)
                .expect("planned case");
            let mut regraded = trial.clone();
            regraded.verified = runtime_valid && suite::grade(&trial.answer, &case.expected);
            // Preserve observed failed-attempt time/retries too. Invalid slots
            // still block every headline and never produce task success.
            if row.slot == *slot
                && trial.elapsed_seconds.is_finite()
                && trial.elapsed_seconds >= 0.0
            {
                if slot.candidate {
                    candidate.push(regraded.clone());
                } else {
                    control.push(regraded.clone());
                }
            }
            if !runtime_valid {
                blockers.push(format!(
                    "{} repetition {} candidate {}: invalid runtime or protocol outcome",
                    slot.case_id, slot.repetition, slot.candidate
                ));
                continue;
            }
            pair.push((slot.candidate, regraded.verified));
            if outcomes.contains(&(slot.case_id.clone(), slot.candidate, !regraded.verified)) {
                varied = true;
            }
            outcomes.insert((slot.case_id.clone(), slot.candidate, regraded.verified));
        }
        if pair.len() == 2 {
            complete_pairs += 1;
            let original = pair
                .iter()
                .find(|(candidate, _)| !candidate)
                .expect("control slot")
                .1;
            let treated = pair
                .iter()
                .find(|(candidate, _)| *candidate)
                .expect("candidate slot")
                .1;
            wins += usize::from(treated && !original);
            losses += usize::from(original && !treated);
        }
    }
    let expected_pairs = plan.len() / 2;
    let valid = blockers.is_empty() && complete_pairs == expected_pairs && expected_pairs > 0;
    let control = arm(&control, expected_pairs, valid);
    let candidate = arm(&candidate, expected_pairs, valid);
    let mut limitations = vec![
        "instruction fragments only; workspace discovery and executable skills/tools are not exercised",
        "synthetic task-shaped answer artifacts; no repository patch or customer acceptance",
        "operator-declared model and provider configuration are not remote weight identity",
        "no statistical significance or causal improvement claim",
    ];
    if suite.repetitions < 2 {
        limitations.push("one repetition does not establish stability");
    }
    if varied {
        limitations.push("observed outcome variation across repetitions");
    }
    if valid && (control.verified == expected_pairs || candidate.verified == expected_pairs) {
        limitations.push("at least one arm is saturated");
    }
    if valid && wins <= losses {
        limitations.push("no positive observed success delta");
    }
    Report {
        schema: "maestro.instruction-eval-report.v1",
        protocol_sha256: protocol_sha256.into(),
        comparison_valid: valid,
        expected_pairs,
        complete_pairs,
        blockers,
        control,
        candidate,
        candidate_only: wins,
        control_only: losses,
        difference_percentage_points: valid
            .then(|| 100.0 * (wins as f64 - losses as f64) / expected_pairs as f64),
        limitations,
        promotion_allowed: false,
    }
}

pub fn unique_plan(suite: &InstructionSuite, protocol_sha256: &str) -> Result<()> {
    let mut identities = HashSet::new();
    for slot in suite.plan(protocol_sha256) {
        ensure!(
            identities.insert((slot.case_id, slot.repetition, slot.candidate)),
            "duplicate planned slot"
        );
    }
    Ok(())
}

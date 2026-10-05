//! Synthetic observations prove experiment mechanics, never model efficacy.
use super::{
    hash,
    instructions::{AssignedTrial, InstructionSuite, paired},
    report::{Measurement, Trial},
};
use maestro_runtime::agent::TokenUsage;
use serde_json::json;

fn fixture_suite() -> InstructionSuite {
    serde_json::from_str(include_str!(
        "../../../../evals/instruction-behavior-v1.json"
    ))
    .unwrap()
}

fn fixture_rows(suite: &InstructionSuite) -> Vec<AssignedTrial> {
    suite
        .plan("protocol")
        .into_iter()
        .map(|slot| {
            let case = suite
                .cohort
                .cases
                .iter()
                .find(|case| case.id == slot.case_id)
                .unwrap();
            let mut measurement = Measurement::default();
            measurement.add(Some(&TokenUsage {
                input_tokens: 100,
                output_tokens: 20,
                cache_read_tokens: 50,
                cache_write_tokens: 0,
                cost: Some(0.1),
            }));
            measurement.complete = true;
            AssignedTrial {
                trial: Trial {
                    case_id: slot.case_id.clone(),
                    compacted: false,
                    verified: true,
                    terminal: true,
                    failure: None,
                    elapsed_seconds: 1.0,
                    retries_started: 0,
                    forced_compaction_applied: false,
                    answer: case.expected.to_string(),
                    measurement,
                },
                slot,
            }
        })
        .collect()
}

#[test]
fn task_shaped_cohort_validates_before_inference_and_repeats_alternate() {
    let suite = fixture_suite();
    suite.validate(128_000, 1024).unwrap();
    super::instructions::unique_plan(&suite, "protocol").unwrap();
    let plan = suite.plan("protocol");
    assert_eq!(plan.len(), 18);
    assert_eq!(
        plan.iter()
            .take(6)
            .map(|slot| (slot.repetition, slot.candidate))
            .collect::<Vec<_>>(),
        [
            (1, false),
            (1, true),
            (2, true),
            (2, false),
            (3, false),
            (3, true)
        ]
    );
    for pair in plan.chunks_exact(2) {
        assert_eq!(pair[0].case_id, pair[1].case_id);
        assert_eq!(pair[0].repetition, pair[1].repetition);
        assert_ne!(pair[0].instruction_sha256, pair[1].instruction_sha256);
        assert_eq!(pair[0].protocol_sha256, pair[1].protocol_sha256);
    }
    assert_eq!(
        plan[0].instruction_sha256,
        hash(suite.system_prompt(false).as_bytes())
    );
    let mut duplicate = fixture_suite();
    duplicate.cohort.cases[1].id = duplicate.cohort.cases[0].id.clone();
    assert!(duplicate.validate(128_000, 1024).is_err());
}

#[test]
fn changed_instruction_and_safe_cohort_are_required() {
    for repetitions in [0, 101] {
        let mut bad = fixture_suite();
        bad.repetitions = repetitions;
        assert!(bad.validate(128_000, 1024).is_err());
    }
    let mut bad = fixture_suite();
    bad.candidate_agents_md = bad.control_agents_md.clone();
    assert!(bad.validate(128_000, 1024).is_err());
    let mut bad = fixture_suite();
    bad.shared_system_prompt = String::new();
    assert!(bad.validate(128_000, 1024).is_err());
    let mut bad = fixture_suite();
    bad.candidate_agents_md = "x".repeat(64 * 1024);
    assert!(bad.validate(128_000, 1024).is_err());
    assert!(fixture_suite().validate(8192, 1024).is_err());
    let mut bad = fixture_suite();
    bad.cohort.cases[1].history.remove(1);
    assert!(bad.validate(128_000, 1024).is_err());
}

#[test]
fn actual_artifacts_override_claimed_success_and_keep_failed_attempt_cost() {
    let suite = fixture_suite();
    let mut rows = fixture_rows(&suite);
    rows[0].trial.answer =
        json!({"query":"SELECT id FROM documents WHERE workspace_id = $1", "verified":true})
            .to_string();
    assert!(
        rows[0].trial.verified,
        "stored success is deliberately untrusted"
    );
    rows[1].trial.verified = false;
    let report = paired(&suite, "protocol", &rows);
    assert!(report.comparison_valid);
    assert_eq!(report.expected_pairs, 9);
    assert_eq!(report.complete_pairs, 9);
    assert_eq!(report.control.verified, 8);
    assert_eq!(report.candidate.verified, 9);
    assert_eq!(report.candidate_only, 1);
    assert!((report.difference_percentage_points.unwrap() - 100.0 / 9.0).abs() < 1e-10);
    assert!((report.control.cost_per_verified_task_usd.unwrap() - 0.9 / 8.0).abs() < 1e-10);
    assert!(
        report
            .limitations
            .contains(&"observed outcome variation across repetitions")
    );
    assert!(!report.promotion_allowed);
}

#[test]
fn missing_duplicate_extra_or_changed_protocol_withholds_headline() {
    let suite = fixture_suite();
    let assert_blocked = |rows: &[AssignedTrial]| {
        let report = paired(&suite, "protocol", rows);
        assert!(!report.comparison_valid);
        assert!(!report.blockers.is_empty());
        assert!(report.difference_percentage_points.is_none());
        assert!(report.control.success_rate.is_none());
        assert!(report.candidate.success_rate.is_none());
        assert!(report.control.input_tokens.is_none());
    };
    let mut missing = fixture_rows(&suite);
    missing.pop();
    assert_blocked(&missing);
    let mut duplicate = fixture_rows(&suite);
    duplicate[1].slot = duplicate[0].slot.clone();
    assert_blocked(&duplicate);
    let mut extra = fixture_rows(&suite);
    extra[0].slot.case_id = "unexpected".into();
    assert_blocked(&extra);
    let mut changed = fixture_rows(&suite);
    changed[0].slot.protocol_sha256 = "wrong".into();
    assert_blocked(&changed);
    let mut changed = fixture_rows(&suite);
    changed[0].slot.instruction_sha256 = "wrong".into();
    assert_blocked(&changed);
    assert_blocked(&[]);
    let empty = paired(&suite, "protocol", &[]);
    assert!(empty.control.elapsed_seconds_observed.is_none());
    assert!(empty.candidate.retries_started_observed.is_none());
}

#[test]
fn runtime_failure_timeout_unscored_or_compacted_arm_blocks_comparison() {
    let suite = fixture_suite();
    for failure in ["timeout", "runtime_or_summary_failure"] {
        let mut bad = fixture_rows(&suite);
        bad[0].trial.failure = Some(failure.into());
        bad[0].trial.terminal = false;
        let report = paired(&suite, "protocol", &bad);
        assert!(!report.comparison_valid);
        assert!(report.difference_percentage_points.is_none());
        assert_eq!(report.expected_pairs, 9);
        assert_eq!(report.control.observed, 9);
        assert_eq!(report.control.elapsed_seconds_observed, Some(9.0));
    }
    for mutation in 0..5 {
        let mut bad = fixture_rows(&suite);
        match mutation {
            0 => bad[0].trial.terminal = false,
            1 => bad[0].trial.compacted = true,
            2 => bad[0].trial.forced_compaction_applied = true,
            3 => bad[0].trial.elapsed_seconds = f64::NAN,
            _ => bad[0].trial.measurement.response_count = 0,
        }
        assert!(!paired(&suite, "protocol", &bad).comparison_valid);
    }
}

#[test]
fn unobserved_or_partial_metrics_stay_unavailable() {
    let suite = fixture_suite();
    let mut rows = fixture_rows(&suite);
    rows[0].trial.measurement.add(None);
    let report = paired(&suite, "protocol", &rows);
    assert!(report.comparison_valid);
    assert!(report.control.input_tokens.is_none());
    assert!(report.control.output_tokens.is_none());
    assert!(report.control.cache_read_tokens.is_none());
    assert!(report.control.provider_reported_cost_usd.is_none());
    assert!(report.control.cost_per_verified_task_usd.is_none());
    assert_eq!(report.candidate.input_tokens, Some(900));
    assert!(report.control.user_interventions.is_none());
    assert!(report.candidate.user_interventions.is_none());
    let mut rows = fixture_rows(&suite);
    rows[0].trial.measurement.complete = false;
    assert!(
        paired(&suite, "protocol", &rows)
            .control
            .input_tokens
            .is_none()
    );
}

#[test]
fn no_positive_delta_single_repeat_and_saturation_are_explicit() {
    let mut suite = fixture_suite();
    suite.repetitions = 1;
    let report = paired(&suite, "protocol", &fixture_rows(&suite));
    assert_eq!(report.difference_percentage_points, Some(0.0));
    assert!(
        report
            .limitations
            .contains(&"one repetition does not establish stability")
    );
    assert!(
        report
            .limitations
            .contains(&"at least one arm is saturated")
    );
    assert!(
        report
            .limitations
            .contains(&"no positive observed success delta")
    );
}

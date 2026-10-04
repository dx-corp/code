use super::{
    report::{Measurement, Trial, paired},
    suite::{Suite, grade},
};
use maestro_local_host::agent::TokenUsage;
use serde_json::json;

fn suite() -> Suite {
    serde_json::from_str(include_str!(
        "../../../../evals/compaction-behavior-v1.json"
    ))
    .unwrap()
}

fn usage(cost: Option<f64>) -> TokenUsage {
    TokenUsage {
        input_tokens: 100,
        output_tokens: 20,
        cache_read_tokens: 50,
        cache_write_tokens: 10,
        cost,
    }
}

fn trial(id: &str, compacted: bool, verified: bool, cost: Option<f64>) -> Trial {
    let mut measurement = Measurement::default();
    measurement.add(Some(&usage(cost)));
    measurement.complete = true;
    Trial {
        case_id: id.into(),
        compacted,
        verified,
        terminal: true,
        failure: None,
        elapsed_seconds: 2.0,
        retries_started: 0,
        forced_compaction_applied: compacted,
        answer: String::new(),
        measurement,
    }
}

fn trials(suite: &Suite) -> Vec<Trial> {
    suite
        .cases
        .iter()
        .flat_map(|case| {
            [
                trial(&case.id, false, true, Some(0.1)),
                trial(&case.id, true, true, Some(0.2)),
            ]
        })
        .collect()
}

#[test]
fn reviewed_cases_validate_complete_historical_tool_exchanges() {
    let suite = suite();
    suite.validate(128_000, 1024).unwrap();
    assert_eq!(suite.cases.len(), 6);
    let mut invalid = suite;
    invalid.cases[1].history.remove(1);
    assert!(
        invalid.validate(128_000, 1024).is_err(),
        "orphaned historical receipt must be rejected before inference"
    );
}

#[test]
fn ambiguous_or_over_budget_suites_fail_before_provider_calls() {
    let mut invalid = suite();
    invalid.cases[1].id = invalid.cases[0].id.clone();
    assert!(invalid.validate(128_000, 1024).is_err());
    let mut invalid = suite();
    invalid.cases[0].id = "../escape".into();
    assert!(invalid.validate(128_000, 1024).is_err());
    assert!(suite().validate(8_192, 1024).is_err());
}

#[test]
fn grading_rejects_prose_extra_fields_and_wrong_types() {
    let expected = json!({"passed": false});
    assert!(grade(" {\"passed\":false} ", &expected));
    for answer in [
        "done",
        "{\"passed\":true}",
        "{\"passed\":\"false\"}",
        "{\"passed\":false,\"extra\":1}",
        "{}",
        "```json\n{\"passed\":false}\n```",
    ] {
        assert!(!grade(answer, &expected), "invalid answer: {answer}");
    }
    assert!(!grade("{\"passed\":null}", &expected));
}

#[test]
fn failed_attempts_and_summary_overhead_stay_in_cost_per_success() {
    let suite = suite();
    let mut rows = trials(&suite);
    rows[1].verified = false;
    rows[1].measurement.add(Some(&usage(Some(0.3))));
    let report = paired(&suite, &rows).unwrap();
    assert_eq!(report.compacted.attempts, 6);
    assert_eq!(report.compacted.verified, 5);
    assert!((report.compacted.provider_reported_cost_usd.unwrap() - 1.5).abs() < 1e-10);
    assert!((report.compacted.cost_per_verified_task_usd.unwrap() - 0.3).abs() < 1e-10);
    assert_eq!(report.compacted.input_tokens_observed, 700);
    assert_eq!(report.compacted.cache_read_tokens_observed, 350);
    assert_eq!(report.original_only, 1);
    assert_eq!(report.compacted_only, 0);
    assert!((report.difference_percentage_points + 100.0 / 6.0).abs() < 1e-10);
    assert!(report.comparison_valid);
    assert!(!report.promotion_allowed);
}

#[test]
fn unknown_or_partial_spend_never_becomes_a_zero_cost() {
    for unknown in [None, Some(-1.0), Some(f64::NAN), Some(f64::INFINITY)] {
        let mut measurement = Measurement::default();
        measurement.add(Some(&usage(Some(0.2))));
        measurement.add(Some(&usage(unknown)));
        measurement.add(Some(&usage(Some(0.1))));
        assert!(measurement.provider_reported_cost_usd.is_none());
    }
    let suite = suite();
    let mut rows = trials(&suite);
    rows[1].measurement.complete = false;
    let report = paired(&suite, &rows).unwrap();
    assert!(report.compacted.provider_reported_cost_usd.is_none());
    assert!(report.compacted.cost_per_verified_task_usd.is_none());
    assert!(!report.compacted.usage_complete);
    assert!(report.original.provider_reported_cost_usd.is_some());
}

#[test]
fn absent_usage_is_visible_and_zero_success_has_no_cost_ratio() {
    let suite = suite();
    let mut rows = trials(&suite);
    rows[0].measurement.add(None);
    for row in rows.iter_mut().filter(|r| r.compacted) {
        row.verified = false;
    }
    let report = paired(&suite, &rows).unwrap();
    assert!(!report.original.usage_complete);
    assert!(report.original.provider_reported_cost_usd.is_none());
    assert!(report.compacted.provider_reported_cost_usd.is_some());
    assert!(report.compacted.cost_per_verified_task_usd.is_none());
}

#[test]
fn provider_failure_invalidates_pair_but_timeout_keeps_denominator() {
    let suite = suite();
    let mut rows = trials(&suite);
    rows[0].verified = false;
    rows[0].terminal = false;
    rows[0].failure = Some("timeout".into());
    rows[0].measurement.complete = false;
    let report = paired(&suite, &rows).unwrap();
    assert!(report.comparison_valid);
    assert_eq!(report.original.attempts, 6);
    assert_eq!(report.compacted_only, 1);
    rows[0].failure = Some("runtime_or_summary_failure".into());
    assert!(!paired(&suite, &rows).unwrap().comparison_valid);
}

#[test]
fn report_rejects_incomplete_duplicate_or_extra_pairs_and_false_success() {
    let suite = suite();
    let mut rows = trials(&suite);
    rows.pop();
    assert!(paired(&suite, &rows).is_err());
    let mut rows = trials(&suite);
    rows[1].compacted = false;
    assert!(paired(&suite, &rows).is_err());
    let mut rows = trials(&suite);
    rows[1].case_id = "extra".into();
    assert!(paired(&suite, &rows).is_err());
    let mut rows = trials(&suite);
    rows[1].forced_compaction_applied = false;
    assert!(paired(&suite, &rows).is_err());
    rows[1].forced_compaction_applied = true;
    rows[1].terminal = false;
    assert!(paired(&suite, &rows).is_err());
    rows[1].terminal = true;
    rows[1].verified = false;
    rows[1].forced_compaction_applied = false;
    assert!(paired(&suite, &rows).is_err());
    let mut rows = trials(&suite);
    rows[0].forced_compaction_applied = true;
    assert!(paired(&suite, &rows).is_err());
    let mut empty = suite;
    empty.cases.clear();
    assert!(paired(&empty, &[]).is_err());
}

#[test]
fn nonterminal_attempt_without_a_failure_cannot_be_a_valid_comparison() {
    let suite = suite();
    let mut rows = trials(&suite);
    rows[0].terminal = false;
    rows[0].verified = false;
    assert!(paired(&suite, &rows).is_err());
}

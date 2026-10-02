use std::time::Duration;

use dex_loop::{Budget, BudgetAxis, Usage};

#[test]
fn each_axis_is_reported() {
    let budget = Budget {
        max_steps: 2,
        max_tokens: 100,
        max_cost_micros: 50,
        wall: Duration::from_secs(1),
    };
    let usage = |tokens, cost| Usage {
        input_tokens: tokens,
        output_tokens: 0,
        cost_micros: cost,
        ..Usage::default()
    };
    let short = Duration::from_millis(1);
    assert_eq!(budget.exhausted(1, usage(10, 1), short), None);
    // The answer-only call after `max_steps` is still allowed.
    assert_eq!(budget.exhausted(2, usage(10, 1), short), None);
    assert!(!budget.answer_only(1));
    assert!(budget.answer_only(2));
    assert_eq!(
        budget.exhausted(3, usage(10, 1), short),
        Some(BudgetAxis::Steps)
    );
    assert_eq!(
        budget.exhausted(1, usage(100, 1), short),
        Some(BudgetAxis::Tokens)
    );
    assert_eq!(
        budget.exhausted(1, usage(10, 50), short),
        Some(BudgetAxis::Cost)
    );
    assert_eq!(
        budget.exhausted(1, usage(10, 1), Duration::from_secs(1)),
        Some(BudgetAxis::Wall)
    );
}

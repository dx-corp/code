use std::time::Duration;

use dex_loop::{Budget, BudgetAxis, RemainingBudget, Usage};

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

#[test]
fn remaining_is_saturating_and_unlimited_axes_are_explicit() {
    let budget = Budget {
        max_steps: 2,
        max_tokens: 100,
        max_cost_micros: 50,
        wall: Duration::from_millis(100),
    };
    let remaining = budget.remaining(
        1,
        Usage {
            input_tokens: 20,
            output_tokens: 10,
            cost_micros: 7,
            ..Usage::default()
        },
        Duration::from_millis(25),
    );
    assert_eq!(
        remaining,
        RemainingBudget {
            tool_steps: 1,
            tokens: Some(70),
            cost_micros: Some(43),
            wall_ms: 75,
            answer_only: false
        }
    );
    let spent = budget.remaining(
        3,
        Usage {
            input_tokens: 200,
            cost_micros: 90,
            ..Usage::default()
        },
        Duration::from_secs(1),
    );
    assert_eq!(
        spent,
        RemainingBudget {
            tool_steps: 0,
            tokens: Some(0),
            cost_micros: Some(0),
            wall_ms: 0,
            answer_only: true
        }
    );
    assert_eq!(
        Budget::default()
            .remaining(0, Usage::default(), Duration::ZERO)
            .tokens,
        None
    );
    assert_eq!(
        Budget::default()
            .remaining(0, Usage::default(), Duration::ZERO)
            .cost_micros,
        None
    );
}

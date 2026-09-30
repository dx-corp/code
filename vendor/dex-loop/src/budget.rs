//! Per-turn limits.

use std::fmt;
use std::time::Duration;

use crate::event::Usage;

/// Limits for one turn. The engine checks them before every model call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Budget {
    /// Model calls per turn that may offer tools. Once a turn has made this
    /// many calls the engine makes one more, with no tools offered, so the
    /// model writes its answer from what it has. If that call still asks for
    /// a tool, the turn fails with `BudgetExhausted`.
    pub max_steps: u32,
    /// Input plus output tokens per turn.
    pub max_tokens: u64,
    pub max_cost_micros: u64,
    /// Time spent inside one `Engine::run` call. Time parked on an approval
    /// or a question does not count.
    pub wall: Duration,
}

impl Default for Budget {
    /// 60 steps and 25 minutes. Token and cost caps come from org policy, so
    /// the default leaves them open.
    fn default() -> Self {
        Self {
            max_steps: 60,
            max_tokens: u64::MAX,
            max_cost_micros: u64::MAX,
            wall: Duration::from_secs(25 * 60),
        }
    }
}

/// The limit a turn ran out of.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BudgetAxis {
    Steps,
    Tokens,
    Cost,
    Wall,
}

impl fmt::Display for BudgetAxis {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            BudgetAxis::Steps => "steps",
            BudgetAxis::Tokens => "tokens",
            BudgetAxis::Cost => "cost",
            BudgetAxis::Wall => "wall",
        })
    }
}

impl Budget {
    /// Whether the next model call is the answer-only call: `steps` calls
    /// have already been made and the tool-offering allowance is spent.
    pub fn answer_only(&self, steps: u32) -> bool {
        steps >= self.max_steps
    }

    /// The first exhausted axis, if any. `steps` counts model calls made so
    /// far. Steps are exhausted only after the answer-only call
    /// (`max_steps + 1`), which `answer_only` allows for. The other axes are
    /// exhausted as soon as they are reached, answer-only call included.
    pub fn exhausted(&self, steps: u32, usage: Usage, elapsed: Duration) -> Option<BudgetAxis> {
        if steps > self.max_steps {
            Some(BudgetAxis::Steps)
        } else if usage.tokens() >= self.max_tokens {
            Some(BudgetAxis::Tokens)
        } else if usage.cost_micros >= self.max_cost_micros {
            Some(BudgetAxis::Cost)
        } else if elapsed >= self.wall {
            Some(BudgetAxis::Wall)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}

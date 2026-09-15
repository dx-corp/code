import unittest
from statistics_report import paired_intervals


def arm(cost, success=True):
    return dict(
        success=success,
        total_cost_usd=cost,
        input_tokens=cost * 100,
        cache_read_tokens=0,
        cache_write_tokens=0,
        output_tokens=0,
        usage_complete=True,
        tokens_complete=True,
    )


class StatisticsTests(unittest.TestCase):
    def test_paired_fixed_ratio_and_outcome_difference(self):
        pairs = {str(i): dict(fast=arm(i), minimal=arm(i / 2)) for i in range(1, 5)}
        result = paired_intervals(pairs, draws=1000)
        self.assertEqual(result["cost_per_success_ratio_95"], [0.5, 0.5])
        self.assertEqual(result["tokens_per_success_ratio_95"], [0.5, 0.5])
        self.assertEqual(result["success_rate_difference_95"], [0, 0])
        self.assertEqual(result, paired_intervals(pairs, draws=1000))

    def test_no_conditioning_away_zero_success_resamples(self):
        pairs = dict(
            a=dict(fast=arm(1), minimal=arm(0.5)),
            b=dict(fast=arm(1), minimal=arm(0.5, False)),
        )
        result = paired_intervals(pairs, draws=1000)
        self.assertNotIn("cost_per_success_ratio_95", result)
        self.assertIn("zero successes", result["unavailable"]["cost_per_success_ratio"])
        self.assertEqual(result["success_rate_difference_95"], [-1, 0])

    def test_missing_cost_does_not_remove_token_or_quality_results(self):
        pairs = dict(
            a=dict(fast=arm(1), minimal=arm(0.5)), b=dict(fast=arm(2), minimal=arm(1))
        )
        pairs["a"]["minimal"].update(total_cost_usd=None, usage_complete=False)
        result = paired_intervals(pairs, draws=1000)
        self.assertNotIn("cost_per_success_ratio_95", result)
        self.assertEqual(result["tokens_per_success_ratio_95"], [0.5, 0.5])
        self.assertIn("success_rate_difference_95", result)

    def test_one_pair_has_no_interval(self):
        self.assertFalse(
            paired_intervals(dict(a=dict(fast=arm(1), minimal=arm(1))))["available"]
        )

    def test_cache_writes_change_token_ratio(self):
        pairs = {str(i): dict(fast=arm(1), minimal=arm(1)) for i in range(2)}
        for pair in pairs.values():
            pair["fast"]["cache_write_tokens"] = 100
        result = paired_intervals(pairs, draws=1000)
        self.assertEqual(result["tokens_per_success_ratio_95"], [0.5, 0.5])
        self.assertEqual(result["cost_per_success_ratio_95"], [1, 1])

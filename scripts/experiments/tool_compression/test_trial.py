import unittest
from trial import grade, summarize, answer_correct


class TrialTests(unittest.TestCase):
    def test_grade_requires_exact_answer_and_terminal(self):
        expected = {
            "error_count": 2,
            "introduced_codes": ["E0425"],
            "resolved_lines": [2],
            "new_error_lines": [9],
        }
        self.assertTrue(
            grade(
                "```json\n" + __import__("json").dumps(expected) + "\n```",
                expected,
                True,
            )
        )
        self.assertFalse(grade(__import__("json").dumps(expected), expected, False))
        self.assertFalse(grade('{"error_count":true}', {"error_count": 1}, True))
        self.assertFalse(grade("I succeeded", expected, True))

    def test_content_and_format_are_scored_separately(self):
        text = 'Explanation.\n```json\n{"error_count": 2}\n```'
        self.assertFalse(grade(text, {"error_count": 2}, True))
        self.assertTrue(answer_correct(text, {"error_count": 2}, True))
        self.assertFalse(answer_correct(text + text, {"error_count": 2}, True))

    def test_incomplete_pairs_cannot_claim_lift(self):
        with self.assertRaisesRegex(ValueError, "incomplete"):
            summarize(
                ["a"],
                [{"case": "a", "arm": "baseline", "failure": None, "success": True}],
            )

    def test_infrastructure_failure_invalidates_comparison(self):
        rows = [
            {"case": "a", "arm": a, "failure": "provider_error", "success": False}
            for a in ("baseline", "compact", "delta", "adaptive")
        ]
        self.assertFalse(summarize(["a"], rows)["comparison_valid"])


if __name__ == "__main__":
    unittest.main()

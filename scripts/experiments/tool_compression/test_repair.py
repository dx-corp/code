import tempfile
import unittest
from pathlib import Path

from repair import fixture, grade


class RepairGradeTests(unittest.TestCase):
    def test_real_hidden_tests_distinguish_correct_and_broken_source(self):
        case = fixture(0)
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            self.assertTrue(grade(case["reference"], case["tests"], root / "correct"))
            self.assertFalse(grade(case["source"], case["tests"], root / "broken"))
            self.assertFalse(
                grade(
                    "#![cfg(any())]\n" + case["source"],
                    case["tests"],
                    root / "disabled",
                )
            )
            early_exit = case["reference"].replace(
                "n / d + u64::from(n % d != 0)", "std::process::exit(0)"
            )
            self.assertFalse(grade(early_exit, case["tests"], root / "early-exit"))

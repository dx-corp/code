import json
from pathlib import Path
import tempfile
import unittest

from run import build_report, write_new_report


HERE = Path(__file__).resolve().parent
REPO_ROOT = HERE.parents[4]


class ExperimentRunnerTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.report = build_report(REPO_ROOT, HERE / "cases.json", iterations=1)

    def test_report_separates_modeled_and_measured_evidence(self):
        report = self.report
        self.assertEqual(
            report["schema"], "evalops.maestro.tool-runtime-ir-results.v1"
        )
        self.assertEqual(
            set(report),
            {
                "schema",
                "source",
                "modeled",
                "measured",
                "gates",
                "promotion_allowed",
                "limitations",
            },
        )
        self.assertIn("tool_plan", report["modeled"])
        self.assertIn("speculation", report["modeled"])
        self.assertIn("repository_graph", report["measured"])
        self.assertEqual(len(report["source"]["cases_sha256"]), 64)
        self.assertEqual(
            set(report["source"]["experiment_source_sha256"]),
            {"optimizer.py", "repo_graph.py", "run.py"},
        )
        self.assertTrue(
            all(
                len(value) == 64
                for value in report["source"]["experiment_source_sha256"].values()
            )
        )
        self.assertFalse(report["promotion_allowed"])
        self.assertIn("online", " ".join(report["limitations"]).lower())

    def test_every_gate_is_explicit_but_cannot_enable_promotion(self):
        gates = self.report["gates"]
        self.assertEqual(
            set(gates),
            {
                "tool_plan_compiler",
                "speculative_admission_safety",
                "speculative_utility",
                "repository_graph",
                "all_experimental_gates_passed",
            },
        )
        self.assertTrue(all(type(value) is bool for value in gates.values()))
        self.assertFalse(gates["speculative_utility"])
        self.assertEqual(
            gates["all_experimental_gates_passed"],
            all(value for key, value in gates.items() if key != "all_experimental_gates_passed"),
        )
        self.assertFalse(self.report["promotion_allowed"])

    def test_output_is_new_file_only_and_round_trips_exact_report(self):
        with tempfile.TemporaryDirectory() as tmp:
            output = Path(tmp) / "report.json"
            write_new_report(output, self.report)
            self.assertEqual(json.loads(output.read_text()), self.report)
            with self.assertRaises(FileExistsError):
                write_new_report(output, self.report)


if __name__ == "__main__":
    unittest.main()

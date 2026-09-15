import tempfile
import subprocess
import unittest
from pathlib import Path
from trial import CHECK, approved, cases, grade_answer, required_tool_succeeded


class TrialTests(unittest.TestCase):
    def test_agent_check_compiles_in_workspace_and_reports_syntax_errors(self):
        with tempfile.TemporaryDirectory() as tmp:
            cwd = Path(tmp)
            (cwd / "src").mkdir()
            source = cwd / "src/lib.rs"
            source.write_text("pub fn value() -> u32 { 42 }\n")
            check = cwd / "check"
            check.write_text(CHECK)
            check.chmod(0o755)
            good = subprocess.run(["./check"], cwd=cwd, capture_output=True, timeout=30)
            self.assertEqual(good.returncode, 0, good.stderr.decode())
            self.assertTrue((cwd / ".check.rlib").is_file())
            source.write_text("pub fn value() -> u32 { false }\n")
            bad = subprocess.run(["./check"], cwd=cwd, capture_output=True, timeout=30)
            self.assertNotEqual(bad.returncode, 0)
            self.assertIn(b"mismatched types", bad.stderr)

    def test_answer_requires_exact_json_types_and_unique_keys(self):
        expected = {"cause": "c", "retryable": False}
        self.assertTrue(grade_answer('{"cause":"c","retryable":false}', expected))
        self.assertFalse(grade_answer('{"cause":"c","retryable":0}', expected))
        self.assertFalse(
            grade_answer('{"cause":"bad","cause":"c","retryable":false}', expected)
        )

    def test_discovery_requires_success_and_matching_evidence(self):
        case = {"required_tool": "grep", "expected": {"path": "configs/a"}}
        calls = [{"tool": "grep", "call_id": "a"}]
        self.assertFalse(
            required_tool_succeeded(case, calls, {"a": False}, {"a": "configs/a"})
        )
        self.assertFalse(
            required_tool_succeeded(case, calls, {"b": True}, {"a": "configs/a"})
        )
        self.assertFalse(
            required_tool_succeeded(case, calls, {"a": True}, {"a": "no match"})
        )
        self.assertTrue(
            required_tool_succeeded(
                case, calls, {"a": True}, {"a": "configs/a:2:marker=x"}
            )
        )

    def test_fixed_denominator_and_strata(self):
        cs = cases()
        self.assertEqual(len({c["id"] for c in cs}), 12)
        self.assertEqual(
            {
                f: sum(c["family"] == f for c in cs)
                for f in ("repair", "investigation", "native-search")
            },
            {"repair": 4, "investigation": 4, "native-search": 4},
        )
        self.assertEqual(cs, cases())

    def test_investigation_requires_unprompted_evidence(self):
        for c in cases():
            if c["family"] != "investigation":
                continue
            cause = c["expected"]["cause"]
            self.assertNotIn(cause, c["prompt"])
            lines = c["files"]["run.log"].splitlines()
            matches = [line for line in lines if f"cause={cause}" in line]
            self.assertEqual(len(matches), 1)
            self.assertIn("tenant=orchid", matches[0])
            self.assertIn("retryable=false", matches[0])

    def test_native_search_answers_require_the_named_tool(self):
        for c in cases():
            if c["family"] != "native-search":
                continue
            self.assertIn(c["expected"]["path"], c["files"])
            self.assertIn(c["required_tool"], ("grep", "glob"))

    def test_approval_does_not_allow_compound_or_external_mutation(self):
        with tempfile.TemporaryDirectory() as d:
            cwd = Path(d)
            c = cases()[0]
            for command in (
                "./check; curl example.com",
                "rg --pre=evil x .",
                "cat x | sh",
                "$(evil)",
                "python3 evil.py",
            ):
                self.assertFalse(
                    approved({"tool": "bash", "args": {"command": command}}, cwd, c)
                )
            self.assertTrue(
                approved({"tool": "bash", "args": {"command": "./check"}}, cwd, c)
            )
            self.assertTrue(
                approved(
                    {"tool": "bash", "args": {"command": "rg -n cause run.log"}}, cwd, c
                )
            )
            self.assertFalse(
                approved({"tool": "write", "args": {"path": "../outside"}}, cwd, c)
            )
            self.assertFalse(
                approved({"tool": "write", "args": {"path": "check"}}, cwd, c)
            )
            self.assertTrue(
                approved({"tool": "write", "args": {"path": "src/lib.rs"}}, cwd, c)
            )

    def test_qualification_failure_stops_before_cohort_and_preserves_attempts(self):
        import json
        from trial import execute

        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            calls = []

            def runner(case, arm, output, binary, timeout):
                calls.append((case["id"], arm))
                return dict(
                    case=case["id"],
                    arm=arm,
                    success=False,
                    failure="provider_error",
                    tokens_complete=False,
                )

            execute(cases(), root, Path("/unused"), 1, runner=runner)
            self.assertEqual(len(calls), 2)
            self.assertFalse((root / "rows.json").exists())
            q = json.loads((root / "qualification.json").read_text())
            self.assertFalse(q["passed"])
            self.assertEqual(len(q["rows"]), 2)

    def test_quality_loss_continues_but_infrastructure_failure_stops_at_pair(self):
        import json
        from trial import execute

        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            cs = cases()[:4]
            calls = []

            def runner(case, arm, output, binary, timeout):
                qualifying = output.name == "qualification"
                calls.append((qualifying, case["id"], arm))
                broken = (
                    not qualifying and case["id"] == cs[1]["id"] and arm == "minimal"
                )
                return dict(
                    case=case["id"],
                    arm=arm,
                    success=qualifying,
                    tokens_complete=not broken,
                    failure="provider_error" if broken else None,
                )

            execute(cs, root, Path("/unused"), 1, runner=runner)
            self.assertEqual(len(calls), 8)
            rows = json.loads((root / "rows.json").read_text())
            self.assertEqual(len(rows), 4)
            self.assertEqual({r["arm"] for r in rows[2:]}, {"fast", "minimal"})
            stopped = json.loads((root / "stopped.json").read_text())
            self.assertEqual(stopped["unrun_cases"], [c["id"] for c in cs[2:]])

import contextlib
import copy
import io
import json
from pathlib import Path
import tempfile
import unittest

from evidence import load_json, source_hashes
from report import report
from adversarial import cases
from trial import grade_answer


class EvidenceTests(unittest.TestCase):
    def bundle(self, root):
        case = dict(
            id="case",
            family="investigation",
            files={"data": "original"},
            expected={"value": 7},
        )
        manifest = dict(
            schema="maestro.minimal-tools-screen.v3",
            cases=[case],
            arms=["fast", "minimal"],
            source_hashes=source_hashes(),
        )
        (root / "manifest.json").write_text(json.dumps(manifest))
        rows = []
        for arm in manifest["arms"]:
            path = root / "case" / arm / "0"
            (path / "workspace").mkdir(parents=True)
            (path / "workspace/data").write_text("original")
            (path / "workspace/answer.json").write_text('{"value":7}')
            usage = dict(
                input_tokens=10,
                cache_read_tokens=2,
                cache_write_tokens=0,
                output_tokens=3,
                total_cost_usd=0.5,
            )
            events = [
                dict(type="response_start", response_id="r1"),
                dict(type="response_end", response_id="r1", usage=usage),
                dict(type="turn_completed", response_id="done"),
            ]
            (path / "events.jsonl").write_text("\n".join(map(json.dumps, events)))
            rows.append(
                dict(
                    case="case",
                    family="investigation",
                    arm=arm,
                    selected_path=str(path),
                    success=True,
                    correct=True,
                    intact=True,
                    required_tool_used=True,
                    terminal=True,
                    failure=None,
                    elapsed_seconds=2,
                    attempts=1,
                    provider_calls=999,
                    **usage,
                )
            )
        self.save(root, rows)
        return manifest, rows

    def save(self, root, rows):
        (root / "rows.json").write_text(json.dumps(rows))

    def analyze(self, root):
        with contextlib.redirect_stdout(io.StringIO()):
            report(root)
        return json.loads((root / "analysis.json").read_text())

    def test_regrades_files_and_derives_call_counts(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            _, rows = self.bundle(root)
            result = self.analyze(root)
            self.assertTrue(result["outcomes_regraded"])
            self.assertEqual(result["aggregate"]["fast"]["provider_calls"], 1)
            self.assertEqual(result["aggregate"]["fast"]["total_tokens"], 15)
            self.assertEqual(result["aggregate"]["fast"]["cost_per_success_usd"], 0.5)
            answer = Path(rows[0]["selected_path"]) / "workspace/answer.json"
            answer.write_text('{"value":8}')
            with self.assertRaisesRegex(ValueError, "outcome mismatch"):
                self.analyze(root)
            self.assertFalse((root / "analysis.json").exists())

    def test_zero_success_does_not_become_free_success(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            _, rows = self.bundle(root)
            rows[1].update(success=False, correct=False)
            (Path(rows[1]["selected_path"]) / "workspace/answer.json").write_text(
                '{"value":8}'
            )
            self.save(root, rows)
            result = self.analyze(root)
            arm = result["aggregate"]["minimal"]
            self.assertEqual(arm["total_cost_usd"], 0.5)
            self.assertIsNone(arm["cost_per_success_usd"])
            self.assertEqual(result["candidate_only_losses"], ["case"])

    def test_interrupted_cohort_reports_missing_rows_without_intervals(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            _, rows = self.bundle(root)
            self.save(root, rows[:1])
            result = self.analyze(root)
            self.assertFalse(result["cohort_complete"])
            self.assertEqual(result["missing_rows"], [dict(case="case", arm="minimal")])
            self.assertIsNone(result["aggregate"]["minimal"]["total_cost_usd"])
            self.assertFalse(any("bootstrap" in k for k in result))
            (root / "rows.json").unlink()
            result = self.analyze(root)
            self.assertEqual(len(result["missing_rows"]), 2)

    def test_duplicate_unexpected_rows_and_source_changes_fail_closed(self):
        for mutation in ("duplicate", "foreign", "source"):
            with self.subTest(mutation=mutation), tempfile.TemporaryDirectory() as tmp:
                root = Path(tmp)
                manifest, rows = self.bundle(root)
                if mutation == "duplicate":
                    rows.append(copy.deepcopy(rows[0]))
                elif mutation == "foreign":
                    rows[0]["case"] = "foreign"
                else:
                    manifest["source_hashes"]["trial.py"] = "changed"
                    (root / "manifest.json").write_text(json.dumps(manifest))
                self.save(root, rows)
                with self.assertRaises(ValueError):
                    self.analyze(root)

    def test_missing_terminal_and_hidden_provider_failure_fail_closed(self):
        for kind in ("missing", "duplicate", "error"):
            with self.subTest(kind=kind), tempfile.TemporaryDirectory() as tmp:
                root = Path(tmp)
                _, rows = self.bundle(root)
                path = Path(rows[0]["selected_path"]) / "events.jsonl"
                events = [json.loads(x) for x in path.read_text().splitlines()]
                if kind == "missing":
                    events.pop()
                elif kind == "duplicate":
                    events.append(events[-1])
                else:
                    events.insert(1, dict(type="provider_error"))
                path.write_text("\n".join(map(json.dumps, events)))
                with self.assertRaises(ValueError):
                    self.analyze(root)

    def test_changed_evidence_cannot_be_hidden_by_correct_answer(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            _, rows = self.bundle(root)
            (Path(rows[0]["selected_path"]) / "workspace/data").write_text("altered")
            with self.assertRaisesRegex(ValueError, "intact"):
                self.analyze(root)

    def test_nonfinite_and_duplicate_json_fields_are_rejected(self):
        for text in ('{"x":NaN}', '{"x":Infinity}', '{"x":1,"x":2}'):
            with self.assertRaises(ValueError):
                load_json(text)

    def test_adversarial_cases_have_exact_oracles_and_are_deterministic(self):
        cs = cases()
        self.assertEqual(cs, cases())
        self.assertEqual(len(cs), 8)
        self.assertEqual(len({c["id"] for c in cs}), len(cs))
        for c in cs:
            with self.subTest(case=c["id"]):
                self.assertTrue(grade_answer(json.dumps(c["expected"]), c["expected"]))
                for key, value in c["expected"].items():
                    wrong = dict(c["expected"])
                    wrong[key] = (
                        not value
                        if type(value) is bool
                        else (value + 1 if type(value) is int else value + "-wrong")
                    )
                    self.assertFalse(grade_answer(json.dumps(wrong), c["expected"]))
                self.assertFalse(grade_answer("{}", c["expected"]))

    def test_repair_is_recompiled_against_hidden_tests_on_reanalysis(self):
        from trial import CHECK, cases as screen_cases

        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            manifest, rows = self.bundle(root)
            case = screen_cases()[0]
            case["id"] = "case"
            manifest["cases"] = [case]
            (root / "manifest.json").write_text(json.dumps(manifest))
            for row in rows:
                row["family"] = "repair"
                cwd = Path(row["selected_path"]) / "workspace"
                (cwd / "src").mkdir()
                (cwd / "src/lib.rs").write_text(case["reference"])
                (cwd / "check").write_text(CHECK)
            self.save(root, rows)
            self.assertTrue(self.analyze(root)["outcomes_regraded"])
            (Path(rows[0]["selected_path"]) / "workspace/src/lib.rs").write_text(
                case["files"]["src/lib.rs"]
            )
            with self.assertRaisesRegex(ValueError, "correct"):
                self.analyze(root)

    def test_real_controller_process_and_report_roundtrip(self):
        # Protocol fixture only: no inference, and no model-quality claim.
        import sys
        from trial import MODEL, run

        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            case = dict(
                id="wire",
                family="investigation",
                prompt="Write the answer.",
                files={"data": "7"},
                expected={"value": 7},
            )
            binary = root / "protocol-fixture"
            binary.write_text(f"""#!{sys.executable}
import json, pathlib, sys
def emit(event):
    print(json.dumps(event), flush=True)
emit({{"type":"ready","model":{MODEL!r}}})
for line in sys.stdin:
    event = json.loads(line)
    if event["type"] == "init":
        emit({{"type":"status","message":"init applied"}})
    elif event["type"] == "prompt":
        pathlib.Path("answer.json").write_text('{{"value":7}}')
        emit({{"type":"response_start","response_id":"r"}})
        emit({{"type":"response_end","response_id":"r","usage":{{"input_tokens":2,"cache_read_tokens":0,"cache_write_tokens":0,"output_tokens":1,"total_cost_usd":0.01}}}})
        emit({{"type":"turn_completed","response_id":"done"}})
    elif event["type"] == "shutdown":
        break
""")
            binary.chmod(0o755)
            manifest = dict(
                schema="maestro.minimal-tools-screen.v3",
                cases=[case],
                arms=["fast", "minimal"],
                source_hashes=source_hashes(),
            )
            (root / "manifest.json").write_text(json.dumps(manifest))
            with contextlib.redirect_stdout(io.StringIO()):
                rows = [run(case, arm, root, binary, 10) for arm in manifest["arms"]]
            self.save(root, rows)
            result = self.analyze(root)
            self.assertTrue(result["cohort_complete"])
            self.assertTrue(result["outcomes_regraded"])
            self.assertEqual(result["aggregate"]["minimal"]["passed"], 1)

    def test_cache_writes_are_counted_and_unknown_writes_are_unavailable(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            _, rows = self.bundle(root)
            path = Path(rows[0]["selected_path"]) / "events.jsonl"
            events = [json.loads(x) for x in path.read_text().splitlines()]
            events[1]["usage"]["cache_write_tokens"] = 30
            path.write_text("\n".join(map(json.dumps, events)))
            # Older summary rows can acquire this field from authoritative events.
            del rows[0]["cache_write_tokens"]
            self.save(root, rows)
            result = self.analyze(root)
            self.assertEqual(result["aggregate"]["fast"]["total_tokens"], 45)
            self.assertEqual(result["aggregate"]["fast"]["all_prompt_tokens"], 42)
            del events[1]["usage"]["cache_write_tokens"]
            path.write_text("\n".join(map(json.dumps, events)))
            result = self.analyze(root)
            self.assertIsNone(result["aggregate"]["fast"]["total_tokens"])
            self.assertEqual(result["aggregate"]["fast"]["tokens_complete_runs"], 0)

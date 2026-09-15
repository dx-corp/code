import contextlib
import io
import json
from pathlib import Path
import tempfile
import unittest

from report import report


class ReportTests(unittest.TestCase):
    def test_partial_stream_usage_cannot_be_reported_as_complete_cost(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            manifest = {"cases": [{"id": "a"}], "arms": ["fast", "minimal"]}
            (root / "manifest.json").write_text(json.dumps(manifest))
            usage = dict(
                input_tokens=10,
                cache_read_tokens=0,
                cache_write_tokens=0,
                output_tokens=1,
                total_cost_usd=0.01,
            )
            rows = []
            for arm in manifest["arms"]:
                path = root / arm
                path.mkdir()
                (path / "events.jsonl").write_text(
                    "\n".join(
                        json.dumps(e)
                        for e in [
                            {"type": "response_start", "response_id": "one"},
                            {
                                "type": "response_end",
                                "response_id": "one",
                                "usage": usage,
                            },
                            *(
                                [
                                    {
                                        "type": "response_end",
                                        "response_id": "done",
                                        "usage": None,
                                    }
                                ]
                                if arm == "fast"
                                else []
                            ),
                            *(
                                [{"type": "turn_completed", "response_id": "done"}]
                                if arm == "fast"
                                else []
                            ),
                        ]
                    )
                    + "\n"
                )
                rows.append(
                    dict(
                        case="a",
                        arm=arm,
                        family="repair",
                        selected_path=str(path),
                        terminal=arm == "fast",
                        failure=None if arm == "fast" else "provider_error",
                        success=arm == "fast",
                        provider_calls=1,
                        elapsed_seconds=1,
                        attempts=1,
                        **usage,
                    )
                )
            (root / "rows.json").write_text(json.dumps(rows))
            with contextlib.redirect_stdout(io.StringIO()):
                report(root)
            result = json.loads((root / "analysis.json").read_text())
            self.assertEqual(result["aggregate"]["fast"]["total_cost_usd"], 0.01)
            self.assertIsNone(result["aggregate"]["minimal"]["total_cost_usd"])
            self.assertNotIn("exploratory_paired_bootstrap_cost_ratio_95", result)
            self.assertEqual(result["candidate_only_losses"], ["a"])

            # Even a terminal success is incomplete when a later response
            # carries no usage. Earlier usage must not silently fill the gap.
            rows[1].update(terminal=True, failure=None, success=True)
            (root / "rows.json").write_text(json.dumps(rows))
            with (root / "minimal/events.jsonl").open("a") as events:
                events.write(
                    json.dumps({"type": "turn_completed", "response_id": "done"}) + "\n"
                )
                events.write(
                    json.dumps({"type": "response_start", "response_id": "two"}) + "\n"
                )
                events.write(
                    json.dumps(
                        {"type": "response_end", "response_id": "two", "usage": None}
                    )
                    + "\n"
                )
            with contextlib.redirect_stdout(io.StringIO()):
                report(root)
            result = json.loads((root / "analysis.json").read_text())
            self.assertIsNone(result["aggregate"]["minimal"]["total_cost_usd"])
            self.assertEqual(result["aggregate"]["minimal"]["usage_complete_runs"], 0)
            self.assertNotIn("exploratory_paired_bootstrap_cost_ratio_95", result)

    def test_missing_price_preserves_complete_tokens_without_inventing_dollars(self):
        from report import reconcile_row

        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            usage = dict(
                input_tokens=10,
                cache_read_tokens=4,
                cache_write_tokens=0,
                output_tokens=2,
            )
            events = [
                dict(type="response_start", response_id="r"),
                dict(type="response_end", response_id="r", usage=usage),
                dict(type="turn_completed", response_id="done"),
            ]
            (root / "events.jsonl").write_text("\n".join(map(json.dumps, events)))
            row = dict(
                case="a",
                arm="fast",
                selected_path=str(root),
                terminal=True,
                failure=None,
                total_cost_usd=None,
                **usage,
            )
            reconcile_row(row)
            self.assertTrue(row["tokens_complete"])
            self.assertFalse(row["usage_complete"])
            self.assertIsNone(row["total_cost_usd"])
            events.insert(2, dict(type="response_start", response_id="missing"))
            (root / "events.jsonl").write_text("\n".join(map(json.dumps, events)))
            reconcile_row(row)
            self.assertFalse(row["tokens_complete"])

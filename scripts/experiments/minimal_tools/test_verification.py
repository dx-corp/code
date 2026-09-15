import unittest
from unittest.mock import patch

from verification import holdout, protocol, settle, verdict


def plan(n=2):
    return dict(schema="maestro.verification-plan.v1", pairs=n,
                minimum_cost_reduction=.05, minimum_speed_reduction=.05,
                maximum_quality_loss=.1, sample_size_rationale="power simulation",
                holdout_provenance="independent owner", cache_policy="warm",
                time_window="scheduled window", stopping_rule="fixed_sample_no_optional_stopping")


class VerificationTests(unittest.TestCase):
    def fixture(self):
        rows = [dict(case="a", arm="fast", receipt=[dict(request_id="r", record_id="rec", lineage_id="l")])]
        records = [dict(request_id="r", record_id="rec", lineage_id="l", organization_id="o", workspace_id="w", provider="p", created_at="2026-09-15T00:00:00Z", attempts=[dict(ordinal=0), dict(ordinal=1)])]
        bill = dict(schema="maestro.settled-billing.v1", basis="settled_provider_charges", currency="USD", settled=True, organization_id="o", workspace_id="w", provider_account="account", source_reference="invoice-123", period_start="2026-09-01T00:00:00Z", period_end="2026-10-01T00:00:00Z", export_sha256="a"*64, complete_scope=True, scoped_total_usd="0.3", lines=[dict(request_id="r", record_id="rec", attempt_ordinal=i, provider="p", provider_request_id=f"provider-{i}", line_id=f"line-{i}", net_charge_usd=value) for i, value in enumerate(("0.1", "0.2"))])
        return rows, records, bill

    def settle(self, rows, records, bill):
        with patch("billing.receipts", side_effect=lambda row: row["receipt"]):
            return settle(rows, records, bill, "o", "w")

    def test_retries_included_with_decimal_totals(self):
        self.assertEqual(self.settle(*self.fixture()), ["0.3"])

    def test_missing_attempt_cannot_be_free(self):
        rows, records, bill = self.fixture()
        bill["lines"].pop()
        bill["scoped_total_usd"] = "0.1"
        with self.assertRaisesRegex(ValueError, "incomplete"):
            self.settle(rows, records, bill)

    def test_rejects_wrong_tenant_estimates_unsettled_and_missing_scope(self):
        for key, value in (("organization_id", "other"), ("basis", "published_list_price_estimate"), ("settled", False), ("complete_scope", False), ("currency", "EUR")):
            rows, records, bill = self.fixture()
            bill[key] = value
            with self.subTest(key=key), self.assertRaises(ValueError):
                self.settle(rows, records, bill)

    def test_duplicates_and_totals(self):
        for mutation in (lambda b: b["lines"].append(b["lines"][0]), lambda b: b.update(scoped_total_usd="9"), lambda b: b["lines"][1].update(provider_request_id="provider-0"), lambda b: b["lines"][0].update(net_charge_usd="NaN")):
            rows, records, bill = self.fixture()
            mutation(bill)
            with self.assertRaises(ValueError):
                self.settle(rows, records, bill)

    def test_missing_native_record(self):
        rows, records, bill = self.fixture()
        records[0]["request_id"] = "different"
        with self.assertRaisesRegex(ValueError, "native gateway"):
            self.settle(rows, records, bill)

    def test_native_lineage_mismatch_cannot_move_spend(self):
        rows, records, bill = self.fixture()
        rows.append(dict(receipt=[dict(request_id="other", record_id="other-rec", lineage_id="other-line")]))
        records[0]["lineage_id"] = "other-line"
        with self.assertRaisesRegex(ValueError, "native gateway"):
            self.settle(rows, records, bill)

    def test_zero_attempt_requires_explicit_zero_charge(self):
        rows, records, bill = self.fixture()
        records[0]["attempts"] = []
        bill["lines"] = [dict(bill["lines"][0], attempt_ordinal=None, net_charge_usd="0", non_execution_confirmed=True)]
        bill["scoped_total_usd"] = "0"
        self.assertEqual(self.settle(rows, records, bill), ["0"])
        bill["lines"][0]["non_execution_confirmed"] = False
        with self.assertRaises(ValueError):
            self.settle(rows, records, bill)

    def test_holdout_rejects_traversal_duplicate_and_answer_leak(self):
        case = dict(id="a", family="investigation", prompt="find x", files={"x.txt":"x"}, expected={"x":1})
        self.assertEqual(holdout([case]), [case])
        for bad in ([case, case], [dict(case, id="../a")], [dict(case, files={"../outside":"x"})], [dict(case, files={"answer.json":"x"})]):
            with self.assertRaises(ValueError):
                holdout(bad)

    def cohort(self, n=2):
        manifest = dict(cases=[dict(id=str(i)) for i in range(n)], verification_plan=plan(n), order=[(str(i), ["fast", "minimal"]) for i in range(n)])
        rows = [dict(case=str(i), arm=a, success=True, elapsed_seconds=10 if a=="fast" else 5) for i in range(n) for a in ("fast", "minimal")]
        return manifest, rows

    def test_no_plan_or_amendment_never_verifies(self):
        manifest, rows = self.cohort()
        self.assertFalse(verdict(manifest, rows, ["1"]*4, amended=True)["faster_verified"])
        manifest.pop("verification_plan")
        self.assertFalse(verdict(manifest, rows, ["1"]*4)["cheaper_verified"])

    def test_small_perfect_study_does_not_prove_quality(self):
        manifest, rows = self.cohort()
        value = verdict(manifest, rows, ["1", ".5"]*2)
        self.assertFalse(value["quality_preserved"])
        self.assertFalse(value["faster_verified"])

    def test_independent_speed_and_cost_verdicts(self):
        manifest, rows = self.cohort()
        bounds = dict(cost=None, latency=[.4,.6], quality=[-.01,.01])
        with patch("verification.intervals", return_value=bounds):
            result = verdict(manifest, rows, None)
        self.assertTrue(result["faster_verified"])
        self.assertFalse(result["cheaper_verified"])
        self.assertFalse(result["billing_verified"])

    def test_complete_study_can_pass_without_production_claim(self):
        manifest, rows = self.cohort()
        bounds = dict(cost=[.4,.6], latency=[.4,.6], quality=[-.01,.01])
        with patch("verification.intervals", return_value=bounds):
            result = verdict(manifest, rows, ["1", ".5"]*2)
        self.assertTrue(result["cheaper_verified"])
        self.assertTrue(result["faster_verified"])
        self.assertFalse(result["production_verified"])

    def test_actual_intervals_allow_large_clear_study(self):
        manifest, rows = self.cohort(100)
        result = verdict(manifest, rows, ["1", ".5"]*100)
        self.assertTrue(result["quality_preserved"])
        self.assertTrue(result["cheaper_verified"])
        self.assertTrue(result["faster_verified"])

    def test_missing_reordered_rows_and_bad_plan_rejected(self):
        manifest, rows = self.cohort()
        self.assertIn("incomplete or reordered cohort", verdict(manifest, rows[::-1], None)["reasons"])
        manifest["verification_plan"]["pairs"] = 3
        with self.assertRaises(ValueError):
            verdict(manifest, rows, None)

class VerificationCliTests(unittest.TestCase):
    def test_fixture_cli_freezes_plan_and_reports_unrun_study(self):
        import json
        from pathlib import Path
        import subprocess
        import sys
        import tempfile
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            cases = [dict(id=name, family="investigation", prompt="Return x in answer.json", files={"x":"1"}, expected={"x":1}) for name in ("one", "two")]
            (root/"holdout.json").write_text(json.dumps(cases))
            (root/"plan.json").write_text(json.dumps(plan()))
            here = Path(__file__).parent
            result = subprocess.run([sys.executable, str(here/"trial.py"), "--holdout", str(root/"holdout.json"), "--verification-plan", str(root/"plan.json"), "--output", str(root/"study")], capture_output=True, text=True)
            self.assertEqual(result.returncode, 0, result.stderr)
            manifest = json.loads((root/"study/manifest.json").read_text())
            self.assertEqual(manifest["verification_plan"], plan())
            args = [sys.executable, str(here/"verification.py"), str(root/"study"), "--organization", "o", "--workspace", "w", "--output", str(root/"result.json")]
            result = subprocess.run(args, capture_output=True, text=True)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertFalse(json.loads((root/"result.json").read_text())["faster_verified"])
            manifest["source_hashes"]["trial.py"] = "bad"
            (root/"study/manifest.json").write_text(json.dumps(manifest))
            result = subprocess.run(args, capture_output=True, text=True)
            self.assertNotEqual(result.returncode, 0)
            self.assertFalse((root/"result.json").exists())


if __name__ == "__main__":
    unittest.main()

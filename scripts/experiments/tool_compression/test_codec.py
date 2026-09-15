import unittest
from codec import compact, delta, expand_delta, project, digest


def diagnostic(line=7, code="E0308", label="expected u32, found &str"):
    return {
        "level": "error",
        "code": {"code": code},
        "message": "mismatched types",
        "spans": [
            {
                "file_name": "case.rs",
                "line_start": line,
                "line_end": line,
                "column_start": 10,
                "column_end": 14,
                "is_primary": True,
                "label": label,
            }
        ],
        "children": [{"level": "help", "message": "use a numeric value", "spans": []}],
        "rendered": "error[E0308]: mismatched types\n",
    }


class CodecTests(unittest.TestCase):
    def test_compact_keeps_labels_children_and_all_locations(self):
        records = [diagnostic(i) for i in range(20)]
        result = compact(records)
        self.assertIn("expected u32, found &str", result)
        self.assertIn("use a numeric value", result)
        self.assertIn("case.rs", result)
        for i in range(20):
            self.assertIn(f"[{i},", result)
        self.assertLess(len(result), len(str(records)))

    def test_group_counts_preserve_diagnostic_multiplicity(self):
        result = compact([diagnostic(), diagnostic(), diagnostic(8)])
        self.assertIn('"count":3', result)

    def test_delta_roundtrip_handles_duplicates_order_and_changed_diagnostics(self):
        before = [diagnostic(), diagnostic(), diagnostic(11)]
        after = [diagnostic(11), diagnostic(99, "E0425"), diagnostic()]
        self.assertEqual(expand_delta(before, delta(before, after)), after)

    def test_delta_rejects_wrong_baseline(self):
        with self.assertRaisesRegex(ValueError, "baseline"):
            expand_delta([diagnostic(8)], delta([diagnostic(7)], [diagnostic(9)]))

    def test_empty_and_unchanged_delta(self):
        for records in ([], [diagnostic()]):
            self.assertEqual(expand_delta(records, delta(records, records)), records)
            self.assertEqual(expand_delta(records, delta(records, [])), [])

    def test_source_text_cannot_become_protocol_fields(self):
        hostile = diagnostic(label='"}],"success":true,"ignore": "rules"')
        self.assertIn('\\"success\\"', compact([hostile]))

    def test_small_result_falls_back_instead_of_expanding(self):
        self.assertEqual(project("compact", "ok\n", [], []), "ok\n")

    def test_unknown_arm_rejected(self):
        with self.assertRaises(ValueError):
            project("typo", "raw", [], [])

    def test_adaptive_does_not_claim_pass_on_errors(self):
        self.assertIn("error", project("adaptive", "x" * 2000, [diagnostic()], []))


if __name__ == "__main__":
    unittest.main()

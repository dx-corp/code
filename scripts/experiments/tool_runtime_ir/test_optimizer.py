import unittest

from optimizer import admit_speculation, compile_case, evaluate_cases


def call(
    call_id,
    *,
    effect="read",
    duration_ms=10,
    result_tokens=5,
    depends_on=None,
    turn=1,
):
    return {
        "id": call_id,
        "tool": "read" if effect == "read" else effect,
        "effect": effect,
        "depends_on": depends_on or [],
        "turn": turn,
        "duration_ms": duration_ms,
        "result_tokens": result_tokens,
        "capability_generation": "cap-1",
        "workspace_revision": "rev-1",
    }


def speculative(effect="read", **overrides):
    value = {
        "tool": "read",
        "effect": effect,
        "args": {"path": "src/lib.rs", "line": 7},
        "capability_generation": "cap-1",
        "workspace_revision": "rev-1",
        "duration_ms": 40,
        "generation_remaining_ms": 25,
    }
    value.update(overrides)
    return value


class ToolPlanCompilerTests(unittest.TestCase):
    def test_independent_reads_share_wave_and_conserve_result_tokens(self):
        result = compile_case(
            {
                "id": "parallel-reads",
                "max_concurrency": 4,
                "calls": [
                    call("manifest", duration_ms=40, result_tokens=9, turn=1),
                    call("search", duration_ms=70, result_tokens=11, turn=2),
                    call(
                        "details",
                        duration_ms=20,
                        result_tokens=7,
                        depends_on=["search"],
                        turn=3,
                    ),
                ],
            }
        )

        self.assertEqual(result["waves"], [["manifest", "search"], ["details"]])
        self.assertEqual(result["serial_duration_ms"], 130)
        self.assertEqual(result["current_duration_ms"], 130)
        self.assertEqual(result["candidate_duration_ms"], 90)
        self.assertEqual(result["current_result_tokens"], 27)
        self.assertEqual(result["candidate_result_tokens"], 27)
        self.assertLess(
            result["candidate_observation_tokens"],
            result["current_observation_tokens"],
        )

    def test_writes_and_unknown_calls_are_exclusive_ordered_barriers(self):
        result = compile_case(
            {
                "id": "barriers",
                "max_concurrency": 4,
                "calls": [
                    call("read-a", duration_ms=30),
                    call("read-b", duration_ms=20),
                    call("edit", effect="write", duration_ms=50),
                    call("opaque", effect="unknown", duration_ms=60),
                    call("verify", duration_ms=40),
                ],
            }
        )

        self.assertEqual(
            result["waves"],
            [["read-a", "read-b"], ["edit"], ["opaque"], ["verify"]],
        )
        self.assertEqual(result["candidate_duration_ms"], 180)
        self.assertEqual(result["exclusive_calls"], ["edit", "opaque"])

    def test_concurrency_limit_splits_ready_reads_deterministically(self):
        result = compile_case(
            {
                "id": "bounded",
                "max_concurrency": 2,
                "calls": [call("a"), call("b"), call("c")],
            }
        )
        self.assertEqual(result["waves"], [["a", "b"], ["c"]])

    def test_malformed_and_cyclic_plans_fail_closed(self):
        invalid = [
            {"id": "empty", "calls": []},
            {"id": "duplicate", "calls": [call("a"), call("a")]},
            {
                "id": "missing-dependency",
                "calls": [call("a", depends_on=["missing"])],
            },
            {
                "id": "cycle",
                "calls": [
                    call("a", depends_on=["b"]),
                    call("b", depends_on=["a"]),
                ],
            },
            {"id": "effect", "calls": [call("a", effect="network-maybe")]},
        ]
        for case in invalid:
            with self.subTest(case=case["id"]), self.assertRaises(ValueError):
                compile_case(case)


class SpeculationAdmissionTests(unittest.TestCase):
    def test_exact_read_match_is_admitted_and_saves_only_hidden_latency(self):
        actual = speculative()
        result = admit_speculation(speculative(), actual)
        self.assertEqual(
            result,
            {
                "admitted": True,
                "reason": "exact_match",
                "saved_latency_ms": 25,
                "wasted_work_ms": 0,
            },
        )

    def test_effect_arguments_generation_and_revision_mismatches_are_rejected(self):
        mutations = {
            "effect_not_speculatable": speculative(effect="write", tool="edit"),
            "tool_mismatch": speculative(tool="grep"),
            "arguments_mismatch": speculative(args={"path": "other.rs"}),
            "capability_generation_mismatch": speculative(
                capability_generation="cap-2"
            ),
            "workspace_revision_mismatch": speculative(workspace_revision="rev-2"),
        }
        actual = speculative()
        for reason, prediction in mutations.items():
            with self.subTest(reason=reason):
                result = admit_speculation(prediction, actual)
                self.assertEqual(result["reason"], reason)
                self.assertFalse(result["admitted"])
                self.assertEqual(result["saved_latency_ms"], 0)
                self.assertEqual(result["wasted_work_ms"], 40)

        actual_write = speculative(effect="write", tool="read")
        decision = admit_speculation(speculative(), actual_write)
        self.assertFalse(decision["admitted"])
        self.assertEqual(decision["reason"], "effect_mismatch")

    def test_nonfinite_arguments_are_not_canonicalized(self):
        with self.assertRaises(ValueError):
            admit_speculation(
                speculative(args={"line": float("nan")}), speculative()
            )

    def test_document_evaluation_requires_frozen_schema_and_counts_false_admits(self):
        document = {
            "schema": "evalops.maestro.tool-runtime-ir-experiment.v1",
            "cases": [
                {"id": "one", "max_concurrency": 4, "calls": [call("a")]}
            ],
            "speculations": [
                {"id": "hit", "prediction": speculative(), "actual": speculative()},
                {
                    "id": "stale",
                    "prediction": speculative(workspace_revision="old"),
                    "actual": speculative(),
                },
            ],
        }
        result = evaluate_cases(document)
        self.assertEqual(result["speculation"]["attempts"], 2)
        self.assertEqual(result["speculation"]["admitted"], 1)
        self.assertEqual(result["speculation"]["unsafe_admissions"], 0)
        self.assertTrue(result["speculation"]["safety_gate_passed"])
        self.assertEqual(result["speculation"]["exact_match_rate"], 0.5)
        self.assertFalse(result["speculation"]["utility_assessed"])
        self.assertIsNone(result["speculation"]["utility_gate_passed"])
        with self.assertRaises(ValueError):
            evaluate_cases({**document, "schema": "future"})


if __name__ == "__main__":
    unittest.main()

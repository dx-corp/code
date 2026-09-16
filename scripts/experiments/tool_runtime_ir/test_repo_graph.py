import tempfile
from pathlib import Path
import unittest

from repo_graph import RustSymbolGraph, compare_queries


class RustSymbolGraphTests(unittest.TestCase):
    def fixture(self, root):
        source = root / "packages/runtime/src"
        source.mkdir(parents=True)
        (source / "alpha.rs").write_text(
            "pub struct Needle;\n"
            "pub fn construct() -> Needle { Needle }\n",
            encoding="utf-8",
        )
        (source / "beta.rs").write_text(
            "use crate::alpha::Needle;\n"
            + "\n".join(f"fn caller_{i}() {{ let _ = Needle; }}" for i in range(30))
            + "\n",
            encoding="utf-8",
        )
        ignored = root / "vendor/copied"
        ignored.mkdir(parents=True)
        (ignored / "wrong.rs").write_text("pub struct Needle;\n", encoding="utf-8")
        return source

    def test_definitions_references_and_ignored_trees_are_indexed_by_revision(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            source = self.fixture(root)
            graph = RustSymbolGraph.build(root)
            result = graph.query("Needle")

            self.assertEqual(
                result["definitions"],
                [
                    {
                        "path": "packages/runtime/src/alpha.rs",
                        "line": 1,
                        "kind": "struct",
                    }
                ],
            )
            self.assertEqual(
                result["referencing_files"],
                [
                    "packages/runtime/src/alpha.rs",
                    "packages/runtime/src/beta.rs",
                ],
            )
            self.assertEqual(len(result["revision"]), 64)

            before = graph.revision
            (source / "alpha.rs").write_text(
                "pub struct Needle(u8);\n", encoding="utf-8"
            )
            self.assertNotEqual(before, RustSymbolGraph.build(root).revision)

    def test_compare_uses_real_rg_and_reports_recall_payload_and_warm_latency(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            self.fixture(root)
            result = compare_queries(
                root,
                [
                    {
                        "symbol": "Needle",
                        "expected_path": "packages/runtime/src/alpha.rs",
                    }
                ],
                iterations=3,
            )

            self.assertEqual(result["query_count"], 1)
            self.assertEqual(result["recall_at_1"], 1.0)
            self.assertGreater(result["baseline_response_bytes"], 0)
            self.assertLess(
                result["candidate_response_bytes"], result["baseline_response_bytes"]
            )
            self.assertGreater(result["response_byte_reduction"], 0)
            self.assertGreaterEqual(result["index_build_ms"], 0)
            self.assertGreaterEqual(result["baseline_median_query_ms"], 0)
            self.assertGreaterEqual(result["candidate_median_query_ms"], 0)
            self.assertGreaterEqual(result["amortized_query_break_even"], 1)
            self.assertGreater(result["warm_query_speedup"], 0)
            self.assertTrue(result["queries"][0]["recalled_at_1"])

    def test_query_and_comparison_reject_invalid_symbols_and_oracles(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            self.fixture(root)
            graph = RustSymbolGraph.build(root)
            for symbol in ("", "Needle.*", "a/b"):
                with self.subTest(symbol=symbol), self.assertRaises(ValueError):
                    graph.query(symbol)
            with self.assertRaises(ValueError):
                compare_queries(
                    root,
                    [{"symbol": "Needle", "expected_path": "missing.rs"}],
                )

    def test_external_symlinks_and_oracle_traversal_are_rejected(self):
        with tempfile.TemporaryDirectory() as tmp:
            parent = Path(tmp)
            root = parent / "repo"
            source = self.fixture(root)
            outside = parent / "outside.rs"
            outside.write_text("pub struct Escaped;\n", encoding="utf-8")
            (source / "external.rs").symlink_to(outside)

            self.assertEqual(RustSymbolGraph.build(root).query("Escaped")["definitions"], [])
            with self.assertRaises(ValueError):
                compare_queries(
                    root,
                    [{"symbol": "Needle", "expected_path": "../outside.rs"}],
                )


if __name__ == "__main__":
    unittest.main()

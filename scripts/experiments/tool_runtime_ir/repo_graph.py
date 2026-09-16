"""Revision-addressed Rust symbol localization experiment.

This intentionally small parser measures a possible SDK response shape. It is
not a Rust parser and is never used to authorize or edit source code.
"""

from collections import defaultdict
import hashlib
import json
import math
from pathlib import Path
import re
import statistics
import subprocess
import time


IDENTIFIER = re.compile(r"^[A-Za-z_][A-Za-z0-9_]*$")
IDENTIFIERS = re.compile(r"[A-Za-z_][A-Za-z0-9_]*")
DEFINITION = re.compile(
    r"^\s*(?:pub(?:\([^)]*\))?\s+)?"
    r"(?:(?:async|unsafe|const)\s+)*"
    r"(?P<kind>struct|enum|trait|fn|type|mod|const|static)\s+"
    r"(?P<name>[A-Za-z_][A-Za-z0-9_]*)"
)
IGNORED_PARTS = frozenset(
    (".git", "generated", "gen", "node_modules", "target", "vendor")
)


def _source_paths(root):
    return [
        path
        for path in sorted(root.rglob("*.rs"))
        if not any(part in IGNORED_PARTS for part in path.relative_to(root).parts)
        and not path.is_symlink()
        and path.resolve().is_relative_to(root)
    ]


class RustSymbolGraph:
    def __init__(self, root, revision, definitions, references):
        self.root = root
        self.revision = revision
        self._definitions = definitions
        self._references = references

    @classmethod
    def build(cls, root):
        root = Path(root).resolve()
        if not root.is_dir():
            raise ValueError("repository root must be a directory")
        sources = _source_paths(root)
        if not sources:
            raise ValueError("repository root contains no eligible Rust source")
        digest = hashlib.sha256()
        contents = {}
        definitions = defaultdict(list)
        for path in sources:
            relative = path.relative_to(root).as_posix()
            data = path.read_bytes()
            digest.update(relative.encode())
            digest.update(b"\0")
            digest.update(data)
            digest.update(b"\0")
            try:
                text = data.decode("utf-8")
            except UnicodeDecodeError as error:
                raise ValueError(f"Rust source is not UTF-8: {relative}") from error
            contents[relative] = text
            for line_number, line in enumerate(text.splitlines(), start=1):
                match = DEFINITION.match(line)
                if match:
                    definitions[match.group("name")].append(
                        {
                            "path": relative,
                            "line": line_number,
                            "kind": match.group("kind"),
                        }
                    )
        known_symbols = set(definitions)
        references = defaultdict(set)
        for relative, text in contents.items():
            for symbol in set(IDENTIFIERS.findall(text)) & known_symbols:
                references[symbol].add(relative)
        return cls(
            root,
            digest.hexdigest(),
            {
                symbol: sorted(items, key=lambda item: (item["path"], item["line"]))
                for symbol, items in definitions.items()
            },
            {symbol: sorted(paths) for symbol, paths in references.items()},
        )

    def query(self, symbol):
        if not isinstance(symbol, str) or not IDENTIFIER.fullmatch(symbol):
            raise ValueError("symbol must be one Rust identifier")
        return {
            "symbol": symbol,
            "revision": self.revision,
            "definitions": self._definitions.get(symbol, []),
            "referencing_files": self._references.get(symbol, []),
        }


def _rg(root, symbol):
    command = [
        "rg",
        "--no-config",
        "-n",
        "-w",
        "--glob",
        "*.rs",
    ]
    for ignored in sorted(IGNORED_PARTS):
        command.extend(("--glob", f"!**/{ignored}/**"))
    command.extend((symbol, "."))
    started = time.perf_counter_ns()
    process = subprocess.run(
        command,
        cwd=root,
        check=False,
        capture_output=True,
        text=False,
    )
    elapsed_ms = (time.perf_counter_ns() - started) / 1_000_000
    if process.returncode not in (0, 1):
        detail = process.stderr.decode("utf-8", errors="replace").strip()
        raise RuntimeError(f"ripgrep failed: {detail}")
    return process.stdout, elapsed_ms


def _encoded(value):
    return json.dumps(
        value, ensure_ascii=False, separators=(",", ":"), sort_keys=True
    ).encode()


def compare_queries(root, queries, *, iterations=5):
    root = Path(root).resolve()
    _iterations = iterations
    if type(_iterations) is not int or not 1 <= _iterations <= 100:
        raise ValueError("iterations must be between 1 and 100")
    if not isinstance(queries, list) or not queries:
        raise ValueError("queries must be a nonempty list")
    seen = set()
    for query in queries:
        if not isinstance(query, dict) or set(query) != {"symbol", "expected_path"}:
            raise ValueError("query fields do not match the v1 schema")
        symbol = query["symbol"]
        expected = query["expected_path"]
        if not isinstance(symbol, str) or not IDENTIFIER.fullmatch(symbol):
            raise ValueError("query symbol must be one Rust identifier")
        if symbol in seen:
            raise ValueError("query symbols must be unique")
        seen.add(symbol)
        if (
            not isinstance(expected, str)
            or not expected
            or Path(expected).is_absolute()
            or ".." in Path(expected).parts
            or not (root / expected).is_file()
        ):
            raise ValueError(f"query oracle does not exist: {expected}")

    build_started = time.perf_counter_ns()
    graph = RustSymbolGraph.build(root)
    build_ms = (time.perf_counter_ns() - build_started) / 1_000_000
    results = []
    baseline_times = []
    candidate_times = []
    for query in queries:
        baseline_payload = b""
        candidate = None
        for _ in range(iterations):
            baseline_payload, baseline_ms = _rg(root, query["symbol"])
            baseline_times.append(baseline_ms)
            started = time.perf_counter_ns()
            candidate = graph.query(query["symbol"])
            candidate_payload = _encoded(candidate)
            candidate_times.append((time.perf_counter_ns() - started) / 1_000_000)
        definitions = candidate["definitions"]
        recalled = bool(definitions) and definitions[0]["path"] == query["expected_path"]
        results.append(
            {
                "symbol": query["symbol"],
                "expected_path": query["expected_path"],
                "recalled_at_1": recalled,
                "baseline_response_bytes": len(baseline_payload),
                "candidate_response_bytes": len(candidate_payload),
                "candidate_definition_count": len(definitions),
                "candidate_reference_file_count": len(candidate["referencing_files"]),
            }
        )
    baseline_bytes = sum(item["baseline_response_bytes"] for item in results)
    candidate_bytes = sum(item["candidate_response_bytes"] for item in results)
    reduction = (baseline_bytes - candidate_bytes) / baseline_bytes if baseline_bytes else 0.0
    recall = sum(item["recalled_at_1"] for item in results) / len(results)
    baseline_median = statistics.median(baseline_times)
    candidate_median = statistics.median(candidate_times)
    warm_savings_ms = baseline_median - candidate_median
    return {
        "repository_revision": graph.revision,
        "query_count": len(results),
        "recall_at_1": recall,
        "baseline_response_bytes": baseline_bytes,
        "candidate_response_bytes": candidate_bytes,
        "response_byte_reduction": reduction,
        "baseline_estimated_prompt_tokens": math.ceil(baseline_bytes / 4),
        "candidate_estimated_prompt_tokens": math.ceil(candidate_bytes / 4),
        "index_build_ms": build_ms,
        "baseline_median_query_ms": baseline_median,
        "candidate_median_query_ms": candidate_median,
        "warm_query_speedup": baseline_median / candidate_median
        if candidate_median
        else None,
        "amortized_query_break_even": math.ceil(build_ms / warm_savings_ms)
        if warm_savings_ms > 0
        else None,
        "graph_gate_passed": recall == 1.0 and reduction >= 0.50,
        "queries": results,
    }

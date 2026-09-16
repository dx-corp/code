#!/usr/bin/env python3
"""Run offline Maestro tool-runtime experiments and write one immutable report."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess

from optimizer import evaluate_cases
from repo_graph import compare_queries


RESULT_SCHEMA = "evalops.maestro.tool-runtime-ir-results.v1"
SYMBOL_QUERIES = [
    {
        "symbol": "NativeReadOnlyToolCall",
        "expected_path": "packages/runtime-rs/src/agent/native_host.rs",
    },
    {
        "symbol": "QueuedReadOnlyToolExecution",
        "expected_path": "packages/runtime-rs/src/agent/native/read_only_tools.rs",
    },
    {
        "symbol": "ToolOutputReference",
        "expected_path": "packages/context-rs/src/compaction.rs",
    },
    {
        "symbol": "ToolResponseCoordinator",
        "expected_path": "packages/runtime-rs/src/tool_responses.rs",
    },
    {
        "symbol": "IndexerConfig",
        "expected_path": "packages/workspace-rs/src/files/indexer.rs",
    },
    {
        "symbol": "ExperimentAssignment",
        "expected_path": "packages/runtime-contracts-rs/src/experiments.rs",
    },
]


def _experiment_source_hashes():
    here = Path(__file__).resolve().parent
    return {
        name: hashlib.sha256((here / name).read_bytes()).hexdigest()
        for name in ("optimizer.py", "repo_graph.py", "run.py")
    }


def _load_json(path):
    def unique(pairs):
        value = {}
        for key, item in pairs:
            if key in value:
                raise ValueError(f"duplicate JSON key: {key}")
            value[key] = item
        return value

    return json.loads(path.read_text(encoding="utf-8"), object_pairs_hook=unique)


def _git_head(repo_root):
    process = subprocess.run(
        ["git", "rev-parse", "HEAD"],
        cwd=repo_root,
        check=False,
        capture_output=True,
        text=True,
    )
    head = process.stdout.strip()
    if process.returncode != 0 or len(head) != 40:
        raise ValueError("repo root must be a Git checkout with a resolved HEAD")
    return head


def build_report(repo_root, cases_path, *, iterations=5):
    repo_root = Path(repo_root).resolve()
    cases_path = Path(cases_path).resolve()
    maestro_root = repo_root / "products/maestro"
    if not maestro_root.is_dir():
        raise ValueError("repo root does not contain products/maestro")
    if not cases_path.is_file():
        raise ValueError("cases file does not exist")
    raw_cases = cases_path.read_bytes()
    modeled = evaluate_cases(_load_json(cases_path))
    graph = compare_queries(maestro_root, SYMBOL_QUERIES, iterations=iterations)
    gates = {
        "tool_plan_compiler": modeled["tool_plan"]["compiler_gate_passed"],
        "speculative_admission_safety": modeled["speculation"][
            "safety_gate_passed"
        ],
        "speculative_utility": modeled["speculation"]["utility_gate_passed"]
        is True,
        "repository_graph": graph["graph_gate_passed"],
    }
    gates["all_experimental_gates_passed"] = all(gates.values())
    return {
        "schema": RESULT_SCHEMA,
        "source": {
            "repo_head": _git_head(repo_root),
            "cases_file": cases_path.name,
            "cases_sha256": hashlib.sha256(raw_cases).hexdigest(),
            "experiment_source_sha256": _experiment_source_hashes(),
            "repository_revision": graph["repository_revision"],
        },
        "modeled": modeled,
        "measured": {"repository_graph": graph},
        "gates": gates,
        "promotion_allowed": False,
        "limitations": [
            "Tool-plan durations and token counts are representative replay-model inputs, not production measurements.",
            "Repository timings measure a warm local checkout and do not include model generation or provider latency.",
            "The symbol prototype recognizes common Rust definitions but is not a complete Rust parser.",
            "No online paired task-success experiment has been run, so production promotion is prohibited.",
        ],
    }


def write_new_report(path, report):
    path = Path(path)
    if not path.parent.is_dir():
        raise ValueError("output parent directory does not exist")
    descriptor = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    with os.fdopen(descriptor, "w", encoding="utf-8") as output:
        json.dump(report, output, ensure_ascii=False, indent=2, sort_keys=True)
        output.write("\n")


def main(argv=None):
    here = Path(__file__).resolve().parent
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo-root", type=Path, required=True)
    parser.add_argument("--cases", type=Path, default=here / "cases.json")
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--iterations", type=int, default=5)
    args = parser.parse_args(argv)
    report = build_report(args.repo_root, args.cases, iterations=args.iterations)
    write_new_report(args.output, report)
    print(json.dumps({"output": str(args.output), "gates": report["gates"]}))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

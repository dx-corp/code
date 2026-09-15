"""Reconcile every selected native receipt before reporting this small screen."""

import json
import math
from collections import Counter
from pathlib import Path
import random
import statistics
import sys

FIELDS = ("input_tokens", "cache_read_tokens", "output_tokens", "total_cost_usd")


def reconcile_row(row):
    path = Path(row["selected_path"])
    events = [json.loads(x) for x in (path / "events.jsonl").read_text().splitlines()]
    started = [
        e.get("response_id") for e in events if e.get("type") == "response_start"
    ]
    # Native commands.rs emits response_end(done, None) as a turn marker,
    # followed by turn_completed(done). It is not a provider response.
    done_terminal = any(
        e.get("type") == "turn_completed" and e.get("response_id") == "done"
        for e in events
    )
    synthetic = [
        e
        for e in events
        if e.get("type") == "response_end"
        and e.get("response_id") == "done"
        and e.get("usage") is None
        and "done" not in started
        and done_terminal
    ]
    all_ends = [
        e for e in events if e.get("type") == "response_end" and e not in synthetic
    ]
    ends = [
        e
        for e in events
        if e.get("type") == "response_end" and e.get("usage") is not None
    ]
    for field in FIELDS:
        values = [e["usage"].get(field) for e in ends]
        reconciled = (
            sum(values)
            if values and all(type(x) in (int, float) and x >= 0 for x in values)
            else None
        )
        assert reconciled == row[field], (row["case"], row["arm"], field)
    row["discovery_calls"] = sum(
        e.get("type") == "tool_call" and e.get("tool") == "tool_search" for e in events
    )
    row["denied_or_failed_tool_calls"] = sum(
        e.get("type") == "tool_end" and e.get("success") is False for e in events
    )
    # A failed stream can have usage from earlier calls, but no final usage
    # for the failed call. Do not mistake that partial subtotal for cost.
    row["tokens_complete"] = (
        row["terminal"]
        and row["failure"] is None
        and bool(all_ends)
        and len(synthetic) <= 1
        and all(isinstance(rid, str) and rid for rid in started)
        and all(count == 1 for count in Counter(started).values())
        and Counter(started) == Counter(e.get("response_id") for e in all_ends)
        and all(
            isinstance(e.get("usage"), dict)
            and all(
                type(e["usage"].get(f)) in (int, float)
                and math.isfinite(e["usage"][f])
                and e["usage"][f] >= 0
                for f in FIELDS[:-1]
            )
            for e in all_ends
        )
    )
    row["usage_complete"] = row["tokens_complete"] and all(
        type(e["usage"].get("total_cost_usd")) in (int, float)
        and math.isfinite(e["usage"]["total_cost_usd"])
        and e["usage"]["total_cost_usd"] >= 0
        for e in all_ends
    )
    return row


def report(root):
    manifest = json.loads((root / "manifest.json").read_text())
    rows = json.loads((root / "rows.json").read_text())
    expected = {(c["id"], a) for c in manifest["cases"] for a in manifest["arms"]}
    assert {(r["case"], r["arm"]) for r in rows} == expected and len(rows) == len(
        expected
    )
    for row in rows:
        reconcile_row(row)

    def aggregate(selected):
        out = {}
        for arm in manifest["arms"]:
            rs = [r for r in selected if r["arm"] == arm]
            sums = {
                f: sum(r[f] for r in rs)
                if all(
                    r[f] is not None
                    and r[
                        "usage_complete" if f == "total_cost_usd" else "tokens_complete"
                    ]
                    for r in rs
                )
                else None
                for f in FIELDS
            }
            out[arm] = {
                "runs": len(rs),
                "passed": sum(r["success"] for r in rs),
                "usage_complete_runs": sum(r["usage_complete"] for r in rs),
                "tokens_complete_runs": sum(r["tokens_complete"] for r in rs),
                **sums,
                "all_prompt_tokens": sums["input_tokens"] + sums["cache_read_tokens"]
                if sums["input_tokens"] is not None
                and sums["cache_read_tokens"] is not None
                else None,
                "provider_calls": sum(r["provider_calls"] for r in rs),
                "discovery_calls": sum(r["discovery_calls"] for r in rs),
                "median_elapsed_seconds": statistics.median(
                    r["elapsed_seconds"] for r in rs
                ),
                "startup_attempts": sum(r["attempts"] for r in rs),
            }
        return out

    out = {
        "promotion_allowed": False,
        "aggregate": aggregate(rows),
        "strata": {
            f: aggregate([r for r in rows if r["family"] == f])
            for f in sorted({r["family"] for r in rows})
        },
        "rows": rows,
    }
    paired = {
        c["id"]: {
            a: next(r for r in rows if r["case"] == c["id"] and r["arm"] == a)
            for a in manifest["arms"]
        }
        for c in manifest["cases"]
    }
    out["candidate_only_losses"] = [
        k
        for k, p in paired.items()
        if p["fast"]["success"] and not p["minimal"]["success"]
    ]
    out["candidate_only_wins"] = [
        k
        for k, p in paired.items()
        if p["minimal"]["success"] and not p["fast"]["success"]
    ]
    for metric, complete in (
        ("total_cost_usd", "usage_complete"),
        ("all_prompt_tokens", "tokens_complete"),
    ):
        if not all(r[complete] for r in rows):
            continue

        def value(row):
            return (
                row["input_tokens"] + row["cache_read_tokens"]
                if metric == "all_prompt_tokens"
                else row[metric]
            )

        rng = random.Random(99231)
        ratios = []
        keys = list(paired)
        for _ in range(10000):
            chosen = rng.choices(keys, k=len(keys))
            base = sum(value(paired[k]["fast"]) for k in chosen)
            new = sum(value(paired[k]["minimal"]) for k in chosen)
            if base > 0:
                ratios.append(new / base)
        if ratios:
            ratios.sort()
            name = "cost" if metric == "total_cost_usd" else metric
            out[f"exploratory_paired_bootstrap_{name}_ratio_95"] = [
                ratios[int(len(ratios) * 0.025)],
                ratios[int(len(ratios) * 0.975)],
            ]
    (root / "analysis.json").write_text(json.dumps(out, indent=2) + "\n")
    print(json.dumps({k: v for k, v in out.items() if k != "rows"}, indent=2))


if __name__ == "__main__":
    report(Path(sys.argv[1]).resolve())

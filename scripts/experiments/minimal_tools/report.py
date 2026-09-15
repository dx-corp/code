"""Reconcile every selected native receipt before reporting this small screen."""

import json
import math
from collections import Counter
from pathlib import Path
import random
import statistics
import sys

from evidence import load_json, source_hashes, verify_outcome
from statistics_report import paired_intervals

FIELDS = (
    "input_tokens",
    "cache_read_tokens",
    "cache_write_tokens",
    "output_tokens",
    "total_cost_usd",
)


def reconcile_row(row):
    path = Path(row["selected_path"])
    events = [load_json(x) for x in (path / "events.jsonl").read_text().splitlines()]
    terminals = [e for e in events if e.get("type") == "turn_completed"]
    if type(row["terminal"]) is not bool or row["terminal"] != bool(terminals):
        raise ValueError("terminal summary disagrees with events")
    if len(terminals) > 1:
        raise ValueError("duplicate turn completion")
    if (
        any(
            e.get("type") in ("error", "provider_error", "turn_interrupted")
            for e in events
        )
        and row["failure"] is None
    ):
        raise ValueError("failure omitted from summary")
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
            if values
            and all(
                type(x) in (int, float) and math.isfinite(x) and x >= 0 for x in values
            )
            else None
        )
        if field == "cache_write_tokens" and field not in row:
            row[field] = reconciled
        if reconciled != row[field]:
            raise ValueError(f"usage mismatch: {row['case']} {row['arm']} {field}")
    row["provider_calls"] = len(started)
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
                and math.isfinite(e["usage"].get(f, 0))
                and e["usage"].get(f, 0) >= 0
                and float(e["usage"].get(f, 0)).is_integer()
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
    # A failed re-analysis must not leave an older success report behind.
    (root / "analysis.json").unlink(missing_ok=True)
    manifest = load_json((root / "manifest.json").read_text())
    rows = (
        load_json((root / "rows.json").read_text())
        if (root / "rows.json").exists()
        else []
    )
    arms = manifest["arms"]
    if arms != ["fast", "minimal"]:
        raise ValueError("expected fast and minimal arms in fixed order")
    cases = {c["id"]: c for c in manifest["cases"]}
    if not cases or len(cases) != len(manifest["cases"]):
        raise ValueError("empty or duplicate cases")
    expected = {(case, arm) for case in cases for arm in arms}
    observed = [(r["case"], r["arm"]) for r in rows]
    if len(set(observed)) != len(observed) or not set(observed) <= expected:
        raise ValueError("duplicate or unexpected result row")
    if manifest.get("order_method"):
        planned = [(case, arm) for case, arms in manifest["order"] for arm in arms]
        if len(planned) != len(expected) or set(planned) != expected:
            raise ValueError("invalid declared execution order")
        if observed != planned[:len(observed)]:
            raise ValueError("observed execution differs from declared order")
    if manifest.get("schema") not in (
        None,
        "maestro.minimal-tools-screen.v2",
        "maestro.minimal-tools-screen.v3",
    ):
        raise ValueError("unsupported manifest schema")
    verified = manifest.get("schema") == "maestro.minimal-tools-screen.v3"
    if verified and manifest.get("source_hashes") != source_hashes():
        raise ValueError(
            "analysis sources differ from frozen manifest; use the original sources"
        )
    for row in rows:
        for field in ("elapsed_seconds", "attempts"):
            value = row[field]
            if type(value) not in (int, float) or not math.isfinite(value) or value < 0:
                raise ValueError(f"invalid {field}")
        if row["attempts"] != 1:
            raise ValueError("this protocol permits exactly one runtime attempt")
        if type(row["success"]) is not bool:
            raise ValueError("success must be boolean")
        if row["family"] != cases[row["case"]].get("family", row["family"]):
            raise ValueError("case family mismatch")
        reconcile_row(row)
        if verified:
            expected_path = (root / row["case"] / row["arm"] / "0").resolve()
            if Path(row["selected_path"]).resolve() != expected_path:
                raise ValueError(
                    "selected artifact path does not match planned attempt"
                )
            verify_outcome(row, cases[row["case"]])
    cohort_complete = set(observed) == expected

    def aggregate(selected):
        out = {}
        for arm in manifest["arms"]:
            rs = [r for r in selected if r["arm"] == arm]
            sums = {
                f: sum(r[f] for r in rs)
                if rs
                and all(
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
                "success_rate": sum(r["success"] for r in rs) / len(rs) if rs else None,
                "cost_per_success_usd": sums["total_cost_usd"]
                / sum(r["success"] for r in rs)
                if sums["total_cost_usd"] is not None and any(r["success"] for r in rs)
                else None,
                "total_tokens": sum(sums[f] for f in FIELDS[:-1])
                if all(sums[f] is not None for f in FIELDS[:-1])
                else None,
                "tokens_per_success": sum(sums[f] for f in FIELDS[:-1])
                / sum(r["success"] for r in rs)
                if all(sums[f] is not None for f in FIELDS[:-1])
                and any(r["success"] for r in rs)
                else None,
                "passed": sum(r["success"] for r in rs),
                "usage_complete_runs": sum(r["usage_complete"] for r in rs),
                "tokens_complete_runs": sum(r["tokens_complete"] for r in rs),
                **sums,
                "all_prompt_tokens": sums["input_tokens"]
                + sums["cache_read_tokens"]
                + sums["cache_write_tokens"]
                if sums["input_tokens"] is not None
                and sums["cache_read_tokens"] is not None
                and sums["cache_write_tokens"] is not None
                else None,
                "provider_calls": sum(r["provider_calls"] for r in rs),
                "discovery_calls": sum(r["discovery_calls"] for r in rs),
                "median_elapsed_seconds": statistics.median(
                    r["elapsed_seconds"] for r in rs
                )
                if rs
                else None,
                "startup_attempts": sum(r["attempts"] for r in rs),
            }
        return out

    out = {
        "promotion_allowed": False,
        "cohort_complete": cohort_complete,
        "planned_pairs": len(cases),
        "missing_rows": [
            dict(case=c, arm=a) for c, a in sorted(expected - set(observed))
        ],
        "outcomes_regraded": verified and bool(rows),
        "gateway_verified": False,
        "limitations": [
            "Development cases are author-visible, not an independent holdout.",
            "Intervals are exploratory paired task bootstraps, not population or rollout evidence.",
            "Usage is native-runtime evidence, not independently verified gateway billing.",
            "Cost per success includes spend on failed tasks; zero successes is unavailable.",
            "Interrupted cohorts have descriptive partial totals only; no comparative intervals.",
        ],
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
        if all((c["id"], arm) in set(observed) for arm in arms)
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
        if not cohort_complete or len(paired) < 2 or not all(r[complete] for r in rows):
            continue

        def value(row):
            return (
                row["input_tokens"]
                + row["cache_read_tokens"]
                + row["cache_write_tokens"]
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
            if base <= 0:
                ratios = []
                break
            ratios.append(new / base)
        if ratios:
            ratios.sort()
            name = "cost" if metric == "total_cost_usd" else metric
            out[f"exploratory_paired_bootstrap_{name}_ratio_95"] = [
                ratios[int(len(ratios) * 0.025)],
                ratios[int(len(ratios) * 0.975)],
            ]
    out["outcome_intervals"] = (
        paired_intervals(paired)
        if cohort_complete
        else {"available": False, "reason": "interrupted cohort"}
    )
    out["paired_outcomes"] = {
        "complete_pairs": len(paired),
        "both_pass": sum(
            p["fast"]["success"] and p["minimal"]["success"] for p in paired.values()
        ),
        "both_fail": sum(
            not p["fast"]["success"] and not p["minimal"]["success"]
            for p in paired.values()
        ),
    }
    temporary = root / "analysis.json.tmp"
    temporary.write_text(json.dumps(out, indent=2, allow_nan=False) + "\n")
    temporary.replace(root / "analysis.json")
    print(json.dumps({k: v for k, v in out.items() if k != "rows"}, indent=2))


if __name__ == "__main__":
    report(Path(sys.argv[1]).resolve())

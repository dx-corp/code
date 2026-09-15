"""Exploratory paired task resampling; never a sequential promotion rule."""

import random
import math
import statistics


def paired_intervals(pairs, draws=10000, seed=99231):
    if len(pairs) < 2:
        return {"available": False, "reason": "fewer than two complete task pairs"}
    rng = random.Random(seed)
    values = list(pairs.values())
    samples = {
        "success_rate_difference": [],
        "cost_per_success_ratio": [],
        "tokens_per_success_ratio": [],
        "median_latency_ratio": [],
    }
    unavailable = {}
    for _ in range(draws):
        chosen = rng.choices(values, k=len(values))
        successes = {
            a: sum(p[a]["success"] for p in chosen) for a in ("fast", "minimal")
        }
        samples["success_rate_difference"].append(
            (successes["minimal"] - successes["fast"]) / len(chosen)
        )
        elapsed = [p[a].get("elapsed_seconds") for p in chosen for a in successes]
        if all(type(v) in (int, float) and math.isfinite(v) and v > 0 for v in elapsed):
            samples["median_latency_ratio"].append(
                statistics.median(p["minimal"]["elapsed_seconds"] for p in chosen)
                / statistics.median(p["fast"]["elapsed_seconds"] for p in chosen)
            )
        else:
            unavailable["median_latency_ratio"] = "missing or invalid elapsed time"
        for metric, complete in (
            ("cost_per_success_ratio", "usage_complete"),
            ("tokens_per_success_ratio", "tokens_complete"),
        ):
            if metric in unavailable:
                continue
            if not all(p[a][complete] for p in values for a in successes):
                unavailable[metric] = "incomplete usage"
                continue
            if not all(successes.values()):
                unavailable[metric] = "zero successes in at least one bootstrap sample"
                continue

            def amount(row):
                return (
                    row["total_cost_usd"]
                    if metric.startswith("cost")
                    else sum(
                        row[f]
                        for f in (
                            "input_tokens",
                            "cache_read_tokens",
                            "cache_write_tokens",
                            "output_tokens",
                        )
                    )
                )

            base = sum(amount(p["fast"]) for p in chosen) / successes["fast"]
            candidate = sum(amount(p["minimal"]) for p in chosen) / successes["minimal"]
            if base <= 0:
                unavailable[metric] = "zero baseline spend"
                continue
            samples[metric].append(candidate / base)
    out = {
        "available": True,
        "unit": "task pair",
        "draws": draws,
        "seed": seed,
        "method": "exploratory percentile bootstrap",
        "unavailable": unavailable,
    }
    for metric, sample in samples.items():
        if metric not in unavailable:
            sample.sort()
            out[metric + "_95"] = [
                sample[int(draws * 0.025)],
                sample[int(draws * 0.975)],
            ]
    # Bonferroni Wilson bounds remain nondegenerate when both arms pass all tasks.
    # This is a descriptive uncertainty bound, not a noninferiority certificate.
    def wilson(k, n):
        z = statistics.NormalDist().inv_cdf(0.9875)
        den = 1 + z*z/n
        center = (k/n + z*z/(2*n))/den
        radius = z * math.sqrt(k/n*(1-k/n)/n + z*z/(4*n*n))/den
        return [max(0, center-radius), min(1, center+radius)]

    bounds = {a: wilson(sum(p[a]["success"] for p in values), len(values))
              for a in ("fast", "minimal")}
    out["success_difference_conservative_95"] = [
        bounds["minimal"][0] - bounds["fast"][1],
        bounds["minimal"][1] - bounds["fast"][0],
    ]
    return out

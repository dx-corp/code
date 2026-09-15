"""Frozen study rules and reconciliation of trusted, settled billing exports.

This checks local evidence consistency, not provider authenticity or population
representativeness. Billing exports must come from an authorized billing owner.
"""
import argparse
from decimal import Decimal
from datetime import datetime
import hashlib
import json
import math
from pathlib import Path
import random
import re
import statistics

from evidence import load_json

ARMS = ("fast", "minimal")


def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def protocol(value, cases):
    if value.get("schema") != "maestro.verification-plan.v1":
        raise ValueError("unsupported verification plan")
    if type(value.get("pairs")) is not int or value["pairs"] != len(cases) or len(cases) < 2:
        raise ValueError("plan must fix the exact number of task pairs")
    for key in ("minimum_cost_reduction", "minimum_speed_reduction", "maximum_quality_loss"):
        number = value.get(key)
        if type(number) not in (int, float) or not math.isfinite(number) or not 0 < number < 1:
            raise ValueError("plan margins must be finite fractions between zero and one")
    for key in ("sample_size_rationale", "holdout_provenance", "cache_policy", "time_window"):
        if not isinstance(value.get(key), str) or not value[key].strip():
            raise ValueError("plan requires " + key)
    if value.get("stopping_rule") != "fixed_sample_no_optional_stopping":
        raise ValueError("verification requires fixed-sample stopping")
    return value


def holdout(value):
    if not isinstance(value, list) or not value:
        raise ValueError("holdout must contain cases")
    ids = set()
    for case in value:
        name = case.get("id", "")
        if not re.fullmatch(r"[a-z0-9][a-z0-9_-]{0,79}", name) or name in ids:
            raise ValueError("unsafe or duplicate case ID")
        ids.add(name)
        if case.get("family") != "investigation" or not isinstance(case.get("expected"), dict):
            raise ValueError("external holdout currently requires exact-JSON investigation cases")
        if not isinstance(case.get("prompt"), str) or not case["prompt"]:
            raise ValueError("missing prompt")
        if not isinstance(case.get("files"), dict) or not case["files"]:
            raise ValueError("missing evidence files")
        for name, content in case["files"].items():
            path = Path(name)
            if (path.is_absolute() or ".." in path.parts or not path.parts
                    or name in ("answer.json", "check") or not isinstance(content, str)):
                raise ValueError("unsafe evidence path")
    return value


def money(value):
    if not isinstance(value, str):
        raise ValueError("billing amounts must be decimal strings")
    number = Decimal(value)
    if not number.is_finite() or number < 0 or not math.isfinite(float(number)):
        raise ValueError("billing amounts must be nonnegative and finite")
    return number


def settle(rows, records, bill, organization, workspace):
    """One settled provider line per attempt; never sum gateway mirror costs."""
    from billing import receipts
    if (bill.get("schema") != "maestro.settled-billing.v1"
            or bill.get("basis") != "settled_provider_charges"
            or bill.get("currency") != "USD" or bill.get("settled") is not True
            or bill.get("organization_id") != organization or bill.get("workspace_id") != workspace):
        raise ValueError("billing basis, settlement, currency or tenant mismatch")
    for key in ("provider_account", "source_reference", "period_start", "period_end", "export_sha256"):
        if not isinstance(bill.get(key), str) or not bill[key]:
            raise ValueError("missing billing provenance: " + key)
    if not re.fullmatch(r"[a-f0-9]{64}", bill["export_sha256"]):
        raise ValueError("invalid export digest")
    start, end = (datetime.fromisoformat(bill[k].replace("Z", "+00:00")) for k in ("period_start", "period_end"))
    if start.tzinfo is None or end.tzinfo is None or start >= end:
        raise ValueError("invalid billing period")
    if bill.get("complete_scope") is not True:
        raise ValueError("billing owner must attest complete scoped attempt coverage")
    expected, owners, lineages = {}, {}, {}
    native = {}
    for index, row in enumerate(rows):
        for receipt in receipts(row):
            if receipt["request_id"] in native:
                raise ValueError("duplicate native receipt")
            native[receipt["request_id"]] = (receipt["record_id"], receipt["lineage_id"])
            lineage = receipt["lineage_id"]
            if lineage in lineages and lineages[lineage] != index:
                raise ValueError("shared lineage")
            lineages[lineage] = index
    found = {}
    for record in records:
        if record["request_id"] in found:
            raise ValueError("duplicate gateway request")
        found[record["request_id"]] = (record["record_id"], record["lineage_id"])
        if (record["organization_id"], record["workspace_id"]) != (organization, workspace):
            raise ValueError("gateway tenant mismatch")
        created = datetime.fromisoformat(record["created_at"].replace("Z", "+00:00"))
        if created.tzinfo is None or not start <= created < end:
            raise ValueError("gateway request outside billing period")
        if record["lineage_id"] not in lineages:
            raise ValueError("unattributed gateway request")
        # No-attempt requests require explicit zero-charge billing evidence too.
        actual = record.get("attempts") or []
        if any(type(a.get("ordinal")) is not int for a in actual) or sorted(a["ordinal"] for a in actual) != list(range(len(actual))):
            raise ValueError("invalid gateway attempt sequence")
        attempts = actual or [{"ordinal": None}]
        for attempt in attempts:
            key = (record["request_id"], attempt["ordinal"])
            if key in expected:
                raise ValueError("duplicate gateway attempt")
            expected[key] = record
            owners[key] = lineages[record["lineage_id"]]
    if not native or any(found.get(key) != value for key, value in native.items()):
        raise ValueError("missing or mismatched native gateway receipt")
    totals = [Decimal(0) for _ in rows]
    seen, line_ids, provider_ids = set(), set(), set()
    for line in bill["lines"]:
        ordinal = line["attempt_ordinal"]
        if ordinal is not None and (type(ordinal) is not int or ordinal < 0):
            raise ValueError("invalid billing attempt ordinal")
        key = (line["request_id"], ordinal)
        if key in seen or line["line_id"] in line_ids or key not in expected:
            raise ValueError("duplicate or unexpected billing line")
        if not isinstance(line["line_id"], str) or not line["line_id"]:
            raise ValueError("invalid billing line ID")
        record = expected[key]
        if line["record_id"] != record["record_id"] or line["provider"] != record["provider"]:
            raise ValueError("billing attempt identity mismatch")
        if key[1] is not None and not line.get("provider_request_id"):
            raise ValueError("executed attempt requires provider request identity")
        if key[1] is not None:
            provider_key = (line["provider"], line["provider_request_id"])
            if provider_key in provider_ids:
                raise ValueError("duplicate provider request")
            provider_ids.add(provider_key)
        amount = money(line["net_charge_usd"])
        if key[1] is None and (line.get("non_execution_confirmed") is not True or amount != 0):
            raise ValueError("no-attempt request requires confirmed zero charge")
        seen.add(key)
        line_ids.add(line["line_id"])
        totals[owners[key]] += amount
    if seen != set(expected) or not expected or set(owners.values()) != set(range(len(rows))):
        raise ValueError("incomplete billing attempt coverage")
    if sum(totals) != money(bill["scoped_total_usd"]):
        raise ValueError("billing total does not reconcile")
    return [str(value) for value in totals]


def intervals(pairs, draws=12000):
    """Paired bootstrap cost/time and conservative Wilson quality bounds.

Each endpoint uses alpha=.05/3 (Bonferroni); bootstrap coverage is approximate.
    """
    alpha = .05 / 3
    rng = random.Random(37291)
    samples = {"cost": [], "latency": []}
    for _ in range(draws):
        selected = rng.choices(pairs, k=len(pairs))
        successes = {a: sum(p[a]["success"] for p in selected) for a in ARMS}
        for metric in samples:
            if metric == "cost":
                if not all(successes.values()) or any(p[a]["billed_cost_usd"] is None for p in selected for a in ARMS):
                    continue
                amounts = {a: sum(float(p[a]["billed_cost_usd"]) for p in selected) / successes[a] for a in ARMS}
            else:
                amounts = {a: statistics.median(p[a]["elapsed_seconds"] for p in selected) for a in ARMS}
            if amounts["fast"] > 0:
                ratio = amounts["minimal"] / amounts["fast"]
                if math.isfinite(ratio):
                    samples[metric].append(ratio)
    result = {}
    for metric, values in samples.items():
        values.sort()
        result[metric] = [values[int(draws*alpha/2)], values[min(draws-1, int(draws*(1-alpha/2)))]] if len(values) == draws else None
    z = statistics.NormalDist().inv_cdf(1-alpha/4)
    n = len(pairs)
    def wilson(arm):
        p = sum(pair[arm]["success"] for pair in pairs)/n
        den = 1+z*z/n
        mid = (p+z*z/(2*n))/den
        radius = z*math.sqrt(p*(1-p)/n+z*z/(4*n*n))/den
        return max(0, mid-radius), min(1, mid+radius)
    base, candidate = wilson("fast"), wilson("minimal")
    result["quality"] = [candidate[0]-base[1], candidate[1]-base[0]]
    return result


def verdict(manifest, rows, costs, amended=False):
    plan = manifest.get("verification_plan")
    result = dict(billing_verified=costs is not None, cheaper_verified=False,
                  faster_verified=False, quality_preserved=False, reasons=[],
                  production_verified=False, method="paired percentile bootstrap; Bonferroni across three endpoints; conservative Wilson quality bounds; approximate coverage")
    if plan is None:
        result["reasons"].append("no frozen verification plan")
        return result
    protocol(plan, manifest["cases"])
    if amended:
        result["reasons"].append("protocol amended after execution")
    expected = [(c, a) for c, arms in manifest["order"] for a in arms]
    if [(r["case"], r["arm"]) for r in rows] != expected:
        result["reasons"].append("incomplete or reordered cohort")
    if any(type(r.get("elapsed_seconds")) not in (int, float) or not math.isfinite(r["elapsed_seconds"]) or r["elapsed_seconds"] <= 0 for r in rows):
        result["reasons"].append("invalid end-to-end timing")
    if result["reasons"]:
        return result
    pairs = {}
    for row, cost in zip(rows, costs if costs is not None else [None]*len(rows), strict=True):
        pairs.setdefault(row["case"], {})[row["arm"]] = dict(row, billed_cost_usd=cost)
    bounds = intervals(list(pairs.values()))
    result["intervals"] = bounds
    result["quality_preserved"] = bounds["quality"][0] >= -plan["maximum_quality_loss"]
    result["cheaper_verified"] = bool(result["quality_preserved"] and bounds["cost"] and bounds["cost"][1] < 1-plan["minimum_cost_reduction"])
    result["faster_verified"] = bool(result["quality_preserved"] and bounds["latency"] and bounds["latency"][1] < 1-plan["minimum_speed_reduction"])
    if costs is None:
        result["reasons"].append("settled billing unavailable; cost claim suppressed")
    result["scope"] = "this frozen study and trusted billing export; holdout independence is supplied provenance, not authenticated"
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("root", type=Path)
    parser.add_argument("--records", type=Path)
    parser.add_argument("--billing", type=Path)
    parser.add_argument("--provider-export", type=Path)
    parser.add_argument("--organization", required=True)
    parser.add_argument("--workspace", required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    output = args.output.resolve()
    inputs = [args.records, args.billing, args.provider_export]
    if any(p is not None and output == p.resolve() for p in inputs) or (
            output.is_relative_to(args.root.resolve()) and output != (args.root/"verification.json").resolve()):
        parser.error("output must not overwrite study evidence or imported inputs; use verification.json inside the study")
    args.output.unlink(missing_ok=True)
    from report import report
    report(args.root)  # Regrade original evidence and check frozen sources first.
    manifest = load_json((args.root/"manifest.json").read_text())
    row_path = args.root/"rows.json"
    rows = load_json(row_path.read_text()) if row_path.exists() else []
    if manifest.get("verification_plan"):
        for row in rows:
            path = Path(row["selected_path"])
            timing = load_json((path/"timing.json").read_text())
            if (timing.get("manifest_sha256") != sha(args.root/"manifest.json")
                    or timing["events_sha256"] != sha(path/"events.jsonl")
                    or timing["elapsed_seconds"] != row["elapsed_seconds"]):
                raise ValueError("controller timing evidence mismatch")
    costs = None
    bill = None
    if any((args.billing, args.records, args.provider_export)):
        if not all((args.billing, args.records, args.provider_export)):
            parser.error("billing verification requires records, billing and provider-export")
        bill = load_json(args.billing.read_text())
        if sha(args.provider_export) != bill["export_sha256"]:
            raise ValueError("provider export hash mismatch")
        records = load_json(args.records.read_text())
        # Qualification has separate accounting, never enters cohort efficiency.
        qpath = args.root/"qualification.json"
        qualification = load_json(qpath.read_text())["rows"] if qpath.exists() else []
        from report import reconcile_row
        from evidence import verify_outcome
        from trial import cases as qualification_cases
        qcases = {c["id"]: c for c in qualification_cases()}
        for row in qualification:
            if Path(row["selected_path"]).resolve() != (args.root/"qualification"/row["case"]/row["arm"]/"0").resolve():
                raise ValueError("unexpected qualification artifact path")
            reconcile_row(row)
            verify_outcome(row, qcases[row["case"]])
        all_rows = rows + qualification
        costs_all = settle(all_rows, records, bill, args.organization, args.workspace)
        costs = costs_all[:len(rows)]
    result = verdict(manifest, rows, costs, (args.root/"continuation-amendment.json").exists())
    result.update(schema="maestro.study-verification.v1", manifest_sha256=sha(args.root/"manifest.json"),
                  billing_sha256=sha(args.billing) if bill else None,
                  provider_export_sha256=sha(args.provider_export) if bill else None,
                  records_sha256=sha(args.records) if bill else None,
                  rows_sha256=sha(row_path) if row_path.exists() else None,
                  billing_trust="Operator-supplied settled export; consistency checked, provider authenticity not verified",
                  cohort_billed_usd=str(sum(map(Decimal, costs))) if costs else None,
                  qualification_billed_usd=str(sum(map(Decimal, costs_all[len(rows):]))) if bill else None)
    args.output.write_text(json.dumps(result, indent=2)+"\n")


if __name__ == "__main__":
    main()

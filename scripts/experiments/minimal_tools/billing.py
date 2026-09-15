"""Read-only gateway reconciliation. List-price estimates never become billed spend."""

import argparse
from collections import Counter
from decimal import Decimal
import hashlib
import json
from pathlib import Path

from evidence import load_json, verify_outcome
from report import reconcile_row
from statistics_report import paired_intervals

TOKENS = ("input_tokens", "cache_read_tokens", "cache_write_tokens", "output_tokens")


def receipts(row):
    events = [load_json(line) for line in
              (Path(row["selected_path"]) / "events.jsonl").read_text().splitlines()]
    return [e for e in events if e.get("type") == "managed_gateway_receipt"]


def literal(value):
    if not isinstance(value, str) or not value or "\x00" in value:
        raise ValueError("invalid SQL scope identifier")
    return "'" + value.replace("'", "''") + "'"


def export_query(rows, organization, workspace):
    # Query whole lineages, not just known response IDs: include auxiliary calls,
    # failed calls, and gateway failovers that the final response may hide.
    lineages = sorted({e["lineage_id"] for r in rows for e in receipts(r)})
    if not lineages:
        raise ValueError("no gateway receipts; cannot bound export")
    return """BEGIN READ ONLY;
SET LOCAL statement_timeout='20s';
SELECT coalesce(json_agg(json_build_object(
 'organization_id',r.organization_id,'workspace_id',r.workspace_id,
 'request_id',r.request_id,'record_id',r.record_id,'lineage_id',r.lineage_id,
 'model',r.model,'provider',r.provider,'lifecycle_state',r.lifecycle_state,
 'terminal_http_status',r.terminal_http_status,'error_code',r.error_code,
 'created_at',r.created_at,'completed_at',r.completed_at,
 'usage_delivery',json_build_object('body',r.usage_delivery->'body'),
 'primary_delivered',r.usage_primary_delivered,'mirror_delivered',r.usage_mirror_delivered,
 'attempts',(SELECT json_agg(json_build_object('ordinal',a.attempt_ordinal,
   'outcome',a.outcome,'http_status',a.http_status,'retry_max_attempts',a.retry_max_attempts))
   FROM llm_gateway_provider_attempts a WHERE a.organization_id=r.organization_id
   AND a.workspace_id=r.workspace_id AND a.record_id=r.record_id))), '[]'::json)
FROM llm_gateway_request_records r WHERE r.organization_id=""" + literal(organization) + \
        " AND r.workspace_id=" + literal(workspace) + \
        " AND r.lineage_id IN (" + ",".join(map(literal, lineages)) + ");\nROLLBACK;\n"


def priced_tokens(body, rates):
    if body.get("provider") != rates["provider"] or body.get("model") != rates["model"]:
        raise ValueError("price/model mismatch")
    raw = body.get("data", {})
    # The gateway flattens unknown counts to zero. Only provider raw counters
    # establish input/output/cache-read completeness. Fireworks does not expose
    # separately charged cache writes, so its absent write counter can be zero.
    for field in TOKENS:
        value = raw.get(field)
        if field == "cache_write_tokens" and value is None and body.get("provider") == "fireworks" and body.get(field) == 0:
            continue
        if type(value) is not int or value < 0 or value != body.get(field):
            return None
    total = Decimal(0)
    for field in TOKENS:
        count = body.get(field)
        if type(count) is not int or count < 0:
            return None
        rate = rates["usd_per_million"].get(field)
        if rate is None:
            if count:
                return None
            continue
        rate = Decimal(str(rate))
        if not rate.is_finite() or rate < 0:
            raise ValueError("invalid price")
        total += Decimal(count) * rate / Decimal(1000000)
    return total


def non_execution(record, timeline):
    # Two observed owner stages that precede provider dispatch. Absence of a
    # manifest alone is insufficient: require the complete ordered timeline too.
    if (not timeline or record.get("attempts") or record.get("lifecycle_state") != "failed"
            or (record.get("usage_delivery") or {}).get("body") is not None):
        return False
    if any(timeline.get(k) != record.get(k) for k in ("request_id", "organization_id", "workspace_id")):
        raise ValueError("timeline tenant or request mismatch")
    if record.get("terminal_http_status") != 503 or record.get("error_code") != "http_error":
        return False
    if timeline.get("terminal_status") != 503 or timeline.get("error_code") != "http_error":
        return False
    phases = load_json(timeline["phases"])
    order = ["decode", "auth", "plan", "admission", "prompts", "token_estimate",
             "rate_limit", "token_budget", "governance_input", "governance", "keys", "budget"]
    return (bool(phases) and [p.get("stage") for p in phases] == order[:len(phases)]
            and phases[-1].get("stage") in ("governance_input", "budget")
            and phases[-1].get("outcome") == "rejected"
            and all(p.get("outcome") == "accepted" for p in phases[:-1]))


def reconcile(rows, records, organization, workspace, rates, timelines=None):
    by_request, by_lineage = {}, {}
    for record in records:
        if (record["organization_id"], record["workspace_id"]) != (organization, workspace):
            raise ValueError("gateway tenant mismatch")
        rid = record["request_id"]
        if rid in by_request:
            raise ValueError("duplicate gateway request")
        by_request[rid] = record
        by_lineage.setdefault(record["lineage_id"], []).append(record)
    used_lineages, used_requests, output = set(), set(), []
    for row in rows:
        local = receipts(row)
        ids = [e["request_id"] for e in local]
        if len(set(ids)) != len(ids) or used_requests.intersection(ids):
            raise ValueError("duplicate native gateway receipt")
        used_requests.update(ids)
        lineages = {e["lineage_id"] for e in local}
        if used_lineages.intersection(lineages):
            raise ValueError("lineage shared by multiple task attempts")
        used_lineages.update(lineages)
        reasons = []
        for receipt in local:
            found = by_request.get(receipt["request_id"])
            if not found:
                reasons.append("missing_gateway_record")
            elif any(found[k] != receipt[k] for k in ("record_id", "lineage_id")):
                raise ValueError("gateway receipt identity mismatch")
        if not local:
            reasons.append("no_gateway_receipts")
        matched = [r for lineage in lineages for r in by_lineage.get(lineage, [])]
        totals = {field: 0 for field in TOKENS}
        native_totals = {field: 0 for field in TOKENS}
        estimate, ledger_micros = Decimal(0), 0
        retry_count = 0
        not_executed = 0
        pricing_sources = Counter()
        ledger_available = True
        for record in matched:
            attempts = record.get("attempts") or []
            retry_count += max(0, len(attempts)-1)
            if non_execution(record, (timelines or {}).get(record["request_id"])):
                not_executed += 1
                continue
            if len(attempts) != 1 or attempts[0].get("ordinal") != 0 or attempts[0].get("outcome") != "succeeded":
                reasons.append("provider_attempt_cost_coverage_unknown")
            body = (record.get("usage_delivery") or {}).get("body")
            if not body:
                reasons.append("missing_usage_envelope")
                ledger_available = False
                continue
            metadata = body.get("metadata", {})
            if body.get("request_id") != record["request_id"] or any(
                metadata.get(k) != record[k] for k in
                ("organization_id", "workspace_id", "record_id", "lineage_id")
            ):
                raise ValueError("usage envelope identity mismatch")
            if body.get("model") != record["model"] or body.get("provider") != record["provider"]:
                raise ValueError("usage envelope model mismatch")
            if record["lifecycle_state"] != "succeeded":
                reasons.append("failed_gateway_request_cost_coverage_unknown")
            value = priced_tokens(body, rates)
            if value is None:
                reasons.append("incomplete_tokens_or_rates")
            else:
                estimate += value
            for field in TOKENS:
                if type(body.get(field)) is int and body[field] >= 0:
                    totals[field] += body[field]
                    if record["request_id"] in ids:
                        native_totals[field] += body[field]
            source = metadata.get("pricing_source", "missing")
            pricing_sources[source] += 1
            micros = body.get("cost_micros")
            valid_price = (metadata.get("pricing_available") is True
                           and source in ("provider_reported", "provider_ref")
                           and bool(metadata.get("pricing_version"))
                           and type(micros) is int and micros >= 0)
            if valid_price:
                ledger_micros += micros
            else:
                ledger_available = False
        # Compare receipt-matched usage separately, while retaining auxiliary
        # calls in total spend. Extras cannot conceal a main-loop discrepancy.
        extras = sorted(r["request_id"] for r in matched if r["request_id"] not in ids)
        native_match = all(native_totals[f] == row.get(f) for f in TOKENS)
        if row.get("tokens_complete") and not native_match:
            reasons.append("gateway_native_token_mismatch")
        complete = bool(matched) and not reasons
        output.append({
            "case": row["case"], "arm": row["arm"], "success": row["success"],
            "elapsed_seconds": row["elapsed_seconds"],
            "gateway_requests": len(matched), "extra_request_ids": extras,
            "tokens": totals, "native_tokens_match": native_match,
            "pricing_sources": dict(pricing_sources), "reasons": sorted(set(reasons)),
            "list_price_estimate_usd": str(estimate) if complete else None,
            "list_price_lower_bound_usd": str(estimate)
            if "gateway_native_token_mismatch" not in reasons else None,
            "gateway_recorded_cost_usd": str(Decimal(ledger_micros)/1000000)
            if complete and ledger_available else None,
            "gateway_retries_observed": retry_count,
            "verified_non_execution_requests": not_executed,
            # Configured ledger prices are not account invoice proof.
            "billed_cost_usd": None,
        })
    if set(by_lineage) - used_lineages:
        raise ValueError("export includes unrelated lineages")
    return output


def summarize(rows, cohort_complete):
    summary = {}
    pairs = {}
    for row in rows:
        pair_row = dict(row, total_cost_usd=float(row["list_price_estimate_usd"])
                        if row["list_price_estimate_usd"] is not None else None,
                        usage_complete=row["list_price_estimate_usd"] is not None,
                        tokens_complete=not row["reasons"], **row["tokens"])
        pairs.setdefault(row["case"], {})[row["arm"]] = pair_row
    for arm in ("fast", "minimal"):
        selected = [r for r in rows if r["arm"] == arm]
        successes = sum(r["success"] for r in selected)
        values = [r["list_price_estimate_usd"] for r in selected]
        total = sum(map(Decimal, values)) if values and all(v is not None for v in values) else None
        bounds = [r["list_price_lower_bound_usd"] for r in selected]
        lower = sum(map(Decimal, bounds)) if bounds and all(v is not None for v in bounds) else None
        summary[arm] = dict(tasks=len(selected), successes=successes,
                            list_price_lower_bound_usd=str(lower) if lower is not None else None,
                            list_price_per_success_lower_bound_usd=str(lower/successes)
                            if lower is not None and successes else None,
                            list_price_estimate_usd=str(total) if total is not None else None,
                            list_price_per_success_usd=str(total/successes)
                            if total is not None and successes else None)
    intervals = paired_intervals(pairs) if cohort_complete else {"available":False,"reason":"incomplete cohort"}
    # Name the money metric honestly; never let estimates masquerade as invoices.
    if "cost_per_success_ratio_95" in intervals:
        intervals["list_price_per_success_ratio_95"] = intervals.pop("cost_per_success_ratio_95")
    baseline_lower = summary["fast"]["list_price_per_success_lower_bound_usd"]
    candidate = summary["minimal"]["list_price_per_success_usd"]
    ratio_upper = (Decimal(candidate)/Decimal(baseline_lower)
                   if cohort_complete and candidate is not None
                   and baseline_lower is not None and Decimal(baseline_lower) > 0 else None)
    return dict(arms=summary, intervals=intervals, cohort_complete=cohort_complete,
                candidate_baseline_list_price_ratio_upper_bound=str(ratio_upper) if ratio_upper is not None else None,
                bound_assumption="Unobserved token charges are nonnegative at the declared list rates; this is a deterministic bound, not an invoice or confidence interval.")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("root", type=Path)
    parser.add_argument("--organization", required=True)
    parser.add_argument("--workspace", required=True)
    parser.add_argument("--write-query", type=Path)
    parser.add_argument("--records", type=Path)
    parser.add_argument("--rates", type=Path)
    parser.add_argument("--timeline-logs", type=Path)
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()
    rows = load_json((args.root/"rows.json").read_text())
    qualification_path = args.root/"qualification.json"
    qualification = load_json(qualification_path.read_text())["rows"] if qualification_path.exists() else []
    if args.write_query:
        args.write_query.write_text(export_query(rows + qualification, args.organization, args.workspace))
        return
    if not all((args.records, args.rates, args.output)):
        parser.error("reconciliation requires --records, --rates and --output")
    args.output.unlink(missing_ok=True)
    manifest = load_json((args.root/"manifest.json").read_text())
    cases = {c["id"]: c for c in manifest["cases"]}
    expected = {(c, a) for c in cases for a in ("fast", "minimal")}
    observed = [(r["case"], r["arm"]) for r in rows]
    if len(set(observed)) != len(observed) or not set(observed) <= expected:
        raise ValueError("invalid task rows")
    for row in rows:
        if Path(row["selected_path"]).resolve() != (args.root/row["case"]/row["arm"]/"0").resolve():
            raise ValueError("unexpected artifact path")
        reconcile_row(row)
        verify_outcome(row, cases[row["case"]])
    rates = load_json(args.rates.read_text())
    if rates.get("basis") != "published_list_price_estimate" or not rates.get("source_url") or not rates.get("observed_at"):
        raise ValueError("rates require explicit estimate basis and provenance")
    from trial import cases as qualification_cases
    qcases = {c["id"]: c for c in qualification_cases()}
    for row in qualification:
        if Path(row["selected_path"]).resolve() != (args.root/"qualification"/row["case"]/row["arm"]/"0").resolve():
            raise ValueError("unexpected qualification path")
        reconcile_row(row)
        verify_outcome(row, qcases[row["case"]])
    all_records = load_json(args.records.read_text())
    qlineages = {e["lineage_id"] for r in qualification for e in receipts(r)}
    cohort_records = [r for r in all_records if r["lineage_id"] not in qlineages]
    qrecords = [r for r in all_records if r["lineage_id"] in qlineages]
    timelines = {}
    if args.timeline_logs:
        for entry in load_json(args.timeline_logs.read_text()):
            fields = entry.get("jsonPayload", {}).get("fields", {})
            if fields.get("message") != "gateway request timeline":
                continue
            rid = fields["request_id"]
            if rid in timelines:
                raise ValueError("duplicate gateway request timeline")
            timelines[rid] = fields
    result = reconcile(rows, cohort_records, args.organization, args.workspace, rates, timelines)
    qualification_result = reconcile(qualification, qrecords, args.organization, args.workspace, rates, timelines)
    if qlineages & {e["lineage_id"] for r in rows for e in receipts(r)}:
        raise ValueError("qualification and cohort share lineage")
    output = dict(schema="maestro.gateway-cost-reconciliation.v1", rows=result,
                  rates=rates, **summarize(result, set(observed) == expected),
                  qualification=qualification_result,
                  protocol_amendment=load_json((args.root/"continuation-amendment.json").read_text())
                  if (args.root/"continuation-amendment.json").exists() else None,
                  invoice_verified=False,
                  limitations=["Published rates may differ from account rates or invoice rounding.",
                               "Failed or multiple provider attempts require separate per-attempt usage to establish full spend.",
                               "Author-visible development tasks do not establish production equivalence."],
                  source_sha256=hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
                  records_sha256=hashlib.sha256(args.records.read_bytes()).hexdigest(),
                  timeline_logs_sha256=hashlib.sha256(args.timeline_logs.read_bytes()).hexdigest()
                  if args.timeline_logs else None)
    args.output.write_text(json.dumps(output, indent=2)+"\n")


if __name__ == "__main__":
    main()

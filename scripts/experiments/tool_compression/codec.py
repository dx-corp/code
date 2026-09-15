"""Experimental rustc diagnostic projections; never installed in production.

Inputs are typed rustc --error-format=json observations. Raw output is retained
by the trial controller. Compact views omit rendered source excerpts, expansion
metadata and suggestions on spans: they are lossy, and retrieval is available.
"""

import hashlib
import json
from collections import Counter


def encode(value):
    return json.dumps(value, ensure_ascii=False, separators=(",", ":"), sort_keys=True)


def digest(value):
    return hashlib.sha256(encode(value).encode()).hexdigest()


def compact(records):
    groups = {}
    for record in records:
        key = encode(
            {
                "level": record.get("level"),
                "code": (record.get("code") or {}).get("code"),
                "message": record.get("message"),
                "children": record.get("children"),
            }
        )
        group = groups.setdefault(key, dict(json.loads(key), count=0, locations=[]))
        group["count"] += 1
        for span in record.get("spans", []):
            group["locations"].append(
                [
                    span.get("line_start"),
                    span.get("column_start"),
                    span.get("file_name"),
                    span.get("line_end"),
                    span.get("column_end"),
                    span.get("is_primary"),
                    span.get("label"),
                ]
            )
    return (
        "Diagnostic groups; count is the number of diagnostics in each group. Locations=[line,column,file,end_line,end_column,primary,label]. "
        "Source excerpts and expansion details omitted. Originals: previous-full.txt (before), full.txt (after).\n"
        + encode(list(groups.values()))
    )


def delta(before, after):
    # Retain order and multiplicity. References are indices into the exact baseline.
    lookup = {encode(record): i for i, record in enumerate(before)}
    return {
        "baseline_sha256": digest(before),
        "items": [
            {"ref": lookup[encode(record)]}
            if encode(record) in lookup
            else {"new": record}
            for record in after
        ],
    }


def expand_delta(before, patch):
    if patch["baseline_sha256"] != digest(before):
        raise ValueError("baseline digest mismatch")
    result = []
    for item in patch["items"]:
        if (
            set(item) == {"ref"}
            and type(item["ref"]) is int
            and 0 <= item["ref"] < len(before)
        ):
            result.append(before[item["ref"]])
        elif set(item) == {"new"} and isinstance(item["new"], dict):
            result.append(item["new"])
        else:
            raise ValueError("invalid delta item")
    return result


def delta_view(before, after):
    # Multiset differences identify moved/changed diagnostics as removed + added.
    remaining = Counter(compact([x]) for x in before)
    added = []
    for record in after:
        key = compact([record])
        if remaining[key]:
            remaining[key] -= 1
        else:
            added.append(record)
    removed = []
    for record in before:
        key = compact([record])
        if remaining[key]:
            removed.append(record)
            remaining[key] -= 1
    return (
        "Diagnostic delta against the previous observation in this trial. "
        "All other diagnostics are unchanged. Current error-code counts: "
        + encode(dict(Counter(r["code"]["code"] for r in after if r.get("code"))))
        + "\nREMOVED:\n"
        + compact(removed)
        + "\nADDED:\n"
        + compact(added)
    )


def project(arm, raw, records, before=None):
    if arm == "baseline":
        return raw
    if arm == "compact":
        candidate = compact(records)
    elif arm == "delta":
        candidate = compact(records) if before is None else delta_view(before, records)
    elif arm == "adaptive":
        candidate = (
            "Diagnostic overview; detailed locations omitted. "
            "Use cat full.txt for the current original or cat previous-full.txt for the prior original.\n"
            + encode(
                {
                    "levels": dict(Counter(x["level"] for x in records)),
                    "codes": dict(
                        Counter(x["code"]["code"] for x in records if x.get("code"))
                    ),
                }
            )
        )
    else:
        raise ValueError("unknown arm: " + arm)
    return candidate if len(candidate.encode()) < len(raw.encode()) else raw

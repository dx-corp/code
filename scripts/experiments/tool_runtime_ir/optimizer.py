"""Offline tool-plan and speculative-admission experiment.

The scheduler models possible execution; it is not used by the Maestro runtime.
All token figures are fixture estimates and all latency figures exclude model
generation time.
"""

from collections import Counter
import hashlib
import json


SCHEMA = "evalops.maestro.tool-runtime-ir-experiment.v1"
SAFE_EFFECTS = frozenset(("pure", "read"))
EFFECTS = SAFE_EFFECTS | frozenset(("write", "unknown"))
OBSERVATION_ENVELOPE_TOKENS = 12


def _canonical_digest(value):
    encoded = json.dumps(
        value,
        allow_nan=False,
        ensure_ascii=False,
        separators=(",", ":"),
        sort_keys=True,
    ).encode()
    return hashlib.sha256(encoded).hexdigest()


def _positive_int(value, field, *, allow_zero=False):
    minimum = 0 if allow_zero else 1
    if type(value) is not int or value < minimum:
        raise ValueError(f"{field} must be an integer >= {minimum}")
    return value


def _validate_call(raw, position):
    required = {
        "id",
        "tool",
        "effect",
        "depends_on",
        "turn",
        "duration_ms",
        "result_tokens",
        "capability_generation",
        "workspace_revision",
    }
    if not isinstance(raw, dict) or set(raw) != required:
        raise ValueError(f"call {position} fields do not match the v1 schema")
    for field in ("id", "tool", "capability_generation", "workspace_revision"):
        if not isinstance(raw[field], str) or not raw[field]:
            raise ValueError(f"call {position} {field} must be a nonempty string")
    if raw["effect"] not in EFFECTS:
        raise ValueError(f"call {raw['id']} has unsupported effect")
    dependencies = raw["depends_on"]
    if (
        not isinstance(dependencies, list)
        or any(not isinstance(item, str) or not item for item in dependencies)
        or len(dependencies) != len(set(dependencies))
    ):
        raise ValueError(f"call {raw['id']} has invalid dependencies")
    _positive_int(raw["turn"], "turn")
    _positive_int(raw["duration_ms"], "duration_ms", allow_zero=True)
    _positive_int(raw["result_tokens"], "result_tokens", allow_zero=True)
    return dict(raw)


def _validated_case(case):
    if not isinstance(case, dict) or set(case) != {"id", "max_concurrency", "calls"}:
        raise ValueError("case fields do not match the v1 schema")
    if not isinstance(case["id"], str) or not case["id"]:
        raise ValueError("case id must be a nonempty string")
    concurrency = _positive_int(case["max_concurrency"], "max_concurrency")
    if concurrency > 64:
        raise ValueError("max_concurrency exceeds experiment bound")
    if not isinstance(case["calls"], list) or not case["calls"]:
        raise ValueError("case calls must be nonempty")
    calls = [_validate_call(raw, index) for index, raw in enumerate(case["calls"])]
    ids = [call["id"] for call in calls]
    if len(ids) != len(set(ids)):
        raise ValueError("call ids must be unique")
    known = set(ids)
    for call in calls:
        if call["id"] in call["depends_on"] or not set(call["depends_on"]) <= known:
            raise ValueError(f"call {call['id']} has an invalid dependency")
    return case["id"], concurrency, calls


def _dependencies_with_barriers(calls):
    dependencies = {call["id"]: set(call["depends_on"]) for call in calls}
    for index, call in enumerate(calls):
        if call["effect"] not in SAFE_EFFECTS:
            dependencies[call["id"]].update(item["id"] for item in calls[:index])
            for later in calls[index + 1 :]:
                dependencies[later["id"]].add(call["id"])
    return dependencies


def _schedule(calls, max_concurrency, *, preserve_turns):
    dependencies = _dependencies_with_barriers(calls)
    by_id = {call["id"]: call for call in calls}
    positions = {call["id"]: index for index, call in enumerate(calls)}
    remaining = set(by_id)
    completed = set()
    waves = []
    duration_ms = 0
    while remaining:
        ready = [
            call
            for call in calls
            if call["id"] in remaining
            and dependencies[call["id"]].issubset(completed)
        ]
        if not ready:
            raise ValueError("tool plan contains a dependency cycle")
        first = min(ready, key=lambda call: positions[call["id"]])
        if first["effect"] not in SAFE_EFFECTS:
            selected = [first]
        else:
            selected = [call for call in ready if call["effect"] in SAFE_EFFECTS]
            if preserve_turns:
                earliest_turn = min(call["turn"] for call in selected)
                selected = [call for call in selected if call["turn"] == earliest_turn]
            selected = selected[:max_concurrency]
        waves.append([call["id"] for call in selected])
        duration_ms += max(call["duration_ms"] for call in selected)
        for call in selected:
            remaining.remove(call["id"])
            completed.add(call["id"])
    return waves, duration_ms


def compile_case(case):
    case_id, concurrency, calls = _validated_case(case)
    current_waves, current_duration = _schedule(
        calls, concurrency, preserve_turns=True
    )
    candidate_waves, candidate_duration = _schedule(
        calls, concurrency, preserve_turns=False
    )
    result_tokens = sum(call["result_tokens"] for call in calls)
    current_observation_tokens = result_tokens + (
        len(current_waves) * OBSERVATION_ENVELOPE_TOKENS
    )
    candidate_observation_tokens = result_tokens + (
        len(candidate_waves) * OBSERVATION_ENVELOPE_TOKENS
    )
    improvement = (
        (current_duration - candidate_duration) / current_duration
        if current_duration
        else 0.0
    )
    return {
        "id": case_id,
        "serial_duration_ms": sum(call["duration_ms"] for call in calls),
        "current_duration_ms": current_duration,
        "candidate_duration_ms": candidate_duration,
        "modeled_latency_reduction": improvement,
        "modeled_speedup": current_duration / candidate_duration
        if candidate_duration
        else 1.0,
        "current_result_tokens": result_tokens,
        "candidate_result_tokens": result_tokens,
        "current_observation_tokens": current_observation_tokens,
        "candidate_observation_tokens": candidate_observation_tokens,
        "current_waves": current_waves,
        "waves": candidate_waves,
        "exclusive_calls": [
            call["id"] for call in calls if call["effect"] not in SAFE_EFFECTS
        ],
        "eligible_for_cross_turn_parallelism": candidate_waves != current_waves,
    }


def _validate_speculation(value, label):
    required = {
        "tool",
        "effect",
        "args",
        "capability_generation",
        "workspace_revision",
        "duration_ms",
        "generation_remaining_ms",
    }
    if not isinstance(value, dict) or set(value) != required:
        raise ValueError(f"{label} fields do not match the v1 schema")
    for field in ("tool", "capability_generation", "workspace_revision"):
        if not isinstance(value[field], str) or not value[field]:
            raise ValueError(f"{label} {field} must be a nonempty string")
    if value["effect"] not in EFFECTS or not isinstance(value["args"], dict):
        raise ValueError(f"{label} effect or args are invalid")
    _positive_int(value["duration_ms"], f"{label}.duration_ms", allow_zero=True)
    _positive_int(
        value["generation_remaining_ms"],
        f"{label}.generation_remaining_ms",
        allow_zero=True,
    )
    return value


def admit_speculation(prediction, actual):
    prediction = _validate_speculation(prediction, "prediction")
    actual = _validate_speculation(actual, "actual")
    reason = "exact_match"
    if prediction["effect"] not in SAFE_EFFECTS:
        reason = "effect_not_speculatable"
    elif prediction["tool"] != actual["tool"]:
        reason = "tool_mismatch"
    elif prediction["effect"] != actual["effect"]:
        reason = "effect_mismatch"
    elif _canonical_digest(prediction["args"]) != _canonical_digest(actual["args"]):
        reason = "arguments_mismatch"
    elif prediction["capability_generation"] != actual["capability_generation"]:
        reason = "capability_generation_mismatch"
    elif prediction["workspace_revision"] != actual["workspace_revision"]:
        reason = "workspace_revision_mismatch"
    admitted = reason == "exact_match"
    return {
        "admitted": admitted,
        "reason": reason,
        "saved_latency_ms": min(
            prediction["duration_ms"], actual["generation_remaining_ms"]
        )
        if admitted
        else 0,
        "wasted_work_ms": 0 if admitted else prediction["duration_ms"],
    }


def evaluate_cases(document):
    if not isinstance(document, dict) or set(document) != {
        "schema",
        "cases",
        "speculations",
    }:
        raise ValueError("document fields do not match the v1 schema")
    if document["schema"] != SCHEMA:
        raise ValueError("unsupported experiment schema")
    if not isinstance(document["cases"], list) or not document["cases"]:
        raise ValueError("experiment requires cases")
    if not isinstance(document["speculations"], list) or not document["speculations"]:
        raise ValueError("experiment requires speculation attempts")
    compiled = [compile_case(case) for case in document["cases"]]
    if len({case["id"] for case in compiled}) != len(compiled):
        raise ValueError("case ids must be unique")
    attempts = []
    ids = set()
    for attempt in document["speculations"]:
        if not isinstance(attempt, dict) or set(attempt) != {
            "id",
            "prediction",
            "actual",
        }:
            raise ValueError("speculation fields do not match the v1 schema")
        if not isinstance(attempt["id"], str) or not attempt["id"] or attempt["id"] in ids:
            raise ValueError("speculation ids must be unique nonempty strings")
        ids.add(attempt["id"])
        decision = admit_speculation(attempt["prediction"], attempt["actual"])
        attempts.append({"id": attempt["id"], **decision})
    unsafe = sum(
        item["admitted"]
        and (
            attempt["actual"]["effect"] not in SAFE_EFFECTS
            or item["reason"] != "exact_match"
        )
        for item, attempt in zip(attempts, document["speculations"], strict=True)
    )
    eligible = [case for case in compiled if case["eligible_for_cross_turn_parallelism"]]
    improved = sum(case["modeled_latency_reduction"] >= 0.20 for case in eligible)
    reasons = Counter(item["reason"] for item in attempts)
    admitted = sum(item["admitted"] for item in attempts)
    saved_latency_ms = sum(item["saved_latency_ms"] for item in attempts)
    wasted_work_ms = sum(item["wasted_work_ms"] for item in attempts)
    return {
        "tool_plan": {
            "cases": compiled,
            "eligible_cases": len(eligible),
            "cases_with_at_least_20_percent_modeled_reduction": improved,
            "compiler_gate_passed": bool(eligible) and improved * 2 >= len(eligible),
        },
        "speculation": {
            "attempts": len(attempts),
            "admitted": admitted,
            "exact_match_rate": admitted / len(attempts),
            "unsafe_admissions": unsafe,
            "safety_gate_passed": unsafe == 0,
            "utility_assessed": False,
            "utility_gate_passed": None,
            "utility_reason": "adversarial admission fixtures do not measure a real predictor cohort",
            "reasons": dict(sorted(reasons.items())),
            "saved_latency_ms": saved_latency_ms,
            "wasted_work_ms": wasted_work_ms,
            "waste_to_saved_ratio": wasted_work_ms / saved_latency_ms
            if saved_latency_ms
            else None,
            "decisions": attempts,
        },
    }

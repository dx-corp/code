"""Controller-owned artifact regrading, independent of model completion claims."""

import hashlib
import json
from pathlib import Path
import tempfile


def load_json(text):
    def unique(pairs):
        result = {}
        for key, value in pairs:
            if key in result:
                raise ValueError(f"duplicate JSON key: {key}")
            result[key] = value
        return result

    def invalid(value):
        raise ValueError(f"nonfinite JSON number: {value}")

    return json.loads(text, object_pairs_hook=unique, parse_constant=invalid)


def source_hashes():
    here = Path(__file__).resolve().parent
    paths = [
        here / name
        for name in (
            "trial.py",
            "report.py",
            "evidence.py",
            "adversarial.py",
            "followup.py",
            "statistics_report.py",
        )
    ]
    paths.append(here.parent / "tool_compression/repair.py")
    return {p.name: hashlib.sha256(p.read_bytes()).hexdigest() for p in paths}


def verify_outcome(row, case):
    # Imports stay lazy: trial imports report while running a live attempt.
    from trial import CHECK, grade_answer, grade_rust, required_tool_succeeded

    path = Path(row["selected_path"])
    cwd = path / "workspace"
    events = [
        load_json(line) for line in (path / "events.jsonl").read_text().splitlines()
    ]
    calls = [e for e in events if e.get("type") == "tool_call"]
    ends, outputs = {}, {}
    for event in events:
        if event.get("type") == "tool_end":
            if event["call_id"] in ends:
                raise ValueError("duplicate tool completion")
            ends[event["call_id"]] = event.get("success")
        if event.get("type") == "tool_output":
            key = event["call_id"]
            outputs[key] = outputs.get(key, "") + event.get("content", "")
    originals = dict(case["files"])
    if case["family"] == "repair":
        originals["check"] = CHECK
    try:
        intact = all(
            (cwd / name).is_file()
            and not (cwd / name).is_symlink()
            and (cwd / name).read_text() == content
            for name, content in originals.items()
            if name != "src/lib.rs"
        )
        target = cwd / ("src/lib.rs" if case["family"] == "repair" else "answer.json")
        if target.is_symlink():
            correct = False
        elif case["family"] == "repair":
            with tempfile.TemporaryDirectory(prefix="maestro-regrade-") as tmp:
                correct = grade_rust(
                    target.read_text(), case["hidden"], Path(tmp) / "grading"
                )
        else:
            correct = grade_answer(target.read_text(), case["expected"])
    except (OSError, UnicodeError):
        intact, correct = False, False
    required_used = required_tool_succeeded(case, calls, ends, outputs)
    verified = dict(correct=correct, intact=intact, required_tool_used=required_used)
    verified["success"] = (
        row["terminal"] and row["failure"] is None and all(verified.values())
    )
    for key, value in verified.items():
        if type(row.get(key)) is not bool or row[key] != value:
            raise ValueError(f"outcome mismatch: {row['case']} {row['arm']} {key}")
    return verified

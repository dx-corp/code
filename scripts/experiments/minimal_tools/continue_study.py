"""Explicit exploratory amendment: retain failed attempts and finish unrun arms."""

import argparse
from datetime import datetime, timezone
import hashlib
import json
from pathlib import Path

from evidence import load_json, source_hashes
import trial


def remaining(manifest, rows):
    planned = [(c, a) for c, arms in manifest["order"] for a in arms]
    observed = [(r["case"], r["arm"]) for r in rows]
    if observed != planned[:len(observed)]:
        raise ValueError("existing rows are not an exact prefix of the frozen plan")
    return planned[len(observed):]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("root", type=Path)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--reason", required=True)
    args = parser.parse_args()
    root = args.root.resolve()
    manifest = load_json((root/"manifest.json").read_text())
    rows = load_json((root/"rows.json").read_text())
    if source_hashes() != manifest["source_hashes"]:
        raise ValueError("frozen experiment sources changed")
    if trial.digest(args.binary.read_bytes()) != manifest["binary_sha256"]:
        raise ValueError("frozen runtime binary changed")
    unrun = remaining(manifest, rows)
    amendment = root/"continuation-amendment.json"
    if amendment.exists():
        raise ValueError("continuation already attempted; inspect retained evidence")
    if not (root/"stopped.json").exists() or not unrun:
        raise ValueError("requires a stopped, unfinished cohort")
    amendment.write_text(json.dumps(dict(
        recorded_at=datetime.now(timezone.utc).isoformat(), reason=args.reason,
        source_sha256=hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
        original_stop=load_json((root/"stopped.json").read_text()),
        retained_rows_sha256=trial.digest((root/"rows.json").read_bytes()),
        retained_attempts=len(rows), remaining_order=unrun,
        policy="finish every remaining arm; retain all failures; no answer retries",
        inference="exploratory protocol amendment, not confirmatory evidence",
    ), indent=2)+"\n")
    trial.MODEL = manifest["model"]
    cases = {c["id"]:c for c in manifest["cases"]}
    for case, arm in unrun:
        rows.append(trial.run(cases[case], arm, root, args.binary.resolve(), manifest["timeout"]))
        temporary = root/"rows.json.tmp"
        temporary.write_text(json.dumps(rows, indent=2)+"\n")
        temporary.replace(root/"rows.json")
    if trial.digest(args.binary.read_bytes()) != manifest["binary_sha256"]:
        raise ValueError("runtime binary changed during continuation")
    from report import report
    report(root)


if __name__ == "__main__":
    main()

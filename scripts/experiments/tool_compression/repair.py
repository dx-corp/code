#!/usr/bin/env python3
"""Paired native diagnostic projection experiment, graded on executable repairs."""

import argparse
from concurrent.futures import ThreadPoolExecutor
import hashlib
import json
import os
from pathlib import Path
import random
import re
import selectors
import signal
import statistics
import subprocess
import time

SYSTEM = """Repair the Rust source in src/lib.rs to satisfy the user's specification. Run the exact bash command ./check before editing and after your changes. Do not add pipes, redirections, compound commands, or background execution: the benchmark approves only ./check. Edit only src/lib.rs. Do not modify check or .observations. Compiler views may omit details; original JSONL observations in .observations remain available. Use native read/edit/write tools and bash for ./check. Continue until the check passes and your implementation satisfies the specification. Finish with a short summary."""
CHECK = """#!/usr/bin/env python3
import hashlib, json, pathlib, subprocess
root = pathlib.Path(__file__).resolve().parent
records = root / '.observations'
records.mkdir(exist_ok=True)
index = len(list(records.glob('*.jsonl')))
p = subprocess.run(['rustc', '--edition=2024', '--crate-type=lib', '--error-format=json', str(root/'src/lib.rs'), '-o', str(records/'lib.rlib')], capture_output=True, text=True)
raw = p.stderr + json.dumps({'reason':'build-finished','success':p.returncode == 0}) + '\\n'
current = records / f'{index:03d}.jsonl'
current.write_text(raw)
current.with_suffix('.source-sha256').write_text(hashlib.sha256((root/'src/lib.rs').read_bytes()).hexdigest())
if ARM == 'delta':
    command = [BINARY, 'diagnostics', str(current)]
    if index:
        previous = records / f'{index-1:03d}.jsonl'
        command += ['--previous', str(previous), '--previous-sha256', hashlib.sha256(previous.read_bytes()).hexdigest()]
    view = subprocess.run(command, capture_output=True, text=True)
    if view.returncode:
        raise RuntimeError(view.stderr)
    text = view.stdout
else:
    text = ''.join(json.loads(line).get('rendered', line) for line in p.stderr.splitlines())
    text += 'build_succeeded=' + str(p.returncode == 0).lower() + '\\n'
current.with_suffix('.view').write_text(text)
print(text, end='')
print('Original observation:', current.relative_to(root))
raise SystemExit(p.returncode)
"""


def sha(data):
    return hashlib.sha256(data).hexdigest()


def fixture(index):
    families = [
        (
            "ceil_div",
            "Return the ceiling of n / d for unsigned 64-bit integers. d is positive. Avoid overflow even for u64::MAX.",
            "n / d",
            "n / d + u64::from(n % d != 0)",
            [(0, 3), (1, 3), (8, 3), (9, 3), (2**64 - 1, 2), (2**64 - 1, 1)],
        ),
        (
            "saturating_sum",
            "Return a + b for unsigned 64-bit integers, saturating to u64::MAX on overflow.",
            "a + b",
            "a.saturating_add(b)",
            [(0, 0), (3, 9), (2**64 - 1, 1), (2**64 - 2, 1), (2**64 - 2, 10)],
        ),
        (
            "distance",
            "Return the absolute distance between two unsigned 64-bit integers a and b without overflow.",
            "a - b",
            "a.abs_diff(b)",
            [(0, 0), (1, 9), (9, 1), (0, 2**64 - 1), (2**64 - 1, 0)],
        ),
        (
            "clamped_sub",
            "Return a - b for unsigned 64-bit integers, clamped to zero when b exceeds a.",
            "a - b",
            "a.saturating_sub(b)",
            [(0, 1), (3, 9), (9, 3), (2**64 - 1, 1), (0, 0)],
        ),
    ]
    name, spec, broken, fixed, inputs = families[index % len(families)]
    args = "n: u64, d: u64" if name == "ceil_div" else "a: u64, b: u64"
    count = (12, 24)[index // 4 % 2]
    source = "#![allow(dead_code)]\ntype Amount = String;\n"
    source += f"pub fn {name}({args}) -> u64 {{ {broken} }}\n"
    source += (
        "\n".join(
            f"pub fn sample_{i}() -> Amount {{ {i + 1}_u64 }}" for i in range(count)
        )
        + "\n"
    )
    # Oracle remains in the controller, outside the agent working directory.
    expected = []
    for a, b in inputs:
        expected.append(
            {
                "ceil_div": lambda: (a + b - 1) // b,
                "saturating_sum": lambda: min(2**64 - 1, a + b),
                "distance": lambda: abs(a - b),
                "clamped_sub": lambda: max(0, a - b),
            }[name]()
        )
    tests = "\n#[cfg(test)] mod hidden { use super::*;\n"
    for i, ((a, b), result) in enumerate(zip(inputs, expected)):
        tests += (
            f"#[test] fn behavior_{i}() {{ assert_eq!({name}({a}, {b}), {result}); }}\n"
        )
    for i in range(count):
        tests += f"#[test] fn sample_{i}_value() {{ let n: u64 = sample_{i}(); assert_eq!(n, {i+1}); }}\n"
    tests += "}\n"
    return {
        "id": f"repair-{index:02d}",
        "source": source,
        "tests": tests,
        "reference": source.replace(
            "type Amount = String;", "type Amount = u64;"
        ).replace(broken, fixed),
        "prompt": f"{spec} Fix the Amount alias so every sample function returns its documented u64 literal unchanged. Keep all public function names and signatures, except correcting the Amount alias.",
    }


def grade(source, tests, directory):
    directory.mkdir()
    library = directory / "repaired.rs"
    library.write_text(source)
    artifact = directory / "librepaired.rlib"
    p = subprocess.run(
        [
            "rustc",
            "--edition=2024",
            "--crate-type=lib",
            "--crate-name=repaired",
            str(library),
            "-o",
            str(artifact),
        ],
        capture_output=True,
        text=True,
        timeout=60,
    )
    (directory / "library.log").write_text(p.stdout + p.stderr)
    if p.returncode:
        return False
    path = directory / "hidden.rs"
    path.write_text(tests.replace("use super::*;", "use repaired::*;"))
    p = subprocess.run(
        [
            "rustc",
            "--edition=2024",
            "--test",
            str(path),
            "--extern",
            f"repaired={artifact}",
            "-o",
            str(directory / "tests"),
        ],
        capture_output=True,
        text=True,
        timeout=60,
    )
    (directory / "compile.log").write_text(p.stdout + p.stderr)
    if p.returncode:
        return False
    expected = {
        "hidden::" + name for name in re.findall(r"#\[test\] fn (\w+)\(", tests)
    }
    p = subprocess.run(
        [str(directory / "tests"), "--test-threads=1"],
        capture_output=True,
        text=True,
        timeout=30,
    )
    (directory / "tests.log").write_text(p.stdout + p.stderr)
    passed = set(re.findall(r"^test (\S+) \.\.\. ok$", p.stdout, re.MULTILINE))
    return (
        bool(expected)
        and p.returncode == 0
        and passed == expected
        and f"test result: ok. {len(expected)} passed; 0 failed; 0 ignored;" in p.stdout
    )


def shutdown(p, send):
    """Reap the child even when it closes stdin before shutdown is sent."""
    try:
        if p.poll() is None:
            try:
                send({"type": "shutdown"})
            except BrokenPipeError:
                # A closed input pipe is normal while a failed startup exits.
                # Wait for that exit before attempting any signal.
                pass
            try:
                p.wait(timeout=3)
            except subprocess.TimeoutExpired:
                try:
                    os.killpg(p.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass  # The process group exited between wait and signal.
                except PermissionError:
                    p.kill()  # Signal only our direct child if group signaling is denied.
                p.wait(timeout=3)
        return None
    except (OSError, subprocess.TimeoutExpired) as error:
        return f"cleanup_failed:{type(error).__name__}:pid={p.pid}"


def run_attempt(case, arm, root, binary, model, timeout):
    out = root / f"{case['id']}-{arm}"
    cwd = out / "workspace"
    (cwd / "src").mkdir(parents=True)
    (cwd / "src/lib.rs").write_text(case["source"])
    check = f"ARM = {arm!r}\nBINARY = {str(binary)!r}\n"
    check = CHECK.replace(
        "import hashlib, json, pathlib, subprocess\n",
        "import hashlib, json, pathlib, subprocess\n" + check,
    )
    (cwd / "check").write_text(check)
    (cwd / "check").chmod(0o755)
    start = time.monotonic()
    usage, calls = [], []
    outputs = {}
    prompt_started = None
    terminal, failure, initialized = False, None, False
    with (
        (out / "stderr.log").open("wb") as err,
        (out / "events.jsonl").open("w") as events,
    ):
        p = subprocess.Popen(
            [str(binary), "--headless", "--no-session", "--model", model],
            cwd=cwd,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=err,
            start_new_session=True,
        )
        selector = selectors.DefaultSelector()
        selector.register(p.stdout, selectors.EVENT_READ)
        buffer = b""

        def send(event):
            p.stdin.write((json.dumps(event) + "\n").encode())
            p.stdin.flush()

        try:
            while (
                not terminal and failure is None and time.monotonic() - start < timeout
            ):
                if not initialized and time.monotonic() - start > 120:
                    failure = "initialization_timeout"
                    break
                if not selector.select(0.5):
                    if p.poll() is not None:
                        failure = "runtime_exited"
                    continue
                chunk = os.read(p.stdout.fileno(), 65536)
                if not chunk:
                    failure = "runtime_exited"
                    break
                buffer += chunk
                while b"\n" in buffer:
                    line, buffer = buffer.split(b"\n", 1)
                    e = json.loads(line)
                    events.write(json.dumps(e) + "\n")
                    events.flush()
                    kind = e.get("type")
                    if kind == "ready":
                        if e.get("model") != model:
                            failure = "model_mismatch"
                            break
                        send(
                            {
                                "type": "init",
                                "approval_mode": "prompt",
                                "thinking_level": "minimal",
                                "system_prompt": SYSTEM,
                            }
                        )
                    elif (
                        kind == "status"
                        and e.get("message") == "init applied"
                        and not initialized
                    ):
                        initialized = True
                        prompt_started = time.monotonic()
                        send({"type": "prompt", "content": case["prompt"]})
                    elif kind == "tool_call":
                        calls.append(e)
                        if e.get("requires_approval"):
                            # Approve only the fixed check command; native file tools
                            # are checked again by controller integrity verification.
                            args = e.get("args") or {}
                            approved = (
                                e.get("tool") == "bash"
                                and args.get("command") == "./check"
                            ) or (
                                e.get("tool") in ("edit", "write")
                                and (cwd / args.get("path", "")).resolve()
                                == (cwd / "src/lib.rs").resolve()
                            )
                            send(
                                {
                                    "type": "tool_response",
                                    "call_id": e["call_id"],
                                    "approved": approved,
                                }
                            )
                    elif kind == "tool_output":
                        outputs[e["call_id"]] = outputs.get(e["call_id"], "") + e.get(
                            "content", ""
                        )
                    elif kind == "response_end" and e.get("usage") is not None:
                        usage.append(e["usage"])
                    elif kind in ("error", "provider_error", "turn_interrupted"):
                        failure = kind
                    elif kind == "turn_completed":
                        terminal = True
            if not terminal and failure is None:
                failure = "timeout"
        except (OSError, ValueError) as error:
            failure = f"transport_error:{type(error).__name__}"
        finally:
            cleanup_failure = shutdown(p, send)
            if cleanup_failure:
                failure = cleanup_failure
            selector.close()
            try:
                p.stdin.close()
            except BrokenPipeError:
                if p.poll() is None:
                    failure = "cleanup_failed:stdin_close"
            p.stdout.close()
    elapsed = time.monotonic() - start
    active_seconds = time.monotonic() - prompt_started if prompt_started else None
    source = (cwd / "src/lib.rs").read_text()
    intact = (cwd / "check").read_text() == check
    passed = grade(source, case["tests"], out / "grading")

    def total(field):
        values = [u.get(field) for u in usage]
        return (
            sum(values)
            if values and all(type(v) in (int, float) and v >= 0 for v in values)
            else None
        )

    views = sorted((cwd / ".observations").glob("*.view"))
    delivered = sum(
        any(view.read_text().strip() in output for output in outputs.values())
        for view in views
    )
    initial = cwd / ".observations/000.source-sha256"
    initial_before_edit = initial.exists() and initial.read_text() == sha(
        case["source"].encode()
    )
    row = {
        "case": case["id"],
        "arm": arm,
        "success": terminal and failure is None and intact and passed,
        "hidden_tests_passed": passed,
        "check_intact": intact,
        "terminal": terminal,
        "failure": failure,
        "elapsed_seconds": elapsed,
        "active_seconds": active_seconds,
        "delivered_views": delivered,
        "initial_before_edit": initial_before_edit,
        "tool_calls": len(calls),
        "compiler_checks": len(list((cwd / ".observations").glob("*.jsonl"))),
        "source_sha256": sha(source.encode()),
        **{
            f: total(f)
            for f in (
                "input_tokens",
                "output_tokens",
                "cache_read_tokens",
                "total_cost_usd",
            )
        },
    }
    (out / "receipt.json").write_text(json.dumps(row, indent=2))
    print(json.dumps(row), flush=True)
    return row


def run(case, arm, root, binary, model, timeout):
    attempts = root / f"{case['id']}-{arm}-attempts"
    attempts.mkdir()
    started = time.monotonic()
    for index in range(3):
        attempt = attempts / str(index)
        attempt.mkdir()
        row = run_attempt(case, arm, attempt, binary, model, timeout)
        stderr = (attempt / f"{case['id']}-{arm}" / "stderr.log").read_text()
        retryable = (
            row["failure"] == "runtime_exited"
            and row["active_seconds"] is None
            and row["input_tokens"] is None
            and row["tool_calls"] == 0
            and "identity.evalops.dev/v1/tokens/introspect" in stderr
            and "timed out" in stderr
        )
        if not retryable:
            break
    row["startup_attempts"] = index + 1
    row["wall_seconds_including_retries"] = time.monotonic() - started
    row["selected_attempt"] = str((attempt / f"{case['id']}-{arm}").relative_to(root))
    (attempts / "selected.json").write_text(json.dumps(row, indent=2))
    return row


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--output", type=Path, required=True)
    ap.add_argument("--binary", type=Path, required=True)
    ap.add_argument("--model", default="evalops/accounts/fireworks/models/glm-5p3")
    ap.add_argument("--count", type=int, default=8)
    ap.add_argument("--workers", type=int, default=2)
    ap.add_argument("--seed", type=int, default=90421)
    ap.add_argument("--timeout", type=int, default=240)
    ap.add_argument("--live", action="store_true")
    args = ap.parse_args()
    args.output = args.output.resolve()
    args.binary = args.binary.resolve()
    args.output.mkdir()
    cases = [fixture(i) for i in range(args.count)]
    for case in cases:
        assert not grade(
            case["source"], case["tests"], args.output / (case["id"] + "-broken")
        )
        assert grade(
            case["reference"], case["tests"], args.output / (case["id"] + "-reference")
        )
    order = [(c, a) for c in cases for a in ("baseline", "delta")]
    random.Random(args.seed).shuffle(order)
    (args.output / "manifest.json").write_text(
        json.dumps(
            {
                "binary": str(args.binary),
                "binary_sha256": sha(args.binary.read_bytes()),
                "harness_sha256": sha(Path(__file__).read_bytes()),
                "model": args.model,
                "system": SYSTEM,
                "seed": args.seed,
                "cases": cases,
                "order": [(c["id"], a) for c, a in order],
            },
            indent=2,
        )
    )
    if not args.live:
        return
    with ThreadPoolExecutor(max_workers=args.workers) as pool:
        futures = [
            pool.submit(run, c, a, args.output, args.binary, args.model, args.timeout)
            for c, a in order
        ]
        rows = [f.result() for f in futures]
    report = {
        "comparison_valid": all(
            r["failure"] in (None, "timeout")
            and r["compiler_checks"] >= 2
            and r["delivered_views"] == r["compiler_checks"]
            and r["initial_before_edit"]
            for r in rows
        ),
        "rows": rows,
        "arms": {},
    }
    for arm in ("baseline", "delta"):
        selected = [r for r in rows if r["arm"] == arm]
        report["arms"][arm] = {
            "successes": sum(r["success"] for r in selected),
            "runs": len(selected),
            "median_seconds": statistics.median(r["elapsed_seconds"] for r in selected),
        }
        for field in ("input_tokens", "output_tokens", "total_cost_usd"):
            values = [r[field] for r in selected]
            report["arms"][arm][field] = (
                sum(values) if all(v is not None for v in values) else None
            )
    (args.output / "report.json").write_text(json.dumps(report, indent=2))


if __name__ == "__main__":
    main()

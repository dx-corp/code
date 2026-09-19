#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# dependencies = ["pyte==0.8.2", "Pillow==11.3.0"]
# ///
"""Capture a real Dex gallery and optionally check reviewed PNG baselines."""

import argparse
import importlib.util
import json
from pathlib import Path
import shutil
import subprocess
import tempfile

from PIL import Image, ImageChops

spec = importlib.util.spec_from_file_location(
    "capture_tui", Path(__file__).with_name("capture-tui.py")
)
capture_tui = importlib.util.module_from_spec(spec)
spec.loader.exec_module(capture_tui)

preview_spec = importlib.util.spec_from_file_location(
    "review_ui", Path(__file__).with_name("review-ui.py")
)
preview = importlib.util.module_from_spec(preview_spec)
preview_spec.loader.exec_module(preview)

CASES = [
    (name, 100, 30)
    for name in (
        "idle",
        "typing",
        "conversation",
        "markdown",
        "command-palette",
        "streaming",
        "approval",
        "error",
        "long-conversation",
        "dex-appearance",
        "dex-appearance-picker",
        "dex-pet",
        "dex-quiet",
        "dex-suggestion",
        "expanded-tool",
        "details",
        "details-return",
        "pet-reactions",
        "theme-picker",
        "theme-picker-empty",
        "theme-picker-long-query",
        "theme-preview-cancel",
        "dex-preview-cancel",
        "dex-preview-save",
    )
] + [
    (name, 60, 20)
    for name in (
        "idle",
        "conversation",
        "markdown",
        "command-palette",
        "long-conversation",
        "details",
        "dex-appearance-picker-scrolled",
        "theme-picker",
        "theme-picker-empty",
        "theme-picker-long-query",
    )
]
# Bounded cohesion coverage: real light palette selection, narrow modals and
# an explicit reduced-motion command. Long Unicode queries are covered above.
CASES += [
    ("idle-light", 100, 30),
    ("conversation-light", 100, 30),
    ("approval-light", 60, 20),
    ("command-palette-light", 60, 20),
    ("theme-picker-light", 100, 30),
    ("model-picker-light", 100, 30),
    ("session-picker-light", 100, 30),
    ("dex-reduced-motion", 100, 30),
]
CASES += [(f"{scene}-{color}", 100, 30)
          for color in ("green", "pink", "blue", "green-dark", "pink-dark", "blue-dark")
          for scene in ("conversation", "theme-picker")]
# Live progress includes an elapsed timer. Keep the actual capture for review;
# do not normalize or paint over it to manufacture a stable screenshot.
LIVE_SCENES = {"streaming"}


def appearance_scenes(binary):
    """Select by command ID, never by a row index in the interactive picker."""
    import re

    scenes = preview.catalog(binary)
    actions = {}
    for scene in scenes:
        identifier = scene["id"]
        if identifier.startswith(("accessory-", "accent-")):
            actions[identifier] = scene["label"]
    if not actions:
        raise ValueError("preview catalog has no appearance actions")
    return [
        {
            "name": identifier,
            "steps": [
                {"wait": "(?s)Dex Code.*GPT-6 Astra.*release-planner"},
                {"text": "/dex standard"},
                {"key": "Enter"},
                {"wait": "Dex: standard"},
                {"text": "/dex " + identifier},
                {"key": "Enter"},
                {"wait": re.escape(label) + ".*saved"},
                {"absent": "Make Dex yours"},
            ],
        }
        for identifier, label in sorted(actions.items())
    ]


def compare_images(actual, expected, difference):
    with Image.open(actual) as a, Image.open(expected) as b:
        if a.size != b.size:
            return False
        diff = ImageChops.difference(a.convert("RGB"), b.convert("RGB"))
        if diff.getbbox() is None:
            return True
        diff.save(difference)
        return False


def selected_cases(names, cases=None):
    """Select named native scenarios while retaining each supported size."""
    cases = CASES if cases is None else cases
    if not names:
        return cases
    unknown = set(names) - {name for name, _, _ in cases}
    if unknown:
        raise ValueError(f"unknown cases: {', '.join(sorted(unknown))}")
    return [case for case in cases if case[0] in names]


def run(args):
    if args.output.exists() or (args.record_baseline and args.record_baseline.exists()):
        raise ValueError("output and candidate baseline directories must be new")
    binary = getattr(args, "catalog_binary", None) or preview.build()
    appearances = appearance_scenes(binary)
    args.output.mkdir(parents=True)
    generated = args.output / "scenarios"
    generated.mkdir()
    for scenario in appearances:
        (generated / (scenario["name"] + ".json")).write_text(
            json.dumps(scenario, indent=2) + "\n"
        )
    cases = CASES + [(scene["name"], 100, 30) for scene in appearances]
    cases = selected_cases(getattr(args, "case", None), cases)
    results = []
    for scene, columns, rows in cases:
        name = f"{scene}-{columns}x{rows}"
        output = args.output / name
        result = {"case": name, "status": "failed"}
        try:
            capture_tui.capture(
                argparse.Namespace(
                    binary=args.binary,
                    fixture=True,
                    scenario=(
                        generated
                        if (generated / f"{scene}.json").exists()
                        else capture_tui.FIXTURES
                    )
                    / f"{scene}.json",
                    output=output,
                    columns=columns,
                    rows=rows,
                    font=args.font,
                    font_size=18,
                    timeout=20,
                )
            )
            result["status"] = "captured"
            if scene in LIVE_SCENES:
                result["comparison"] = "live progress; manual visual review"
            elif args.check_baseline:
                baseline = args.check_baseline / f"{name}.png"
                if not baseline.is_file():
                    raise ValueError(f"missing baseline: {baseline}")
                if not compare_images(
                    output / "screen.png", baseline, output / "difference.png"
                ):
                    raise ValueError(
                        "visual baseline differs; inspect screen.png and difference.png"
                    )
                result["comparison"] = "passed"
        except Exception as error:
            result.update(status="failed", error=str(error))
        results.append(result)
        (args.output / "results.json").write_text(json.dumps(results, indent=2) + "\n")
    if args.record_baseline and all(
        result["status"] == "captured" for result in results
    ):
        args.record_baseline.parent.mkdir(parents=True, exist_ok=True)
        with tempfile.TemporaryDirectory(
            prefix=".native-baseline-", dir=args.record_baseline.parent
        ) as temp:
            stage = Path(temp) / "candidate"
            stage.mkdir()
            for scene, columns, rows in cases:
                if scene not in LIVE_SCENES:
                    name = f"{scene}-{columns}x{rows}"
                    shutil.copy2(
                        args.output / name / "screen.png", stage / f"{name}.png"
                    )
            stage.rename(args.record_baseline)
    gallery = [
        "# Dex native UI gallery",
        "",
        "Real local fixture sessions; 100-column and 60-column terminals.",
        "",
    ]
    for result in results:
        gallery += [
            f"## {result['case']}",
            "",
            f"![{result['case']}]({result['case']}/screen.png)",
            "",
            result.get("error", result.get("comparison", "Captured for review.")),
            "",
        ]
    (args.output / "gallery.md").write_text("\n".join(gallery))
    return 1 if any(result["status"] == "failed" for result in results) else 0


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--font")
    parser.add_argument(
        "--catalog-binary",
        type=Path,
        help="current maestro-ui-preview binary; otherwise build it locally",
    )
    parser.add_argument(
        "--case", action="append",
        help="capture only this scenario at its supported sizes; repeatable, including catalog IDs",
    )
    baselines = parser.add_mutually_exclusive_group()
    baselines.add_argument("--check-baseline", type=Path)
    baselines.add_argument(
        "--record-baseline",
        type=Path,
        help="create a NEW candidate baseline directory for review",
    )
    try:
        raise SystemExit(run(parser.parse_args()))
    except (ValueError, OSError, subprocess.SubprocessError) as error:
        parser.exit(1, f"Native capture failed: {error}\n")


if __name__ == "__main__":
    main()

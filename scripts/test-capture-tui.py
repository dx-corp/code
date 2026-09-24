# /// script
# requires-python = ">=3.11"
# dependencies = ["pyte==0.8.2", "Pillow==11.3.0"]
# ///
"""Local unit and real-tmux integration tests; no Maestro build required."""

import argparse
import importlib.util
import json
from pathlib import Path
import shutil
import tempfile
import unittest
import urllib.error
import urllib.request

from PIL import Image

spec = importlib.util.spec_from_file_location(
    "capture_tui", Path(__file__).with_name("capture-tui.py")
)
capture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(capture)


class CaptureTests(unittest.TestCase):
    def test_suite_case_selection_preserves_sizes_and_rejects_unknown_names(self):
        spec = importlib.util.spec_from_file_location(
            "capture_suite", Path(__file__).with_name("capture-tui-suite.py")
        )
        suite = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(suite)
        self.assertEqual(suite.selected_cases(None), suite.CASES)
        selected = suite.selected_cases(["theme-picker", "theme-picker-empty"])
        self.assertIn(("theme-picker", 100, 30), selected)
        self.assertIn(("theme-picker", 60, 20), selected)
        self.assertTrue(all(case[0].startswith("theme-picker") for case in selected))
        generated = ("accessory-crown", 100, 30)
        self.assertEqual(
            suite.selected_cases(["accessory-crown"], suite.CASES + [generated]),
            [generated],
        )
        with self.assertRaises(ValueError):
            suite.selected_cases(["misspelled-case"])

    def test_light_scenes_select_real_palette_and_reduced_motion_is_explicit(self):
        for name in (
            "approval-light", "command-palette-light", "theme-picker-light",
            "model-picker-light", "session-picker-light", "idle-light", "conversation-light",
        ):
            with self.subTest(scene=name):
                scenario = capture.load_scenario(capture.FIXTURES / f"{name}.json")
                steps = scenario["steps"]
                self.assertEqual(steps[1:4], [
                    {"text": "/theme"}, {"key": "Enter"}, {"wait": "Select Theme"},
                ])
                self.assertEqual(steps[4], {"text": "light"})
                self.assertEqual(steps[6:8], [
                    {"key": "Enter"}, {"absent": "Select Theme"},
                ])
        reduced = capture.load_scenario(capture.FIXTURES / "dex-reduced-motion.json")
        self.assertIn({"text": "/dex motion-off"}, reduced["steps"])
        self.assertEqual(reduced["steps"][-1], {"wait": "Dex appreciates the boop"})

    def test_fixture_serves_only_local_scripted_contracts(self):
        from tui_capture_fixture import CaptureFixture

        with CaptureFixture() as fixture:
            base = fixture.environment()["MAESTRO_IDENTITY_URL"]

            def post(path, body=None):
                request = urllib.request.Request(
                    base + path,
                    data=json.dumps(body or {}).encode(),
                    headers={"Content-Type": "application/json"},
                )
                with urllib.request.urlopen(request, timeout=3) as response:
                    return response.read() if response.headers.get_content_type() == "application/proto" else response.read().decode()

            identity = json.loads(post("/v1/tokens/introspect"))
            self.assertEqual(identity["organization_id"], "capture-org")
            self.assertEqual(identity["workspace_id"], "capture-workspace")
            policy = post("/deixicpublic.v1.DeixicPublicService/GetClientSetup")
            self.assertEqual(
                policy, b"\x0a\x0d\x0a\x0bcapture-org\x10\x01\x32\x02\x08\x02"
            )
            self.assertIn("tool_calls", post("/v1/chat/completions"))
            self.assertIn(
                "The README is a good starting point",
                post(
                    "/v1/chat/completions",
                    {"messages": [{"role": "tool", "content": "# Release checklist"}]},
                ),
            )
            with self.assertRaises(urllib.error.HTTPError) as error:
                post("/v1/chat/completions")
            self.assertEqual(error.exception.code, 409)
            error.exception.close()
            with self.assertRaises(urllib.error.HTTPError) as error:
                post("/unknown")
            self.assertEqual(error.exception.code, 404)
            error.exception.close()

    def test_astra_responses_requires_the_real_tool_result(self):
        from tui_capture_fixture import CaptureFixture

        with CaptureFixture() as fixture:
            def post(body):
                request = urllib.request.Request(
                    fixture.environment()["OPENAI_BASE_URL"] + "/responses",
                    data=json.dumps(body).encode(),
                    headers={"Content-Type": "application/json"},
                )
                with urllib.request.urlopen(request, timeout=3) as response:
                    return json.loads(response.read().decode().removeprefix("data: "))

            event = post({"model": "gpt-6-astra", "input": []})
            self.assertEqual(event["response"]["model"], "gpt-6-astra")
            call = event["response"]["output"][0]
            self.assertEqual(call["type"], "function_call")
            self.assertEqual(call["name"], "read")
            event = post({"input": [{"type": "function_call_output", "call_id": call["call_id"], "output": "# Release checklist"}]})
            self.assertIn("README", event["response"]["output"][0]["content"][0]["text"])
        with CaptureFixture() as fixture:
            post({"input": []})
            with self.assertRaises(urllib.error.HTTPError) as error:
                post({"input": []})
            self.assertEqual(error.exception.code, 409)
            error.exception.close()

    def test_capture_waits_past_partial_frame_and_cursor_updates(self):
        class Redrawing:
            columns, rows = 40, 10
            calls = 0

            def capture(self):
                self.calls += 1
                return (
                    "ready: partial" if self.calls < 3 else "ready: complete controls"
                )

            def cursor(self):
                return (self.calls, 0) if self.calls < 4 else (4, 0)

        terminal = Redrawing()
        ansi, cursor = capture.Terminal.settle(terminal, "ready", 3)
        self.assertEqual(ansi, "ready: complete controls")
        self.assertEqual(cursor, (4, 0))
        self.assertGreaterEqual(terminal.calls, 7)

    def test_visual_regression_detects_pixel_and_size_changes(self):
        spec = importlib.util.spec_from_file_location(
            "capture_suite", Path(__file__).with_name("capture-tui-suite.py")
        )
        suite = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(suite)
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            expected, actual, diff = (
                root / name for name in ("expected.png", "actual.png", "difference.png")
            )
            Image.new("RGB", (8, 8), "black").save(expected)
            shutil.copyfile(expected, actual)
            self.assertTrue(suite.compare_images(actual, expected, diff))
            changed = Image.new("RGB", (8, 8), "black")
            changed.putpixel((3, 4), (255, 0, 0))
            changed.save(actual)
            self.assertFalse(suite.compare_images(actual, expected, diff))
            self.assertTrue(diff.is_file())
            Image.new("RGB", (9, 8), "black").save(actual)
            self.assertFalse(suite.compare_images(actual, expected, diff))

    def test_scenes_use_real_approval_and_error_tool_paths(self):
        from tui_capture_fixture import CaptureFixture

        for scene, expected in [
            ("approval", "printf release-ready"),
            ("approval-light", "printf release-ready"),
            ("error", "missing-checklist.md"),
        ]:
            with self.subTest(scene=scene), CaptureFixture(scene) as fixture:
                request = urllib.request.Request(
                    fixture.environment()["OPENAI_BASE_URL"] + "/chat/completions",
                    data=b"{}",
                    headers={"Content-Type": "application/json"},
                )
                with urllib.request.urlopen(request, timeout=3) as response:
                    body = response.read().decode()
                    self.assertIn(expected, body)
                    self.assertIn('"tool_calls"', body)

    def test_ansi_colors_wide_and_combining_characters(self):
        screen = capture.screen_from_ansi("\x1b[31mR\x1b[0m 界e\u0301\nnext", 40, 10)
        self.assertEqual(screen.buffer[0][0].fg, "red")
        self.assertEqual(screen.buffer[0][2].data, "界")
        self.assertEqual(screen.buffer[0][3].data, "")
        self.assertEqual(screen.buffer[0][4].data, "é")
        self.assertTrue(screen.display[1].startswith("next"))

    def test_scenarios_require_readiness_and_safe_names(self):
        with tempfile.TemporaryDirectory() as temp:
            path = Path(temp) / "scenario.json"
            for scenario in [
                [],
                {"name": 1, "steps": [{"wait": "ready"}]},
                {"name": "../escape", "steps": [{"wait": "ready"}]},
                {"name": "idle", "steps": [{"key": "Enter"}]},
                {"name": "idle", "steps": [{"key": "C-z"}, {"wait": "ready"}]},
                {"name": "idle", "steps": [{"wait": "ready", "text": "oops"}]},
                {"name": "idle", "steps": [{"shell": "echo unsafe"}]},
            ]:
                with self.subTest(scenario=scenario):
                    path.write_text(json.dumps(scenario))
                    with self.assertRaises(ValueError):
                        capture.load_scenario(path)
        for path in capture.FIXTURES.glob("*.json"):
            capture.load_scenario(path)

    def test_indexed_theme_truecolor_and_dim_are_distinct(self):
        screen = capture.screen_from_ansi(
            "\x1b[38;5;2mA\x1b[38;2;0;255;0mB\x1b[2mC\x1b[22mD", 40, 10
        )
        self.assertEqual(screen.buffer[0][0].fg, "green")
        self.assertEqual(screen.buffer[0][1].fg, "00ff00")
        self.assertTrue(screen.buffer[0][2].dim)
        self.assertFalse(screen.buffer[0][3].dim)

    def test_renderer_preserves_background_and_braille(self):
        with tempfile.TemporaryDirectory() as temp:
            output = Path(temp) / "screen.png"
            screen = capture.screen_from_ansi("\x1b[41m \x1b[0m⣿", 40, 10)
            capture.render_png(screen, output, capture.font_path())
            with Image.open(output) as image:
                self.assertEqual(image.format, "PNG")
                self.assertEqual(image.getpixel((16, 16)), (210, 139, 135))
                self.assertGreater(len(image.getcolors() or []), 2)

    @unittest.skipUnless(shutil.which("tmux"), "tmux is required for PTY integration")
    def test_capture_preserves_blank_canvas_background_to_right_edge(self):
        with tempfile.TemporaryDirectory(prefix="mst-", dir="/tmp") as temp:
            root = Path(temp)
            binary = root / "fixture"
            binary.write_text(
                "#!/bin/sh\nprintf 'ready\\n\\033[48;2;238;232;224m%60s\\033[0m\\n' ''\nread answer\n"
            )
            binary.chmod(0o755)
            with capture.Terminal(root, binary, 60, 15) as terminal:
                terminal.wait("ready", 3)
                screen = capture.screen_from_ansi(terminal.capture(), 60, 15)
                self.assertEqual(screen.buffer[1][59].bg, "eee8e0")

    @unittest.skipUnless(shutil.which("tmux"), "tmux is required for PTY integration")
    def test_real_terminal_capture_timeout_and_cleanup(self):
        with tempfile.TemporaryDirectory(prefix="mst-", dir="/tmp") as temp:
            root = Path(temp)
            binary = root / "fake native binary"
            binary.write_text(
                "#!/bin/sh\nif [ \"$1\" = --version ]; then echo fixture-1; exit 0; fi\nprintf '\\033[32mfixture ready\\033[0m\\n'\nread answer\nprintf 'received: %s\\n' \"$answer\"\nread rest\n"
            )
            binary.chmod(0o755)
            terminal = capture.Terminal(root, binary, 60, 15)
            with terminal:
                terminal.wait("fixture ready", 3)
                terminal.run(
                    "send-keys", "-t", "capture:0.0", "-l", "--", "literal ; text"
                )
                terminal.run("send-keys", "-t", "capture:0.0", "Enter")
                self.assertIn(
                    "received: literal ; text",
                    terminal.wait("received: literal ; text", 3),
                )
                terminal.wait("not on screen", 0.3, absent=True)
                with self.assertRaises(TimeoutError):
                    terminal.wait("never visible", 0.2)
            self.assertEqual(terminal.run("list-sessions", check=False), "")

    @unittest.skipUnless(shutil.which("tmux"), "tmux is required for PTY integration")
    def test_failed_capture_records_failure_and_refuses_stale_output(self):
        with tempfile.TemporaryDirectory(prefix="mst-", dir="/tmp") as temp:
            root = Path(temp)
            binary = root / "fixture"
            binary.write_text(
                "#!/bin/sh\nif [ \"$1\" = --version ]; then echo fixture-1; exit 0; fi\nprintf 'actual screen\\n'\nread answer\n"
            )
            binary.chmod(0o755)
            scenario = root / "scenario.json"
            scenario.write_text(
                json.dumps({"name": "timeout", "steps": [{"wait": "missing content"}]})
            )
            args = argparse.Namespace(
                scenario=scenario,
                binary=str(binary),
                font=None,
                output=root / "output",
                columns=60,
                rows=15,
                font_size=18,
                timeout=0.3,
            )
            with self.assertRaises(TimeoutError):
                capture.capture(args)
            manifest = json.loads((args.output / "manifest.json").read_text())
            self.assertEqual(manifest["status"], "failed")
            self.assertIn("actual screen", (args.output / "failure.ansi").read_text())
            self.assertFalse((args.output / "screen.png").exists())
            with self.assertRaises(FileExistsError):
                capture.capture(args)

    @unittest.skipUnless(shutil.which("tmux"), "tmux is required for PTY integration")
    def test_successful_capture_writes_complete_bundle(self):
        with tempfile.TemporaryDirectory(prefix="mst-", dir="/tmp") as temp:
            root = Path(temp)
            binary = root / "fixture"
            binary.write_text(
                "#!/bin/sh\nif [ \"$1\" = --version ]; then echo fixture-1; exit 0; fi\nprintf 'fixture ready\\n'\nread answer\n"
            )
            binary.chmod(0o755)
            scenario = root / "scenario.json"
            scenario.write_text(
                json.dumps({"name": "ready", "steps": [{"wait": "fixture ready"}]})
            )
            args = argparse.Namespace(
                scenario=scenario,
                binary=str(binary),
                font=None,
                output=root / "output",
                columns=60,
                rows=15,
                font_size=18,
                timeout=3,
            )
            capture.capture(args)
            manifest = json.loads((args.output / "manifest.json").read_text())
            self.assertEqual(manifest["status"], "passed")
            self.assertEqual(len(manifest["binary_sha256"]), 64)
            self.assertIn("fixture ready", (args.output / "screen.txt").read_text())
            self.assertTrue((args.output / "screen.ansi").exists())
            with Image.open(args.output / "screen.png") as image:
                image.verify()


if __name__ == "__main__":
    unittest.main()

#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# dependencies = ["pyte==0.8.2", "Pillow==11.3.0"]
# ///
"""Capture the real native TUI in an isolated tmux server (development only)."""

import argparse
from collections import namedtuple
from contextlib import ExitStack
import hashlib
import json
import math
import os
from pathlib import Path
import re
import shlex
import shutil
import subprocess
import sys
import tempfile
import time

import pyte
from PIL import Image, ImageDraw, ImageFont


FIXTURES = Path(__file__).resolve().parent.parent / "test/fixtures/tui-capture"
COLORS = {
    "default": "#e6e4e0",
    "black": "#1b1c1e",
    "red": "#d28b87",
    "green": "#a0b69a",
    "brown": "#c9b58c",
    "blue": "#96a9c7",
    "magenta": "#b0a2cb",
    "cyan": "#93b6b3",
    "white": "#d8d6d2",
    "brightblack": "#85868b",
    "brightred": "#e3a19b",
    "brightgreen": "#b3c8ac",
    "brightbrown": "#ddc9a1",
    "brightblue": "#acbdd8",
    "brightmagenta": "#c3b5de",
    "brightcyan": "#a8cbc8",
    "brightwhite": "#f4f2ee",
}
BACKGROUND = COLORS["black"]
CaptureChar = namedtuple("CaptureChar", [*pyte.screens.Char._fields, "dim"])


class CaptureScreen(pyte.Screen):
    def reset(self):
        super().reset()
        self.cursor.attrs = self.default_char

    @property
    def default_char(self):
        return CaptureChar(**super().default_char._asdict(), dim=False)

    def select_graphic_rendition(self, *attrs):
        # Preserve palette indices. pyte otherwise flattens 38;5;0..15 to
        # xterm RGB, bypassing the terminal theme and confusing truecolor.
        tokens = list(attrs or (0,))
        while tokens:
            attr = tokens.pop(0)
            if attr in (38, 48) and tokens:
                mode = tokens.pop(0)
                count = 1 if mode == 5 else 3 if mode == 2 else 0
                values, tokens = tokens[:count], tokens[count:]
                if mode == 5 and len(values) == 1 and 0 <= values[0] < 16:
                    index = values[0]
                    code = (
                        (30 if attr == 38 else 40)
                        + index % 8
                        + (60 if index >= 8 else 0)
                    )
                    super().select_graphic_rendition(code)
                else:
                    super().select_graphic_rendition(attr, mode, *values)
            elif attr == 2:
                self.cursor.attrs = self.cursor.attrs._replace(dim=True)
            else:
                super().select_graphic_rendition(attr)
                if attr == 22:
                    self.cursor.attrs = self.cursor.attrs._replace(dim=False)


def load_scenario(path):
    scenario = json.loads(path.read_text())
    if not isinstance(scenario, dict) or set(scenario) != {"name", "steps"}:
        raise ValueError("scenario requires exactly name and steps")
    if not isinstance(scenario["name"], str) or not re.fullmatch(
        r"[a-z0-9][a-z0-9-]{0,63}", scenario["name"]
    ):
        raise ValueError("scenario name must be a safe lowercase filename")
    if not isinstance(scenario["steps"], list) or not scenario["steps"]:
        raise ValueError("scenario steps must be a nonempty list")
    for step in scenario["steps"]:
        if not isinstance(step, dict) or len(step) != 1:
            raise ValueError(
                "each step requires exactly one of wait, absent, text, key"
            )
        kind, value = next(iter(step.items()))
        if (
            kind not in {"wait", "absent", "text", "key"}
            or not isinstance(value, str)
            or not value
        ):
            raise ValueError(
                "steps require nonempty wait, absent, text, or key strings"
            )
        if kind in {"wait", "absent"}:
            re.compile(value)
        if kind == "key" and value not in {
            "Enter",
            "Escape",
            "Tab",
            "Up",
            "Down",
            "C-s",
            "C-r",
            "C-k",
            "C-t",
            "C-e",
            "C-o",
            "C-c",
        }:
            raise ValueError(f"unsupported key: {value}")
    if not any("wait" in step for step in scenario["steps"]) or not (
        {"wait", "absent"} & scenario["steps"][-1].keys()
    ):
        raise ValueError("the final step must wait for visible screenshot content")
    return scenario


def font_path(explicit=None):
    candidates = (
        [explicit]
        if explicit
        else [
            "/System/Library/Fonts/Menlo.ttc",
            "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf",
            "/usr/share/fonts/truetype/liberation2/LiberationMono-Regular.ttf",
        ]
    )
    for candidate in candidates:
        if candidate and Path(candidate).is_file():
            return Path(candidate).resolve()
    raise ValueError("no monospace font found; pass --font /path/to/font.ttf")


def screen_from_ansi(ansi, columns, rows):
    screen = CaptureScreen(columns, rows)
    # capture-pane emits newline-separated rows, not cursor motion.
    pyte.Stream(screen).feed(ansi.rstrip("\n").replace("\n", "\r\n"))
    return screen


def color(value, background=False):
    if value == "default" and background:
        return BACKGROUND
    if value in COLORS:
        return COLORS[value]
    if re.fullmatch(r"[0-9a-fA-F]{6}", value):
        return "#" + value
    raise ValueError(f"unsupported terminal color: {value}")


def font_faces(font, size):
    face = ImageFont.truetype(str(font), size)
    bold_face = face
    if font.suffix.lower() == ".ttc":
        candidate = ImageFont.truetype(str(font), size, index=1)
        if "bold" in candidate.getname()[1].lower():
            bold_face = candidate
    else:
        try:
            axes = face.get_variation_axes()
        except OSError:
            axes = []
        if axes:
            values = [axis["default"] for axis in axes]
            for index, axis in enumerate(axes):
                if axis["name"].lower() == b"weight":
                    values[index] = min(axis["maximum"], max(700, axis["default"]))
            candidate = ImageFont.truetype(str(font), size)
            candidate.set_variation_by_axes(values)
            bold_face = candidate
    return face, bold_face


def render_png(screen, output, font, size=18, cursor=None):
    face, bold_face = font_faces(font, size)
    cell_width = math.ceil(face.getlength("M"))
    ascent, descent = face.getmetrics()
    cell_height = ascent + descent
    padding = 16
    image = Image.new(
        "RGB",
        (
            screen.columns * cell_width + padding * 2,
            screen.lines * cell_height + padding * 2,
        ),
        BACKGROUND,
    )
    draw = ImageDraw.Draw(image)
    # Paint all backgrounds first so wide glyphs can span their continuation cell.
    for row in range(screen.lines):
        for col in range(screen.columns):
            cell = screen.buffer[row][col]
            if cursor == (col, row):
                cell = cell._replace(reverse=not cell.reverse)
            fg, bg = color(cell.fg), color(cell.bg, True)
            if cell.reverse:
                fg, bg = bg, fg
            x, y = padding + col * cell_width, padding + row * cell_height
            draw.rectangle((x, y, x + cell_width - 1, y + cell_height - 1), fill=bg)
    for row in range(screen.lines):
        for col in range(screen.columns):
            cell = screen.buffer[row][col]
            if cursor == (col, row):
                cell = cell._replace(reverse=not cell.reverse)
            fg = color(cell.bg, True) if cell.reverse else color(cell.fg)
            if cell.dim:
                bg = color(cell.fg) if cell.reverse else color(cell.bg, True)
                fg = tuple(
                    round(int(fg[i : i + 2], 16) * 0.65 + int(bg[i : i + 2], 16) * 0.35)
                    for i in (1, 3, 5)
                )
            x, y = padding + col * cell_width, padding + row * cell_height
            legs = {
                "─": "lr",
                "│": "ud",
                "┌": "rd",
                "┐": "ld",
                "└": "ru",
                "┘": "lu",
                "├": "urd",
                "┤": "uld",
                "┬": "lrd",
                "┴": "lru",
                "┼": "lrud",
                "╭": "rd",
                "╮": "ld",
                "╰": "ru",
                "╯": "lu",
            }.get(cell.data)
            if legs:
                center = (x + cell_width // 2, y + cell_height // 2)
                edges = {
                    "l": (x, center[1]),
                    "r": (x + cell_width, center[1]),
                    "u": (center[0], y),
                    "d": (center[0], y + cell_height),
                }
                if cell.data in "╭╮╰╯":
                    start, end = (edges[leg] for leg in legs)
                    points = [
                        (
                            (1 - t) ** 2 * start[0]
                            + 2 * (1 - t) * t * center[0]
                            + t * t * end[0],
                            (1 - t) ** 2 * start[1]
                            + 2 * (1 - t) * t * center[1]
                            + t * t * end[1],
                        )
                        for t in (i / 12 for i in range(13))
                    ]
                    draw.line(points, fill=fg, width=max(1, size // 16))
                else:
                    for leg in legs:
                        draw.line(
                            (center, edges[leg]), fill=fg, width=max(1, size // 16)
                        )
            elif len(cell.data) == 1 and 0x2800 <= ord(cell.data) <= 0x28FF:
                # Braille is used by the native welcome art. Draw its actual dot
                # pattern because common macOS monospace fonts lack these glyphs.
                dots = ord(cell.data) - 0x2800
                for bit, (dx, dy) in enumerate(
                    ((0, 0), (0, 1), (0, 2), (1, 0), (1, 1), (1, 2), (0, 3), (1, 3))
                ):
                    if dots & (1 << bit):
                        cx, cy = (
                            x + (dx + 0.5) * cell_width / 2,
                            y + (dy + 0.5) * cell_height / 4,
                        )
                        radius = max(1, cell_width / 7)
                        draw.ellipse(
                            (cx - radius, cy - radius, cx + radius, cy + radius),
                            fill=fg,
                        )
            elif cell.data:
                draw.text(
                    (x, y), cell.data, font=bold_face if cell.bold else face, fill=fg
                )
            if cell.underscore:
                draw.line(
                    (x, y + ascent + 3, x + cell_width - 1, y + ascent + 3), fill=fg
                )
            if cell.strikethrough:
                draw.line(
                    (x, y + ascent // 2, x + cell_width - 1, y + ascent // 2),
                    fill=fg,
                )
    image.save(output)


class Terminal:
    def __init__(self, root, binary, columns, rows, fixture=None):
        self.socket = str(root / "tmux.sock")
        self.config = root / "tmux.conf"
        self.config.write_text(
            'set -g default-terminal "xterm-256color"\nset -g escape-time 0\nset -g status off\n'
        )
        self.workspace = root / "release-planner"
        self.workspace.mkdir()
        if fixture:
            (self.workspace / "README.md").write_text(
                "# Release checklist\n\nA small workspace for planning a team release.\n\n"
                "1. Choose a release owner.\n2. Review the changes.\n3. Run the checks.\n4. Publish the release.\n5. Write a short release note.\n"
            )
        self.env = {
            "PATH": os.environ.get("PATH", "/usr/bin:/bin"),
            "HOME": str(root),
            "XDG_CONFIG_HOME": str(root / "config"),
            "XDG_DATA_HOME": str(root / "data"),
            "TERM": "xterm-256color",
            "LANG": "en_US.UTF-8",
            "COLORTERM": "truecolor",
            "MAESTRO_AUTO_UPDATE": "0",
            "OPENAI_API_KEY": "screenshot-fixture",
        }
        if fixture:
            self.env.update(fixture.environment())
            self.env["MAESTRO_TELEMETRY"] = "0"
        self.env["MAESTRO_HOME"] = str(root / "maestro-home")
        if fixture:
            preferences = root / "maestro-home" / "ui.json"
            preferences.parent.mkdir()
            preferences.write_text(
                json.dumps({"animations": False, "timestamps": False, "onboardingSeen": True})
            )
        self.model = fixture.model if fixture else "gpt-4.1-mini"
        self.root, self.binary, self.columns, self.rows = root, binary, columns, rows

    def run(self, *args, check=True):
        return subprocess.run(
            ["tmux", "-S", self.socket, "-f", str(self.config), *args],
            env=self.env,
            capture_output=True,
            text=True,
            timeout=10,
            check=check,
        ).stdout

    def __enter__(self):
        try:
            self.run(
                "new-session",
                "-d",
                "-s",
                "capture",
                "-x",
                str(self.columns),
                "-y",
                str(self.rows),
                "-c",
                str(self.workspace),
                shlex.join(
                    [str(self.binary), "--provider", "openai", "-m", self.model]
                ),
            )
            return self
        except BaseException:
            self.close()
            raise

    def close(self):
        self.run("kill-server", check=False)

    def __exit__(self, *_):
        self.close()

    def capture(self):
        # Preserve trailing blank cells: they carry the canvas background.
        return self.run("capture-pane", "-p", "-e", "-N", "-t", "capture:0.0")

    def cursor(self):
        x, y, visible = map(
            int,
            self.run(
                "display-message",
                "-p",
                "-t",
                "capture:0.0",
                "#{cursor_x} #{cursor_y} #{cursor_flag}",
            ).split(),
        )
        return (x, y) if visible else None

    def wait(self, pattern, timeout, absent=False):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            ansi = self.capture()
            screen = screen_from_ansi(ansi, self.columns, self.rows)
            found = re.search(pattern, "\n".join(screen.display)) is not None
            if found != absent:
                return ansi
            time.sleep(0.1)
        raise TimeoutError(f"TUI did not show {pattern!r} within {timeout:g}s")

    def settle(self, pattern, timeout, absent=False):
        """Observe a complete stable frame, including cursor, before saving it."""
        deadline = time.monotonic() + timeout
        previous = None
        stable = 0
        while time.monotonic() < deadline:
            ansi = self.capture()
            cursor = self.cursor()
            screen = screen_from_ansi(ansi, self.columns, self.rows)
            ready = (
                re.search(pattern, "\n".join(screen.display)) is not None
            ) != absent
            frame = (ansi, cursor)
            stable = stable + 1 if ready and frame == previous else 0
            if stable >= 3:
                return frame
            previous = frame
            time.sleep(0.1)
        raise TimeoutError("TUI did not reach a stable, ready frame before capture")


def capture(args):
    scenario_path = args.scenario.resolve()
    scenario = load_scenario(scenario_path)
    if scenario_path.parent == FIXTURES.resolve() and not getattr(
        args, "fixture", False
    ):
        raise ValueError(
            "built-in scenes require --fixture and a current native debug build; see docs/tui-screenshots.md"
        )
    binary = shutil.which(args.binary)
    if not binary:
        raise ValueError(f"native binary not found: {args.binary}")
    binary = Path(binary).resolve()
    if not shutil.which("tmux"):
        raise ValueError("tmux is required; install it before capturing")
    font = font_path(args.font)
    # Refuse reuse so failed captures cannot leave an old success manifest.
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=False)
    manifest = {
        "scenario": scenario["name"],
        "scenario_sha256": hashlib.sha256(scenario_path.read_bytes()).hexdigest(),
        "binary": str(binary),
        "columns": args.columns,
        "rows": args.rows,
        "font": str(font),
        "font_sha256": hashlib.sha256(font.read_bytes()).hexdigest(),
        "font_size": args.font_size,
        "terminal_palette": COLORS,
        "status": "failed",
    }
    try:
        with ExitStack() as stack:
            temp = stack.enter_context(
                tempfile.TemporaryDirectory(prefix="ms-", dir="/tmp")
            )
            snapshot = Path(temp) / "maestro-capture"
            # Shared Cargo targets and installed launchers can be replaced by
            # other builds. Hash and execute the same private copy.
            with binary.open("rb") as source, snapshot.open("wb") as target:
                shutil.copyfileobj(source, target)
            snapshot.chmod(0o700)
            with snapshot.open("rb") as source:
                manifest["binary_sha256"] = hashlib.file_digest(
                    source, "sha256"
                ).hexdigest()
            fixture = None
            if getattr(args, "fixture", False):
                from tui_capture_fixture import CaptureFixture

                fixture = stack.enter_context(CaptureFixture(scenario["name"]))
            manifest["backend"] = (
                "local-scripted-fixture" if fixture else "no-provider-fixture"
            )
            with Terminal(
                Path(temp), snapshot, args.columns, args.rows, fixture
            ) as terminal:
                version = subprocess.run(
                    [str(snapshot), "--version"],
                    env=terminal.env,
                    stdin=subprocess.DEVNULL,
                    capture_output=True,
                    text=True,
                    timeout=10,
                    check=True,
                )
                manifest["binary_version"] = version.stdout.strip()
                try:
                    for step in scenario["steps"]:
                        kind, value = next(iter(step.items()))
                        if kind in {"wait", "absent"}:
                            ansi = terminal.wait(
                                value, args.timeout, absent=kind == "absent"
                            )
                        elif kind == "text":
                            terminal.run(
                                "send-keys", "-t", "capture:0.0", "-l", "--", value
                            )
                        else:
                            terminal.run("send-keys", "-t", "capture:0.0", value)
                    final_kind, final_pattern = next(
                        iter(scenario["steps"][-1].items())
                    )
                    ansi, cursor = terminal.settle(
                        final_pattern, args.timeout, absent=final_kind == "absent"
                    )
                except BaseException:
                    try:
                        (output / "failure.ansi").write_text(terminal.capture())
                    except subprocess.SubprocessError:
                        pass  # Preserve the original error if the process already exited.
                    raise
                screen = screen_from_ansi(ansi, args.columns, args.rows)
                (output / "screen.ansi").write_text(ansi)
                (output / "screen.txt").write_text("\n".join(screen.display) + "\n")
                render_png(screen, output / "screen.png", font, args.font_size, cursor)
                manifest["cursor"] = cursor
                manifest["status"] = "passed"
                if fixture:
                    manifest["identity_requests"] = fixture.identity_requests
                    manifest["model_requests"] = fixture.turn
    except BaseException as error:
        manifest["error"] = str(error)
        raise
    finally:
        (output / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
    print(output / "screen.png")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--binary",
        default="maestro",
        help="native binary to capture (default: maestro on PATH)",
    )
    parser.add_argument(
        "--fixture",
        action="store_true",
        help="use local Identity/model fixtures; requires a debug build",
    )
    parser.add_argument("--scenario", type=Path, default=FIXTURES / "idle.json")
    parser.add_argument(
        "--output", type=Path, required=True, help="new artifact directory"
    )
    parser.add_argument("--columns", type=int, default=100)
    parser.add_argument("--rows", type=int, default=30)
    parser.add_argument("--font", help="monospace TTF/TTC font")
    parser.add_argument("--font-size", type=int, default=18)
    parser.add_argument(
        "--timeout", type=float, default=20, help="seconds per readiness check"
    )
    args = parser.parse_args()
    if not (
        40 <= args.columns <= 240
        and 10 <= args.rows <= 100
        and 8 <= args.font_size <= 48
    ):
        parser.error("columns must be 40–240, rows 10–100, font-size 8–48")
    if not math.isfinite(args.timeout) or not 0 < args.timeout <= 120:
        parser.error("timeout must be greater than zero and at most 120 seconds")
    try:
        capture(args)
    except (ValueError, OSError, subprocess.SubprocessError, TimeoutError) as error:
        parser.exit(1, f"capture failed: {error}\n")


if __name__ == "__main__":
    main()

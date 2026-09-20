"""Contract tests for the public ./dev ui contributor front door."""

import json
import importlib.machinery
import importlib.util
from pathlib import Path
import subprocess
import unittest
from unittest.mock import patch


MAESTRO = Path(__file__).resolve().parents[2]
MONO = MAESTRO.parents[1]
DEV = MAESTRO / "dev"
loader = importlib.machinery.SourceFileLoader("maestro_dev", str(DEV))
spec = importlib.util.spec_from_loader(loader.name, loader)
maestro_dev = importlib.util.module_from_spec(spec)
loader.exec_module(maestro_dev)


class DevUiTests(unittest.TestCase):
    def test_doctor_is_machine_readable_and_portable(self):
        result = subprocess.run(
            [str(DEV), "ui", "doctor", "--json"],
            cwd=MAESTRO,
            capture_output=True,
            text=True,
            timeout=10,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        report = json.loads(result.stdout)
        self.assertEqual(report["maestro_root"], str(MAESTRO))
        self.assertEqual(report["status"], "ready")
        self.assertIn("cargo", report["tools"])
        self.assertIn("python3", report["tools"])

    def test_help_exposes_the_complete_ui_loop(self):
        result = subprocess.run(
            [str(DEV), "ui", "--help"],
            cwd=MAESTRO,
            capture_output=True,
            text=True,
            timeout=10,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        for command in ("list", "new", "check", "inspect", "replay", "migrate", "review", "doctor"):
            self.assertIn(command, result.stdout)

    def test_bare_ui_opens_the_default_workbench(self):
        completed = subprocess.CompletedProcess([], 0)
        with patch.object(maestro_dev.subprocess, "run", return_value=completed) as run:
            self.assertEqual(maestro_dev.main(["ui"]), 0)
        command = run.call_args.args[0]
        self.assertIn(str(MAESTRO / "scripts/dev/ui-workbench.py"), command)
        self.assertEqual(command[-2:], ["--port", "8770"])

    def test_mono_root_uses_the_same_front_door(self):
        if MONO / "products/maestro" != MAESTRO:
            self.skipTest("standalone public layout")
        result = subprocess.run(
            [str(MONO / "dev"), "ui", "doctor", "--json"],
            cwd=MONO,
            capture_output=True,
            text=True,
            timeout=10,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(json.loads(result.stdout)["layout"], "mono")


if __name__ == "__main__":
    unittest.main()

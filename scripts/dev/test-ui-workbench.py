"""Rebuild failures must not replace the last successfully rendered artifact."""
import importlib.util
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('workbench', Path(__file__).with_name('ui-workbench.py'))
workbench = importlib.util.module_from_spec(spec)
spec.loader.exec_module(workbench)


class WorkbenchTests(unittest.TestCase):
    def test_failure_preserves_previous_capture_and_revision(self):
        with tempfile.TemporaryDirectory() as directory, patch.object(workbench, 'WORKSPACE', Path(directory)):
            preview = workbench.Preview(['cargo'], {})
            with patch.object(workbench.subprocess, 'run', return_value=subprocess.CompletedProcess([], 0, b'<html>valid</html>', b'')):
                preview.build()
            revision = preview.revision
            with patch.object(workbench.subprocess, 'run', return_value=subprocess.CompletedProcess([], 1, b'partial', b'compile failed')):
                preview.build()
            self.assertEqual(preview.html, b'<html>valid</html>')
            self.assertEqual(preview.revision, revision)
            self.assertIn('compile failed', preview.error)

    def test_watches_new_source_but_not_generated_artifacts(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root/'packages').mkdir()
            before = workbench.fingerprint(root)
            (root/'packages/new.rs').write_text('source')
            changed = workbench.fingerprint(root)
            self.assertNotEqual(before, changed)
            (root/'preview.html').write_text('generated')
            self.assertEqual(changed, workbench.fingerprint(root))


if __name__ == '__main__':
    unittest.main()

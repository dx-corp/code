"""Rebuild failures must not replace the last successfully rendered artifact."""
import importlib.util
import http.client
import json
from pathlib import Path
import subprocess
import tempfile
import threading
from types import SimpleNamespace
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('workbench', Path(__file__).with_name('ui-workbench.py'))
workbench = importlib.util.module_from_spec(spec)
spec.loader.exec_module(workbench)


class WorkbenchTests(unittest.TestCase):
    @staticmethod
    def menu_recipe():
        return {
            'version': 1,
            'id': 'new-menu',
            'label': 'New menu',
            'title': 'Choose a workspace',
            'placeholder': 'Filter workspaces',
            'empty': 'No workspaces',
            'items': [
                {'id': 'application', 'label': 'Application'},
                {'id': 'docs', 'label': 'Docs 夜'},
            ],
            'state': 'ready',
            'status_message': '',
            'width': 60,
            'height': 20,
            'inputs': [],
        }

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

    def test_replay_validation_bounds_events_text_and_dimensions(self):
        request = {'version': 1, 'fixture': 'ready', 'width': 60, 'height': 20, 'inputs': [
            {'type': 'text', 'text': '夜'},
            {'type': 'key', 'key': 'down', 'ctrl': False},
            {'type': 'resize', 'width': 30, 'height': 12},
        ]}
        workbench.validate_replay(json.dumps(request).encode())
        for change in (
            lambda value: value.update(width=241),
            lambda value: value['inputs'].append({'type': 'text', 'text': '\n'}),
            lambda value: value.update(inputs=[{'type': 'retry'}] * 65),
            lambda value: value.update(inputs=[{'type': 'text', 'text': 'x' * 4097}]),
        ):
            invalid = json.loads(json.dumps(request))
            change(invalid)
            with self.assertRaises(ValueError):
                workbench.validate_replay(json.dumps(invalid).encode())

    def test_replay_invokes_only_fixed_command_with_structured_stdin(self):
        body = json.dumps({'version': 1, 'fixture': 'ready', 'width': 60, 'height': 20, 'inputs': []}).encode()
        preview = workbench.Preview(['renderer', '--html'], {}, replay_command=['fixed-renderer', '--replay-stdin'])
        result = subprocess.CompletedProcess([], 0, b'{"capture":{},"effects":[]}', b'')
        with patch.object(workbench.subprocess, 'run', return_value=result) as run:
            self.assertEqual(preview.replay(body), result.stdout)
        self.assertEqual(run.call_args.args[0], ['fixed-renderer', '--replay-stdin'])
        self.assertEqual(run.call_args.kwargs['input'], body)
        self.assertNotIn('shell', run.call_args.kwargs)

    def test_studio_validates_typed_items_and_invokes_only_fixed_command(self):
        request = self.menu_recipe()
        body = json.dumps(request).encode()
        workbench.validate_studio(body)
        for mutate in (
            lambda value: value['items'].append({'id': 'docs', 'label': 'Duplicate'}),
            lambda value: value['items'][0].update(id='../command'),
            lambda value: value.update(path='/tmp/output'),
            lambda value: value.update(inputs=[{'type': 'retry'}] * 65),
            lambda value: value.update(state={}),
            lambda value: value.update(inputs=[{'type': {}}]),
        ):
            invalid = json.loads(json.dumps(request))
            mutate(invalid)
            with self.assertRaises(ValueError):
                workbench.validate_studio(json.dumps(invalid).encode())

        preview = workbench.Preview(
            ['renderer', '--html'],
            {},
            studio_command=['fixed-renderer', '--studio-stdin'],
        )
        result = subprocess.CompletedProcess([], 0, b'{"capture":{},"coverage":[]}', b'')
        with patch.object(workbench.subprocess, 'run', return_value=result) as run:
            self.assertEqual(preview.studio(body), result.stdout)
        self.assertEqual(run.call_args.args[0], ['fixed-renderer', '--studio-stdin'])
        self.assertEqual(run.call_args.kwargs['input'], body)
        self.assertNotIn('shell', run.call_args.kwargs)

    def test_invalid_studio_edit_cannot_replay_the_previous_valid_request(self):
        valid = json.dumps(self.menu_recipe()).encode()
        invalid_recipe = self.menu_recipe()
        invalid_recipe['items'] = [{'id': 'unfinished-id'}]
        invalid = json.dumps(invalid_recipe).encode()
        preview = workbench.Preview(
            ['renderer', '--html'],
            {},
            studio_command=['fixed-renderer', '--studio-stdin'],
        )
        result = subprocess.CompletedProcess([], 0, b'{"capture":{},"coverage":[]}', b'')
        with patch.object(workbench.subprocess, 'run', return_value=result) as run:
            preview.studio(valid)
            with self.assertRaises(ValueError):
                preview.studio(invalid)
        run.assert_called_once()
        self.assertEqual(run.call_args.kwargs['input'], valid)

    def test_http_replay_rejects_foreign_origin_and_accepts_same_origin(self):
        seen = []
        preview = SimpleNamespace(
            html=b'<html></html>', sequences=b'{"fixtures":[],"presets":[]}', revision='r', error='',
            replay=lambda body: seen.append(body) or b'{"capture":{},"effects":[]}',
            studio=lambda body: seen.append(body) or b'{"capture":{},"coverage":[]}',
        )
        server = workbench.ThreadingHTTPServer(('127.0.0.1', 0), workbench.handler(preview))
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        port = server.server_port
        body = json.dumps({'version': 1, 'fixture': 'ready', 'width': 60, 'height': 20, 'inputs': []})
        try:
            connection = http.client.HTTPConnection('127.0.0.1', port)
            connection.request('POST', '/__replay', body, {
                'Content-Type': 'application/json', 'Origin': 'https://attacker.example',
            })
            self.assertEqual(connection.getresponse().status, 403)
            self.assertEqual(seen, [])

            connection = http.client.HTTPConnection('127.0.0.1', port)
            connection.request('POST', '/__replay', body, {
                'Content-Type': 'application/json', 'Origin': f'http://127.0.0.1:{port}',
            })
            response = connection.getresponse()
            self.assertEqual(response.status, 200)
            self.assertEqual(json.loads(response.read()), {'capture': {}, 'effects': []})
            self.assertEqual(seen, [body.encode()])

            studio = json.dumps(self.menu_recipe())
            connection = http.client.HTTPConnection('127.0.0.1', port)
            connection.request('POST', '/__studio', studio, {
                'Content-Type': 'application/json', 'Origin': f'http://localhost:{port}',
            })
            response = connection.getresponse()
            self.assertEqual(response.status, 200)
            self.assertEqual(json.loads(response.read()), {'capture': {}, 'coverage': []})
            self.assertEqual(seen[-1], studio.encode())
        finally:
            server.shutdown()
            server.server_close()

    def test_host_and_origin_parsing_fail_closed(self):
        self.assertTrue(workbench.valid_host('localhost:8770', 8770))
        self.assertFalse(workbench.valid_host('example.com:8770', 8770))
        self.assertTrue(workbench.valid_origin('http://127.0.0.1:8770', 8770))
        self.assertFalse(workbench.valid_origin('http://127.0.0.1:bad', 8770))


if __name__ == '__main__':
    unittest.main()

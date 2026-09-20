#!/usr/bin/env python3
"""Build and watch native UI fixtures. Localhost only; no browser-triggered commands."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

WORKSPACE = Path(__file__).resolve().parents[2]


def fingerprint(workspace):
    """Watch source inputs, never generated captures or Cargo output."""
    paths = [workspace / 'Cargo.toml', workspace / 'Cargo.lock']
    for root in (workspace / 'packages', workspace / 'scripts' / 'dev'):
        paths.extend(p for p in root.rglob('*') if p.suffix in {'.rs', '.html', '.toml', '.txt', '.py'})
    result = []
    for path in sorted(paths):
        try:
            if path.is_file():
                stat = path.stat()
                result.append((str(path), stat.st_mtime_ns, stat.st_size))
        except FileNotFoundError:
            # Editors can atomically replace a file between discovery and stat.
            continue
    return tuple(result)


class Preview:
    def __init__(self, command, env, build_command=None, replay_command=None, studio_command=None, sequences_command=None):
        self.command, self.env = command, env
        self.build_command = build_command
        self.replay_command = replay_command
        self.studio_command = studio_command
        self.sequences_command = sequences_command
        self.html = b''
        self.sequences = b''
        self.revision = ''
        self.error = ''
        self.lock = threading.Lock()

    def build(self):
        try:
            with self.lock:
                mono = WORKSPACE.parent.parent
                if (mono / 'scripts/dev/local_build_capacity.py').exists():
                    subprocess.run(['make', 'local-build-capacity-check'], cwd=mono, env=self.env, check=True, timeout=60)
                if self.build_command:
                    built = subprocess.run(self.build_command, cwd=WORKSPACE, env=self.env, capture_output=True, timeout=600)
                    if built.returncode:
                        raise RuntimeError(built.stderr.decode(errors='replace')[-4000:])
                result = subprocess.run(self.command, cwd=WORKSPACE, env=self.env, capture_output=True, timeout=60)
                if result.returncode:
                    raise RuntimeError(result.stderr.decode(errors='replace')[-4000:])
                if b'<html' not in result.stdout[:200].lower():
                    raise RuntimeError('Renderer did not return an HTML document')
                sequences = b''
                if self.sequences_command:
                    listed = subprocess.run(self.sequences_command, cwd=WORKSPACE, env=self.env, capture_output=True, timeout=5)
                    if listed.returncode:
                        raise RuntimeError(listed.stderr.decode(errors='replace')[-4000:])
                    json.loads(listed.stdout)
                    sequences = listed.stdout
                self.html = result.stdout
                self.sequences = sequences
                self.revision = hashlib.sha256(self.html + self.sequences).hexdigest()
                self.error = ''
                print('Preview updated', flush=True)
        except (OSError, RuntimeError, ValueError, subprocess.SubprocessError) as exc:
            self.error = str(exc)
            print('Preview rebuild failed: ' + self.error, flush=True)

    def replay(self, body):
        if not self.replay_command:
            raise LookupError('Interactive replay is unavailable for this preview')
        validate_replay(body)
        if not self.lock.acquire(blocking=False):
            raise BlockingIOError('Renderer is busy; retry this input')
        try:
            result = subprocess.run(
                self.replay_command,
                cwd=WORKSPACE,
                env=self.env,
                input=body,
                capture_output=True,
                timeout=5,
            )
            if result.returncode:
                raise ValueError(result.stderr.decode(errors='replace')[-2000:])
            json.loads(result.stdout)
            return result.stdout
        finally:
            self.lock.release()

    def studio(self, body):
        if not self.studio_command:
            raise LookupError('Scene authoring is unavailable for this preview')
        validate_studio(body)
        if not self.lock.acquire(blocking=False):
            raise BlockingIOError('Renderer is busy; retry this edit')
        try:
            result = subprocess.run(
                self.studio_command,
                cwd=WORKSPACE,
                env=self.env,
                input=body,
                capture_output=True,
                timeout=5,
            )
            if result.returncode:
                raise ValueError(result.stderr.decode(errors='replace')[-2000:])
            json.loads(result.stdout)
            return result.stdout
        finally:
            self.lock.release()


def validate_replay(body):
    if len(body) > 16_384:
        raise ValueError('Replay request exceeds 16384 bytes')
    request = json.loads(body)
    if not isinstance(request, dict) or not isinstance(request.get('inputs'), list):
        raise ValueError('Replay request must contain an input list')
    if len(request['inputs']) > 64:
        raise ValueError('Replay request exceeds 64 inputs')
    width, height = request.get('width'), request.get('height')
    if type(width) is not int or type(height) is not int or not 8 <= width <= 240 or not 3 <= height <= 100:
        raise ValueError('Replay dimensions are out of bounds')
    text_bytes = 0
    for event in request['inputs']:
        event_type = event.get('type') if isinstance(event, dict) else None
        if not isinstance(event_type, str) or event_type not in {'key', 'text', 'resize', 'retry'}:
            raise ValueError('Replay input is invalid')
        if event_type == 'text':
            text = event.get('text')
            if not isinstance(text, str) or any(ord(char) < 32 or ord(char) == 127 for char in text):
                raise ValueError('Replay text is invalid')
            text_bytes += len(text.encode())
        if event_type == 'resize':
            event_width, event_height = event.get('width'), event.get('height')
            if type(event_width) is not int or type(event_height) is not int or not 8 <= event_width <= 240 or not 3 <= event_height <= 100:
                raise ValueError('Replay resize is out of bounds')
    if text_bytes > 4_096:
        raise ValueError('Replay text exceeds 4096 bytes')


def validate_studio(body):
    if len(body) > 16_384:
        raise ValueError('Menu recipe exceeds 16384 bytes')
    request = json.loads(body)
    required = {
        'version', 'id', 'label', 'title', 'placeholder', 'empty', 'items',
        'state', 'status_message', 'width', 'height', 'inputs',
    }
    if not isinstance(request, dict) or set(request) != required:
        raise ValueError('Menu recipe fields are invalid')
    state = request['state']
    if request['version'] != 1 or not isinstance(state, str) or state not in {'ready', 'empty', 'loading', 'error'}:
        raise ValueError('Menu recipe version or state is invalid')
    if not isinstance(request['id'], str) or not re.fullmatch(r'[a-z0-9-]{1,64}', request['id']):
        raise ValueError('Menu recipe ID is invalid')
    text_bytes = 0
    for field, maximum, allow_empty in (
        ('label', 160, False), ('title', 160, False),
        ('placeholder', 160, False), ('empty', 240, False),
        ('status_message', 240, True),
    ):
        value = request[field]
        if not isinstance(value, str) or (not allow_empty and not value) or len(value.encode()) > maximum:
            raise ValueError(f'Menu recipe {field} is invalid')
        if any(ord(char) < 32 or ord(char) == 127 for char in value):
            raise ValueError(f'Menu recipe {field} contains control characters')
        text_bytes += len(value.encode())
    items = request['items']
    if not isinstance(items, list) or len(items) > 64:
        raise ValueError('Menu recipe items are invalid')
    ids = set()
    for item in items:
        if not isinstance(item, dict) or set(item) != {'id', 'label'}:
            raise ValueError('Menu recipe item fields are invalid')
        item_id, label = item['id'], item['label']
        if not isinstance(item_id, str) or not re.fullmatch(r'[a-z0-9-]{1,64}', item_id) or item_id in ids:
            raise ValueError('Menu recipe item ID is invalid or duplicated')
        if not isinstance(label, str) or not label or len(label.encode()) > 160:
            raise ValueError('Menu recipe item label is invalid')
        if any(ord(char) < 32 or ord(char) == 127 for char in label):
            raise ValueError('Menu recipe item label contains control characters')
        ids.add(item_id)
        text_bytes += len(item_id.encode()) + len(label.encode())
    if text_bytes > 8_192:
        raise ValueError('Menu recipe text exceeds 8192 bytes')
    validate_replay(json.dumps({
        'width': request['width'],
        'height': request['height'],
        'inputs': request['inputs'],
    }).encode())


def valid_host(value, port):
    return value in {f'127.0.0.1:{port}', f'localhost:{port}'}


def valid_origin(value, port):
    return value in {f'http://127.0.0.1:{port}', f'http://localhost:{port}'}


def handler(preview):
    class Handler(BaseHTTPRequestHandler):
        def trusted_host(self):
            if valid_host(self.headers.get('Host', ''), self.server.server_port):
                return True
            self.send_error(403, 'Untrusted Host')
            return False

        def do_GET(self):
            if not self.trusted_host():
                return
            path = self.path.split('?', 1)[0]
            if path == '/__revision':
                body = json.dumps({'revision': preview.revision, 'error': preview.error}).encode()
                content_type, status = 'application/json', 200
            elif path == '/__sequences' and preview.sequences:
                body = preview.sequences
                content_type, status = 'application/json', 200
            elif path in {'/', '/index.html'}:
                body = preview.html or b'<html><body>Preview has not built. See the terminal for the build error.</body></html>'
                content_type, status = 'text/html; charset=utf-8', 200 if preview.html else 503
            else:
                body, content_type, status = b'Not found', 'text/plain', 404
            self.send_response(status)
            self.send_header('Content-Type', content_type)
            self.send_header('Content-Length', str(len(body)))
            self.send_header('Cache-Control', 'no-store')
            self.end_headers()
            self.wfile.write(body)

        def do_POST(self):
            if not self.trusted_host():
                return
            path = self.path.split('?', 1)[0]
            if path not in {'/__replay', '/__studio'}:
                self.send_error(404, 'Not found')
                return
            if not valid_origin(self.headers.get('Origin', ''), self.server.server_port):
                self.send_error(403, 'Untrusted Origin')
                return
            if self.headers.get_content_type() != 'application/json':
                self.send_error(415, 'Expected application/json')
                return
            try:
                length = int(self.headers.get('Content-Length', ''))
                if not 0 < length <= 16_384:
                    raise ValueError('Invalid Content-Length')
                body = self.rfile.read(length)
                rendered = preview.replay(body) if path == '/__replay' else preview.studio(body)
                status = 200
            except BlockingIOError as exc:
                rendered, status = json.dumps({'error': str(exc)}).encode(), 429
            except (LookupError, ValueError, json.JSONDecodeError) as exc:
                rendered, status = json.dumps({'error': str(exc)}).encode(), 400
            except (OSError, subprocess.SubprocessError) as exc:
                rendered, status = json.dumps({'error': str(exc)}).encode(), 500
            self.send_response(status)
            self.send_header('Content-Type', 'application/json')
            self.send_header('Content-Length', str(len(rendered)))
            self.send_header('Cache-Control', 'no-store')
            self.send_header('X-Content-Type-Options', 'nosniff')
            self.end_headers()
            self.wfile.write(rendered)

        def log_message(self, *_):
            pass
    return Handler


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--port', type=int, default=8770)
    parser.add_argument('--components-only', action='store_true')
    args = parser.parse_args()
    env = dict(os.environ, COLORTERM='truecolor')
    env.setdefault('CARGO_TARGET_DIR', str(Path.home() / '.cache' / 'maestro-ui-target'))
    target = Path(env['CARGO_TARGET_DIR']) / 'debug'
    executable_suffix = '.exe' if os.name == 'nt' else ''
    if args.components_only:
        binary = target / f'maestro-ui-preview{executable_suffix}'
        build_command = ['cargo', 'build', '--locked', '-p', 'maestro-ui-preview']
        preview = Preview([str(binary), '--html'], env, build_command=build_command)
    else:
        binary = target / 'examples' / f'onboarding-preview{executable_suffix}'
        build_command = ['cargo', 'build', '--locked', '-p', 'maestro-tui', '--example', 'onboarding-preview']
        preview = Preview(
            [str(binary), '--html'],
            env,
            build_command=build_command,
            replay_command=[str(binary), '--replay-stdin'],
            studio_command=[str(binary), '--studio-stdin'],
            sequences_command=[str(binary), '--sequences'],
        )
    # Bind before building, so a busy port fails without starting an unused build.
    server = ThreadingHTTPServer(('127.0.0.1', args.port), handler(preview))
    previous = fingerprint(WORKSPACE)
    preview.build()
    stop = threading.Event()

    def watch():
        nonlocal previous
        while not stop.wait(1):
            current = fingerprint(WORKSPACE)
            if current != previous:
                previous = current
                preview.build()

    thread = threading.Thread(target=watch, daemon=True)
    thread.start()
    print(f'UI workbench: http://127.0.0.1:{args.port}/?watch=1&interactive=1', flush=True)
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass
    finally:
        stop.set()
        server.server_close()


if __name__ == '__main__':
    main()

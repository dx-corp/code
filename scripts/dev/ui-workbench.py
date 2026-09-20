#!/usr/bin/env python3
"""Build and watch native UI fixtures. Localhost only; no browser-triggered commands."""
import argparse
import hashlib
import json
import os
from pathlib import Path
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
    def __init__(self, command, env):
        self.command, self.env = command, env
        self.html = b''
        self.revision = ''
        self.error = ''

    def build(self):
        try:
            mono = WORKSPACE.parent.parent
            if (mono / 'scripts/dev/local_build_capacity.py').exists():
                subprocess.run(['make', 'local-build-capacity-check'], cwd=mono, env=self.env, check=True, timeout=60)
            result = subprocess.run(self.command, cwd=WORKSPACE, env=self.env, capture_output=True, timeout=600)
            if result.returncode:
                raise RuntimeError(result.stderr.decode(errors='replace')[-4000:])
            if b'<html' not in result.stdout[:200].lower():
                raise RuntimeError('Renderer did not return an HTML document')
            self.html = result.stdout
            self.revision = hashlib.sha256(self.html).hexdigest()
            self.error = ''
            print('Preview updated', flush=True)
        except (OSError, RuntimeError, subprocess.SubprocessError) as exc:
            self.error = str(exc)
            print('Preview rebuild failed: ' + self.error, flush=True)


def handler(preview):
    class Handler(BaseHTTPRequestHandler):
        def do_GET(self):
            path = self.path.split('?', 1)[0]
            if path == '/__revision':
                body = json.dumps({'revision': preview.revision, 'error': preview.error}).encode()
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
    command = ['cargo', 'run', '--locked', '-p']
    command += ['maestro-ui-preview'] if args.components_only else ['maestro-tui', '--example', 'onboarding-preview']
    command += ['--', '--html']
    preview = Preview(command, env)
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
    print(f'UI workbench: http://127.0.0.1:{args.port}/?watch=1', flush=True)
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass
    finally:
        stop.set()
        server.server_close()


if __name__ == '__main__':
    main()

#!/usr/bin/env python3
"""Build and watch native UI fixtures. Localhost only; no browser-triggered commands."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import tempfile
import threading
import time
from dataclasses import dataclass
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

WORKSPACE = Path(__file__).resolve().parents[2]


def repository_provenance(workspace):
    """Return bounded checkout identity without exposing the local absolute path."""
    try:
        def git_output(*args):
            return (
                subprocess.run(
                    ["git", "-C", str(workspace), *args],
                    stdout=subprocess.PIPE,
                    stderr=subprocess.DEVNULL,
                    timeout=2,
                    check=True,
                )
                .stdout.decode()
                .strip()
            )

        remote = git_output("remote", "get-url", "origin")
        branch = git_output("branch", "--show-current") or "detached"
        revision = git_output("rev-parse", "HEAD")
        if remote.startswith("git@") and ":" in remote:
            repository = remote.split(":", 1)[1]
        elif "://" in remote:
            location = remote.split("://", 1)[1]
            repository = location.split("/", 1)[1] if "/" in location else location
        else:
            repository = Path(remote).name
        repository = repository.removesuffix(".git").strip("/")
        return {
            "repository": (repository or "workspace")[:128],
            "branch": branch[:256],
            "revision": revision[:64],
        }
    except (OSError, subprocess.SubprocessError, UnicodeDecodeError, ValueError):
        return {"repository": "workspace", "branch": "unknown", "revision": ""}


def source_paths(workspace, filter_kind="", filter_value="", manifests=None):
    paths = [
        workspace / "Cargo.toml",
        workspace / "Cargo.lock",
        workspace / ".cargo" / "config.toml",
    ]
    roots = None
    if filter_kind == "adapter" and manifests and filter_value in manifests:
        manifest = manifests[filter_value]
        roots = [
            workspace / "packages" / "ui-preview-rs",
            workspace / "packages" / "interaction-rs",
            workspace / "packages" / "ui-rs",
            workspace / "packages" / "presentation-rs",
            workspace / manifest["fixture_dir"],
        ]
        if manifest.get("template") == "theme-selector":
            roots.append(workspace / "packages" / "tui-rs")
        paths.append(workspace / manifest["registration_file"])
    if roots is None:
        roots = (workspace / "packages", workspace / "scripts" / "dev")
    for root in roots:
        if root.is_file():
            paths.append(root)
            continue
        paths.extend(
            p
            for p in root.rglob("*")
            if p.suffix in {".rs", ".html", ".js", ".toml", ".txt", ".py"}
        )
    return sorted(set(paths))


def fingerprint(workspace, filter_kind="", filter_value="", manifests=None):
    """Watch source inputs, never generated captures or Cargo output."""
    result = []
    for path in source_paths(workspace, filter_kind, filter_value, manifests):
        try:
            if path.is_file():
                stat = path.stat()
                result.append((str(path), stat.st_mtime_ns, stat.st_size))
        except FileNotFoundError:
            # Editors can atomically replace a file between discovery and stat.
            continue
    return tuple(result)


def source_content_digest(workspace, filter_kind="", filter_value="", manifests=None):
    digest = hashlib.sha256()
    total = 0
    mono = workspace.parents[1] if workspace.parent.name == "products" else workspace
    for index, path in enumerate(
        source_paths(workspace, filter_kind, filter_value, manifests)
    ):
        if index >= 10_000:
            raise RuntimeError("Source fingerprint exceeds 10000 files")
        try:
            content = path.read_bytes()
        except FileNotFoundError:
            continue
        total += len(content)
        if total > 128 * 1024 * 1024:
            raise RuntimeError("Source fingerprint exceeds 128 MiB")
        try:
            identity = path.relative_to(mono).as_posix()
        except ValueError:
            identity = path.relative_to(workspace).as_posix()
        digest.update(identity.encode())
        digest.update(b"\0")
        digest.update(content)
        digest.update(b"\0")
    return digest.hexdigest()


def rebuild_changed_sources(preview, previous, workspace, filter_kind="", filter_value=""):
    current = fingerprint(
        workspace, filter_kind, filter_value, preview.manifests
    )
    if current != previous:
        preview.build()
    return current


@dataclass(frozen=True)
class Generation:
    id: str
    renderer_digest: str
    source_digest: str
    filter_kind: str
    filter_value: str
    html: bytes
    sequences: bytes
    built_at_monotonic: float
    build_ms: int
    replay_command: tuple = ()
    studio_command: tuple = ()


class Preview:
    MAX_ERROR_BYTES = 4_000

    def __init__(
        self,
        command,
        env,
        build_command=None,
        replay_command=None,
        studio_command=None,
        sequences_command=None,
        manifest_command=None,
        filter_kind="",
        filter_value="",
        source_provenance=None,
    ):
        self.command, self.env = command, env
        self.build_command = build_command
        self.replay_command = replay_command
        self.studio_command = studio_command
        self.sequences_command = sequences_command
        self.manifest_command = manifest_command
        self.filter_kind, self.filter_value = filter_kind, filter_value
        if callable(source_provenance):
            self._source_provenance_provider = source_provenance
        else:
            fixed_provenance = dict(source_provenance or {})
            self._source_provenance_provider = lambda: dict(fixed_provenance)
        self._source_provenance = {}
        self.manifests = {}
        self.generation = None
        self.error = ""
        self.lock = threading.Lock()
        self.build_lock = threading.Lock()
        self._next_request = 0
        self._building_request = None
        self._last_failed_request = None
        self._in_use_digests = {}
        snapshot_parent = (
            Path(
                self.env.get(
                    "CARGO_TARGET_DIR",
                    Path(tempfile.gettempdir()) / "maestro-ui-workbench",
                )
            )
            / "ui-workbench-generations"
        )
        snapshot_parent.mkdir(parents=True, exist_ok=True)
        self.snapshot_root = Path(
            tempfile.mkdtemp(prefix=f"{os.getpid()}-", dir=snapshot_parent)
        )

    @property
    def html(self):
        return self.generation.html if self.generation else b""

    @property
    def sequences(self):
        return self.generation.sequences if self.generation else b""

    @property
    def revision(self):
        return self.generation.id if self.generation else ""

    def begin_build(self, request_id=None):
        with self.lock:
            self._next_request += 1
            request_id = request_id or f"g{self._next_request}"
            self._building_request = request_id
            return request_id

    def install_generation(self, generation):
        source_provenance = self._source_provenance_provider()
        with self.lock:
            self.generation = generation
            self._source_provenance = source_provenance
            self.error = ""

    def finish_build(self, generation, request_id=None):
        request_id = request_id or generation.id
        with self.lock:
            if request_id != self._building_request:
                return False
        source_provenance = self._source_provenance_provider()
        with self.lock:
            if request_id != self._building_request:
                return False
            self.generation = generation
            self._source_provenance = source_provenance
            self._building_request = None
            self.error = ""
            return True

    def fail_build(self, request_id, error):
        with self.lock:
            if request_id != self._building_request:
                return False
            self._building_request = None
            self._last_failed_request = request_id
            encoded = str(error).encode(errors="replace")[-self.MAX_ERROR_BYTES :]
            self.error = encoded.decode(errors="replace")
            return True

    def build(self):
        request_id = self.begin_build()
        with self.build_lock:
            started = time.monotonic()
            try:
                source_before = source_content_digest(
                    WORKSPACE, self.filter_kind, self.filter_value, self.manifests
                )
                mono = WORKSPACE.parent.parent
                if (mono / "scripts/dev/local_build_capacity.py").exists():
                    subprocess.run(
                        ["make", "local-build-capacity-check"],
                        cwd=mono,
                        env=self.env,
                        check=True,
                        timeout=60,
                    )
                if self.build_command:
                    built = subprocess.run(
                        self.build_command,
                        cwd=WORKSPACE,
                        env=self.env,
                        capture_output=True,
                        timeout=600,
                    )
                    if built.returncode:
                        raise RuntimeError(
                            built.stderr.decode(errors="replace")[
                                -self.MAX_ERROR_BYTES :
                            ]
                        )
                result = subprocess.run(
                    self.command,
                    cwd=WORKSPACE,
                    env=self.env,
                    capture_output=True,
                    timeout=60,
                )
                if result.returncode:
                    raise RuntimeError(
                        result.stderr.decode(errors="replace")[-self.MAX_ERROR_BYTES :]
                    )
                if b"<html" not in result.stdout[:200].lower():
                    raise RuntimeError("Renderer did not return an HTML document")
                sequences = b""
                if self.sequences_command:
                    listed = subprocess.run(
                        self.sequences_command,
                        cwd=WORKSPACE,
                        env=self.env,
                        capture_output=True,
                        timeout=5,
                    )
                    if listed.returncode:
                        raise RuntimeError(
                            listed.stderr.decode(errors="replace")[
                                -self.MAX_ERROR_BYTES :
                            ]
                        )
                    json.loads(listed.stdout)
                    sequences = listed.stdout
                next_manifests = None
                if self.manifest_command:
                    listed = subprocess.run(
                        self.manifest_command,
                        cwd=WORKSPACE,
                        env=self.env,
                        capture_output=True,
                        timeout=5,
                    )
                    if listed.returncode:
                        raise RuntimeError(
                            listed.stderr.decode(errors="replace")[
                                -self.MAX_ERROR_BYTES :
                            ]
                        )
                    manifests = json.loads(listed.stdout)
                    next_manifests = {
                        manifest["id"]: manifest for manifest in manifests
                    }
                renderer_digest = (
                    hashlib.sha256(Path(self.command[0]).read_bytes()).hexdigest()
                    if Path(self.command[0]).is_file()
                    else hashlib.sha256(result.stdout).hexdigest()
                )
                generation_commands = self._snapshot_commands(renderer_digest)
                source_digest = source_content_digest(
                    WORKSPACE, self.filter_kind, self.filter_value, self.manifests
                )
                if source_digest != source_before:
                    raise RuntimeError(
                        "Source inputs changed during build; retrying on the next watch cycle"
                    )
                generation_id = hashlib.sha256(
                    result.stdout
                    + sequences
                    + renderer_digest.encode()
                    + source_digest.encode()
                ).hexdigest()
                generation = Generation(
                    id=generation_id,
                    renderer_digest=renderer_digest,
                    source_digest=source_digest,
                    filter_kind=self.filter_kind,
                    filter_value=self.filter_value,
                    html=result.stdout,
                    sequences=sequences,
                    built_at_monotonic=time.monotonic(),
                    build_ms=int((time.monotonic() - started) * 1000),
                    replay_command=generation_commands.get("replay", ()),
                    studio_command=generation_commands.get("studio", ()),
                )
                if self.finish_build(generation, request_id):
                    if next_manifests is not None:
                        self.manifests = next_manifests
                    self._cleanup_snapshots(generation.renderer_digest)
                    print("Preview updated", flush=True)
            except (
                OSError,
                RuntimeError,
                ValueError,
                subprocess.SubprocessError,
            ) as exc:
                if self.fail_build(request_id, exc):
                    print("Preview rebuild failed: " + self.error, flush=True)

    def _snapshot_commands(self, renderer_digest):
        commands = {"replay": self.replay_command, "studio": self.studio_command}
        executable = Path(self.command[0])
        if not executable.is_file():
            return {name: tuple(command or ()) for name, command in commands.items()}
        generation_dir = self.snapshot_root / renderer_digest
        generation_dir.mkdir(parents=True, exist_ok=True)
        snapshot = generation_dir / executable.name
        if not snapshot.exists():
            temporary = snapshot.with_suffix(snapshot.suffix + ".new")
            shutil.copy2(executable, temporary)
            os.replace(temporary, snapshot)
        output = {}
        for name, command in commands.items():
            output[name] = (str(snapshot), *command[1:]) if command else ()
        return output

    def _cleanup_snapshots(self, active_digest):
        root = self.snapshot_root
        if not root.is_dir():
            return
        with self.lock:
            protected = {active_digest, *self._in_use_digests}
        candidates = sorted(
            (
                path
                for path in root.iterdir()
                if path.is_dir() and path.name not in protected
            ),
            key=lambda path: path.stat().st_mtime_ns,
            reverse=True,
        )
        for path in candidates[3:]:
            shutil.rmtree(path, ignore_errors=True)

    def _invoke(self, command_kind, body, validator, unavailable):
        validator(body)
        with self.lock:
            generation = self.generation
            if generation is None:
                raise BlockingIOError("Renderer has no successful generation")
            command = (
                generation.replay_command
                if command_kind == "replay"
                else generation.studio_command
            )
            if not command:
                raise LookupError(unavailable)
            self._in_use_digests[generation.renderer_digest] = (
                self._in_use_digests.get(generation.renderer_digest, 0) + 1
            )
        try:
            result = subprocess.run(
                command,
                cwd=WORKSPACE,
                env=self.env,
                input=body,
                capture_output=True,
                timeout=5,
            )
        finally:
            with self.lock:
                remaining = self._in_use_digests[generation.renderer_digest] - 1
                if remaining:
                    self._in_use_digests[generation.renderer_digest] = remaining
                else:
                    del self._in_use_digests[generation.renderer_digest]
        if result.returncode:
            raise ValueError(result.stderr.decode(errors="replace")[-2000:])
        payload = json.loads(result.stdout)
        if isinstance(payload, dict):
            payload["generation"] = generation.id
        return json.dumps(payload, ensure_ascii=False, separators=(",", ":")).encode()

    def close(self):
        shutil.rmtree(self.snapshot_root, ignore_errors=True)

    def replay(self, body):
        return self._invoke(
            "replay",
            body,
            validate_replay,
            "Interactive replay is unavailable for this preview",
        )

    def studio(self, body):
        return self._invoke(
            "studio",
            body,
            validate_studio,
            "Scene authoring is unavailable for this preview",
        )

    def revision_state(self):
        with self.lock:
            generation = self.generation
            building = self._building_request is not None
            return {
                "generation": generation.id if generation else "",
                "renderer_digest": generation.renderer_digest if generation else "",
                "source_digest": generation.source_digest if generation else "",
                "source": self._source_provenance,
                "filter": {
                    "kind": generation.filter_kind,
                    "value": generation.filter_value,
                }
                if generation
                else None,
                "build_ms": generation.build_ms if generation else 0,
                "building": building,
                "stale": bool(generation and (building or self.error)),
                "error": self.error,
            }


def _wire_value(body, schema, label):
    if len(body) > 16_384:
        raise ValueError(f"{label} exceeds 16384 bytes")
    request = json.loads(body)
    if isinstance(request, dict) and "schema" in request:
        if set(request) != {"schema", "version", "value"}:
            raise ValueError(f"{label} envelope fields are invalid")
        if request["schema"] != schema or request["version"] != 1:
            raise ValueError(f"{label} envelope schema or version is unsupported")
        request = request["value"]
    return request


def validate_replay(body):
    request = _wire_value(body, "maestro.ui.theme-replay", "Replay request")
    if not isinstance(request, dict) or not isinstance(request.get("inputs"), list):
        raise ValueError("Replay request must contain an input list")
    if len(request["inputs"]) > 64:
        raise ValueError("Replay request exceeds 64 inputs")
    width, height = request.get("width"), request.get("height")
    if (
        type(width) is not int
        or type(height) is not int
        or not 8 <= width <= 240
        or not 3 <= height <= 100
    ):
        raise ValueError("Replay dimensions are out of bounds")
    text_bytes = 0
    for event in request["inputs"]:
        event_type = event.get("type") if isinstance(event, dict) else None
        if not isinstance(event_type, str) or event_type not in {
            "key",
            "text",
            "resize",
            "retry",
        }:
            raise ValueError("Replay input is invalid")
        if event_type == "text":
            text = event.get("text")
            if not isinstance(text, str) or any(
                ord(char) < 32 or ord(char) == 127 for char in text
            ):
                raise ValueError("Replay text is invalid")
            text_bytes += len(text.encode())
        if event_type == "resize":
            event_width, event_height = event.get("width"), event.get("height")
            if (
                type(event_width) is not int
                or type(event_height) is not int
                or not 8 <= event_width <= 240
                or not 3 <= event_height <= 100
            ):
                raise ValueError("Replay resize is out of bounds")
    if text_bytes > 4_096:
        raise ValueError("Replay text exceeds 4096 bytes")


def validate_studio(body):
    request = _wire_value(body, "maestro.ui.menu-recipe", "Menu recipe")
    required = {
        "version",
        "id",
        "label",
        "title",
        "placeholder",
        "empty",
        "items",
        "state",
        "status_message",
        "width",
        "height",
        "inputs",
    }
    if not isinstance(request, dict) or set(request) != required:
        raise ValueError("Menu recipe fields are invalid")
    state = request["state"]
    if (
        request["version"] != 1
        or not isinstance(state, str)
        or state not in {"ready", "empty", "loading", "error"}
    ):
        raise ValueError("Menu recipe version or state is invalid")
    if not isinstance(request["id"], str) or not re.fullmatch(
        r"[a-z0-9-]{1,64}", request["id"]
    ):
        raise ValueError("Menu recipe ID is invalid")
    text_bytes = 0
    for field, maximum, allow_empty in (
        ("label", 160, False),
        ("title", 160, False),
        ("placeholder", 160, False),
        ("empty", 240, False),
        ("status_message", 240, True),
    ):
        value = request[field]
        if (
            not isinstance(value, str)
            or (not allow_empty and not value)
            or len(value.encode()) > maximum
        ):
            raise ValueError(f"Menu recipe {field} is invalid")
        if any(ord(char) < 32 or ord(char) == 127 for char in value):
            raise ValueError(f"Menu recipe {field} contains control characters")
        text_bytes += len(value.encode())
    items = request["items"]
    if not isinstance(items, list) or len(items) > 64:
        raise ValueError("Menu recipe items are invalid")
    ids = set()
    for item in items:
        if not isinstance(item, dict) or set(item) != {"id", "label"}:
            raise ValueError("Menu recipe item fields are invalid")
        item_id, label = item["id"], item["label"]
        if (
            not isinstance(item_id, str)
            or not re.fullmatch(r"[a-z0-9-]{1,64}", item_id)
            or item_id in ids
        ):
            raise ValueError("Menu recipe item ID is invalid or duplicated")
        if not isinstance(label, str) or not label or len(label.encode()) > 160:
            raise ValueError("Menu recipe item label is invalid")
        if any(ord(char) < 32 or ord(char) == 127 for char in label):
            raise ValueError("Menu recipe item label contains control characters")
        ids.add(item_id)
        text_bytes += len(item_id.encode()) + len(label.encode())
    if text_bytes > 8_192:
        raise ValueError("Menu recipe text exceeds 8192 bytes")
    validate_replay(
        json.dumps(
            {
                "width": request["width"],
                "height": request["height"],
                "inputs": request["inputs"],
            }
        ).encode()
    )


def valid_host(value, port):
    return value in {f"127.0.0.1:{port}", f"localhost:{port}"}


def valid_origin(value, port):
    return value in {f"http://127.0.0.1:{port}", f"http://localhost:{port}"}


def handler(preview):
    class Handler(BaseHTTPRequestHandler):
        def trusted_host(self):
            if valid_host(self.headers.get("Host", ""), self.server.server_port):
                return True
            self.send_error(403, "Untrusted Host")
            return False

        def do_GET(self):
            if not self.trusted_host():
                return
            path = self.path.split("?", 1)[0]
            if path == "/__revision":
                body = json.dumps(preview.revision_state()).encode()
                content_type, status = "application/json", 200
            elif path == "/__sequences" and preview.sequences:
                body = preview.sequences
                content_type, status = "application/json", 200
            elif path in {"/", "/index.html"}:
                body = (
                    preview.html
                    or b"<html><body>Preview has not built. See the terminal for the build error.</body></html>"
                )
                content_type, status = (
                    "text/html; charset=utf-8",
                    200 if preview.html else 503,
                )
            else:
                body, content_type, status = b"Not found", "text/plain", 404
            self.send_response(status)
            self.send_header("Content-Type", content_type)
            self.send_header("Content-Length", str(len(body)))
            self.send_header("Cache-Control", "no-store")
            self.end_headers()
            self.wfile.write(body)

        def do_POST(self):
            if not self.trusted_host():
                return
            path = self.path.split("?", 1)[0]
            if path not in {"/__replay", "/__studio"}:
                self.send_error(404, "Not found")
                return
            if not valid_origin(
                self.headers.get("Origin", ""), self.server.server_port
            ):
                self.send_error(403, "Untrusted Origin")
                return
            if self.headers.get_content_type() != "application/json":
                self.send_error(415, "Expected application/json")
                return
            try:
                length = int(self.headers.get("Content-Length", ""))
                if not 0 < length <= 16_384:
                    raise ValueError("Invalid Content-Length")
                body = self.rfile.read(length)
                rendered = (
                    preview.replay(body)
                    if path == "/__replay"
                    else preview.studio(body)
                )
                status = 200
            except BlockingIOError as exc:
                rendered, status = json.dumps({"error": str(exc)}).encode(), 429
            except (LookupError, ValueError, json.JSONDecodeError) as exc:
                rendered, status = json.dumps({"error": str(exc)}).encode(), 400
            except (OSError, subprocess.SubprocessError) as exc:
                rendered, status = json.dumps({"error": str(exc)}).encode(), 500
            self.send_response(status)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(rendered)))
            self.send_header("Cache-Control", "no-store")
            self.send_header("X-Content-Type-Options", "nosniff")
            self.end_headers()
            self.wfile.write(rendered)

        def log_message(self, *_):
            pass

    return Handler


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--port", type=int, default=8770)
    parser.add_argument("--components-only", action="store_true")
    filter_group = parser.add_mutually_exclusive_group()
    filter_group.add_argument("--story")
    filter_group.add_argument("--adapter")
    args = parser.parse_args()
    env = dict(os.environ, COLORTERM="truecolor")
    env.setdefault(
        "CARGO_TARGET_DIR", str(Path.home() / ".cache" / "maestro-ui-target")
    )
    target = Path(env["CARGO_TARGET_DIR"]) / "debug"
    executable_suffix = ".exe" if os.name == "nt" else ""
    filter_kind = "story" if args.story else ("adapter" if args.adapter else "")
    filter_value = args.story or args.adapter or ""
    filter_args = [f"--{filter_kind}", filter_value] if filter_kind else []
    source_provenance = lambda: repository_provenance(WORKSPACE)
    components_only = args.components_only or bool(args.adapter)
    if components_only:
        binary = target / f"maestro-ui-preview{executable_suffix}"
        build_command = ["cargo", "build", "--locked", "-p", "maestro-ui-preview"]
        preview = Preview(
            [str(binary), "--html", *filter_args],
            env,
            build_command=build_command,
            sequences_command=[str(binary), "--sequences", *filter_args],
            manifest_command=[str(binary), "studio", "manifests"],
            filter_kind=filter_kind,
            filter_value=filter_value,
            source_provenance=source_provenance,
        )
    else:
        binary = target / "examples" / f"onboarding-preview{executable_suffix}"
        build_command = [
            "cargo",
            "build",
            "--locked",
            "-p",
            "maestro-tui",
            "--example",
            "onboarding-preview",
        ]
        preview = Preview(
            [str(binary), "--html", *filter_args],
            env,
            build_command=build_command,
            replay_command=[str(binary), "--replay-stdin"],
            studio_command=[str(binary), "--studio-stdin"],
            sequences_command=[str(binary), "--sequences"],
            filter_kind=filter_kind,
            filter_value=filter_value,
            source_provenance=source_provenance,
        )
    # Bind before building, so a busy port fails without starting an unused build.
    server = ThreadingHTTPServer(("127.0.0.1", args.port), handler(preview))
    previous = fingerprint(WORKSPACE, filter_kind, filter_value, preview.manifests)
    preview.build()
    if preview.generation:
        previous = fingerprint(WORKSPACE, filter_kind, filter_value, preview.manifests)
    stop = threading.Event()

    def watch():
        nonlocal previous
        while not stop.wait(1):
            previous = rebuild_changed_sources(
                preview, previous, WORKSPACE, filter_kind, filter_value
            )

    thread = threading.Thread(target=watch, daemon=True)
    thread.start()
    print(
        f"UI workbench: http://127.0.0.1:{args.port}/?watch=1&interactive=1", flush=True
    )
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass
    finally:
        stop.set()
        server.server_close()
        preview.close()


if __name__ == "__main__":
    main()

"""Rebuild failures must not replace the last successfully rendered artifact."""

import importlib.util
import http.client
import json
import os
from pathlib import Path
import subprocess
import tempfile
import threading
import time
from types import SimpleNamespace
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location(
    "workbench", Path(__file__).with_name("ui-workbench.py")
)
workbench = importlib.util.module_from_spec(spec)
spec.loader.exec_module(workbench)


class WorkbenchTests(unittest.TestCase):
    @staticmethod
    def generation(
        identifier="g1",
        replay=("fixed-renderer", "--replay-stdin"),
        studio=("fixed-renderer", "--studio-stdin"),
    ):
        return workbench.Generation(
            id=identifier,
            renderer_digest="renderer-" + identifier,
            source_digest="source-" + identifier,
            filter_kind="",
            filter_value="",
            html=b"<html>valid</html>",
            sequences=b"{}",
            built_at_monotonic=1.0,
            build_ms=12,
            replay_command=tuple(replay),
            studio_command=tuple(studio),
        )

    @staticmethod
    def menu_recipe():
        return {
            "version": 1,
            "id": "new-menu",
            "label": "New menu",
            "title": "Choose a workspace",
            "placeholder": "Filter workspaces",
            "empty": "No workspaces",
            "items": [
                {"id": "application", "label": "Application"},
                {"id": "docs", "label": "Docs 夜"},
            ],
            "state": "ready",
            "status_message": "",
            "width": 60,
            "height": 20,
            "inputs": [],
        }

    def test_failure_preserves_previous_capture_and_revision(self):
        with (
            tempfile.TemporaryDirectory() as directory,
            patch.object(workbench, "WORKSPACE", Path(directory)),
        ):
            preview = workbench.Preview(["cargo"], {})
            with patch.object(
                workbench.subprocess,
                "run",
                return_value=subprocess.CompletedProcess(
                    [], 0, b"<html>valid</html>", b""
                ),
            ):
                preview.build()
            revision = preview.revision
            with patch.object(
                workbench.subprocess,
                "run",
                return_value=subprocess.CompletedProcess(
                    [], 1, b"partial", b"compile failed"
                ),
            ):
                preview.build()
            self.assertEqual(preview.html, b"<html>valid</html>")
            self.assertEqual(preview.revision, revision)
            self.assertIn("compile failed", preview.error)

    def test_watches_new_source_but_not_generated_artifacts(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "packages").mkdir()
            before = workbench.fingerprint(root)
            (root / "packages/new.rs").write_text("source")
            changed = workbench.fingerprint(root)
            self.assertNotEqual(before, changed)
            (root / "preview.html").write_text("generated")
            self.assertEqual(changed, workbench.fingerprint(root))

    def test_bundled_web_module_edit_changes_the_renderer_fingerprint(self):
        with tempfile.TemporaryDirectory() as directory:
            workspace = Path(directory)
            module = workspace / "packages/ui-preview-rs/src/web/catalog.js"
            module.parent.mkdir(parents=True)
            module.write_text("export const catalog = 1;", encoding="utf-8")
            before = workbench.fingerprint(workspace)
            digest_before = workbench.source_content_digest(workspace)
            module.write_text("export const catalog = 2;", encoding="utf-8")
            preview = SimpleNamespace(manifests={}, build=lambda: None)
            with patch.object(preview, "build") as build:
                after = workbench.rebuild_changed_sources(preview, before, workspace)
            build.assert_called_once_with()
            self.assertNotEqual(before, after)
            self.assertNotEqual(
                digest_before, workbench.source_content_digest(workspace)
            )

    def test_replay_validation_bounds_events_text_and_dimensions(self):
        request = {
            "version": 1,
            "fixture": "ready",
            "width": 60,
            "height": 20,
            "inputs": [
                {"type": "text", "text": "夜"},
                {"type": "key", "key": "down", "ctrl": False},
                {"type": "resize", "width": 30, "height": 12},
            ],
        }
        workbench.validate_replay(json.dumps(request).encode())
        workbench.validate_replay(
            json.dumps(
                {"schema": "maestro.ui.theme-replay", "version": 1, "value": request}
            ).encode()
        )
        with self.assertRaisesRegex(ValueError, "unsupported"):
            workbench.validate_replay(
                json.dumps(
                    {
                        "schema": "maestro.ui.theme-replay",
                        "version": 99,
                        "value": request,
                    }
                ).encode()
            )
        for change in (
            lambda value: value.update(width=241),
            lambda value: value["inputs"].append({"type": "text", "text": "\n"}),
            lambda value: value.update(inputs=[{"type": "retry"}] * 65),
            lambda value: value.update(inputs=[{"type": "text", "text": "x" * 4097}]),
        ):
            invalid = json.loads(json.dumps(request))
            change(invalid)
            with self.assertRaises(ValueError):
                workbench.validate_replay(json.dumps(invalid).encode())

    def test_replay_invokes_only_fixed_command_with_structured_stdin(self):
        body = json.dumps(
            {"version": 1, "fixture": "ready", "width": 60, "height": 20, "inputs": []}
        ).encode()
        preview = workbench.Preview(
            ["renderer", "--html"],
            {},
            replay_command=["fixed-renderer", "--replay-stdin"],
        )
        preview.install_generation(self.generation())
        result = subprocess.CompletedProcess([], 0, b'{"capture":{},"effects":[]}', b"")
        with patch.object(workbench.subprocess, "run", return_value=result) as run:
            self.assertEqual(json.loads(preview.replay(body))["generation"], "g1")
        self.assertEqual(run.call_args.args[0], ("fixed-renderer", "--replay-stdin"))
        self.assertEqual(run.call_args.kwargs["input"], body)
        self.assertNotIn("shell", run.call_args.kwargs)

    def test_studio_validates_typed_items_and_invokes_only_fixed_command(self):
        request = self.menu_recipe()
        body = json.dumps(request).encode()
        workbench.validate_studio(body)
        workbench.validate_studio(
            json.dumps(
                {"schema": "maestro.ui.menu-recipe", "version": 1, "value": request}
            ).encode()
        )
        with self.assertRaisesRegex(ValueError, "unsupported"):
            workbench.validate_studio(
                json.dumps(
                    {"schema": "maestro.ui.menu-recipe", "version": 2, "value": request}
                ).encode()
            )
        for mutate in (
            lambda value: value["items"].append({"id": "docs", "label": "Duplicate"}),
            lambda value: value["items"][0].update(id="../command"),
            lambda value: value.update(path="/tmp/output"),
            lambda value: value.update(inputs=[{"type": "retry"}] * 65),
            lambda value: value.update(state={}),
            lambda value: value.update(inputs=[{"type": {}}]),
        ):
            invalid = json.loads(json.dumps(request))
            mutate(invalid)
            with self.assertRaises(ValueError):
                workbench.validate_studio(json.dumps(invalid).encode())

        preview = workbench.Preview(
            ["renderer", "--html"],
            {},
            studio_command=["fixed-renderer", "--studio-stdin"],
        )
        preview.install_generation(self.generation())
        result = subprocess.CompletedProcess(
            [], 0, b'{"capture":{},"coverage":[]}', b""
        )
        with patch.object(workbench.subprocess, "run", return_value=result) as run:
            self.assertEqual(json.loads(preview.studio(body))["generation"], "g1")
        self.assertEqual(run.call_args.args[0], ("fixed-renderer", "--studio-stdin"))
        self.assertEqual(run.call_args.kwargs["input"], body)
        self.assertNotIn("shell", run.call_args.kwargs)

    def test_invalid_studio_edit_cannot_replay_the_previous_valid_request(self):
        valid = json.dumps(self.menu_recipe()).encode()
        invalid_recipe = self.menu_recipe()
        invalid_recipe["items"] = [{"id": "unfinished-id"}]
        invalid = json.dumps(invalid_recipe).encode()
        preview = workbench.Preview(
            ["renderer", "--html"],
            {},
            studio_command=["fixed-renderer", "--studio-stdin"],
        )
        preview.install_generation(self.generation())
        result = subprocess.CompletedProcess(
            [], 0, b'{"capture":{},"coverage":[]}', b""
        )
        with patch.object(workbench.subprocess, "run", return_value=result) as run:
            preview.studio(valid)
            with self.assertRaises(ValueError):
                preview.studio(invalid)
        run.assert_called_once()
        self.assertEqual(run.call_args.kwargs["input"], valid)

    def test_http_replay_rejects_foreign_origin_and_accepts_same_origin(self):
        seen = []
        preview = SimpleNamespace(
            html=b"<html></html>",
            sequences=b'{"fixtures":[],"presets":[]}',
            revision="r",
            error="",
            revision_state=lambda: {"generation": "r", "error": ""},
            replay=lambda body: seen.append(body) or b'{"capture":{},"effects":[]}',
            studio=lambda body: seen.append(body) or b'{"capture":{},"coverage":[]}',
        )
        server = workbench.ThreadingHTTPServer(
            ("127.0.0.1", 0), workbench.handler(preview)
        )
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        port = server.server_port
        body = json.dumps(
            {"version": 1, "fixture": "ready", "width": 60, "height": 20, "inputs": []}
        )
        try:
            connection = http.client.HTTPConnection("127.0.0.1", port)
            connection.request(
                "POST",
                "/__replay",
                body,
                {
                    "Content-Type": "application/json",
                    "Origin": "https://attacker.example",
                },
            )
            self.assertEqual(connection.getresponse().status, 403)
            self.assertEqual(seen, [])

            connection = http.client.HTTPConnection("127.0.0.1", port)
            connection.request(
                "POST",
                "/__replay",
                body,
                {
                    "Content-Type": "application/json",
                    "Origin": f"http://127.0.0.1:{port}",
                },
            )
            response = connection.getresponse()
            self.assertEqual(response.status, 200)
            self.assertEqual(
                json.loads(response.read()), {"capture": {}, "effects": []}
            )
            self.assertEqual(seen, [body.encode()])

            studio = json.dumps(self.menu_recipe())
            connection = http.client.HTTPConnection("127.0.0.1", port)
            connection.request(
                "POST",
                "/__studio",
                studio,
                {
                    "Content-Type": "application/json",
                    "Origin": f"http://localhost:{port}",
                },
            )
            response = connection.getresponse()
            self.assertEqual(response.status, 200)
            self.assertEqual(
                json.loads(response.read()), {"capture": {}, "coverage": []}
            )
            self.assertEqual(seen[-1], studio.encode())
        finally:
            server.shutdown()
            server.server_close()

    def test_host_and_origin_parsing_fail_closed(self):
        self.assertTrue(workbench.valid_host("localhost:8770", 8770))
        self.assertFalse(workbench.valid_host("example.com:8770", 8770))
        self.assertTrue(workbench.valid_origin("http://127.0.0.1:8770", 8770))
        self.assertFalse(workbench.valid_origin("http://127.0.0.1:bad", 8770))

    def test_failed_and_stale_builds_never_replace_last_good_generation(self):
        body = json.dumps(
            {"version": 1, "fixture": "ready", "width": 60, "height": 20, "inputs": []}
        ).encode()
        preview = workbench.Preview(
            ["renderer"], {}, replay_command=["shared", "--replay-stdin"]
        )
        preview.install_generation(
            self.generation("g1", replay=("renderer-g1", "--replay-stdin"))
        )
        g2 = preview.begin_build("g2")
        response = subprocess.CompletedProcess([], 0, b'{"capture":{}}', b"")
        with patch.object(workbench.subprocess, "run", return_value=response) as run:
            self.assertEqual(json.loads(preview.replay(body))["generation"], "g1")
        self.assertEqual(run.call_args.args[0][0], "renderer-g1")
        preview.fail_build(g2, "compile failed")
        self.assertEqual(preview.generation.id, "g1")

        preview.begin_build("g2-stale")
        preview.begin_build("g3")
        self.assertTrue(preview.finish_build(self.generation("g3")))
        self.assertFalse(preview.finish_build(self.generation("g2-stale")))
        self.assertEqual(preview.generation.id, "g3")

    def test_revision_reports_last_good_build_while_next_generation_builds(self):
        preview = workbench.Preview(
            ["renderer"], {}, filter_kind="adapter", filter_value="shared-menu"
        )
        preview.install_generation(self.generation("g1"))
        preview.begin_build("g2")
        state = preview.revision_state()
        self.assertEqual(state["generation"], "g1")
        self.assertTrue(state["building"])
        self.assertTrue(state["stale"])
        preview.fail_build("g2", "compile failed")
        failed = preview.revision_state()
        self.assertTrue(failed["stale"])
        self.assertEqual(failed["generation"], "g1")

    def test_adapter_fingerprint_narrows_and_unknown_falls_back(self):
        with tempfile.TemporaryDirectory() as directory:
            mono = Path(directory)
            workspace = mono / "products" / "maestro"
            for path in [
                workspace / "packages/ui-preview-rs/src",
                workspace / "packages/ui-rs",
                workspace / "packages/interaction-rs/src",
                workspace / "packages/presentation-rs",
                workspace / "packages/tui-rs/src",
                workspace / "packages/owned/src",
                workspace / "packages/unrelated",
                workspace / ".cargo",
            ]:
                path.mkdir(parents=True, exist_ok=True)
            (workspace / "Cargo.toml").write_text("")
            (workspace / "Cargo.lock").write_text("")
            (workspace / ".cargo/config.toml").write_text("config")
            (workspace / "packages/ui-preview-rs/Cargo.toml").write_text("crate")
            (workspace / "packages/ui-preview-rs/build.rs").write_text("build")
            interaction = workspace / "packages/interaction-rs/src/lib.rs"
            interaction.write_text("interaction")
            theme_selector = workspace / "packages/tui-rs/src/theme_selector.rs"
            theme_selector.write_text("theme selector")
            (workspace / "packages/owned/src/story.rs").write_text("owned")
            (workspace / "packages/owned/register.rs").write_text("register")
            (workspace / "packages/unrelated/noise.rs").write_text("noise")
            manifests = {
                "owned": {
                    "fixture_dir": "products/maestro/packages/owned/src",
                    "registration_file": "products/maestro/packages/owned/register.rs",
                    "template": "theme-selector",
                }
            }
            paths = workbench.source_paths(workspace, "adapter", "owned", manifests)
            self.assertIn(workspace / ".cargo/config.toml", paths)
            self.assertIn(workspace / "packages/ui-preview-rs/Cargo.toml", paths)
            self.assertIn(workspace / "packages/ui-preview-rs/build.rs", paths)
            self.assertIn(interaction, paths)
            self.assertIn(theme_selector, paths)
            narrowed = workbench.fingerprint(workspace, "adapter", "owned", manifests)
            interaction.write_text("interaction changed")
            changed_dependency = workbench.fingerprint(
                workspace, "adapter", "owned", manifests
            )
            self.assertNotEqual(narrowed, changed_dependency)
            narrowed = changed_dependency
            (workspace / "packages/unrelated/noise.rs").write_text("changed")
            self.assertEqual(
                narrowed,
                workbench.fingerprint(workspace, "adapter", "owned", manifests),
            )
            fallback = workbench.fingerprint(workspace, "adapter", "missing", manifests)
            (workspace / "packages/unrelated/noise.rs").write_text("changed again")
            self.assertNotEqual(
                fallback,
                workbench.fingerprint(workspace, "adapter", "missing", manifests),
            )

    def test_source_digest_uses_contents_even_when_metadata_is_restored(self):
        with tempfile.TemporaryDirectory() as directory:
            workspace = Path(directory)
            (workspace / "packages").mkdir()
            source = workspace / "packages" / "same-size.rs"
            source.write_text("aa")
            stat = source.stat()
            before = workbench.source_content_digest(workspace)
            source.write_text("bb")
            os.utime(source, ns=(stat.st_atime_ns, stat.st_mtime_ns))
            after = workbench.source_content_digest(workspace)
            self.assertNotEqual(before, after)

    def test_source_digest_is_stable_across_equivalent_worktrees(self):
        with (
            tempfile.TemporaryDirectory() as first,
            tempfile.TemporaryDirectory() as second,
        ):
            for directory in [first, second]:
                workspace = Path(directory)
                (workspace / "packages").mkdir()
                (workspace / "Cargo.toml").write_text("[workspace]")
                (workspace / "packages/story.rs").write_text("same source")
            self.assertEqual(
                workbench.source_content_digest(Path(first)),
                workbench.source_content_digest(Path(second)),
            )

    def test_build_bodies_are_serialized_without_blocking_last_good_replay(self):
        with (
            tempfile.TemporaryDirectory() as directory,
            patch.object(workbench, "WORKSPACE", Path(directory)),
        ):
            active = 0
            maximum = 0
            guard = threading.Lock()

            def render(*_args, **_kwargs):
                nonlocal active, maximum
                with guard:
                    active += 1
                    maximum = max(maximum, active)
                time.sleep(0.03)
                with guard:
                    active -= 1
                return subprocess.CompletedProcess([], 0, b"<html>valid</html>", b"")

            preview = workbench.Preview(["renderer"], {})
            preview.install_generation(self.generation("g1"))
            with patch.object(workbench.subprocess, "run", side_effect=render):
                first = threading.Thread(target=preview.build)
                second = threading.Thread(target=preview.build)
                first.start()
                second.start()
                first.join()
                second.join()
            self.assertEqual(maximum, 1)
            self.assertIsNotNone(preview.generation)

    def test_source_change_during_build_keeps_last_good_generation(self):
        with (
            tempfile.TemporaryDirectory() as directory,
            patch.object(workbench, "WORKSPACE", Path(directory)),
        ):
            root = Path(directory)
            (root / "packages").mkdir()
            source = root / "packages/story.rs"
            source.write_text("before")
            preview = workbench.Preview(["renderer"], {})
            preview.install_generation(self.generation("g1"))

            def render(*_args, **_kwargs):
                source.write_text("after")
                return subprocess.CompletedProcess([], 0, b"<html>new</html>", b"")

            with patch.object(workbench.subprocess, "run", side_effect=render):
                preview.build()
            self.assertEqual(preview.generation.id, "g1")
            self.assertIn("Source inputs changed", preview.error)

    def test_snapshot_cleanup_keeps_active_and_bounds_recent_generations(self):
        with tempfile.TemporaryDirectory() as directory:
            target = Path(directory)
            preview = workbench.Preview(["renderer"], {"CARGO_TARGET_DIR": str(target)})
            root = preview.snapshot_root
            for index in range(7):
                path = root / f"digest-{index}"
                path.mkdir(parents=True)
                (path / "renderer").write_text(str(index))
                os.utime(path, ns=(index + 1, index + 1))
            preview.install_generation(
                self.generation("active").__class__(
                    **{
                        **self.generation("active").__dict__,
                        "renderer_digest": "digest-0",
                    }
                )
            )
            preview._in_use_digests["digest-1"] = 1
            preview._cleanup_snapshots("digest-0")
            remaining = {path.name for path in root.iterdir()}
            self.assertIn("digest-0", remaining)
            self.assertIn("digest-1", remaining)
            self.assertLessEqual(len(remaining), 5)

    def test_snapshot_sessions_do_not_clean_other_process_namespaces(self):
        with tempfile.TemporaryDirectory() as directory:
            env = {"CARGO_TARGET_DIR": directory}
            first = workbench.Preview(["renderer"], env)
            second = workbench.Preview(["renderer"], env)
            self.assertNotEqual(first.snapshot_root, second.snapshot_root)
            foreign = second.snapshot_root / "foreign-digest"
            foreign.mkdir()
            first._cleanup_snapshots("active")
            self.assertTrue(foreign.exists())
            first.close()
            self.assertTrue(second.snapshot_root.exists())

    def test_in_flight_renderer_is_protected_during_generation_cleanup(self):
        with tempfile.TemporaryDirectory() as directory:
            preview = workbench.Preview(["renderer"], {"CARGO_TARGET_DIR": directory})
            old = self.generation("old", replay=("old-renderer", "--replay-stdin"))
            preview.install_generation(old)
            for digest in [old.renderer_digest, "new", "a", "b", "c", "d"]:
                (preview.snapshot_root / digest).mkdir()
            entered = threading.Event()
            release = threading.Event()

            def replay(*_args, **_kwargs):
                entered.set()
                release.wait(1)
                return subprocess.CompletedProcess([], 0, b'{"capture":{}}', b"")

            body = json.dumps(
                {
                    "version": 1,
                    "fixture": "ready",
                    "width": 60,
                    "height": 20,
                    "inputs": [],
                }
            ).encode()
            with patch.object(workbench.subprocess, "run", side_effect=replay):
                thread = threading.Thread(target=lambda: preview.replay(body))
                thread.start()
                self.assertTrue(entered.wait(1))
                preview._cleanup_snapshots("new")
                self.assertTrue((preview.snapshot_root / old.renderer_digest).exists())
                release.set()
                thread.join()


if __name__ == "__main__":
    unittest.main()

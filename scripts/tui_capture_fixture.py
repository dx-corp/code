"""Loopback-only Identity and scripted model fixtures for development captures.

Uses the same debug-only Identity admission path as tests/pty_e2e.rs.
Never used by the installed runtime. No upstream requests or real credentials.
"""

from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import threading


class CaptureFixture:
    model = "gpt-6-astra"

    def __init__(self, scene="conversation"):
        self.scene = scene
        self.stopped = threading.Event()
        self.turn = 0
        self.identity_requests = 0
        fixture = self

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *_):
                pass

            def do_POST(self):
                self.connection.settimeout(5)
                length = int(self.headers.get("Content-Length", "0"))
                if length > 2_000_000:
                    self.send_error(413)
                    return
                request_body = self.rfile.read(length)
                if self.path in {"/v1/chat/completions", "/v1/responses"}:
                    responses = self.path == "/v1/responses"
                    fixture.turn += 1
                    if fixture.scene == "streaming":
                        self.send_response(200)
                        self.send_header("Content-Type", "text/event-stream")
                        self.end_headers()
                        chunk = {
                            "id": "capture-stream",
                            "object": "chat.completion.chunk",
                            "created": 0,
                            "model": fixture.model,
                            "choices": [
                                {
                                    "index": 0,
                                    "delta": {
                                        "role": "assistant",
                                        "content": "I’m reviewing the release checklist.\n\nThe release owner and checks are clear; I’m checking the remaining steps…",
                                    },
                                    "finish_reason": None,
                                }
                            ],
                        }
                        self.wfile.write(
                            ("data: " + json.dumps(
                                {"type": "response.output_text.delta", "delta": chunk["choices"][0]["delta"]["content"]}
                                if responses else chunk
                            ) + "\n\n").encode()
                        )
                        self.wfile.flush()
                        fixture.stopped.wait(20)
                        return
                    if fixture.scene == "long-conversation":
                        delta = {
                            "role": "assistant",
                            "content": "Release review\n\n"
                            + "\n".join(
                                f"{i}. Review step {i}: confirm the owner, run the checks, and record the outcome."
                                for i in range(1, 25)
                            )
                            + "\n\nRelease review complete.",
                        }
                        finish = "stop"
                    elif fixture.turn == 1:
                        delta = {
                            "role": "assistant",
                            "tool_calls": [
                                {
                                    "index": 0,
                                    "id": "read-1",
                                    "type": "function",
                                    "function": {
                                        "name": "bash"
                                        if fixture.scene in {"approval", "approval-light"}
                                        else "read",
                                        "arguments": json.dumps(
                                            {"command": "printf release-ready"}
                                            if fixture.scene in {"approval", "approval-light"}
                                            else {
                                                "path": "missing-checklist.md"
                                                if fixture.scene == "error"
                                                else "README.md"
                                            }
                                        ),
                                    },
                                }
                            ],
                        }
                        finish = "tool_calls"
                    elif fixture.turn == 2:
                        request = json.loads(request_body)
                        if not any(
                            (message.get("role") == "tool" or message.get("type") == "function_call_output")
                            and (
                                fixture.scene == "error"
                                or "Release checklist"
                                in str(message.get("content", message.get("output", "")))
                            )
                            for message in (request.get("input", []) if responses else request.get("messages", []))
                        ):
                            self.send_error(
                                409, "capture requires the actual README tool result"
                            )
                            return
                        delta = {
                            "role": "assistant",
                            "content": "This project keeps a small team’s release checklist in one place.\n\n"
                            "- Track the next release and its owner.\n"
                            "- Review changes before publishing.\n"
                            "- Keep a short record of what shipped.\n\n"
                            "The README is a good starting point. I can help turn the checklist into a working page.",
                        }
                        if fixture.scene == "error":
                            delta["content"] = (
                                "I couldn’t read missing-checklist.md because it does not exist. Check the filename or choose README.md to continue."
                            )
                        if fixture.scene == "markdown":
                            delta["content"] = (
                                "## Release checklist\n\n"
                                "The project tracks **owners**, review steps, and shipped work.\n\n"
                                "| Step | Owner | Status |\n| --- | --- | --- |\n"
                                "| Review | Maya | Ready |\n| Publish | Alex | Pending |\n\n"
                                "```rust\nfn ready(approved: bool) -> bool {\n    approved\n}\n```\n\n"
                                "- Review the changes.\n- Publish after approval.\n\n"
                                "Final row visible."
                            )
                        finish = "stop"
                    elif fixture.scene in {"summary-review", "summary-save"} and fixture.turn == 3:
                        request = json.loads(request_body)
                        if request.get("tools"):
                            self.send_error(409, "summary must not expose tools")
                            return
                        delta = {"role": "assistant", "content": "The user asked for a release checklist review. The README was read successfully. The project tracks release owners, review steps, and shipped work. No files were changed."}
                        finish = "stop"
                    else:
                        self.send_error(409, "capture fixture exhausted")
                        return

                    def chunk(content, reason=None):
                        return {
                            "id": "capture-1",
                            "object": "chat.completion.chunk",
                            "created": 0,
                            "model": fixture.model,
                            "choices": [
                                {"index": 0, "delta": content, "finish_reason": reason}
                            ],
                        }

                    body = (
                        "".join(
                            "data: " + json.dumps(c) + "\n\n"
                            for c in [chunk(delta), chunk({}, finish)]
                        )
                        + "data: [DONE]\n\n"
                    )
                    if responses:
                        output = [
                            {"type": "function_call", "call_id": call["id"], **call["function"]}
                            for call in delta.get("tool_calls", [])
                        ]
                        if "content" in delta:
                            output.append({"type": "message", "role": "assistant", "content": [
                                {"type": "output_text", "text": delta["content"]}
                            ]})
                        event = {"type": "response.completed", "response": {
                            "id": "capture-response", "model": fixture.model, "output": output,
                            "usage": {"input_tokens": 100, "output_tokens": 20},
                        }}
                        body = "data: " + json.dumps(event) + "\n\n"
                    content_type = "text/event-stream"
                elif self.path == "/console.v1.ManagedSetupService/GetManagedSetup":
                    # ManagedSetup v1, empty MCP allowlist, capture organization.
                    body = b"\x08\x01\x2a\x02\x08\x02\x3a\x0bcapture-org"
                    content_type = "application/proto"
                elif self.path == "/v1/tokens/introspect":
                    fixture.identity_requests += 1
                    body = json.dumps(
                        {
                            "active": True,
                            "subject": "capture-user",
                            "token_type": "access",
                            "organization_id": "capture-org",
                            "workspace_id": "capture-workspace",
                            "scopes": ["llm_gateway:invoke"],
                        }
                    )
                    content_type = "application/json"
                else:
                    self.send_error(404)
                    return
                payload = body if isinstance(body, bytes) else body.encode()
                self.send_response(200)
                self.send_header("Content-Type", content_type)
                self.send_header("Content-Length", str(len(payload)))
                self.end_headers()
                self.wfile.write(payload)

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)

    def __enter__(self):
        self.thread.start()
        return self

    def __exit__(self, *_):
        self.stopped.set()
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=5)

    def environment(self):
        base = f"http://127.0.0.1:{self.server.server_port}"
        return {
            "MAESTRO_IDENTITY_URL": base,
            "MAESTRO_MANAGED_SETUP_URL": base,
            "MAESTRO_TEST_IDENTITY_AUTHORITY": "1",
            "MAESTRO_EVALOPS_ACCESS_TOKEN": "capture-token",
            "MAESTRO_EVALOPS_ORG_ID": "capture-org",
            "MAESTRO_DISABLE_KEYCHAIN": "1",
            "OPENAI_BASE_URL": base + "/v1",
        }

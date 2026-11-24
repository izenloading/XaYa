"""Deterministic loopback Responses fixture. Never forwards requests upstream."""

import json
import shlex
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import threading


class ModelFixture:
    def __init__(self, review=None, commands=None, respond=None):
        """`commands` maps a prompt marker (e.g. "TOOL_MARKER") to a shell command to run once.
        `respond(body, tool_done)` may return a response item (dict) to send instead."""
        commands = commands or {}
        self.requests = []
        self.errors = []
        self.release = threading.Event()
        self.slow_started = threading.Event()
        fixture = self

        class Handler(BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def log_message(self, *_):
                pass

            def do_GET(self):
                body = b'{"data":[]}'
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

            def do_POST(self):
                try:
                    size = int(self.headers.get("Content-Length", "0"))
                    if size <= 0 or size > 8 * 1024 * 1024:
                        raise ValueError("Invalid fixture request size")
                    body = json.loads(self.rfile.read(size))
                    fixture.requests.append(body)
                    self.emit(body)
                except (BrokenPipeError, ConnectionResetError):
                    pass  # Expected when interrupting the held response.
                except Exception as error:
                    fixture.errors.append(str(error))
                    self.close_connection = True

            def emit(self, body):
                users = [item for item in body.get("input", []) if item.get("role") == "user"]
                prompt = json.dumps(users[-1] if users else {})
                marker = next((value for value in (
                    "REVIEW_READONLY", "TERMINAL_FIRST", "INJECT_SECOND", "TERMINAL_THIRD", "COMMAND_ACCEPT",
                    "COMMAND_DENY", "FILE_ACCEPT", "FILE_DENY", "FINAL_NATIVE", "SLOW_CANCEL", "AFTER_RESTART",
                    *commands) if value in prompt), "BOOTSTRAP")
                last = (body.get("input") or [{}])[-1]
                tool_done = last.get("type") in ("function_call_output", "custom_tool_call_output")
                number = len(fixture.requests)
                item = {"id": f"msg_{number}", "type": "message", "status": "completed",
                        "role": "assistant", "content": [{"type": "output_text",
                        "text": f"ACK_{marker}_COMPLETE", "annotations": []}]}
                title = body.get("text", {}).get("format", {}).get("schema", {}).get("properties", {}).get("title")
                custom = respond(body, tool_done) if respond else None
                if custom is not None:
                    item = {"id": f"item_{number}", **custom}
                    if item.get("type") == "function_call":
                        item.setdefault("call_id", f"call_{number}")
                elif title:
                    item["content"][0]["text"] = '{"title":"XaYa probe"}'
                elif review and marker == "REVIEW_READONLY":
                    if not tool_done:
                        item = {"id": f"tool_{number}", "call_id": f"call_{number}", "type": "function_call",
                            "status": "completed", "name": "exec_command", "arguments": json.dumps({
                            "cmd": "printf forbidden > " + shlex.quote(str(review["forbidden"])),
                            "shell": "/bin/sh", "login": False, "sandbox_permissions": "require_escalated",
                            "justification": "Intentional fixed write attempt for isolated read-only verification."})}
                    else:
                        item["content"][0]["text"] = json.dumps({"checkpoint": review["checkpoint"],
                            "verdict": "approved", "findings": []})
                elif not tool_done and marker in commands:
                    item = {"id": f"tool_{number}", "call_id": f"call_{number}",
                            "type": "function_call", "status": "completed", "name": "exec_command",
                            "arguments": json.dumps({"cmd": commands[marker], "shell": "/bin/sh",
                                                     "login": False})}
                elif not tool_done and marker.startswith("COMMAND_"):
                    item = {"id": f"tool_{number}", "call_id": f"call_{number}",
                            "type": "function_call", "status": "completed", "name": "exec_command",
                            "arguments": json.dumps({"cmd": f"printf XAYA_{marker}",
                                "shell": "/bin/sh", "login": False,
                                "sandbox_permissions": "require_escalated",
                                "justification": "Run the isolated local probe marker command."})}
                elif not tool_done and marker.startswith("FILE_"):
                    name = "accepted.txt" if marker == "FILE_ACCEPT" else "denied.txt"
                    patch = f"*** Begin Patch\n*** Add File: {name}\n+XAYA_{marker}\n*** End Patch"
                    patch_tool = next((tool for tool in body.get("tools", [])
                                       if tool.get("name") == "apply_patch"), None)
                    if patch_tool:
                        item = {"id": f"tool_{number}", "call_id": f"call_{number}",
                                "type": "custom_tool_call" if patch_tool["type"] == "custom" else "function_call",
                                "name": "apply_patch", "status": "completed"}
                        item.update({"input": patch} if patch_tool["type"] == "custom" else
                                    {"arguments": json.dumps({"patch": patch})})
                    else:
                        item = {"id": f"tool_{number}", "call_id": f"call_{number}",
                                "type": "function_call", "status": "completed", "name": "exec_command",
                                "arguments": json.dumps({"cmd": f"apply_patch <<'PATCH'\n{patch}\nPATCH",
                                                          "shell": "/bin/sh", "login": False})}
                response = {"id": f"resp_{number}", "object": "response", "created_at": 1,
                            "status": "completed", "model": body.get("model", "gpt-5.4"),
                            "output": [item], "usage": {"input_tokens": 20, "output_tokens": 10,
                            "total_tokens": 30, "input_tokens_details": {"cached_tokens": 0},
                            "output_tokens_details": {"reasoning_tokens": 0}}}
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream")
                self.send_header("Connection", "close")
                self.end_headers()
                self.close_connection = True

                def event(kind, **values):
                    payload = json.dumps({"type": kind, **values})
                    self.wfile.write(f"event: {kind}\ndata: {payload}\n\n".encode())
                    self.wfile.flush()

                event("response.created", response={**response, "status": "in_progress", "output": []})
                event("response.output_item.added", output_index=0,
                      item={**item, "status": "in_progress", **({"content": []} if item["type"] == "message" else {})})
                if item["type"] == "message":
                    text = item["content"][0]["text"]
                    event("response.content_part.added", item_id=item["id"], output_index=0,
                          content_index=0, part={"type": "output_text", "text": "", "annotations": []})
                    event("response.output_text.delta", item_id=item["id"], output_index=0,
                          content_index=0, delta=text[:10])
                    if marker == "SLOW_CANCEL":
                        fixture.slow_started.set()
                        fixture.release.wait(40)
                    event("response.output_text.delta", item_id=item["id"], output_index=0,
                          content_index=0, delta=text[10:])
                event("response.output_item.done", output_index=0, item=item)
                event("response.completed", response=response)

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.server.daemon_threads = True
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)

    @property
    def base_url(self):
        return f"http://127.0.0.1:{self.server.server_port}/v1"

    def __enter__(self):
        self.thread.start()
        return self

    def __exit__(self, *_):
        self.release.set()
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=3)

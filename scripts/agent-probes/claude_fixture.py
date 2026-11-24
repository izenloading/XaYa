"""Loopback Anthropic Messages fixture for native Claude background probes."""

from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import re
import threading
import time


def has_native_prompt(request, prompt):
    messages = request.get("messages") or []
    if not messages or messages[-1].get("role") != "user":
        return False
    content = messages[-1].get("content")
    return isinstance(content, list) and any(isinstance(part, dict) and part.get("type") == "text"
        and part.get("text") == prompt for part in content)


class ClaudeFixture:
    def __init__(self, tool_workspace=None, tool_command=None, review=None,
                 review_marker="XAYA_CLAUDE_REVIEW"):
        """`review["structured"]`, if given, maps the request body to the StructuredOutput input."""
        self.requests = []
        self.errors = []
        self.observations = []
        self.holds = {marker: threading.Event() for marker in (
            "XAYA_CLAUDE_SLOW", "XAYA_CLAUDE_CANCEL_TOOL", "XAYA_CLAUDE_NATIVE_ESC")}
        fixture = self

        class Handler(BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def log_message(self, *_):
                pass

            def json(self, value):
                raw = json.dumps(value).encode()
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(raw)))
                self.end_headers()
                self.wfile.write(raw)

            def do_GET(self):
                self.json({"data": []})

            def do_POST(self):
                try:
                    size = int(self.headers.get("Content-Length", "0"))
                    if size <= 0 or size > 8 * 1024 * 1024:
                        raise ValueError("Invalid Claude fixture request size")
                    body = json.loads(self.rfile.read(size))
                    if "count_tokens" in self.path:
                        self.json({"input_tokens": 10})
                        return
                    if "/messages" not in self.path:
                        self.json({"data": []})
                        return
                    fixture.requests.append(body)
                    markers = re.findall(r"XAYA_CLAUDE_[A-Z_]+", json.dumps(body.get("messages", [])))
                    marker = markers[-1] if markers else "XAYA_CLAUDE_BOOTSTRAP"
                    reply = f"ACK_{marker}_COMPLETE"
                    observation = {"marker": marker, "started_at": time.monotonic()}
                    fixture.observations.append(observation)
                    # Background workers issue auxiliary state-classification calls after a turn.
                    # They are not native conversation turns, even when they quote its marker.
                    system = json.dumps(body.get("system", []))
                    if "THE FOUR STATES" in system and "tempo" in system:
                        reply = json.dumps({"state": "done", "detail": "Local probe completed",
                                            "tempo": "idle", "output": {"result": "Fixture replied"}})
                    message = {"id": f"msg_{len(fixture.requests)}", "type": "message",
                        "role": "assistant", "model": body.get("model", "claude-sonnet-4-6"),
                        "content": [{"type": "text", "text": reply}], "stop_reason": "end_turn",
                        "stop_sequence": None, "usage": {"input_tokens": 10, "output_tokens": 8}}
                    latest = (body.get("messages") or [{}])[-1].get("content")
                    tool_result = isinstance(latest, list) and any(p.get("type") == "tool_result" for p in latest)
                    if review and review_marker in json.dumps(body.get("messages", [])):
                        blocks = [part for m in body.get("messages", []) for part in
                                  (m.get("content") if isinstance(m.get("content"), list) else [])]
                        wrote_attempt = any(part.get("type") == "tool_use" and part.get("name") == "Write" for part in blocks)
                        structured_attempt = any(part.get("type") == "tool_use" and part.get("name") == "StructuredOutput" for part in blocks)
                        if not wrote_attempt:
                            content = {"type": "tool_use", "id": "toolu_readonly", "name": "Write",
                                "input": {"file_path": str(review["forbidden"]), "content": "forbidden"}}
                        elif not structured_attempt:
                            answer = (review["structured"](body) if "structured" in review else
                                      {"checkpoint": review["checkpoint"], "verdict": "approved", "findings": []})
                            content = {"type": "tool_use", "id": "toolu_receipt", "name": "StructuredOutput",
                                "input": answer}
                        else:
                            content = {"type": "text", "text": "Review complete"}
                        message.update(content=[content], stop_reason="end_turn" if structured_attempt else "tool_use")
                    if tool_workspace and not tool_result and marker in (
                        "XAYA_CLAUDE_ALLOW", "XAYA_CLAUDE_DENY", "XAYA_CLAUDE_NATIVE_PERMISSION", "XAYA_CLAUDE_CANCEL_TOOL"):
                        tool = {"type": "tool_use", "id": f"toolu_{len(fixture.requests)}", "name": "Write",
                                "input": {"file_path": str(tool_workspace / (marker + ".txt")), "content": marker + "\n"}}
                        if marker == "XAYA_CLAUDE_CANCEL_TOOL":
                            tool.update(name="Bash", input={"command": tool_command,
                                "description": "Local disposable delayed writer", "run_in_background": True})
                        message.update(content=[tool], stop_reason="tool_use")
                    if not body.get("stream"):
                        self.json(message)
                        return
                    self.send_response(200)
                    self.send_header("Content-Type", "text/event-stream")
                    self.send_header("Connection", "close")
                    self.end_headers()
                    self.close_connection = True

                    def event(kind, **values):
                        raw = json.dumps({"type": kind, **values})
                        self.wfile.write(f"event: {kind}\ndata: {raw}\n\n".encode())
                        self.wfile.flush()

                    event("message_start", message={**message, "content": [], "stop_reason": None,
                        "usage": {"input_tokens": 10, "output_tokens": 0}})
                    if message["stop_reason"] == "tool_use":
                        tool = message["content"][0]
                        event("content_block_start", index=0, content_block={**tool, "input": {}})
                        event("content_block_delta", index=0, delta={"type": "input_json_delta", "partial_json": json.dumps(tool["input"])})
                    else:
                        event("content_block_start", index=0, content_block={"type": "text", "text": ""})
                        event("content_block_delta", index=0, delta={"type": "text_delta", "text": reply[:10]})
                        observation["first_delta_at"] = time.monotonic()
                        if marker in fixture.holds:
                            fixture.holds[marker].wait(35)
                        else:
                            time.sleep(0.35)
                        event("content_block_delta", index=0, delta={"type": "text_delta", "text": reply[10:]})
                    event("content_block_stop", index=0)
                    event("message_delta", delta={"stop_reason": message["stop_reason"], "stop_sequence": None},
                          usage={"output_tokens": 8})
                    event("message_stop")
                    observation["finished_at"] = time.monotonic()
                except (BrokenPipeError, ConnectionResetError):
                    pass
                except Exception as error:
                    fixture.errors.append(str(error))
                    self.close_connection = True

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.server.daemon_threads = True
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)

    @property
    def base_url(self):
        return f"http://127.0.0.1:{self.server.server_port}"

    def __enter__(self):
        self.thread.start()
        return self

    def __exit__(self, *_):
        for release in self.holds.values():
            release.set()
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=3)

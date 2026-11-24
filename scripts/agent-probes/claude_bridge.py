"""Authenticated, loopback-only bridge for a single disposable Claude worker."""

from collections import deque
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import secrets
import threading
import time
import uuid


class ClaudeBridge:
    def __init__(self):
        self.token = secrets.token_hex(24)
        self.events = []
        self.acks = {}
        self.commands = deque()
        self.permissions = []
        self.permission_ready = threading.Event()
        self.lock = threading.Condition()
        bridge = self

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *_):
                pass

            def reply(self, value, status=200):
                raw = json.dumps(value).encode()
                self.send_response(status)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(raw)))
                self.end_headers()
                self.wfile.write(raw)

            def do_POST(self):
                try:
                    if not secrets.compare_digest(self.headers.get("X-Probe-Token", ""), bridge.token):
                        self.reply({"error": "unauthorized"}, 403)
                        return
                    size = int(self.headers.get("Content-Length", "0"))
                    if size <= 0 or size > 2 * 1024 * 1024:
                        self.reply({"error": "invalid size"}, 413)
                        return
                    value = json.loads(self.rfile.read(size))
                    if not isinstance(value, dict):
                        self.reply({"error": "object required"}, 400)
                        return
                    if self.path == "/events":
                        with bridge.lock:
                            bridge.events.append({**value, "received_at": time.monotonic()})
                            if value.get("type") == "turn.complete":
                                for pending in bridge.permissions:
                                    if pending["state"] == "pending" and pending["event"].get("session_id") == value.get("sessionId"):
                                        pending["state"] = "expired"
                                bridge.lock.notify_all()
                        self.reply({})
                    elif self.path == "/poll":
                        with bridge.lock:
                            command = bridge.commands.popleft() if bridge.commands else None
                        self.reply({"command": command})
                    elif self.path == "/acks":
                        with bridge.lock:
                            bridge.acks[value["id"]] = value
                        self.reply({})
                    elif self.path == "/hooks":
                        if value.get("hook_event_name") != "PermissionRequest":
                            self.reply({})
                            return
                        pending = {"id": str(uuid.uuid4()), "event": value, "state": "pending", "decision": None}
                        with bridge.lock:
                            bridge.permissions.append(pending)
                            bridge.permission_ready.set()
                            bridge.lock.wait_for(lambda: pending["state"] != "pending", timeout=20)
                            if pending["state"] == "pending":
                                pending["state"] = "expired"
                        if pending["state"] == "answered":
                            self.reply({"hookSpecificOutput": {"hookEventName": "PermissionRequest",
                                "decision": {"behavior": pending["decision"], "message": "Local probe decision"}}})
                        else:
                            self.reply({})  # Native completion/timeout has expired this request.
                    else:
                        self.reply({"error": "unknown route"}, 404)
                except (BrokenPipeError, ConnectionResetError):
                    pass
                except (ValueError, KeyError, TypeError):
                    self.reply({"error": "invalid payload"}, 400)

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.server.daemon_threads = True
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)

    @property
    def endpoint(self):
        return f"http://127.0.0.1:{self.server.server_port}"

    def enqueue(self, action):
        if action.get("type") not in ("send", "abort", "messages", "stop-task", "clear-draft"):
            raise ValueError("Unknown probe command")
        identifier = str(uuid.uuid4())
        with self.lock:
            self.commands.append({**action, "id": identifier})
        return identifier

    def answer(self, identifier, decision):
        if decision not in ("allow", "deny"):
            raise ValueError("Unknown permission decision")
        with self.lock:
            pending = next((p for p in self.permissions if p["id"] == identifier), None)
            if pending is None or pending["state"] != "pending":
                return False
            pending.update(state="answered", decision=decision)
            self.lock.notify_all()
            return True

    def __enter__(self):
        self.thread.start()
        return self

    def __exit__(self, *_):
        with self.lock:
            for pending in self.permissions:
                if pending["state"] == "pending":
                    pending["state"] = "expired"
            self.lock.notify_all()
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=3)

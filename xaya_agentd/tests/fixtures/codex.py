"""Deterministic engine fixture; never contacts a model or reads user config.

Speaks JSON-RPC over stdio by default, or as a WebSocket server on `--listen unix://PATH` like
`codex app-server`. Thread and turn state outlive a connection: a turn keeps running while no
client is connected, and `thread/resume` re-sends an unanswered approval, as Codex 0.169 does.
"""
import base64
import hashlib
import json
import os
import socket
import struct
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path

write_lock = threading.Lock()
counts = {"sends": 0, "answers": 0}
turns = {}
active_turn = None
approval_turn = None
approval_id = 100
connection = None  # Current WebSocket client, or None while disconnected.
initialized = False  # Per connection: a client must initialize exactly once.

# Audit outside the checkout: a launch must not leave untracked files in the session cwd.
home = Path(os.environ.get("CODEX_HOME", "."))
home.mkdir(parents=True, exist_ok=True)
# `--remote` runs the native TUI's role instead of the backend's.
TUI = "--remote" in sys.argv
if not TUI:
    with (home / "backend-launches.txt").open("a") as launches:
        launches.write(f"{os.getpid()}\n")

# Like Codex, `-c shell_environment_policy.set.NAME="VALUE"` reaches exec tools only, never
# the backend itself or its MCP servers.
tool_env = {}
for index, value in enumerate(sys.argv[:-1]):
    prefix = "shell_environment_policy.set."
    if value == "-c" and sys.argv[index + 1].startswith(prefix):
        name, raw = sys.argv[index + 1][len(prefix):].split("=", 1)
        tool_env[name] = json.loads(raw)


def spawn_tool():
    """An exec tool that leaves a setsid() grandchild behind, as a backgrounded command may."""
    script = ("import os, sys, time\n"
              "if os.fork() == 0:\n"
              "    os.setsid()\n"
              "    open(sys.argv[1], 'w').write(str(os.getpid()))\n"
              "    time.sleep(60)\n"
              "    os._exit(0)\n"
              "time.sleep(60)\n")
    grandchild = home / f"tool-grandchild-{time.monotonic_ns()}.pid"
    child = subprocess.Popen([sys.executable, "-c", script, str(grandchild)],
                             env={**os.environ, **tool_env}, stdin=subprocess.DEVNULL,
                             stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    deadline = time.monotonic() + 5
    while not grandchild.exists() or not grandchild.read_text():
        if time.monotonic() > deadline:
            raise RuntimeError("tool grandchild did not start")
        time.sleep(0.01)
    # Codex reaps the commands it runs; an unreaped child would linger as a zombie.
    threading.Thread(target=child.wait, daemon=True).start()
    return {"pid": child.pid, "grandchild": int(grandchild.read_text())}


def send_frame(stream, opcode, payload):
    header = bytes([0x80 | opcode])
    if len(payload) < 126:
        header += bytes([len(payload)])
    elif len(payload) < 1 << 16:
        header += bytes([126]) + struct.pack(">H", len(payload))
    else:
        header += bytes([127]) + struct.pack(">Q", len(payload))
    stream.sendall(header + payload)


def read_exact(stream, length):
    data = b""
    while len(data) < length:
        chunk = stream.recv(length - len(data))
        if not chunk:
            raise EOFError
        data += chunk
    return data


def read_message(stream):
    """Returns the next text message, answering pings; None when the client closes."""
    message = b""
    while True:
        first, second = read_exact(stream, 2)
        length = second & 0x7F
        if length == 126:
            length = struct.unpack(">H", read_exact(stream, 2))[0]
        elif length == 127:
            length = struct.unpack(">Q", read_exact(stream, 8))[0]
        mask = read_exact(stream, 4) if second & 0x80 else b"\0\0\0\0"
        payload = bytes(b ^ mask[i % 4] for i, b in enumerate(read_exact(stream, length)))
        opcode = first & 0x0F
        if opcode == 8:
            return None
        if opcode == 9:
            with write_lock:
                if TUI:
                    send_client_frame(stream, payload, 10)
                else:
                    send_frame(stream, 10, payload)
            continue
        if opcode in (0, 1):
            message += payload
            if first & 0x80:
                return message.decode()


def emit(message):
    with write_lock:
        if listen is None:
            print(json.dumps(message), flush=True)
        elif connection is not None:
            try:
                send_frame(connection, 1, json.dumps(message).encode())
            except OSError:
                pass  # Notifications for a vanished client are dropped, as by Codex.


def complete(turn, status="completed", delayed=False):
    global active_turn
    if delayed:
        time.sleep(0.4)
    turns[turn] = status
    emit({"method": "turn/completed", "params": {
        "threadId": "main-thread", "turn": {"id": turn, "status": status}}})
    if active_turn == turn:
        active_turn = None


def request_approval():
    emit({"id": approval_id, "method": "item/commandExecution/requestApproval", "params": {
        "threadId": "main-thread", "turnId": approval_turn, "command": "printf fixture",
        "availableDecisions": ["accept", "decline"]}})


def handle(request):
    global initialized, active_turn, approval_turn, approval_id
    method = request.get("method")
    if method == "initialize":
        if initialized:
            raise RuntimeError("backend was initialized twice")
        initialized = True
        result = {"userAgent": "fixture", "platformFamily": "unix", "platformOs": "macos"}
    elif method in ("thread/start", "thread/resume"):
        emit({"method": "thread/started", "params": {"thread": {"id": "auxiliary"}}})
        result = {"thread": {"id": request.get("params", {}).get("threadId", "main-thread")}}
        if method == "thread/resume" and approval_turn:
            emit({"id": request["id"], "result": result})
            request_approval()
            return
    elif method == "thread/read":
        result = {"thread": {"id": "main-thread", "turns": [
            {"id": turn, "status": status} for turn, status in turns.items()]}}
    elif method == "turn/start":
        counts["sends"] += 1
        parts = request.get("params", {}).get("input", [])
        text = parts[0].get("text", "") if parts else ""
        with Path("turns-audit.txt").open("a") as audit:
            audit.write(text + "\n")
        if text == "ERROR":
            emit({"id": request["id"], "error": {"code": -1, "message": "fixture rejection"}})
            return
        turn = active_turn = f"turn-{counts['sends']}"
        turns[turn] = "inProgress"
        result = {"turn": {"id": turn, "status": "inProgress"}}
        emit({"method": "turn/started", "params": {"threadId": "main-thread", "turn": result["turn"]}})
        emit({"method": "item/agentMessage/delta", "params": {
            "threadId": "main-thread", "turnId": turn, "itemId": "message", "delta": "fixture:" + text}})
        if text == "APPROVAL":
            approval_turn = turn
            approval_id += 1
            request_approval()
        elif text not in ("HOLD", "NO_REPLY"):
            threading.Thread(target=complete, args=(turn, "completed", True), daemon=True).start()
        if text == "NO_REPLY":
            return
    elif method == "turn/interrupt":
        complete(request["params"]["turnId"], "interrupted")
        result = {}
    elif method is None and request.get("id") == approval_id and approval_turn:
        counts["answers"] += 1
        emit({"method": "serverRequest/resolved", "params": {"requestId": approval_id}})
        turn, approval_turn = approval_turn, None
        complete(turn)
        return
    elif method == "fixture/stats":
        result = counts
    elif method == "fixture/large":
        result = "x" * (2 * 1024 * 1024)
    elif method == "fixture/exit":
        sys.exit(7)
    elif method == "fixture/spawn_child":
        child = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(60)"],
                                 stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                                 stderr=subprocess.DEVNULL)
        result = {"pid": child.pid}
    elif method == "fixture/spawn_tool":
        result = spawn_tool()
    else:
        return
    emit({"id": request["id"], "result": result})


def bind(path):
    """Binds like Codex: an over-long path becomes a symlink to a private short socket."""
    server = socket.socket(socket.AF_UNIX)
    if len(path.encode()) <= 100:
        server.bind(path)
    else:
        directory = tempfile.mkdtemp(prefix="xf-", dir="/tmp")
        target = os.path.join(directory, "s")
        server.bind(target)
        os.symlink(target, path)
    os.chmod(path, 0o600)
    server.listen(4)
    return server


def serve(path):
    global connection, initialized
    server = bind(path)
    while True:
        stream, _ = server.accept()
        request = b""
        while b"\r\n\r\n" not in request:
            chunk = stream.recv(4096)
            if not chunk:
                break
            request += chunk
        key = next((line.split(b":", 1)[1].strip() for line in request.split(b"\r\n")
                    if line.lower().startswith(b"sec-websocket-key:")), b"")
        accept = base64.b64encode(hashlib.sha1(
            key + b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11").digest()).decode()
        stream.sendall(("HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\n"
                        f"Connection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n").encode())
        with write_lock:
            connection, initialized = stream, False
        try:
            while (text := read_message(stream)) is not None:
                handle(json.loads(text))
        except (EOFError, OSError):
            pass
        finally:
            with write_lock:
                connection = None
            stream.close()


def send_client_frame(stream, payload, opcode=1):
    """A masked frame, as a WebSocket client must send."""
    mask = os.urandom(4)
    header = bytes([0x80 | opcode])
    if len(payload) < 126:
        header += bytes([0x80 | len(payload)])
    elif len(payload) < 1 << 16:
        header += bytes([0x80 | 126]) + struct.pack(">H", len(payload))
    else:
        header += bytes([0x80 | 127]) + struct.pack(">Q", len(payload))
    stream.sendall(header + mask + bytes(b ^ mask[i % 4] for i, b in enumerate(payload)))


def tui(endpoint, thread):
    """Like the Codex 0.169 `--remote` TUI: on a lost connection it keeps retrying, then
    initializes again and resumes the same thread. Each connection is logged."""
    (home / "tui.pid").write_text(str(os.getpid()))
    # Like the Claude fixture: once its test's directory is gone, nobody is left to stop it.
    while home.exists():
        stream = socket.socket(socket.AF_UNIX)
        try:
            stream.connect(endpoint)
            key = base64.b64encode(os.urandom(16)).decode()
            stream.sendall(("GET / HTTP/1.1\r\nHost: localhost\r\nUpgrade: websocket\r\n"
                            "Connection: Upgrade\r\nSec-WebSocket-Version: 13\r\n"
                            f"Sec-WebSocket-Key: {key}\r\n\r\n").encode())
            response = b""
            while b"\r\n\r\n" not in response:
                chunk = stream.recv(1)
                if not chunk:
                    raise EOFError
                response += chunk
            if b" 101 " not in response.split(b"\r\n", 1)[0]:
                raise EOFError

            def call(request_id, method, params):
                send_client_frame(stream, json.dumps(
                    {"id": request_id, "method": method, "params": params}).encode())
                while True:
                    message = json.loads(read_message(stream))
                    if message.get("id") == request_id and "method" not in message:
                        return message
            call(1, "initialize", {})
            send_client_frame(stream, json.dumps({"method": "initialized"}).encode())
            if thread is None:
                thread = call(2, "thread/start", {})["result"]["thread"]["id"]
            else:
                call(2, "thread/resume", {"threadId": thread})
            with (home / "tui-connections.txt").open("a") as log:
                log.write(f"{thread}\n")
            while read_message(stream) is not None:
                pass
        except (EOFError, OSError, ValueError, KeyError, TypeError):
            pass
        finally:
            stream.close()
        time.sleep(0.2)
    sys.exit(0)


if TUI:
    remote = sys.argv[sys.argv.index("--remote") + 1]
    resume = sys.argv[sys.argv.index("resume") + 1] if "resume" in sys.argv else None
    tui(remote[len("unix://"):], resume)

listen = None
if "--listen" in sys.argv:
    endpoint = sys.argv[sys.argv.index("--listen") + 1]
    if endpoint.startswith("unix://"):
        listen = endpoint[len("unix://"):]
if listen is None:
    for line in sys.stdin:
        handle(json.loads(line))
else:
    serve(listen)

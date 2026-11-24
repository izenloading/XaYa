"""Real Codex TUI and same-connection control over a Unix WebSocket proxy."""

import asyncio
import contextlib
import fcntl
import json
import os
from pathlib import Path
import pty
import re
import signal
import struct
import subprocess
import sys
import tempfile
import termios
import time

from websockets.asyncio.client import unix_connect
from websockets.asyncio.server import unix_serve
from websockets.exceptions import ConnectionClosed

from model_fixture import ModelFixture
from acceptance import OscTracker, match_boundaries


async def until(predicate, label, timeout=25):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        value = predicate()
        if value:
            return value
        await asyncio.sleep(0.05)
    raise TimeoutError(label)


class Rpc:
    def __init__(self, connection, downstream=None):
        self.connection = connection
        self.downstream = downstream
        self.pending = {}
        self.events = []
        self.sequence = 0
        self.tui_requests = {}
        self.thread_id = None
        self.approval_decision = None
        self.approvals = []
        self.approval_states = {}
        self.resolved = set()
        self.reader = asyncio.create_task(self.read())

    async def read(self):
        failure = EOFError("RPC disconnected")
        try:
            async for raw in self.connection:
                message = json.loads(raw)
                self.events.append({**message, "received_at": time.monotonic()})
                request_id = message.get("id")
                if "method" not in message and request_id in self.pending:
                    future = self.pending.pop(request_id)
                    if "error" in message:
                        future.set_exception(RuntimeError(str(message["error"])))
                    else:
                        future.set_result(message["result"])
                    continue
                if message.get("method", "").endswith("/requestApproval"):
                    self.approvals.append(message)
                    self.approval_states[request_id] = "pending"
                    if self.approval_decision is not None:
                        await self.respond(request_id, self.approval_decision)
                        continue
                if message.get("method") == "serverRequest/resolved":
                    self.resolved.add(message["params"]["requestId"])
                    self.approval_states[message["params"]["requestId"]] = "resolved"
                method = self.tui_requests.pop(request_id, None) if "method" not in message else None
                if method in ("thread/start", "thread/resume") and "result" in message:
                    self.thread_id = message["result"]["thread"]["id"]
                if self.downstream:
                    await self.downstream.send(raw)
        except asyncio.CancelledError:
            raise
        except Exception as error:
            failure = error
        finally:
            for future in self.pending.values():
                if not future.done():
                    future.set_exception(failure)
            self.pending.clear()

    async def respond(self, request_id, decision):
        if self.approval_states.get(request_id) != "pending":
            return False
        # Claim synchronously before sending: TUI and proxy replies share this event loop.
        self.approval_states[request_id] = "proxy_decided"
        await self.connection.send(json.dumps({"id": request_id, "result": {"decision": decision}}))
        return True

    async def forward(self, raw):
        message = json.loads(raw)
        request_id = message.get("id")
        if "method" in message and request_id is not None:
            if isinstance(request_id, str) and request_id.startswith("xaya-probe:"):
                raise ValueError("TUI used reserved proxy request prefix")
            self.tui_requests[request_id] = message["method"]
        elif request_id in self.approval_states:
            if self.approval_states[request_id] != "pending":
                return False
            self.approval_states[request_id] = "tui_decided"
        await self.connection.send(raw)
        return True

    async def call(self, method, params, timeout=25):
        self.sequence += 1
        request_id = f"xaya-probe:{self.sequence}"
        future = asyncio.get_running_loop().create_future()
        self.pending[request_id] = future
        try:
            await self.connection.send(json.dumps({"id": request_id, "method": method, "params": params}))
            return await asyncio.wait_for(future, timeout)
        finally:
            self.pending.pop(request_id, None)

    async def close(self):
        await self.connection.close()
        await self.reader


class Terminal:
    def __init__(self, command, env, cwd, log):
        self.master, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 45, 140, 0, 0))

        try:
            helper = Path(__file__).with_name("pty_exec.py")
            self.process = subprocess.Popen([sys.executable, str(helper), *command], env=env,
                                            cwd=cwd, stdin=slave, stdout=slave, stderr=slave,
                                            start_new_session=True)
        except BaseException:
            os.close(self.master)
            raise
        finally:
            os.close(slave)
        os.set_blocking(self.master, False)
        self.output = bytearray()
        self.osc = OscTracker()
        self.tail = b""
        self.log = open(log, "wb")
        asyncio.get_running_loop().add_reader(self.master, self.read)

    def read(self):
        try:
            chunk = os.read(self.master, 65536)
        except OSError:
            chunk = b""
        if not chunk:
            asyncio.get_running_loop().remove_reader(self.master)
            return
        self.output.extend(chunk)
        for reply in self.osc.feed(chunk, time.monotonic()):
            os.write(self.master, reply)
        self.log.write(chunk)
        self.log.flush()
        data = self.tail + chunk
        replies = ((b"\x1b[6n", b"\x1b[1;1R"), (b"\x1b[c", b"\x1b[?62;22c"),
                   (b"\x1b[0c", b"\x1b[?62;22c"))
        for query, reply in replies:
            # Only queries ending in the new chunk, including split queries.
            for match in re.finditer(re.escape(query), data):
                if match.end() > len(self.tail):
                    os.write(self.master, reply)
        self.tail = data[-16:]

    def contains(self, text, since=0):
        # CSI and OSC (including animated titles) can separate streamed text chunks.
        clean = re.sub(rb"\x1b\[[0-?]*[ -/]*[@-~]", b"", bytes(self.output[since:]))
        clean = re.sub(rb"\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)", b"", clean)
        return text.encode() in clean

    def write(self, text):
        os.write(self.master, text.encode())

    async def close(self):
        if self.process.poll() is None:
            with contextlib.suppress(ProcessLookupError):
                os.killpg(self.process.pid, signal.SIGTERM)
        try:
            await asyncio.to_thread(self.process.wait, 3)
        except subprocess.TimeoutExpired:
            with contextlib.suppress(ProcessLookupError):
                os.killpg(self.process.pid, signal.SIGKILL)
            await asyncio.to_thread(self.process.wait, 3)
        asyncio.get_running_loop().remove_reader(self.master)
        os.close(self.master)
        self.log.close()


def completed(rpc, turn_id, status="completed"):
    return next((event for event in rpc.events if event.get("method") == "turn/completed"
                 and event["params"]["turn"]["id"] == turn_id
                 and event["params"]["turn"]["status"] == status), None)


async def start_turn(rpc, thread_id, prompt):
    return await rpc.call("turn/start", {"threadId": thread_id,
        "input": [{"type": "text", "text": prompt, "text_elements": []}]})


async def connect(path):
    return await unix_connect(str(path), max_size=None, compression=None,
                              open_timeout=5, close_timeout=2)


async def initialize(rpc):
    await rpc.call("initialize", {"clientInfo": {"name": "xaya_probe", "version": "0.2.0"},
                                  "capabilities": {"experimentalApi": True}})
    await rpc.connection.send(json.dumps({"method": "initialized", "params": {}}))


async def dual_probe(binary, env, root, checks):
    """Update checks as stages complete; retain partial results on later failure."""
    workspace = root / "dual-workspace"
    workspace.mkdir()
    terminal = backend = observer = None
    proxies = []
    fixture = ModelFixture()
    with fixture, tempfile.TemporaryDirectory(prefix="xd-", dir="/tmp") as sockets:
        config = Path(env["CODEX_HOME"]) / "config.toml"
        config.write_text(
            'model="gpt-5.4"\nmodel_provider="probe"\ncheck_for_update_on_startup=false\n'
            'approval_policy="on-request"\nsandbox_mode="read-only"\n'
            '[model_providers.probe]\nname="Local fixture"\n'
            f'base_url="{fixture.base_url}"\nwire_api="responses"\nrequires_openai_auth=false\n'
            f'[projects.{json.dumps(str(workspace))}]\ntrust_level="trusted"\n'
            '[analytics]\nenabled=false\n[feedback]\nenabled=false\n')
        backend_path = Path(sockets) / "backend.sock"
        proxy_path = Path(sockets) / "proxy.sock"
        backend_log = open(root / "dual-backend.log", "w")

        def launch():
            return subprocess.Popen([binary, "app-server", "--listen", f"unix://{backend_path}"],
                                    cwd=workspace, env=env, stdin=subprocess.DEVNULL,
                                    stdout=backend_log, stderr=backend_log, start_new_session=True)

        async def stop_backend():
            if backend:
                with contextlib.suppress(ProcessLookupError):
                    os.killpg(backend.pid, signal.SIGTERM)
                try:
                    await asyncio.to_thread(backend.wait, 3)
                except subprocess.TimeoutExpired:
                    os.killpg(backend.pid, signal.SIGKILL)
                    await asyncio.to_thread(backend.wait, 3)

        async def proxy(downstream):
            rpc = None
            try:
                rpc = Rpc(await connect(backend_path), downstream)
                proxies.append(rpc)
                async for raw in downstream:
                    await rpc.forward(raw)
            except ConnectionClosed:
                pass  # Expected when the native terminal is terminated during cleanup.
            finally:
                if rpc:
                    await rpc.close()

        try:
            backend = launch()
            await until(backend_path.exists, "backend Unix listener")
            observer = Rpc(await connect(backend_path))
            await initialize(observer)
            started = await observer.call("thread/start", {"cwd": str(workspace), "model": "gpt-5.4",
                "modelProvider": "probe", "approvalPolicy": "on-request", "sandbox": "read-only"})
            thread_id = started["thread"]["id"]
            bootstrap = await start_turn(observer, thread_id, "BOOTSTRAP")
            await until(lambda: completed(observer, bootstrap["turn"]["id"]), "bootstrap completion")
            await observer.close()
            observer = None
            async with unix_serve(proxy, path=str(proxy_path), max_size=None, compression=None,
                                  close_timeout=2):
                terminal = Terminal([binary, "--remote", f"unix://{proxy_path}", "--no-alt-screen",
                                     "resume", thread_id, "TERMINAL_FIRST"], env, workspace, root / "tui.bin")
                rpc = await until(lambda: next((r for r in proxies if r.thread_id == thread_id), None),
                                  "TUI resume response with main thread ID")
                await until(lambda: terminal.contains("ACK_TERMINAL_FIRST_COMPLETE"), "TUI first response")
                await until(lambda: any(e.get("method") == "turn/completed" and
                            e["params"].get("threadId") == thread_id for e in rpc.events),
                            "TUI first turn completion")
                checks["C1"] = {"status": "passed", "evidence": {
                    "transport": "Unix WebSocket", "native_tui": True, "thread_id": rpc.thread_id}}
                await asyncio.sleep(0.3)  # Allow the native terminal to render its settled state.
                injected = await start_turn(rpc, thread_id, "INJECT_SECOND")
                await until(lambda: completed(rpc, injected["turn"]["id"]), "injected turn completion")
                await until(lambda: terminal.contains("ACK_INJECT_SECOND_COMPLETE"), "injected result in TUI")
                await asyncio.sleep(0.3)
                terminal.write("TERMINAL_THIRD")
                await asyncio.sleep(0.3)
                terminal.write("\r")
                await until(lambda: terminal.contains("ACK_TERMINAL_THIRD_COMPLETE"), "return to TUI input")
                await until(lambda: len([e for e in rpc.events if e.get("method") == "turn/completed"
                            and e["params"].get("threadId") == thread_id]) >= 3,
                            "third turn completion")
                checks["C2"] = {"status": "passed", "evidence": {
                    "injection_connection": "same as TUI", "native_input_after_injection": True,
                    "main_thread_from_tui_response": True}}
                await asyncio.sleep(0.3)
                native_turns = []
                for event in rpc.events:
                    if event.get("method") == "turn/started" and event["params"].get("threadId") == thread_id:
                        turn_id = event["params"]["turn"]["id"]
                        finish = next(e for e in rpc.events if e.get("method") == "turn/completed"
                            and e["params"]["turn"]["id"] == turn_id)
                        native_turns.append({"id": turn_id, "start": event["received_at"], "end": finish["received_at"]})
                e1 = checks["E1"]
                if e1["status"] == "not_run":
                    e1.update(status="partial", evidence={})
                try:
                    e1["evidence"]["codex"] = {"status": "passed",
                        "matches": match_boundaries(terminal.osc, native_turns)}
                except ValueError as error:
                    e1["evidence"]["codex"] = {"status": "failed", "error": str(error)}
                (root / "codex-osc.json").write_text(json.dumps({"reports": terminal.osc.reports,
                    "cycles": terminal.osc.cycles, "native_turns": native_turns}, indent=2))

                approval_evidence = []
                checks["C3"] = {"status": "partial", "evidence": approval_evidence}
                for marker, decision, kind in (("COMMAND_ACCEPT", "accept", "commandExecution"),
                    ("COMMAND_DENY", "decline", "commandExecution"),
                    ("FILE_ACCEPT", "accept", "fileChange"), ("FILE_DENY", "decline", "fileChange")):
                    rpc.approval_decision = decision
                    turn = await start_turn(rpc, thread_id, marker)
                    turn_id = turn["turn"]["id"]
                    approval = await until(lambda: next((a for a in rpc.approvals
                        if a["params"].get("turnId") == turn_id), None), f"{marker} approval")
                    await until(lambda: completed(rpc, turn_id), f"{marker} completed")
                    item = await until(lambda: next((e["params"]["item"] for e in rpc.events
                        if e.get("method") == "item/completed" and e["params"].get("turnId") == turn_id
                        and e["params"]["item"].get("type") == kind), None), f"{marker} item")
                    if decision == "accept" and item["status"] != "completed":
                        raise RuntimeError(f"Accepted {kind} did not complete: {item}")
                    if decision == "decline" and item["status"] != "declined":
                        raise RuntimeError(f"Denied {kind} was not declined: {item}")
                    if marker == "COMMAND_ACCEPT" and (item.get("exitCode") != 0 or
                        "XAYA_COMMAND_ACCEPT" not in (item.get("aggregatedOutput") or "")):
                        raise RuntimeError(f"Command output mismatch: {item}")
                    if marker == "COMMAND_DENY" and "XAYA_COMMAND_DENY" in (item.get("aggregatedOutput") or ""):
                        raise RuntimeError("Denied command was executed")
                    if marker == "FILE_ACCEPT" and (workspace / "accepted.txt").read_text() != "XAYA_FILE_ACCEPT\n":
                        raise RuntimeError("Accepted patch content mismatch")
                    if marker == "FILE_DENY" and (workspace / "denied.txt").exists():
                        raise RuntimeError("Denied patch was written")
                    await until(lambda: approval["id"] in rpc.resolved, f"{marker} request resolved")
                    approval_evidence.append({"marker": marker, "method": approval["method"],
                                              "decision": decision, "status": item["status"]})

                rpc.approval_decision = None
                for winner in ("tui", "proxy"):
                    prompt_offset = len(terminal.output)
                    turn = await start_turn(rpc, thread_id, f"COMMAND_ACCEPT approval winner {winner}")
                    turn_id = turn["turn"]["id"]
                    approval = await until(lambda: next((a for a in rpc.approvals
                        if a["params"].get("turnId") == turn_id), None), f"{winner} approval forwarded")
                    await until(lambda: terminal.contains("Would you like to run", prompt_offset),
                                f"{winner} native approval prompt visible")
                    if winner == "tui":
                        terminal.write("\r")
                    else:
                        if not await rpc.respond(approval["id"], "decline"):
                            raise RuntimeError("Proxy approval was already resolved")
                    await until(lambda: completed(rpc, turn_id), f"{winner} approval completion")
                    await until(lambda: approval["id"] in rpc.resolved, f"{winner} approval resolved")
                    result_item = await until(lambda: next((e["params"]["item"] for e in rpc.events
                        if e.get("method") == "item/completed" and e["params"].get("turnId") == turn_id
                        and e["params"]["item"].get("type") == "commandExecution"), None),
                        f"{winner} command result")
                    expected_status = "completed" if winner == "tui" else "declined"
                    if result_item["status"] != expected_status:
                        raise RuntimeError(f"{winner} approval produced {result_item['status']}")
                    if await rpc.respond(approval["id"], "accept"):
                        raise RuntimeError("Expired approval accepted a duplicate reply")
                    if await rpc.forward(json.dumps({"id": approval["id"], "result": {"decision": "accept"}})):
                        raise RuntimeError("Stale TUI approval was forwarded")
                    approval_evidence.append({"forwarded_to_tui": True, "winner": winner,
                                              "resolved": True, "late_replies_rejected": True})
                terminal.write("FINAL_NATIVE")
                await asyncio.sleep(0.3)
                terminal.write("\r")
                await until(lambda: terminal.contains("ACK_FINAL_NATIVE_COMPLETE"),
                            "native prompt usable after proxy approval")
                await until(lambda: any(e.get("method") == "turn/completed" and
                    e["params"].get("threadId") == thread_id and
                    e["params"]["turn"].get("status") == "completed" and
                    any(item.get("text") == "ACK_FINAL_NATIVE_COMPLETE"
                        for item in e["params"]["turn"].get("items", [])) for e in rpc.events),
                    "final native turn completed")
                checks["C3"] = {"status": "passed", "evidence": approval_evidence}
                if terminal.process.poll() is not None:
                    raise RuntimeError("Native TUI exited before interruption test")
                slow = await start_turn(rpc, thread_id, "SLOW_CANCEL")
                slow_id = slow["turn"]["id"]
                await until(fixture.slow_started.is_set, "held model stream")
                await until(lambda: any(e.get("method") == "item/agentMessage/delta"
                    and e["params"].get("turnId") == slow_id for e in rpc.events), "streaming delta")
                await rpc.call("turn/interrupt", {"threadId": thread_id, "turnId": slow_id})
                await until(lambda: completed(rpc, slow_id, "interrupted"), "interruption acknowledged")
                fixture.release.set()
                checks["C4"] = {"status": "partial", "evidence": {"interrupt_acknowledged": True}}
                await terminal.close()
                terminal = None
            await stop_backend()
            backend_path.unlink(missing_ok=True)
            backend = launch()
            await until(backend_path.exists, "restarted backend")
            observer = Rpc(await connect(backend_path))
            await initialize(observer)
            resumed = await observer.call("thread/resume", {"threadId": thread_id})
            if resumed["thread"]["id"] != thread_id:
                raise RuntimeError("Restart resumed a different thread")
            after = await start_turn(observer, thread_id, "AFTER_RESTART")
            await until(lambda: completed(observer, after["turn"]["id"]), "post-restart turn")
            history = await observer.call("thread/read", {"threadId": thread_id, "includeTurns": True})
            prompts = [part["text"] for turn in history["thread"]["turns"]
                       for item in turn["items"] if item["type"] == "userMessage"
                       for part in item["content"] if part["type"] == "text"]
            expected = ["BOOTSTRAP", "TERMINAL_FIRST", "INJECT_SECOND", "TERMINAL_THIRD",
                        "COMMAND_ACCEPT", "COMMAND_DENY", "FILE_ACCEPT", "FILE_DENY",
                        "COMMAND_ACCEPT approval winner tui", "COMMAND_ACCEPT approval winner proxy",
                        "FINAL_NATIVE", "SLOW_CANCEL", "AFTER_RESTART"]
            if prompts != expected:
                raise RuntimeError(f"Conversation history was not preserved: {prompts}")
            checks["C4"] = {"status": "passed", "evidence": {
                "interrupt_acknowledged": True, "backend_restarted": True,
                "same_thread_resumed": True, "post_restart_turn_completed": True,
                "all_13_user_messages_preserved": True}}
            if fixture.errors:
                raise RuntimeError(f"Fixture errors: {fixture.errors}")
        finally:
            if terminal:
                await terminal.close()
            if observer:
                await observer.close()
            for rpc in proxies:
                with contextlib.suppress(Exception):
                    await rpc.close()
            await stop_backend()
            backend_log.close()
            (root / "dual-events.json").write_text(json.dumps([r.events for r in proxies], indent=2))
            (root / "model-requests.json").write_text(json.dumps(fixture.requests, indent=2))

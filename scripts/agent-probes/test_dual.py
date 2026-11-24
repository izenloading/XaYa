"""Local model and proxy regression coverage; no installed Codex required."""

import asyncio
import json
import tempfile
from pathlib import Path
import unittest
from urllib.request import Request, urlopen

from model_fixture import ModelFixture

try:
    from codex_dual import Rpc, Terminal, connect
    from websockets.asyncio.server import unix_serve
except ModuleNotFoundError as error:
    if error.name != "websockets":
        raise
    Rpc = None


class FixtureTests(unittest.TestCase):
    def test_fixture_emits_stream_and_tool_output_without_repeating_tool(self):
        with ModelFixture() as fixture:
            def request(items):
                body = {"model": "gpt-5.4", "input": items}
                req = Request(fixture.base_url + "/responses", json.dumps(body).encode(),
                              {"Content-Type": "application/json"})
                with urlopen(req, timeout=3) as response:
                    return [json.loads(line[6:]) for line in response.read().decode().splitlines()
                            if line.startswith("data: ")]
            user = {"role": "user", "content": [{"type": "input_text", "text": "COMMAND_ACCEPT"}]}
            first = request([user])
            self.assertEqual(first[-1]["response"]["output"][0]["name"], "exec_command")
            second = request([user, {"type": "function_call_output", "call_id": "one", "output": "ok"}])
            self.assertTrue(any(e["type"] == "response.output_text.delta" for e in second))
            item = second[-1]["response"]["output"][0]
            self.assertEqual(item["type"], "message")
            self.assertEqual(item["content"][0]["text"], "ACK_COMMAND_ACCEPT_COMPLETE")
            self.assertFalse(fixture.errors)

    def test_file_fixture_targets_only_known_relative_names(self):
        with ModelFixture() as fixture:
            req = Request(fixture.base_url + "/responses", json.dumps({"input": [
                {"role": "user", "content": "FILE_DENY"}], "tools": [
                {"name": "apply_patch", "type": "custom"}]}).encode())
            with urlopen(req, timeout=3) as response:
                output = response.read().decode()
            self.assertIn("denied.txt", output)
            self.assertNotIn("accepted.txt", output)
            self.assertIn("custom_tool_call", output)


@unittest.skipUnless(Rpc, "optional websockets dependency not installed")
class ProxyTests(unittest.IsolatedAsyncioTestCase):
    def test_terminal_evidence_ignores_csi_and_osc_between_streamed_chunks(self):
        terminal = Terminal.__new__(Terminal)
        for ending in (b"\x07", b"\x1b\\"):
            terminal.output = bytearray(b"ACK_INJECT\x1b]0;hidden-title" + ending
                                        + b"\x1b[32m_SECOND_COMPLETE")
            self.assertTrue(terminal.contains("ACK_INJECT_SECOND_COMPLETE"))
            self.assertFalse(terminal.contains("hidden-title"))

    async def test_first_approval_answer_wins_and_late_answers_are_dropped(self):
        class Connection:
            def __init__(self):
                self.sent = []

            async def send(self, raw):
                await asyncio.sleep(0)  # Expose the race between claim and write.
                self.sent.append(json.loads(raw))

        for winner in ("proxy", "tui"):
            rpc = Rpc.__new__(Rpc)
            rpc.connection = Connection()
            rpc.approval_states = {42: "pending"}
            rpc.tui_requests = {}
            proxy_answer = rpc.respond(42, "decline")
            tui_answer = rpc.forward(json.dumps({"id": 42, "result": {"decision": "accept"}}))
            results = await asyncio.gather(*( (proxy_answer, tui_answer) if winner == "proxy"
                                             else (tui_answer, proxy_answer)))
            self.assertEqual(results, [True, False])
            self.assertEqual(len(rpc.connection.sent), 1)
            expected = "decline" if winner == "proxy" else "accept"
            self.assertEqual(rpc.connection.sent[0]["result"]["decision"], expected)
            rpc.approval_states[42] = "resolved"
            self.assertFalse(await rpc.respond(42, "accept"))

    async def test_own_responses_are_hidden_but_tui_responses_and_large_events_forward(self):
        with tempfile.TemporaryDirectory(prefix="xt-", dir="/tmp") as directory:
            endpoint = Path(directory) / "s"
            payload = "x" * (2 * 1024 * 1024)
            async def backend(connection):
                async for raw in connection:
                    message = json.loads(raw)
                    await connection.send(json.dumps({"id": message["id"], "result": {"ok": True}}))
                    await connection.send(json.dumps({"method": "large/event", "params": payload}))

            class Downstream:
                def __init__(self):
                    self.messages = []

                async def send(self, raw):
                    self.messages.append(json.loads(raw))

            async with unix_serve(backend, path=str(endpoint), max_size=None, compression=None):
                downstream = Downstream()
                rpc = Rpc(await connect(endpoint), downstream)
                try:
                    result = await rpc.call("test", {})
                    self.assertEqual(result, {"ok": True})
                    await rpc.connection.send(json.dumps({"id": 7, "method": "test"}))
                    async with asyncio.timeout(3):
                        while len(downstream.messages) < 3:
                            await asyncio.sleep(0.01)
                    self.assertFalse(any(m.get("id") == "xaya-probe:1" for m in downstream.messages))
                    self.assertTrue(any(m.get("id") == 7 for m in downstream.messages))
                    self.assertTrue(any(m.get("params") == payload for m in downstream.messages))
                finally:
                    await rpc.close()

    async def test_disconnect_fails_inflight_request(self):
        with tempfile.TemporaryDirectory(prefix="xt-", dir="/tmp") as directory:
            endpoint = Path(directory) / "s"
            async def backend(connection):
                await connection.recv()
                await connection.close()
            async with unix_serve(backend, path=str(endpoint)):
                rpc = Rpc(await connect(endpoint))
                try:
                    with self.assertRaises(EOFError):
                        await rpc.call("test", {}, timeout=2)
                finally:
                    await rpc.close()


if __name__ == "__main__":
    unittest.main()

"""Bridge guarantees for isolated Claude Mods and permission hooks."""

import json
from pathlib import Path
import tempfile
import threading
import unittest
from unittest.mock import AsyncMock, patch
from urllib.error import HTTPError
from urllib.request import Request, urlopen

from claude_bridge import ClaudeBridge
from claude_fixture import ClaudeFixture

try:
    from claude_mods import daemon_reply
except ModuleNotFoundError as error:
    if error.name != "websockets":
        raise
    daemon_reply = None


class BridgeTests(unittest.TestCase):
    def post(self, bridge, route, value, token=None):
        req = Request(bridge.endpoint + route, json.dumps(value).encode(),
                      {"Content-Type": "application/json", "X-Probe-Token": token or bridge.token})
        with urlopen(req, timeout=3) as response:
            return json.load(response)

    def test_rejects_wrong_token_and_delivers_command_once(self):
        with ClaudeBridge() as bridge:
            with self.assertRaises(HTTPError) as caught:
                self.post(bridge, "/poll", {}, "wrong")
            self.assertEqual(caught.exception.code, 403)
            caught.exception.close()
            identifier = bridge.enqueue({"type": "messages"})
            self.assertEqual(self.post(bridge, "/poll", {})["command"]["id"], identifier)
            self.assertIsNone(self.post(bridge, "/poll", {})["command"])

    def test_permission_first_answer_wins_and_expired_answer_is_rejected(self):
        with ClaudeBridge() as bridge:
            result = []
            thread = threading.Thread(target=lambda: result.append(self.post(bridge, "/hooks", {
                "hook_event_name": "PermissionRequest", "session_id": "native", "tool_use_id": "use"})))
            thread.start()
            self.assertTrue(bridge.permission_ready.wait(2))
            identifier = bridge.permissions[0]["id"]
            self.assertTrue(bridge.answer(identifier, "allow"))
            self.assertFalse(bridge.answer(identifier, "deny"))
            thread.join(3)
            self.assertFalse(thread.is_alive())
            self.assertEqual(result[0]["hookSpecificOutput"]["decision"]["behavior"], "allow")

    def test_native_turn_completion_expires_pending_http_hook(self):
        with ClaudeBridge() as bridge:
            result = []
            thread = threading.Thread(target=lambda: result.append(self.post(bridge, "/hooks", {
                "hook_event_name": "PermissionRequest", "session_id": "native"})))
            thread.start()
            self.assertTrue(bridge.permission_ready.wait(2))
            self.post(bridge, "/events", {"sequence": 1, "sessionId": "native",
                "type": "turn.complete", "data": {"turnId": "turn"}})
            thread.join(3)
            self.assertEqual(result, [{}])
            self.assertFalse(bridge.answer(bridge.permissions[0]["id"], "deny"))

    def test_tool_fixture_emits_fixed_write_once_then_text(self):
        with tempfile.TemporaryDirectory() as directory, ClaudeFixture(Path(directory)) as fixture:
            body = {"messages": [{"role": "user", "content": [
                {"type": "text", "text": "XAYA_CLAUDE_ALLOW"}]}]}
            def call(value):
                req = Request(fixture.base_url + "/v1/messages", json.dumps(value).encode())
                with urlopen(req, timeout=3) as response:
                    return json.load(response)
            tool = call(body)["content"][0]
            self.assertEqual(tool["name"], "Write")
            self.assertEqual(tool["input"]["file_path"], str(Path(directory) / "XAYA_CLAUDE_ALLOW.txt"))
            body["messages"].extend([{"role": "assistant", "content": [tool]}, {"role": "user",
                "content": [{"type": "tool_result", "tool_use_id": tool["id"], "content": "written"}]}])
            self.assertEqual(call(body)["content"][0]["type"], "text")


@unittest.skipUnless(daemon_reply, "optional websockets dependency not installed")
class PrivateDaemonTests(unittest.IsolatedAsyncioTestCase):
    async def test_wrong_daemon_directory_is_rejected_before_connect_or_key_read(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            with patch("claude_mods.run_cli", AsyncMock(return_value="sock dir: /tmp/unrelated")), \
                    patch("asyncio.open_unix_connection", AsyncMock()) as connect:
                with self.assertRaisesRegex(RuntimeError, "isolated Claude daemon endpoint"):
                    await daemon_reply("claude", {}, root, root / "config", root, "job", "fixed")
                connect.assert_not_awaited()


if __name__ == "__main__":
    unittest.main()

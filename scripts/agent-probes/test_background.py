"""Background-probe regression checks without real engines or credentials."""

import json
from pathlib import Path
import subprocess
import sys
import unittest
from urllib.request import Request, urlopen

from claude_fixture import ClaudeFixture, has_native_prompt

try:
    from claude_background import run_cli, session_id
except ModuleNotFoundError as error:
    if error.name != "websockets":
        raise
    run_cli = None


class ClaudeFixtureTests(unittest.TestCase):
    def test_auxiliary_classification_is_not_the_main_conversation(self):
        actual = {"messages": [{"role": "user", "content": [
            {"type": "text", "text": "XAYA_CLAUDE_REATTACHED"}]}]}
        auxiliary = {"messages": [{"role": "user",
            "content": 'Most recent ask: "XAYA_CLAUDE_REATTACHED"'}]}
        self.assertTrue(has_native_prompt(actual, "XAYA_CLAUDE_REATTACHED"))
        self.assertFalse(has_native_prompt(auxiliary, "XAYA_CLAUDE_REATTACHED"))
        with ClaudeFixture() as fixture:
            body = {**auxiliary, "system": "THE FOUR STATES; output state, detail, tempo"}
            with urlopen(Request(fixture.base_url + "/v1/messages", json.dumps(body).encode()), timeout=3) as response:
                output = json.load(response)
            self.assertEqual(json.loads(output["content"][0]["text"])["state"], "done")

    def test_messages_json_sse_and_count_tokens(self):
        with ClaudeFixture() as fixture:
            def request(path, value):
                req = Request(fixture.base_url + path, json.dumps(value).encode(),
                              {"Content-Type": "application/json"})
                with urlopen(req, timeout=3) as response:
                    return response.read().decode()
            body = {"model": "fixture", "messages": [{"role": "user", "content": "XAYA_CLAUDE_BASELINE"}]}
            direct = json.loads(request("/v1/messages", body))
            self.assertEqual(direct["content"][0]["text"], "ACK_XAYA_CLAUDE_BASELINE_COMPLETE")
            streamed = request("/v1/messages", {**body, "stream": True})
            events = [json.loads(line[6:]) for line in streamed.splitlines() if line.startswith("data: ")]
            self.assertEqual(events[0]["type"], "message_start")
            self.assertEqual(events[-1]["type"], "message_stop")
            self.assertEqual(json.loads(request("/v1/messages/count_tokens", body)), {"input_tokens": 10})
            self.assertEqual(len(fixture.requests), 2)
            self.assertFalse(fixture.errors)

    def test_latest_history_marker_determines_reply(self):
        with ClaudeFixture() as fixture:
            body = {"messages": [{"role": "user", "content": "XAYA_CLAUDE_BACKGROUND"},
                                  {"role": "user", "content": "XAYA_CLAUDE_ATTACHED"}]}
            with urlopen(Request(fixture.base_url + "/v1/messages", json.dumps(body).encode()), timeout=3) as response:
                value = json.load(response)
            self.assertEqual(value["content"][0]["text"], "ACK_XAYA_CLAUDE_ATTACHED_COMPLETE")


@unittest.skipUnless(run_cli, "optional websockets dependency not installed")
class BackgroundTests(unittest.IsolatedAsyncioTestCase):
    def test_native_session_id_is_distinct_from_job_id(self):
        self.assertEqual(session_id({"id": "1234", "sessionId": "uuid"}), "uuid")
        self.assertEqual(session_id({"id": "1234", "session_id": "uuid"}), "uuid")
        self.assertIsNone(session_id({"id": "1234"}))

    async def test_cli_failure_is_not_a_successful_probe(self):
        with self.assertRaisesRegex(RuntimeError, "fixture failure"):
            await run_cli(sys.executable, {}, Path.cwd(), "-c",
                          "import sys; sys.stderr.write('fixture failure'); sys.exit(3)")

    async def test_cli_timeout_is_bounded(self):
        with self.assertRaises(subprocess.TimeoutExpired):
            await run_cli(sys.executable, {}, Path.cwd(), "-c", "import time; time.sleep(10)", timeout=0.05)


if __name__ == "__main__":
    unittest.main()

"""Failure-path coverage for probes; no installed engine or GUI required."""

import os
import json
from pathlib import Path
import queue
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

from run import Server, git_probe, isolated_env


class ProbeTests(unittest.TestCase):
    def rpc(self, messages):
        server = Server.__new__(Server)
        server.sequence = 0
        server.messages = queue.Queue()
        for message in messages:
            server.messages.put(message)
        server.send = lambda value: None
        return server

    def test_notifications_do_not_replace_matching_response(self):
        server = self.rpc([{"method": "thread/started"}, {"id": "other", "result": {}},
                           {"id": "xaya-probe-1", "result": {"thread": "main"}}])
        self.assertEqual(server.call("thread/start", {}), {"thread": "main"})

    def test_rpc_error_is_failure(self):
        server = self.rpc([{"id": "xaya-probe-1", "error": {"message": "denied"}}])
        with self.assertRaisesRegex(RuntimeError, "denied"):
            server.call("thread/start", {})

    def test_missing_response_times_out(self):
        with self.assertRaisesRegex(TimeoutError, "initialize"):
            self.rpc([]).call("initialize", {}, timeout=0.01)

    def test_eof_is_failure(self):
        with self.assertRaises(EOFError):
            self.rpc([EOFError("closed")]).call("initialize", {})

    def test_environment_does_not_inherit_credentials_or_git_overrides(self):
        with tempfile.TemporaryDirectory() as directory:
            with patch.dict(os.environ, {"OPENAI_API_KEY": "real-secret",
                                         "ANTHROPIC_AUTH_TOKEN": "real-token",
                                         "GIT_DIR": "/unrelated", "HTTPS_PROXY": "proxy"}):
                env = isolated_env(Path(directory))
            self.assertEqual(env["OPENAI_API_KEY"], "xaya-probe-fake-key")
            for key in ("ANTHROPIC_AUTH_TOKEN", "GIT_DIR", "HTTPS_PROXY"):
                self.assertNotIn(key, env)
            self.assertEqual(env["HOME"], str(Path(directory) / "home"))

    def test_git_probe_preserves_staged_content(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            result = git_probe(isolated_env(root), root, 5)
            self.assertTrue(result["user_index_unchanged"])

    def test_missing_cli_writes_failure_report_and_returns_nonzero(self):
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / "artifacts"
            result = subprocess.run(
                [sys.executable, str(Path(__file__).with_name("run.py")),
                 "--output", str(output), "--codex", str(Path(directory) / "missing"),
                 "--files", "5"], capture_output=True, text=True, timeout=20)
            self.assertEqual(result.returncode, 1, result.stderr)
            report = json.loads((output / "report.json").read_text())
            self.assertEqual(report["checks"]["C1"]["status"], "failed")
            self.assertEqual(report["checks"]["A1"]["status"], "not_run")
            self.assertTrue((output / "report.md").exists())


if __name__ == "__main__":
    unittest.main()

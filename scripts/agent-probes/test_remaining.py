"""Strict review receipts and OSC turn boundaries: failure paths first."""

import unittest
import tempfile
from pathlib import Path

from acceptance import OscTracker, match_boundaries, validate_review, verify_claude_denial
from git_benchmark import benchmark_repo
from run import git_probe, isolated_env


class RemainingTests(unittest.TestCase):
    def test_review_rejects_stale_hash_extra_fields_and_bad_verdict(self):
        valid = {"checkpoint": "a" * 40, "verdict": "approved", "findings": []}
        self.assertEqual(validate_review(valid, "a" * 40), valid)
        for bad in ({**valid, "checkpoint": "b" * 40}, {**valid, "verdict": "probably"},
                    {**valid, "hidden": True}, {**valid, "findings": "none"}):
            with self.assertRaises(ValueError):
                validate_review(bad, "a" * 40)

    def test_split_osc_probe_and_work_block_work_is_one_turn(self):
        tracker = OscTracker()
        self.assertEqual(tracker.feed(b"\x1b]750", 1), [])
        self.assertEqual(len(tracker.feed(b"1;?\x1b\\", 1)), 1)
        tracker.feed(b"\x1b]7501;state=working:app=codex\x07", 2)
        tracker.feed(b"\x1b]7501;state=blocked:kind=permission\x1b\\", 3)
        tracker.feed(b"\x1b]7501;state=working:progress=50\x07", 4)
        tracker.feed(b"\x1b]7501;state=done\x07", 5)
        self.assertEqual(tracker.cycles, [{"start": 2, "end": 5}])
        self.assertEqual(len(tracker.reports), 4)

    def test_clear_parent_does_not_clear_other_busy_tree(self):
        tracker = OscTracker()
        for time, state in enumerate(("state=working:id=a", "state=working:id=b/child",
                                      "state=clear:id=a", "state=clear:id=b")):
            tracker.feed(b"\x1b]7501;" + state.encode() + b"\x07", time)
        self.assertEqual(tracker.cycles, [{"start": 0, "end": 3}])

    def test_boundary_match_rejects_missing_and_crossed_turns(self):
        turns = [{"id": "one", "start": 1, "end": 2}, {"id": "two", "start": 3, "end": 4}]
        self.assertEqual(len(match_boundaries([{"start": 1.1, "end": 2.1}, {"start": 3.1, "end": 4.1}], turns)), 2)
        with self.assertRaises(ValueError):
            match_boundaries([{"start": 1, "end": 4}], turns)
        with self.assertRaises(ValueError):
            match_boundaries([{"start": 3, "end": 4}, {"start": 1, "end": 2}], turns)

    def test_boundary_match_rejects_extra_unfinished_osc_turn(self):
        tracker = OscTracker()
        for timestamp, state in ((1, "working"), (2, "done"), (3, "working")):
            tracker.feed(b"\x1b]7501;state=" + state.encode() + b"\x07", timestamp)
        with self.assertRaisesRegex(ValueError, "unfinished"):
            match_boundaries(tracker, [{"id": "one", "start": 1, "end": 2}])

    def test_tool_error_alone_is_not_permission_denial(self):
        unrelated_error = {"permission_denials": [], "is_error": False}
        with self.assertRaises(ValueError):
            verify_claude_denial(unrelated_error, "toolu_readonly", "/fixed/forbidden.txt")
        valid = {"permission_denials": [{"tool_name": "Write", "tool_use_id": "toolu_readonly",
                 "tool_input": {"file_path": "/fixed/forbidden.txt"}}]}
        self.assertTrue(verify_claude_denial(valid, "toolu_readonly", "/fixed/forbidden.txt"))
        with self.assertRaises(ValueError):
            verify_claude_denial(valid, "unrelated_tool", "/fixed/forbidden.txt")
        control = [{"tool_name": "Write", "tool_use_id": "toolu_readonly",
                    "input": {"file_path": "/fixed/forbidden.txt"}}]
        error = [{"tool_use_id": "toolu_readonly", "is_error": True, "content": "Invalid tool input"}]
        with self.assertRaises(ValueError):
            verify_claude_denial({}, "toolu_readonly", "/fixed/forbidden.txt", control, error)
        error[0]["content"] = "Denied by XaYa read-only policy"
        self.assertTrue(verify_claude_denial({}, "toolu_readonly", "/fixed/forbidden.txt", control, error))

    def test_real_clone_benchmark_preserves_dirty_source_index_and_files(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            env = isolated_env(root)
            git_probe(env, root, 3)
            source = root / "repository"
            before_index = (source / ".git/index").read_bytes()
            before_files = {p.name: p.read_bytes() for p in source.iterdir() if p.is_file()}
            result = benchmark_repo(source, env, root, 0)
            self.assertTrue(result["source_head_and_index_unchanged"])
            self.assertEqual((source / ".git/index").read_bytes(), before_index)
            self.assertEqual({p.name: p.read_bytes() for p in source.iterdir() if p.is_file()}, before_files)
            self.assertEqual(result["files"], 3)
            self.assertEqual(len(result["two_snapshot_seconds"]), 4)


if __name__ == "__main__":
    unittest.main()

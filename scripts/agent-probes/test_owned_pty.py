"""Process-ownership evidence for an XaYa-owned PTY, without real engines or credentials."""

import os
import subprocess
import sys
import time
import unittest

from owned_pty import (ProcessEntry, OwnershipError, descendants, ownership, parse_ps,
                       snapshot, terminate_owned)

MARKER = "XAYA_OWNER=0123456789abcdef"

SAMPLE = (
    "  100     1   100 Tue 17 Nov 09:44:48 2026     node claude --session-id x XAYA_OWNER=0123456789abcdef HOME=/h\n"
    "  101   100   100 Tue 17 Nov 09:44:49 2026     /bin/zsh -c sleep XAYA_OWNER=0123456789abcdef\n"
    "  102   101   100 Tue 17 Nov 09:44:50 2026     python tool.py XAYA_OWNER=0123456789abcdef\n"
    "  103     1   103 Tue 17 Nov 09:44:51 2026     python grandchild XAYA_OWNER=0123456789abcdef\n"
    "  104   100   100 Tue 17 Nov 09:44:51 2026     helper-with-scrubbed-env PATH=/bin\n"
    "  105     1   105 Tue 17 Nov 09:44:52 2026     unrelated XAYA_OWNER=0123456789abcdef0 SUFFIXED=1\n"
    "  106     1   106 Tue 17 Nov 09:44:52 2026     echo prefixXAYA_OWNER=0123456789abcdef\n"
)


class ParseTests(unittest.TestCase):
    def test_parses_identity_and_exact_marker_word(self):
        entries = {entry.pid: entry for entry in parse_ps(SAMPLE, MARKER)}
        self.assertEqual(entries[102], ProcessEntry(102, 101, 100, "Tue 17 Nov 09:44:50 2026",
                                                    "python tool.py XAYA_OWNER=0123456789abcdef", True))
        self.assertTrue(entries[103].marked)
        self.assertFalse(entries[104].marked)
        # A longer token or a word that merely contains the marker is not ownership evidence.
        self.assertFalse(entries[105].marked)
        self.assertFalse(entries[106].marked)

    def test_rejects_malformed_rows_instead_of_guessing(self):
        with self.assertRaises(OwnershipError):
            parse_ps("not a process row\n", MARKER)
        with self.assertRaises(OwnershipError):
            parse_ps("  100 1 100 Tue 17 Nov\n", MARKER)

    def test_rejects_weak_markers(self):
        with self.assertRaises(OwnershipError):
            parse_ps(SAMPLE, "XAYA_OWNER=short")


class OwnershipTests(unittest.TestCase):
    def test_tree_misses_reparented_process_that_marker_finds(self):
        entries = parse_ps(SAMPLE, MARKER)
        self.assertEqual(descendants(entries, 100), {101, 102, 104})
        result = ownership(entries, 100)
        self.assertEqual(result["tree"], {101, 102, 104})
        self.assertEqual(result["marked"], {101, 102, 103})
        self.assertEqual(result["reparented"], {103})
        self.assertEqual(result["unmarked_descendants"], {104})
        self.assertTrue(result["root_alive"])

    def test_root_exit_leaves_only_marked_survivors(self):
        entries = [entry for entry in parse_ps(SAMPLE, MARKER) if entry.pid not in (100, 101, 102)]
        result = ownership(entries, 100)
        self.assertFalse(result["root_alive"])
        self.assertEqual(result["tree"], set())
        self.assertEqual(result["marked"], {103})


class ProcSnapshotTests(unittest.TestCase):
    def test_linux_proc_uses_exact_environment_entries(self):
        import tempfile
        from owned_pty import proc_snapshot
        with tempfile.TemporaryDirectory() as proc:
            def make(pid, comm, ppid, pgid, start, environ, readable=True):
                base = os.path.join(proc, str(pid))
                os.mkdir(base)
                # comm may contain spaces and parentheses; fields resume after the last ')'.
                rest = ["S", str(ppid), str(pgid)] + ["0"] * 16 + [str(start)]
                with open(os.path.join(base, "stat"), "w") as file:
                    file.write(f"{pid} ({comm}) " + " ".join(rest) + "\n")
                with open(os.path.join(base, "cmdline"), "wb") as file:
                    file.write(b"python\0tool.py\0")
                path = os.path.join(base, "environ")
                with open(path, "wb") as file:
                    file.write(b"\0".join(environ) + b"\0")
                if not readable:
                    os.remove(path)
            make(200, "py (x) y", 1, 200, 9001, [b"HOME=/h", MARKER.encode()])
            make(201, "sh", 200, 200, 9002, [(MARKER + "0").encode()])
            make(202, "sh", 200, 200, 9003, [MARKER.encode()], readable=False)
            os.mkdir(os.path.join(proc, "self-not-a-pid"))
            entries = {e.pid: e for e in proc_snapshot(MARKER, proc)}
            self.assertEqual(entries[200], ProcessEntry(200, 1, 200, "9001", "python tool.py", True))
            self.assertFalse(entries[201].marked)
            self.assertFalse(entries[202].marked)
            self.assertEqual(descendants(list(entries.values()), 200), {201, 202})


class LiveProcessTests(unittest.TestCase):
    def test_marker_survives_reparenting_and_termination_checks_identity(self):
        marker = "XAYA_OWNER=" + os.urandom(16).hex()
        name, value = marker.split("=", 1)
        script = ("import os, sys, time\n"
                  "if os.fork() == 0:\n"
                  "    os.setsid()\n"
                  "    if os.fork() == 0:\n"
                  "        print(os.getpid(), flush=True)\n"
                  "        time.sleep(30)\n"
                  "    os._exit(0)\n"
                  "os.wait()\n")
        launcher = subprocess.Popen([sys.executable, "-c", script], env=dict(os.environ, **{name: value}),
                                    stdout=subprocess.PIPE, text=True)
        grandchild = int(launcher.stdout.readline())
        launcher.wait(5)
        try:
            entries = snapshot(marker)
            found = next(entry for entry in entries if entry.pid == grandchild)
            self.assertTrue(found.marked)
            self.assertEqual(found.ppid, 1)
            self.assertNotIn(grandchild, descendants(entries, os.getpid()))
            stale = ProcessEntry(found.pid, found.ppid, found.pgid, "Thu 01 Jan 00:00:00 1970",
                                 found.command, True)
            # A changed start time means PID reuse: never signal it.
            self.assertEqual(terminate_owned([stale], marker), {"killed": [], "skipped": [grandchild]})
            os.kill(grandchild, 0)
            self.assertEqual(terminate_owned([found], marker), {"killed": [grandchild], "skipped": []})
            deadline = time.monotonic() + 5
            while time.monotonic() < deadline and any(e.pid == grandchild for e in snapshot(marker)):
                time.sleep(0.05)
            self.assertFalse(any(e.pid == grandchild for e in snapshot(marker)))
        finally:
            try:
                os.kill(grandchild, 9)
            except ProcessLookupError:
                pass


if __name__ == "__main__":
    unittest.main()

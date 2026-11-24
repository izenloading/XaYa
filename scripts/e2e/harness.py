"""Shared pieces of xaya's end-to-end tests: an isolated xaya, `xaya msg`, waiting and checks.

Every xaya started here gets its own config, socket and state directory, so tests never touch the
user's session snapshot or talk to a running xaya. Shells are bash with xaya's shell integration
and a plain `$ ` prompt, so their output is predictable.
"""

import json
import os
import shutil
import signal
import subprocess
import sys
import tempfile
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
MACOS = sys.platform == "darwin"


class Checks:
    """Named pass/fail checks, printed as they're made."""

    def __init__(self, test):
        self.test = test
        self.failures = []

    def check(self, name, ok, detail=""):
        print(("  PASS " if ok else "  FAIL ") + name + ("" if ok else f": {detail}"))
        if not ok:
            self.failures.append(f"{self.test}: {name}")
        return ok


def wait_until(predicate, timeout=10.0, interval=0.2):
    """Poll `predicate` until it's truthy or `timeout` seconds pass; returns its last value."""
    deadline = time.monotonic() + timeout
    while True:
        value = predicate()
        if value or time.monotonic() >= deadline:
            return value
        time.sleep(interval)


# An agent that reports working and stays the terminal's foreground job, as a real one does: the
# reporting process is the one left running. (A shell builtin reporting before `sleep` would look
# like an agent that exited once `sleep` took the foreground.)
FAKE_AGENT = "sh -c 'printf \"\\033]7501;state=working:app=codex\\007\"; echo {marker}; exec sleep 600'"


def outside_git(path):
    """Whether `path` is outside every git repository."""
    result = subprocess.run(["git", "-C", str(path), "rev-parse", "--git-dir"],
                            capture_output=True)
    return result.returncode != 0


def fresh_dir(path):
    """`path`, emptied."""
    shutil.rmtree(path, ignore_errors=True)
    path.mkdir(parents=True)
    return path.resolve()


class Xaya:
    """A xaya process isolated in `work`, driven through `xaya msg`."""

    def __init__(self, binary, work, *, send_text=False, config="", env=None, args=()):
        self.binary = str(binary)
        work.mkdir(parents=True, exist_ok=True)
        self.work = work
        self.state = work / "state"
        self.log_path = work / "xaya.log"
        self.args = list(args)
        self.process = None

        # Unix socket paths are short; the work directory may not be.
        self.socket = os.path.join(tempfile.mkdtemp(prefix="xaya-e2e-", dir="/tmp"), "x.sock")

        rc = work / "bashrc"
        rc.write_text(f"PS1='$ '\nsource {ROOT}/extra/shell-integration/xaya.bash\n")
        self.config = work / "xaya.toml"
        self.config.write_text(
            f"[general]\nipc_send_text = {str(send_text).lower()}\n"
            "[notifications]\nenabled = false\n"
            "[terminal.shell]\nprogram = \"/bin/bash\"\n"
            f"args = [\"--noprofile\", \"--rcfile\", \"{rc}\", \"-i\"]\n" + config
        )

        self.env = dict(os.environ, XDG_STATE_HOME=str(self.state), **(env or {}))
        for var in ("ALACRITTY_WINDOW_ID", "ALACRITTY_SOCKET"):
            self.env.pop(var, None)

    def start(self):
        log = open(self.log_path, "a")
        command = [self.binary, "--config-file", str(self.config), "--socket", self.socket]
        command += ["-o", 'debug.log_level="Info"', *self.args]
        self.process = subprocess.Popen(command, env=self.env, stdout=log, stderr=subprocess.STDOUT)
        if not wait_until(lambda: self.msg("status").returncode == 0, timeout=20):
            raise RuntimeError(f"xaya didn't start; see {self.log_path}")
        return self

    def stop(self):
        if self.process and self.process.poll() is None:
            self.process.send_signal(signal.SIGTERM)
            try:
                self.process.wait(10)
            except subprocess.TimeoutExpired:
                self.process.kill()
        shutil.rmtree(os.path.dirname(self.socket), ignore_errors=True)

    def __enter__(self):
        return self.start()

    def __exit__(self, *_):
        self.stop()

    def running(self):
        return self.process is not None and self.process.poll() is None

    def msg(self, *args, timeout=30):
        command = [self.binary, "msg", "--socket", self.socket, *map(str, args)]
        return subprocess.run(command, capture_output=True, text=True, timeout=timeout,
                              env=self.env)

    def windows(self):
        result = self.msg("status", "--json")
        return json.loads(result.stdout) if result.returncode == 0 else []

    def window_ids(self):
        return [window["window_id"] for window in self.windows()]

    def text(self, window, *args):
        return self.msg("get-text", "-w", window, *args).stdout

    def log(self):
        return self.log_path.read_text(errors="replace") if self.log_path.exists() else ""

    def problems(self):
        """Panics and errors in the log."""
        return [line for line in self.log().splitlines() if "panick" in line or "[ERROR]" in line]

    def screenshot(self, name):
        """Capture the first window to `<work>/<name>.png`, without focusing it; macOS only."""
        if not MACOS:
            return None
        window = _macos_window_number(self.process.pid)
        if window is None:
            return None
        path = self.work / f"{name}.png"
        subprocess.run(["screencapture", "-x", "-o", f"-l{window}", str(path)], check=False)
        return path if path.exists() else None

    def resize(self, width, height):
        """Resize the front window through System Events; macOS only, needs Accessibility."""
        if not MACOS:
            return False
        script = [
            'tell application "System Events"',
            f"set p to first application process whose unix id is {self.process.pid}",
            f"set size of window 1 of p to {{{width}, {height}}}",
            "end tell",
        ]
        args = [arg for line in script for arg in ("-e", line)]
        return subprocess.run(["osascript", *args], capture_output=True, timeout=15).returncode == 0

    def press(self, key, *modifiers):
        """Type `key` into this xaya through System Events; macOS only, needs Accessibility.

        `key` is a character or one of KEY_CODES; `modifiers` are "command", "shift", "option" or
        "control". The window is brought to the front first, and the key is only sent while this
        process is frontmost, so it never reaches the user's own windows; returns whether it was
        sent. Start xaya from `keyable_copy`: System Events finds processes by name, so a test
        binary called `xaya` would activate the user's XaYa instead.
        """
        if not MACOS:
            return False
        pid = self.process.pid
        if key in KEY_CODES:
            stroke = f"key code {KEY_CODES[key]}"
        else:
            stroke = 'keystroke "' + key.replace("\\", "\\\\").replace('"', '\\"') + '"'
        if modifiers:
            stroke += " using {" + ", ".join(f"{m} down" for m in modifiers) + "}"
        script = f"""
tell application "System Events"
  set p to first application process whose unix id is {pid}
  repeat 20 times
    if unix id of (first application process whose frontmost is true) is {pid} then exit repeat
    set frontmost of p to true
    delay 0.2
  end repeat
  if unix id of (first application process whose frontmost is true) is not {pid} then return "not frontmost"
  {stroke}
  return "sent"
end tell"""
        result = subprocess.run(["osascript", "-e", script], capture_output=True, text=True,
                                timeout=20)
        return result.stdout.strip() == "sent"


# Keys `Xaya.press` sends by key code rather than as text.
KEY_CODES = {"return": 36, "escape": 53, "up": 126, "down": 125}


def keyable_copy(binary, directory):
    """Copies of `binary` and its `xaya-agentd` in `directory`, under a name `Xaya.press` can use.

    Returns the copy of `binary`, `xaya-e2e`; the service keeps its own name, since xaya finds it
    beside itself.
    """
    directory.mkdir(parents=True, exist_ok=True)
    copy = directory / "xaya-e2e"
    shutil.copy2(binary, copy)
    shutil.copy2(Path(binary).with_name("xaya-agentd"), directory / "xaya-agentd")
    return copy


def _macos_window_number(pid):
    """The CoreGraphics number of an on-screen window of process `pid`."""
    import ctypes as C

    cf = C.CDLL("/System/Library/Frameworks/CoreFoundation.framework/CoreFoundation")
    cg = C.CDLL("/System/Library/Frameworks/CoreGraphics.framework/CoreGraphics")
    cg.CGWindowListCopyWindowInfo.restype = C.c_void_p
    cg.CGWindowListCopyWindowInfo.argtypes = [C.c_uint32, C.c_uint32]
    cf.CFArrayGetCount.restype = C.c_long
    cf.CFArrayGetCount.argtypes = [C.c_void_p]
    cf.CFArrayGetValueAtIndex.restype = C.c_void_p
    cf.CFArrayGetValueAtIndex.argtypes = [C.c_void_p, C.c_long]
    cf.CFDictionaryGetValue.restype = C.c_void_p
    cf.CFDictionaryGetValue.argtypes = [C.c_void_p, C.c_void_p]
    cf.CFStringCreateWithCString.restype = C.c_void_p
    cf.CFStringCreateWithCString.argtypes = [C.c_void_p, C.c_char_p, C.c_uint32]
    cf.CFNumberGetValue.argtypes = [C.c_void_p, C.c_int, C.c_void_p]

    def number(entry, key):
        value = C.c_int64()
        name = cf.CFStringCreateWithCString(None, key.encode(), 0x08000100)
        cf.CFNumberGetValue(cf.CFDictionaryGetValue(entry, name), 4, C.byref(value))
        return value.value

    windows = cg.CGWindowListCopyWindowInfo(1, 0)  # On-screen windows only.
    for i in range(cf.CFArrayGetCount(windows)):
        entry = cf.CFArrayGetValueAtIndex(windows, i)
        if number(entry, "kCGWindowOwnerPID") == pid:
            return number(entry, "kCGWindowNumber")
    return None

#!/usr/bin/env python3
"""Native Pinyin/search/tab/window regression on a logged-in Mac.

Requires Accessibility authorization for System Events and enabled Apple Pinyin.
Temporarily focuses its own test process and selects Pinyin, restoring the prior
input source in finally. No keyboard events are sent before checking focus.
"""

import argparse
import ctypes as C
import json
import os
import plistlib
import re
import shutil
from pathlib import Path
import select
import socket
import subprocess
import sys
import tempfile
import termios
import time
import tty

ROOT = Path(__file__).resolve().parents[1]


class InputSources:
    def __init__(self):
        self.carbon = C.CDLL("/System/Library/Frameworks/Carbon.framework/Carbon")
        self.cf = C.CDLL("/System/Library/Frameworks/CoreFoundation.framework/CoreFoundation")
        for name, args, result in [
            ("TISCreateInputSourceList", [C.c_void_p, C.c_bool], C.c_void_p),
            ("TISCopyCurrentKeyboardInputSource", [], C.c_void_p),
            ("TISGetInputSourceProperty", [C.c_void_p, C.c_void_p], C.c_void_p),
            ("TISSelectInputSource", [C.c_void_p], C.c_int32),
        ]:
            function = getattr(self.carbon, name)
            function.argtypes, function.restype = args, result
        for name, args, result in [
            ("CFArrayGetCount", [C.c_void_p], C.c_long),
            ("CFArrayGetValueAtIndex", [C.c_void_p, C.c_long], C.c_void_p),
            ("CFStringGetLength", [C.c_void_p], C.c_long),
            ("CFStringGetCString", [C.c_void_p, C.c_char_p, C.c_long, C.c_uint32], C.c_bool),
            ("CFRelease", [C.c_void_p], None),
        ]:
            function = getattr(self.cf, name)
            function.argtypes, function.restype = args, result
        self.original = self.carbon.TISCopyCurrentKeyboardInputSource()
        self.sources = self.carbon.TISCreateInputSourceList(None, False)
        if not self.original or not self.sources:
            if self.original:
                self.cf.CFRelease(self.original)
            if self.sources:
                self.cf.CFRelease(self.sources)
            raise RuntimeError("Input source APIs are unavailable in this desktop session")
        self.property = C.c_void_p.in_dll(self.carbon, "kTISPropertyInputSourceID")
        self.pinyin = None
        self.changed = False
        for index in range(self.cf.CFArrayGetCount(self.sources)):
            source = self.cf.CFArrayGetValueAtIndex(self.sources, index)
            value = self.carbon.TISGetInputSourceProperty(source, self.property)
            if value:
                buffer = C.create_string_buffer(self.cf.CFStringGetLength(value) * 4 + 1)
                if self.cf.CFStringGetCString(value, buffer, len(buffer), 0x08000100):
                    if buffer.value.decode() == "com.apple.inputmethod.SCIM.ITABC":
                        self.pinyin = source

    def select_pinyin(self):
        if not self.pinyin:
            raise RuntimeError("Enable Apple's Simplified Chinese Pinyin input source before this test")
        if self.carbon.TISSelectInputSource(self.pinyin) != 0:
            raise RuntimeError("Unable to select Pinyin input source")
        self.changed = True

    def close(self):
        status = self.carbon.TISSelectInputSource(self.original) if self.changed else 0
        self.cf.CFRelease(self.original)
        self.cf.CFRelease(self.sources)
        if status != 0:
            raise RuntimeError("Unable to restore the original keyboard input source")


def scene(work):
    previous = termios.tcgetattr(sys.stdin)
    received = bytearray()
    report = work / f"input-{os.getpid()}.json"
    try:
        tty.setraw(sys.stdin.fileno())
        print("\033[2J\033[HNative IME validation — 中文\r", flush=True)
        (work / f"ready-{os.getpid()}").touch()
        while not (work / "stop").exists():
            if select.select([sys.stdin], [], [], 0.1)[0]:
                value = os.read(sys.stdin.fileno(), 4096)
                if not value:
                    break
                received.extend(value)
                report.write_text(json.dumps({"text": received.decode("utf-8", errors="replace")}))
    finally:
        termios.tcsetattr(sys.stdin, termios.TCSANOW, previous)


def run(binary):
    if sys.platform != "darwin":
        raise SystemExit("Requires macOS")
    output = ROOT / "target/metal-optimization"
    output.mkdir(parents=True, exist_ok=True)
    work = Path(tempfile.mkdtemp(prefix="native-", dir=output))
    # Launch through LaunchServices with a distinct temporary bundle, so macOS
    # can activate it normally without relying on raw CLI process activation.
    bundle = work / "Alacritty Native Test.app"
    shutil.copytree(ROOT / "extra/osx/XaYa.app", bundle)
    executable = bundle / "Contents/MacOS/alacritty"
    executable.parent.mkdir(parents=True)
    shutil.copy2(binary, executable)
    plist_path = bundle / "Contents/Info.plist"
    plist = plistlib.loads(plist_path.read_bytes())
    plist.update(CFBundleIdentifier=f"org.alacritty.native-validation.{os.getpid()}",
                 CFBundleName="Alacritty Native Test", CFBundleDisplayName="Alacritty Native Test")
    plist_path.write_bytes(plistlib.dumps(plist))
    subprocess.run(["codesign", "--force", "--sign", "-", str(bundle)], check=True, capture_output=True)
    config = work / "alacritty.toml"
    shell_args = [str(Path(__file__).resolve()), "--scene", str(work)]
    config.write_text(f'''[debug]
renderer = "Metal"
log_level = "Info"
print_events = true
[terminal]
shell = {{ program = {json.dumps(sys.executable)}, args = {json.dumps(shell_args)} }}
[window]
dimensions = {{ columns = 90, lines = 28 }}
[cursor]
style = {{ blinking = "Off" }}
''')
    socket_path = Path(tempfile.gettempdir()) / f"alacritty-native-{os.getpid()}.sock"
    log_path = work / "application.log"
    process = None
    app_pid = None
    inputs = None
    passed = []

    def wait_for(predicate, description, timeout=15):
        deadline = time.monotonic() + timeout
        while not predicate():
            if process.poll() is not None or time.monotonic() > deadline:
                raise RuntimeError(f"{description}: exited/timed out; inspect {log_path}")
            time.sleep(0.05)
        passed.append(description)

    def log_text():
        return log_path.read_text(errors="replace")

    def ui(*commands):
        # The commands are fixed test actions; pid is an integer from Popen.
        program = ["tell application \"System Events\"",
                   f"set p to first application process whose unix id is {app_pid}",
                   "set frontmost of p to true", "delay 0.1",
                   "if not (frontmost of p) then error \"Test app is not focused\"",
                   *commands, "end tell"]
        try:
            return subprocess.run(["osascript", *[arg for line in program for arg in ("-e", line)]], check=True, capture_output=True, text=True, timeout=15).stdout.strip()
        except subprocess.CalledProcessError as error:
            raise RuntimeError(f"Native UI automation failed: {error.stderr.strip()}") from error

    try:
        inputs = InputSources()
        if not inputs.pinyin:
            raise RuntimeError("Apple Pinyin is not available in this desktop session")
        with log_path.open("w") as log:
            process = subprocess.Popen([
                "open", "-n", "-W", "--stdout", str(log_path), "--stderr", str(work / "gpu.log"),
                "--env", "ALACRITTY_RENDER_METRICS=1", "--env", "MTL_DEBUG_LAYER=1", "--env", "MTL_SHADER_VALIDATION=1",
                str(bundle), "--args", "--config-file", str(config), "--socket", str(socket_path),
            ], stdout=log, stderr=subprocess.STDOUT)
            wait_for(lambda: "Initialisation complete" in log_text() and len(list(work.glob("ready-*"))) == 1,
                     "native terminal ready")
            match = re.search(r"Alacritty-(\d+)\.log", log_text())
            if not match:
                raise RuntimeError("Unable to identify launched test application's PID")
            app_pid = int(match.group(1))
            primary = next(work.glob("ready-*")).name.split("-")[1]
            report = work / f"input-{primary}.json"
            ui()
            inputs.select_pinyin()
            time.sleep(0.2)
            ui('keystroke "nihao"')
            wait_for(lambda: "Preedit(" in log_text(), "real Pinyin composition events")
            ui("key code 53")
            time.sleep(0.2)
            prior = log_text().count("Preedit(")
            ui('keystroke "nihao"')
            wait_for(lambda: log_text().count("Preedit(") > prior, "composition after cancellation")
            # Preserve the composition through a native resize and font reload.
            ui("set size of window 1 of p to {1000, 700}")
            with socket.socket(socket.AF_UNIX) as connection:
                connection.connect(str(socket_path))
                connection.sendall((json.dumps({"Config": {"window_id": -1, "reset": False, "options": ["font.size=18.0"]}}) + "\n").encode())
            time.sleep(0.3)
            ui("key code 49", "key code 36")
            wait_for(lambda: report.exists() and any("\u3400" <= ch <= "\u9fff" for ch in json.loads(report.read_text())["text"]),
                     "Pinyin commits CJK text through the PTY after resize/font reload")
            committed = json.loads(report.read_text())["text"]
            prior = log_text().count("Preedit(")
            ui('keystroke "f" using {command down}')
            ui('keystroke "zhongwen"')
            wait_for(lambda: log_text().count("Preedit(") > prior, "Pinyin composition in search field")
            ui("key code 49", "key code 53")
            time.sleep(0.3)
            if json.loads(report.read_text())["text"] != committed:
                raise RuntimeError("Search input leaked to the terminal PTY")
            passed.append("search composition stays out of the PTY")
            ui("set w to window 1 of p", 'set value of attribute "AXMinimized" of w to true', "delay 0.3",
               'set value of attribute "AXMinimized" of w to false')
            passed.append("native minimize/restore")
            ui('keystroke "t" using {command down}')
            wait_for(lambda: log_text().count("Using native Metal renderer") == 2 and len(list(work.glob("ready-*"))) >= 2,
                     "native new tab initializes a second Metal renderer")
            ui('keystroke "w" using {command down}')
            wait_for(lambda: "Metal metrics:" in log_text(), "native tab closes with completed GPU resources")
            # Request fullscreen using its standard binding and wait for events.
            prior = log_text().count("Resized(")
            ui('keystroke "f" using {command down, control down}')
            wait_for(lambda: log_text().count("Resized(") > prior, "fullscreen transition resize")
            time.sleep(0.5)
            ui('keystroke "f" using {command down, control down}')
            time.sleep(0.7)
            (work / "stop").touch()
            process.wait(timeout=15)
        text = log_text() + (work / "gpu.log").read_text(errors="replace")
        if process.returncode != 0 or any(token in text for token in ("[ERROR]", "[WARN ]", "failed assertion", "GPU backend failed", "panicked at")):
            raise RuntimeError(f"Native validation errors; inspect {log_path}")
        result = {"passed": passed, "committed_text": committed, "log": str(log_path),
                  "limits": "Candidate popup visual placement and real Retina transitions still require visual/device validation."}
        (work / "report.json").write_text(json.dumps(result, indent=2, ensure_ascii=False) + "\n")
        print(json.dumps(result, indent=2, ensure_ascii=False))
    except Exception as error:
        (work / "report.json").write_text(json.dumps({"passed": passed, "failure": str(error), "log": str(log_path)}, indent=2) + "\n")
        raise
    finally:
        restoration_error = None
        if inputs is not None:
            try:
                inputs.close()
            except Exception as error:
                restoration_error = error
        (work / "stop").touch()
        if process is not None and process.poll() is None:
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                if app_pid is not None:
                    try:
                        os.kill(app_pid, 15)
                    except ProcessLookupError:
                        pass
                process.terminate()
                try:
                    process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait()
        socket_path.unlink(missing_ok=True)
        if restoration_error is not None:
            raise restoration_error


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=ROOT / "target/debug/xaya")
    parser.add_argument("--scene", type=Path, help=argparse.SUPPRESS)
    options = parser.parse_args()
    if options.scene:
        scene(options.scene)
    else:
        run(options.binary.resolve())

#!/usr/bin/env python3
"""Exercise the real macOS Metal backend and GL coexistence (Python stdlib only).

Run after `cargo build -p alacritty --no-default-features` on a logged-in Mac.
Logs and the report are retained under target/metal-validation/.
"""

import argparse
import json
import os
from pathlib import Path
import socket
import subprocess
import sys
import tempfile
import time

ROOT = Path(__file__).resolve().parents[1]


def scene(stop, stream=False):
    if stream:
        for i in range(10000):
            print(f"scroll stress {i:05d}: ASCII 中文 🚀 e\u0301 ─│┼")
    print("\033]0;Alacritty renderer validation\007\033[2J\033[H", end="")
    print("Alacritty rendering validation")
    print("ASCII: The quick brown fox 0123456789 !@#$%^&*()")
    print("中文：终端渲染测试  日本語：ターミナル  한국어: 터미널")
    print("Combining: e\u0301 a\u0308 n\u0303   Emoji: 🚀 🌈 😀 🦀 👨‍👩‍👧")
    for code, label in [("1", "Bold"), ("3", "Italic"), ("1;3", "Bold italic"),
                        ("4", "Underline"), ("9", "Strikeout"), ("4:2", "Double underline"),
                        ("4:3", "Undercurl"), ("4:4", "Dotted"), ("4:5", "Dashed")]:
        print(f"\033[{code}m{label}: ABC 中文 123\033[0m")
    for index in range(16):
        print(f"\033[48;5;{index}m {index:02d} \033[0m", end="")
    print("\nBox drawing: ┌──────┬──────┐\n             │ test │ 测试 │\n             └──────┴──────┘")
    print("\033[8mhidden text\033[0m ← hidden text on left")
    print("\033]8;;https://alacritty.org\033\\Hyperlink: alacritty.org\033]8;;\033\\")
    sys.stdout.flush()
    while not stop.exists():
        time.sleep(0.1)


def run(binary):
    if sys.platform != "darwin":
        raise SystemExit("This smoke test requires a logged-in macOS desktop.")
    output = ROOT / "target/metal-validation"
    output.mkdir(parents=True, exist_ok=True)
    work = Path(tempfile.mkdtemp(prefix="smoke-", dir=output))
    # macOS Unix sockets have a short path limit; use a short, unique socket path.
    socket_path = Path(tempfile.gettempdir()) / f"alacritty-metal-{os.getpid()}.sock"
    config = work / "alacritty.toml"
    config.write_text('''[debug]
renderer = "Metal"
log_level = "Debug"
persistent_logging = true
[font]
size = 15
[window]
dimensions = { columns = 90, lines = 28 }
padding = { x = 10, y = 10 }
[cursor]
style = { blinking = "Off" }
''')
    stops = [work / f"stop-{i}" for i in range(3)]
    log_path = work / "application.log"
    environment = dict(os.environ, MTL_DEBUG_LAYER="1", MTL_SHADER_VALIDATION="1", ALACRITTY_RENDER_METRICS="1")
    passed = []
    process = None
    started = time.monotonic()

    def wait_for(predicate, description, timeout=20):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            text = log_path.read_text(errors="replace")
            if predicate(text):
                passed.append(description)
                return
            if process.poll() is not None:
                raise RuntimeError(f"Application exited during {description}:\n{text}")
            time.sleep(0.1)
        raise RuntimeError(f"Timed out: {description}\n{log_path.read_text(errors='replace')}")

    def send(message):
        # Newline framing is supported by the existing IpcListener and avoids
        # relying on half-close timing when the server immediately closes a reply.
        with socket.socket(socket.AF_UNIX) as connection:
            connection.connect(str(socket_path))
            connection.sendall((json.dumps(message) + "\n").encode())

    def create(index, options, stream=False):
        command = [sys.executable, str(Path(__file__).resolve()), "--scene", str(stops[index])]
        if stream:
            command.append("--stream")
        send({"CreateWindow": {
            "terminal_options": {"working_directory": None, "hold": False, "command": command},
            "window_identity": {"title": f"Renderer smoke {index}", "class": None},
            "window_tabbing_id": None, "option": options,
        }})

    try:
        with log_path.open("w") as log:
            process = subprocess.Popen([
                str(binary), "--config-file", str(config), "--socket", str(socket_path),
                "-e", sys.executable, str(Path(__file__).resolve()), "--scene", str(stops[0]),
            ], env=environment, stdout=log, stderr=subprocess.STDOUT)
            wait_for(lambda text: "Initialisation complete" in text and "Using native Metal renderer" in text,
                     "Metal window initialization with API/GPU validation")
            # A stalled client must not indefinitely prevent the real CLI from
            # reading configuration (legacy EOF framing and reply path).
            with socket.socket(socket.AF_UNIX) as stalled:
                stalled.connect(str(socket_path))
                before = time.monotonic()
                reply = subprocess.check_output([
                    str(binary), "msg", "-s", str(socket_path), "get-config",
                ], text=True, timeout=4)
                if "Metal" not in reply or time.monotonic() - before > 3:
                    raise RuntimeError("IPC deadline/CLI configuration reply failed")
            passed.append("stalled IPC client bounded; real CLI get-config reply succeeds")
            send({"Config": {"window_id": -1, "reset": False,
                             "options": ["font.size=20.0", "window.opacity=0.65",
                                         "window.padding.x=18", "window.padding.y=14"]}})
            wait_for(lambda text: "Font size changed" in text, "font/opacity/padding hot reload")
            create(1, [], stream=True)
            wait_for(lambda text: text.count("Using native Metal renderer") >= 2,
                     "second Metal window and scroll stress")
            create(2, ['debug.renderer="glsl3"'])
            wait_for(lambda text: "Using OpenGL 3.3 renderer" in text,
                     "OpenGL window coexisting with Metal windows")
            # Give stream output and asynchronous GPU validation time to complete.
            time.sleep(2)
            stops[1].touch()
            wait_for(lambda text: "Metal renderer destroyed" in text,
                     "close one Metal window while other backends remain alive")
            send({"Config": {"window_id": -1, "reset": True, "options": []}})
            wait_for(lambda text: text.count("Font size changed") >= 2,
                     "font cache reset with windows still alive")
            stops[0].touch()
            stops[2].touch()
            process.wait(timeout=15)
        text = log_path.read_text(errors="replace")
        failures = [line for line in text.splitlines()
                    if any(token in line for token in ("[ERROR]", "[WARN ]", "failed assertion", "Validation Error", "GPU backend failed", "Metal initialization failed", "panicked at"))]
        if process.returncode != 0 or failures:
            raise RuntimeError(f"exit={process.returncode}; validation failures: {failures}")
        metrics = [json.loads(line.split("Metal metrics: ", 1)[1]) for line in text.splitlines() if "Metal metrics: " in line]
        if len(metrics) != 2 or sum(record["pipeline_reused"] for record in metrics) != 1:
            raise RuntimeError("Metal windows did not share the immutable pipeline cache")
        if any(record["submitted_frames"] != record["completed_frames"] for record in metrics):
            raise RuntimeError("Metal window exited without completing its GPU work")
        passed.append("shared pipeline cache and completed GPU frame ownership verified")
        passed.append("clean shutdown without API/GPU validation errors")
        report = {"passed": passed, "elapsed_seconds": round(time.monotonic() - started, 2),
                  "log": str(log_path), "pid": process.pid}
        (work / "report.json").write_text(json.dumps(report, indent=2) + "\n")
        print(json.dumps(report, indent=2))
    finally:
        for stop in stops:
            stop.touch()
        if process is not None and process.poll() is None:
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.terminate()
                try:
                    process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait()
        socket_path.unlink(missing_ok=True)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=ROOT / "target/debug/xaya")
    parser.add_argument("--scene", type=Path, help=argparse.SUPPRESS)
    parser.add_argument("--stream", action="store_true", help=argparse.SUPPRESS)
    arguments = parser.parse_args()
    if arguments.scene:
        scene(arguments.scene, arguments.stream)
    else:
        run(arguments.binary.resolve())

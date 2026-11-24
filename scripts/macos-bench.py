#!/usr/bin/env python3
"""Repeatable macOS renderer diagnostics; opens only its own test windows.

Reports PTY backpressure throughput, additional-window initialization, parent
process idle CPU, CPU submission and Metal GPU execution times. These are not
physical input-to-display latency or energy measurements. GPU validation is off.
"""

import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import socket
import statistics
import subprocess
import sys
import tempfile
import time

ROOT = Path(__file__).resolve().parents[1]


def child_scene(stop, report, stream):
    if stream:
        chunk = ("throughput 中文 abcdefghijklmnopqrstuvwxyz 0123456789\r\n".encode()) * 1024
        start = time.monotonic()
        for _ in range(256):
            sys.stdout.buffer.write(chunk)
        sys.stdout.buffer.flush()
        throughput = {"seconds": time.monotonic() - start, "bytes": len(chunk) * 256}
    else:
        throughput = None
    print("\033[2J\033[HmacOS renderer benchmark: 中文 e\u0301 🚀", flush=True)
    report.write_text(json.dumps({"throughput": throughput}))
    while not stop.exists():
        time.sleep(0.1)


def cpu_seconds(pid):
    value = subprocess.check_output(["ps", "-p", str(pid), "-o", "time="], text=True).strip()
    result = 0.0
    for part in value.split(":"):
        result = result * 60 + float(part)
    return result


def collect_records(text, prefix):
    return [json.loads(line.split(prefix, 1)[1]) for line in text.splitlines() if prefix in line]


def trial(binary, backend, output, idle_seconds):
    work = Path(tempfile.mkdtemp(prefix=f"{backend}-", dir=output))
    socket_path = Path(tempfile.gettempdir()) / f"alacritty-bench-{os.getpid()}.sock"
    config = work / "alacritty.toml"
    config.write_text(f'''[debug]
renderer = "{backend}"
log_level = "Info"
[font]
size = 15
[window]
dimensions = {{ columns = 90, lines = 28 }}
padding = {{ x = 10, y = 10 }}
[cursor]
style = {{ blinking = "Off" }}
''')
    stops = [work / f"stop-{i}" for i in range(3)]
    reports = [work / f"scene-{i}.json" for i in range(3)]
    log_path = work / "application.log"
    process = None
    environment = dict(os.environ, ALACRITTY_RENDER_METRICS="1")
    for key in ("MTL_DEBUG_LAYER", "MTL_SHADER_VALIDATION"):
        environment.pop(key, None)
    started = time.monotonic()

    def command(index, stream=False):
        args = [sys.executable, str(Path(__file__).resolve()), "--scene", str(stops[index]), "--scene-report", str(reports[index])]
        return args + (["--stream"] if stream else [])

    def wait_for(predicate, timeout=20):
        deadline = time.monotonic() + timeout
        while not predicate():
            if process.poll() is not None or time.monotonic() >= deadline:
                raise RuntimeError(f"Benchmark process exited or timed out: {log_path.read_text(errors='replace')}")
            time.sleep(0.005)

    try:
        with log_path.open("w") as log:
            process = subprocess.Popen([
                str(binary), "--config-file", str(config), "--socket", str(socket_path), "-e", *command(0, True),
            ], stdout=log, stderr=subprocess.STDOUT, env=environment)
            wait_for(lambda: "Initialisation complete" in log_path.read_text())
            initialization_ms = (time.monotonic() - started) * 1000
            wait_for(lambda: reports[0].exists())
            throughput = json.loads(reports[0].read_text())["throughput"]
            # Let queued work settle before measuring a static, nonblinking window.
            time.sleep(0.3)
            before = cpu_seconds(process.pid)
            idle_started = time.monotonic()
            time.sleep(idle_seconds)
            idle_cpu = cpu_seconds(process.pid) - before
            idle_wall = time.monotonic() - idle_started
            window_ms = []
            for index in (1, 2):
                opened = time.monotonic()
                message = {"CreateWindow": {
                    "terminal_options": {"working_directory": None, "hold": False, "command": command(index)},
                    "window_identity": {"title": f"Benchmark {index}", "class": None},
                    "window_tabbing_id": None, "option": [],
                }}
                with socket.socket(socket.AF_UNIX) as connection:
                    connection.connect(str(socket_path))
                    connection.sendall((json.dumps(message) + "\n").encode())
                wait_for(lambda: log_path.read_text().count("PTY dimensions") >= index + 1)
                window_ms.append((time.monotonic() - opened) * 1000)
                wait_for(lambda: reports[index].exists())
            # After all windows exist, exercise repeated cache uploads and steady
            # redraws with reversible config updates, independent of PTY parsing.
            for size in (16, 15, 17, 15):
                message = {"Config": {"window_id": -1, "reset": False, "options": [f"font.size={size}.0"]}}
                with socket.socket(socket.AF_UNIX) as connection:
                    connection.connect(str(socket_path))
                    connection.sendall((json.dumps(message) + "\n").encode())
                time.sleep(0.1)
            for stop in stops:
                stop.touch()
            process.wait(timeout=15)
        text = log_path.read_text(errors="replace")
        if process.returncode != 0 or any(token in text for token in ("[ERROR]", "[WARN ]", "panicked at")):
            raise RuntimeError(text)
        result = {
            "backend": backend, "initialization_ms": initialization_ms,
            "additional_window_ms": window_ms,
            "pty_mib_per_second": throughput["bytes"] / throughput["seconds"] / 1024**2,
            "pty_bytes": throughput["bytes"], "pty_seconds": throughput["seconds"],
            "idle_cpu_seconds": idle_cpu, "idle_wall_seconds": idle_wall,
            "render_metrics": collect_records(text, "Render metrics: "),
            "metal_metrics": collect_records(text, "Metal metrics: "), "log": str(log_path),
        }
        (work / "report.json").write_text(json.dumps(result, indent=2) + "\n")
        return result
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


def run(arguments):
    if sys.platform != "darwin":
        raise SystemExit("Requires a logged-in macOS desktop")
    if arguments.runs < 1 or arguments.idle_seconds <= 0:
        raise SystemExit("runs and idle-seconds must be positive")
    binary = arguments.binary.resolve()
    output = ROOT / "target/metal-optimization"
    output.mkdir(parents=True, exist_ok=True)
    results = []
    for index in range(arguments.runs):
        order = ("glsl3", "Metal") if index % 2 == 0 else ("Metal", "glsl3")
        for backend in order:
            results.append(trial(binary, backend, output, arguments.idle_seconds))
    summary = {}
    for backend in ("glsl3", "Metal"):
        records = [result for result in results if result["backend"] == backend]
        summary[backend] = {
            "median_pty_mib_per_second": statistics.median(result["pty_mib_per_second"] for result in records),
            "median_initialization_ms": statistics.median(result["initialization_ms"] for result in records),
            "median_additional_window_ms": statistics.median(value for result in records for value in result["additional_window_ms"]),
            "median_idle_cpu_seconds": statistics.median(result["idle_cpu_seconds"] for result in records),
        }
    report = {"binary": str(binary), "sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
              "macos": platform.mac_ver()[0], "architecture": platform.machine(), "results": results, "summary": summary,
              "scope": "CPU submission/GPU execution and PTY timings; not physical display latency or energy. ps idle CPU is quantized to centiseconds; GPU validation disabled."}
    destination = arguments.output or output / f"benchmark-{time.time_ns()}.json"
    destination.write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps({"report": str(destination), "summary": summary}, indent=2))


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=ROOT / "target/debug/xaya")
    parser.add_argument("--runs", type=int, default=3)
    parser.add_argument("--idle-seconds", type=float, default=2.0)
    parser.add_argument("--output", type=Path)
    parser.add_argument("--scene", type=Path, help=argparse.SUPPRESS)
    parser.add_argument("--scene-report", type=Path, help=argparse.SUPPRESS)
    parser.add_argument("--stream", action="store_true", help=argparse.SUPPRESS)
    args = parser.parse_args()
    if args.scene:
        child_scene(args.scene, args.scene_report, args.stream)
    else:
        run(args)

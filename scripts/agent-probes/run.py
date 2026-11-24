#!/usr/bin/env python3
"""Offline P0 foundations: real CLI transports and isolated git snapshots."""

import argparse
import base64
import hashlib
import json
import os
from pathlib import Path
import queue
import shutil
import signal
import socket
import subprocess
import tempfile
import threading
import time


PROBES = ("C1", "C2", "C3", "C4", "C5", "A1", "A2", "A3", "A4", "A5", "H1", "G1", "E1")


def command(argv, env, cwd):
    return subprocess.run(argv, env=env, cwd=cwd, check=True, capture_output=True,
                          text=True, timeout=30).stdout.strip()


def isolated_env(root):
    # Do not inherit credentials, proxy settings, shell hooks or engine configuration.
    env = {key: os.environ[key] for key in ("PATH", "SYSTEMROOT", "TMPDIR") if key in os.environ}
    for key, name in (("HOME", "home"), ("CODEX_HOME", "codex"),
                      ("CLAUDE_CONFIG_DIR", "claude"), ("XDG_CONFIG_HOME", "config"),
                      ("XDG_DATA_HOME", "data"), ("XDG_STATE_HOME", "state")):
        directory = root / name
        directory.mkdir()
        env[key] = str(directory)
    env.update(TERM="xterm-256color", OPENAI_API_KEY="xaya-probe-fake-key",
               ANTHROPIC_API_KEY="xaya-probe-fake-key", NO_PROXY="127.0.0.1,localhost")
    # Deliberately unreachable local provider. These probes never start a model turn.
    (root / "codex/config.toml").write_text(
        'model_provider = "probe"\ncheck_for_update_on_startup = false\n'
        '[model_providers.probe]\nname = "Offline probe"\n'
        'base_url = "http://127.0.0.1:1/v1"\nwire_api = "responses"\n'
        'requires_openai_auth = false\n[analytics]\nenabled = false\n'
        '[feedback]\nenabled = false\n')
    return env


class Server:
    def __init__(self, binary, transport, env, cwd, log):
        self.log = open(log, "w")
        self.process = subprocess.Popen(
            [binary, "app-server", "--listen", transport], env=env, cwd=cwd,
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=self.log,
            text=True, start_new_session=True)
        self.messages = queue.Queue()
        self.reader = threading.Thread(target=self.read, daemon=True)
        self.reader.start()
        self.sequence = 0

    def read(self):
        try:
            for line in self.process.stdout:
                try:
                    self.messages.put(json.loads(line))
                except ValueError:
                    self.messages.put(RuntimeError("Non-JSON app-server output"))
        finally:
            self.messages.put(EOFError("app-server closed stdout"))

    def send(self, value):
        self.process.stdin.write(json.dumps(value) + "\n")
        self.process.stdin.flush()

    def call(self, method, params, timeout=15):
        self.sequence += 1
        request_id = f"xaya-probe-{self.sequence}"
        self.send(dict(id=request_id, method=method, params=params))
        deadline = time.monotonic() + timeout
        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise TimeoutError(f"RPC timeout: {method}")
            try:
                message = self.messages.get(timeout=remaining)
            except queue.Empty:
                raise TimeoutError(f"RPC timeout: {method}") from None
            if isinstance(message, Exception):
                raise message
            if message.get("id") == request_id:
                if "error" in message:
                    raise RuntimeError(f"{method}: {message['error']}")
                return message["result"]

    def close(self):
        # Kill only this probe's process group, including helper children.
        try:
            os.killpg(self.process.pid, signal.SIGTERM)
        except ProcessLookupError:
            pass
        try:
            self.process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            os.killpg(self.process.pid, signal.SIGKILL)
            self.process.wait(timeout=5)
        self.reader.join(timeout=2)
        self.process.stdin.close()
        self.process.stdout.close()
        self.log.close()


def codex_probe(binary, env, root):
    server = Server(binary, "stdio://", env, root, root / "stdio.log")
    try:
        result = server.call("initialize", {
            "clientInfo": {"name": "xaya_probe", "version": "0.1.0"},
            "capabilities": {"experimentalApi": True}})
        server.send({"method": "initialized", "params": {}})
        if Path(result["codexHome"]).resolve() != Path(env["CODEX_HOME"]).resolve():
            raise RuntimeError("Server did not use the isolated CODEX_HOME")
        thread = server.call("thread/start", {
            "cwd": str(root), "modelProvider": "probe", "approvalPolicy": "on-request",
            "sandbox": "read-only"})
        thread_id = thread["thread"]["id"]
        if not thread_id:
            raise RuntimeError("No thread ID in thread/start response")
    finally:
        server.close()

    # A custom socket is intentionally short enough for macOS sockaddr_un.
    with tempfile.TemporaryDirectory(prefix="xp-", dir="/tmp") as socket_dir:
        endpoint = Path(socket_dir) / "s"
        server = Server(binary, f"unix://{endpoint}", env, root, root / "unix.log")
        try:
            deadline = time.monotonic() + 15
            while not endpoint.exists():
                if server.process.poll() is not None or time.monotonic() >= deadline:
                    raise RuntimeError("Unix socket did not become ready")
                time.sleep(0.05)
            with socket.socket(socket.AF_UNIX) as client:
                client.settimeout(5)
                client.connect(str(endpoint))
                key = base64.b64encode(os.urandom(16)).decode()
                client.sendall(("GET / HTTP/1.1\r\nHost: localhost\r\nUpgrade: websocket\r\n"
                                "Connection: Upgrade\r\nSec-WebSocket-Version: 13\r\n"
                                f"Sec-WebSocket-Key: {key}\r\n\r\n").encode())
                header = b""
                while b"\r\n\r\n" not in header:
                    block = client.recv(4096)
                    if not block or len(header) > 65536:
                        raise RuntimeError("Incomplete WebSocket handshake")
                    header += block
                expected = base64.b64encode(hashlib.sha1(
                    (key + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11").encode()).digest())
                if not header.startswith(b"HTTP/1.1 101 ") or expected not in header:
                    raise RuntimeError("Unix endpoint did not accept WebSocket upgrade")
        finally:
            server.close()
    return {"transport": "WebSocket over Unix socket", "stdio_initialize": True,
            "thread_id_from_response": thread_id}


def git_probe(env, root, files):
    repo = root / "repository"
    repo.mkdir()
    def git(*args, index=None):
        git_env = dict(env, GIT_OPTIONAL_LOCKS="0")
        if index:
            git_env["GIT_INDEX_FILE"] = str(index)
        return command(["git", "-c", "user.name=probe", "-c", "user.email=probe@localhost",
                        *args], git_env, repo)
    git("init", "-q")
    for i in range(files):
        (repo / f"file-{i}.txt").write_text("baseline\n")
    git("add", "--all")
    git("commit", "-qm", "baseline")
    (repo / "staged.txt").write_text("user staged\n")
    git("add", "staged.txt")
    index = Path(git("rev-parse", "--path-format=absolute", "--git-path", "index"))
    original = index.read_bytes()
    def snapshot():
        scratch = root / "scratch-index"
        shutil.copyfile(index, scratch)
        git("add", "--all", index=scratch)
        return git("write-tree", index=scratch)
    start = time.monotonic()
    before = snapshot()
    (repo / "file-0.txt").write_text("baseline\nchanged\n")
    (repo / "new.txt").write_text("new\n")
    after = snapshot()
    elapsed = time.monotonic() - start
    stats = git("diff", "--numstat", before, after).splitlines()
    if len(stats) != 2 or sum(int(row.split()[0]) for row in stats) != 2:
        raise RuntimeError(f"Incorrect snapshot diff: {stats}")
    if original != index.read_bytes():
        raise RuntimeError("User index was modified")
    return {"files": files, "two_snapshots_seconds": round(elapsed, 4),
            "under_one_second": elapsed < 1, "user_index_unchanged": True}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True, help="New artifact directory")
    parser.add_argument("--codex", default="codex")
    parser.add_argument("--files", type=int, default=3000)
    parser.add_argument("--dual", action="store_true", help="Run real TUI and local model probes (needs websockets)")
    parser.add_argument("--daemon", action="store_true", help="Compare isolated Codex daemon and dedicated backends")
    parser.add_argument("--claude", action="store_true", help="Probe Claude background sessions with a local model")
    parser.add_argument("--claude-mods", action="store_true", help="Probe injected Claude Mods, permissions and cancellation")
    parser.add_argument("--claude-bin", default="claude")
    parser.add_argument("--review", action="store_true", help="Verify both headless engines in read-only review mode")
    parser.add_argument("--boundaries", action="store_true", help="Also compare normal Claude TUI OSC/native boundaries")
    parser.add_argument("--benchmark-repo", type=Path, action="append", default=[], help="Benchmark snapshots on a private clone of this repository")
    args = parser.parse_args()
    if args.files < 1:
        parser.error("--files must be positive")
    root = args.output.resolve()
    root.mkdir(parents=True, exist_ok=False)
    root.chmod(0o700)
    in_git = subprocess.run(["git", "-C", str(root), "rev-parse", "--git-dir"],
                            env={"PATH": os.environ.get("PATH", "")}, capture_output=True,
                            timeout=10)
    if in_git.returncode == 0:
        parser.error("--output must be outside any git repository")
    env = isolated_env(root)
    report = {"schema_version": 1, "date": time.strftime("%Y-%m-%d"),
              "checks": {key: {"status": "not_run"} for key in PROBES},
              "limitations": ["Base Codex probes submit no model turns; optional engine probes use loopback fixtures.",
                              "Product daemon and panel integration remain pending.",
                              "G1 is a synthetic repository benchmark, not a large real repository."]}
    failed = False
    try:
        binary = shutil.which(args.codex)
        if binary is None:
            raise FileNotFoundError(f"Codex executable not found: {args.codex}")
        report["codex_version"] = command([binary, "--version"], env, root)
        detail = codex_probe(binary, env, root)
        report["checks"]["C1"] = {"status": "partial", "evidence": detail}
        report["checks"]["C2"] = {"status": "partial", "evidence": "thread/start response ID verified; no turn injected"}
    except Exception as error:
        failed = True
        report["checks"]["C1"] = {"status": "failed", "error": str(error)}
    if args.dual and "codex_version" in report:
        report["limitations"] = [
            "Only loopback deterministic model responses; no real account or production model.",
            "Approvals use a probe controller; XaYa product panel integration remains pending.",
            "See checks for remaining Claude, daemon and OSC boundary coverage.",
            "G1 is a synthetic repository benchmark, not a large real repository."]
        try:
            import asyncio
            from codex_dual import dual_probe
            asyncio.run(dual_probe(binary, env, root, report["checks"]))
        except Exception as error:
            import traceback
            failed = True
            report["dual_error"] = f"{type(error).__name__}: {error}"
            (root / "dual-error.log").write_text(traceback.format_exc())
    if args.daemon and "codex_version" in report:
        try:
            import asyncio
            from codex_daemon import daemon_probe
            asyncio.run(daemon_probe(binary, env, root, report["checks"]))
        except Exception as error:
            import traceback
            failed = True
            report["daemon_error"] = f"{type(error).__name__}: {error}"
            (root / "daemon-error.log").write_text(traceback.format_exc())
        report["limitations"].append("Daemon comparison uses only an isolated home and a loopback model fixture.")
    if args.claude:
        try:
            import asyncio
            from claude_background import claude_probe
            claude_binary = shutil.which(args.claude_bin)
            if claude_binary is None:
                raise FileNotFoundError(f"Claude executable not found: {args.claude_bin}")
            report["claude_version"] = command([claude_binary, "--version"], env, root)
            asyncio.run(claude_probe(claude_binary, env, root, report["checks"]))
        except Exception as error:
            import traceback
            failed = True
            report["claude_error"] = f"{type(error).__name__}: {error}"
            (root / "claude-error.log").write_text(traceback.format_exc())
        report["limitations"].append("Claude background UUID allocation differs from the original specified-UUID criterion.")
    if args.claude_mods:
        try:
            import asyncio
            from claude_mods import mods_probe
            claude_binary = shutil.which(args.claude_bin)
            if claude_binary is None:
                raise FileNotFoundError("Claude executable not found")
            report["claude_version"] = command([claude_binary, "--version"], env, root)
            asyncio.run(mods_probe(claude_binary, env, root, report["checks"]))
        except Exception as error:
            import traceback
            failed = True
            report["mods_error"] = f"{type(error).__name__}: {error}"
            (root / "mods-error.log").write_text(traceback.format_exc())
        report["limitations"].append("Claude Mods results use injected probe assets, not a product daemon.")
        report["limitations"].append("Esc recovery tests the private daemon reply protocol; production should offer explicit recovery.")
    if args.review:
        try:
            from readonly_review import review_probe
            claude_binary = shutil.which(args.claude_bin)
            if claude_binary is None or binary is None:
                raise FileNotFoundError("Both engines are required for H1")
            report["claude_version"] = command([claude_binary, "--version"], env, root)
            review_probe(binary, claude_binary, env, root, report["checks"])
        except Exception as error:
            import traceback
            failed = True
            report["review_error"] = f"{type(error).__name__}: {error}"
            (root / "review-error.log").write_text(traceback.format_exc())
    if args.boundaries:
        try:
            import asyncio
            from claude_boundaries import claude_boundaries
            claude_binary = shutil.which(args.claude_bin)
            if claude_binary is None:
                raise FileNotFoundError("Claude executable not found")
            report["claude_version"] = command([claude_binary, "--version"], env, root)
            asyncio.run(claude_boundaries(claude_binary, env, root, report["checks"]))
        except Exception as error:
            import traceback
            failed = True
            report["boundary_error"] = f"{type(error).__name__}: {error}"
            (root / "boundary-error.log").write_text(traceback.format_exc())
    try:
        detail = git_probe(env, root, args.files)
        report["checks"]["G1"] = {"status": "partial", "evidence": detail}
    except Exception as error:
        failed = True
        report["checks"]["G1"] = {"status": "failed", "error": str(error)}
    if args.benchmark_repo:
        try:
            from git_benchmark import benchmark_repo
            samples = [benchmark_repo(source, env, root, number) for number, source in enumerate(args.benchmark_repo)]
            (root / "git-benchmarks.json").write_text(json.dumps(samples, indent=2))
            report["checks"]["G1"] = {"status": "passed" if all(s["under_one_second"] for s in samples) else "partial",
                "evidence": samples}
            report["limitations"] = [item for item in report["limitations"] if "G1 is a synthetic" not in item]
            report["limitations"].append("G1 timing applies only to the recorded real repositories and machine; caches were not flushed.")
        except Exception as error:
            import traceback
            failed = True
            report["benchmark_error"] = f"{type(error).__name__}: {error}"
            (root / "benchmark-error.log").write_text(traceback.format_exc())
    e1 = report["checks"]["E1"]
    if e1["status"] != "not_run":
        modes = e1["evidence"]
        if any(mode["status"] == "failed" for mode in modes.values()):
            e1["status"] = "failed"
            failed = True
        elif all(modes.get(engine, {}).get("status") == "passed" for engine in ("codex", "claude_native")):
            e1["status"] = "passed"
    (root / "report.json").write_text(json.dumps(report, indent=2) + "\n")
    lines = ["# XaYa P0 capability report", "", f"Date: {report['date']}", "",
             f"Codex: {report.get('codex_version', 'unavailable')}", "",
             "| Probe | Status | Evidence |", "|---|---|---|"]
    if "claude_version" in report:
        lines.insert(6, f"Claude: {report['claude_version']}\n")
    for key, check in report["checks"].items():
        evidence = json.dumps(check.get("evidence", check.get("error", "")), ensure_ascii=False)
        lines.append(f"| {key} | {check['status']} | {evidence.replace('|', '/')} |")
    for key in ("dual_error", "daemon_error", "claude_error", "mods_error", "review_error", "boundary_error", "benchmark_error"):
        if key in report:
            lines += ["", f"{key}: {report[key]}"]
    lines += ["", *[f"- {item}" for item in report["limitations"]], ""]
    (root / "report.md").write_text("\n".join(lines))
    print(json.dumps(report, indent=2))
    return int(failed)


if __name__ == "__main__":
    raise SystemExit(main())

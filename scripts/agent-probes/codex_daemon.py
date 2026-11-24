"""C5: compare dedicated and managed backends without touching the user's daemon."""

import asyncio
import contextlib
import json
import os
from pathlib import Path
import re
import signal
import subprocess
import tempfile

from codex_dual import Rpc, completed, connect, initialize, start_turn, until
from model_fixture import ModelFixture


async def cli(binary, env, cwd, *args):
    result = await asyncio.to_thread(subprocess.run, [binary, *args], env=env, cwd=cwd,
                                    capture_output=True, text=True, timeout=30)
    if result.returncode:
        raise RuntimeError(f"{' '.join(args)}: {result.stderr or result.stdout}")
    return result.stdout.strip()


async def daemon_probe(binary, env, root, checks):
    evidence = []
    checks["C5"] = {"status": "partial", "evidence": evidence}
    for mode in ("dedicated", "managed"):
        with ModelFixture() as fixture, tempfile.TemporaryDirectory(prefix="xc-", dir="/tmp") as temporary:
            home = Path(temporary)
            local_env = dict(env, CODEX_HOME=str(home))
            config = Path(env["CODEX_HOME"]) / "config.toml"
            text = config.read_text()
            text = re.sub(r'base_url\s*=\s*"[^"]*"', f'base_url="{fixture.base_url}"', text)
            (home / "config.toml").write_text(text)
            backend = rpc = None
            log = open(root / f"daemon-{mode}.log", "w")
            managed_started = False
            try:
                if mode == "managed":
                    managed_started = True  # Also stop a daemon created by a timed-out start.
                    startup = await cli(binary, local_env, root, "app-server", "daemon", "start")
                    endpoint = home / "app-server-control/app-server-control.sock"
                else:
                    startup = "dedicated process"
                    endpoint = home / "dedicated.sock"
                    backend = subprocess.Popen([binary, "app-server", "--listen", f"unix://{endpoint}"],
                        env=local_env, cwd=root, stdin=subprocess.DEVNULL, stdout=log, stderr=log,
                        start_new_session=True)
                await until(endpoint.exists, f"{mode} socket ready")
                rpc = Rpc(await connect(endpoint))
                result = await rpc.call("initialize", {"clientInfo": {"name": "xaya_probe", "version": "0.3.0"},
                    "capabilities": {"experimentalApi": True}})
                if Path(result["codexHome"]).resolve() != home.resolve():
                    raise RuntimeError("Daemon escaped its isolated CODEX_HOME")
                await rpc.connection.send(json.dumps({"method": "initialized", "params": {}}))
                started = await rpc.call("thread/start", {"cwd": str(root), "model": "gpt-5.4",
                    "modelProvider": "probe", "approvalPolicy": "on-request", "sandbox": "read-only"})
                thread_id = started["thread"]["id"]
                turn = await start_turn(rpc, thread_id, "SLOW_CANCEL")
                turn_id = turn["turn"]["id"]
                await until(fixture.slow_started.is_set, f"{mode} model stream")
                await until(lambda: any(e.get("method") == "item/agentMessage/delta" and
                    e["params"].get("turnId") == turn_id for e in rpc.events), f"{mode} streaming delta")
                await rpc.close()
                rpc = None
                # There are now no attached protocol clients while the model stream is held.
                await asyncio.sleep(0.5)
                fixture.release.set()
                rpc = Rpc(await connect(endpoint))
                await initialize(rpc)
                status = None
                async with asyncio.timeout(20):
                    while status not in ("completed", "interrupted", "failed"):
                        history = await rpc.call("thread/read", {"threadId": thread_id, "includeTurns": True})
                        status = next((t["status"] for t in history["thread"]["turns"] if t["id"] == turn_id), None)
                        if status not in ("completed", "interrupted", "failed"):
                            await asyncio.sleep(0.1)
                resumed = await rpc.call("thread/resume", {"threadId": thread_id})
                if resumed["thread"]["id"] != thread_id:
                    raise RuntimeError("Reconnect changed thread ID")
                followup = await start_turn(rpc, thread_id, "AFTER_RESTART")
                await until(lambda: completed(rpc, followup["turn"]["id"]), f"{mode} follow-up completion")
                evidence.append({"mode": mode, "startup": startup,
                    "isolated_home_verified": True, "disconnected_turn_status": status,
                    "zero_client_hold_seconds": 0.5,
                    "turn_continued_without_clients": status == "completed",
                    "same_thread_resumed": True, "followup_completed": True})
                if fixture.errors:
                    raise RuntimeError(str(fixture.errors))
            finally:
                try:
                    try:
                        if rpc:
                            await rpc.close()
                    finally:
                        if managed_started:
                            cleanup = await cli(binary, local_env, root, "app-server", "daemon", "stop")
                            (root / "daemon-stop.log").write_text(cleanup + "\n")
                        if backend:
                            with contextlib.suppress(ProcessLookupError):
                                os.killpg(backend.pid, signal.SIGTERM)
                            try:
                                await asyncio.to_thread(backend.wait, 3)
                            except subprocess.TimeoutExpired:
                                os.killpg(backend.pid, signal.SIGKILL)
                                await asyncio.to_thread(backend.wait, 3)
                finally:
                    log.close()
                    (root / f"daemon-{mode}-requests.json").write_text(json.dumps(fixture.requests, indent=2))
    checks["C5"] = {"status": "passed", "evidence": evidence}

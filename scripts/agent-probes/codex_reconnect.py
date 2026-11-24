"""C6: a dedicated unix-socket app-server keeps a turn and its pending approval across a client loss.

The only protocol client disconnects while a command approval is pending, as when agentd crashes.
A new client must initialize, resume the thread, observe the pending approval again, answer it, and
see the turn complete. Uses the real installed Codex and the loopback model fixture only.
"""

import argparse
import asyncio
import contextlib
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile

from codex_dual import Rpc, completed, connect, initialize, start_turn, until
from model_fixture import ModelFixture


async def reconnect_probe(binary, output):
    evidence = {}
    with ModelFixture() as fixture, tempfile.TemporaryDirectory(prefix="xr-", dir="/tmp") as temporary:
        root = Path(temporary)
        home, workspace = root / "home", root / "workspace"
        home.mkdir()
        workspace.mkdir()
        (home / "config.toml").write_text(
            'model="gpt-5.4"\nmodel_provider="probe"\ncheck_for_update_on_startup=false\n'
            'approval_policy="on-request"\nsandbox_mode="read-only"\n'
            '[model_providers.probe]\nname="Local fixture"\n'
            f'base_url="{fixture.base_url}"\nwire_api="responses"\nrequires_openai_auth=false\n'
            f'[projects.{json.dumps(str(workspace))}]\ntrust_level="trusted"\n'
            '[analytics]\nenabled=false\n[feedback]\nenabled=false\n')
        env = {key: value for key, value in os.environ.items() if not key.startswith(("CODEX_", "OPENAI_"))}
        env.update(HOME=str(root), CODEX_HOME=str(home))
        endpoint = root / "backend.sock"
        log = open(output / "c6-backend.log", "w")
        backend = subprocess.Popen([binary, "app-server", "--listen", f"unix://{endpoint}"], env=env,
            cwd=workspace, stdin=subprocess.DEVNULL, stdout=log, stderr=log, start_new_session=True)
        rpc = None
        try:
            await until(endpoint.exists, "socket ready")
            evidence["socket_is_symlink"] = endpoint.is_symlink()
            rpc = Rpc(await connect(endpoint))
            await initialize(rpc)
            started = await rpc.call("thread/start", {"cwd": str(workspace), "approvalPolicy": "on-request",
                                                     "sandbox": "read-only"})
            thread_id = started["thread"]["id"]
            turn = await start_turn(rpc, thread_id, "COMMAND_ACCEPT after reconnect")
            turn_id = turn["turn"]["id"]
            first = await until(lambda: rpc.approvals[-1] if rpc.approvals else None, "first approval")
            evidence["first_approval"] = {"id": first["id"], "method": first["method"]}
            await rpc.close()
            rpc = None
            await asyncio.sleep(1.0)  # No protocol client while the approval is pending.
            evidence["backend_alive_without_client"] = backend.poll() is None

            rpc = Rpc(await connect(endpoint))
            await initialize(rpc)
            history = await rpc.call("thread/read", {"threadId": thread_id, "includeTurns": True})
            evidence["status_before_resume"] = next(
                (t["status"] for t in history["thread"]["turns"] if t["id"] == turn_id), None)
            await rpc.call("thread/resume", {"threadId": thread_id})
            again = await until(lambda: rpc.approvals[-1] if rpc.approvals else None,
                                "approval re-sent after resume", timeout=15)
            evidence["resent_approval"] = {"id": again["id"], "method": again["method"],
                                           "same_item": again["params"].get("itemId") == first["params"].get("itemId")}
            await rpc.respond(again["id"], "accept")
            await until(lambda: completed(rpc, turn_id), "turn completion after reconnect")
            evidence["turn_completed"] = True
            follow = await start_turn(rpc, thread_id, "AFTER_RESTART")
            await until(lambda: completed(rpc, follow["turn"]["id"]), "follow-up completion")
            evidence["followup_completed"] = True
            evidence["status"] = "passed"
        except Exception as error:
            evidence["status"] = "failed"
            evidence["error"] = f"{type(error).__name__}: {error}"
        finally:
            with contextlib.suppress(Exception):
                if rpc:
                    await rpc.close()
            with contextlib.suppress(ProcessLookupError):
                os.killpg(backend.pid, signal.SIGTERM)
            with contextlib.suppress(subprocess.TimeoutExpired):
                await asyncio.to_thread(backend.wait, 5)
            if backend.poll() is None:
                os.killpg(backend.pid, signal.SIGKILL)
            log.close()
            evidence["model_errors"] = fixture.errors
    return evidence


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--codex", default="codex")
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=True)
    evidence = asyncio.run(reconnect_probe(args.codex, args.output))
    (args.output / "c6.json").write_text(json.dumps(evidence, indent=2))
    print(json.dumps(evidence, indent=2))
    sys.exit(0 if evidence.get("status") == "passed" else 1)


if __name__ == "__main__":
    main()

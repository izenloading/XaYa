"""C7: what a second client and a dropped TUI connection do to one Codex app-server.

Answers the questions that decide whether the native TUI can bypass agentd:
  1. Two clients subscribed to one thread: who receives turn events, who receives an approval
     request, does the other see `serverRequest/resolved`, and where does a pending approval go
     when the client that received it disconnects?
  2. A native TUI (`--remote`) whose connection drops: does it exit, and does it reconnect when
     the endpoint comes back?
Uses the real installed Codex and the loopback model fixture only.
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

from websockets.asyncio.server import unix_serve
from websockets.exceptions import ConnectionClosed

from codex_dual import Rpc, Terminal, completed, connect, initialize, start_turn, until
from model_fixture import ModelFixture


def methods(rpc, since=0):
    return [e.get("method") for e in rpc.events[since:] if e.get("method")]


def approvals(rpc, since=0):
    return [e for e in rpc.events[since:] if e.get("method", "").endswith("/requestApproval")]


async def settle(seconds=1.0):
    await asyncio.sleep(seconds)


async def multiclient(endpoint, workspace, evidence):
    a = Rpc(await connect(endpoint))
    await initialize(a)
    started = await a.call("thread/start", {"cwd": str(workspace), "approvalPolicy": "on-request",
                                           "sandbox": "read-only"})
    thread = started["thread"]["id"]
    # A thread has no rollout to resume until its first turn.
    boot = await start_turn(a, thread, "BOOTSTRAP")
    await until(lambda: completed(a, boot["turn"]["id"]), "bootstrap turn")
    b = Rpc(await connect(endpoint))
    await initialize(b)
    await b.call("thread/resume", {"threadId": thread})
    evidence["second_client_resumes_same_thread"] = True

    # 1a. Turn events started by A.
    mark_a, mark_b = len(a.events), len(b.events)
    turn = await start_turn(a, thread, "TERMINAL_FIRST")
    turn_id = turn["turn"]["id"]
    await until(lambda: completed(a, turn_id), "A sees its turn complete")
    await settle(0.5)
    evidence["turn_by_A"] = {
        "A_methods": sorted(set(methods(a, mark_a))),
        "B_methods": sorted(set(methods(b, mark_b))),
        "B_sees_completion": bool(completed(b, turn_id)),
    }

    # 1b. Turn started by B is seen by A.
    mark_a = len(a.events)
    turn = await start_turn(b, thread, "INJECT_SECOND")
    await until(lambda: completed(b, turn["turn"]["id"]), "B's turn completes")
    await settle(0.5)
    evidence["turn_by_B_seen_by_A"] = bool(completed(a, turn["turn"]["id"]))

    # 1c. Approval: who gets it; B answers; does A hear it resolved?
    mark_a, mark_b = len(a.events), len(b.events)
    turn = await start_turn(a, thread, "COMMAND_ACCEPT first")
    turn_id = turn["turn"]["id"]
    await until(lambda: approvals(a, mark_a) or approvals(b, mark_b), "an approval request")
    await settle(1.0)
    got_a, got_b = approvals(a, mark_a), approvals(b, mark_b)
    record = {"A_received": len(got_a), "B_received": len(got_b),
              "same_request_id": bool(got_a and got_b and got_a[0]["id"] == got_b[0]["id"])}
    answerer, other = (b, a) if got_b else (a, b)
    request = (got_b or got_a)[0]
    await answerer.respond(request["id"], "accept")
    await until(lambda: completed(answerer, turn_id), "turn completes after the answer")
    await settle(0.5)
    record["answered_by"] = "B" if answerer is b else "A"
    record["other_sees_resolved"] = request["id"] in other.resolved
    record["other_sees_completion"] = bool(completed(other, turn_id))
    evidence["approval"] = record

    # 1d. The client that holds a pending approval disconnects: does it move to the other?
    mark_a, mark_b = len(a.events), len(b.events)
    turn = await start_turn(a, thread, "COMMAND_ACCEPT second")
    turn_id = turn["turn"]["id"]
    await until(lambda: approvals(a, mark_a) or approvals(b, mark_b), "second approval")
    await settle(1.0)
    holder, survivor = (a, b) if approvals(a, mark_a) else (b, a)
    survivor_before = len(approvals(survivor, mark_b if survivor is b else mark_a))
    await holder.close()
    await settle(3.0)
    survivor_after = approvals(survivor, mark_b if survivor is b else mark_a)
    record = {"holder": "A" if holder is a else "B",
              "survivor_had_it_before": survivor_before,
              "survivor_has_it_after_disconnect": len(survivor_after)}
    if survivor_after:
        await survivor.respond(survivor_after[-1]["id"], "accept")
        record["turn_completes_via_survivor"] = bool(
            await until(lambda: completed(survivor, turn_id), "completion via survivor"))
    else:
        # Resuming again on the surviving connection is the documented way to get it re-sent.
        mark = len(survivor.events)
        await survivor.call("thread/resume", {"threadId": thread})
        await settle(2.0)
        resent = approvals(survivor, mark)
        record["re_sent_after_resume_on_survivor"] = len(resent)
        if resent:
            await survivor.respond(resent[-1]["id"], "accept")
            record["turn_completes_via_survivor"] = bool(
                await until(lambda: completed(survivor, turn_id), "completion after resume"))
    evidence["approval_holder_disconnects"] = record
    await survivor.close()
    return thread


def screen(terminal, since):
    import re
    raw = bytes(terminal.output[since:])
    clean = re.sub(rb"\x1b\[[0-?]*[ -/]*[@-~]", b"", raw)
    clean = re.sub(rb"\x1b\][^\x07\x1b]*(?:\x07|\x1b\\\\)", b"", clean)
    return " ".join(clean.decode("utf-8", "replace").split())[-400:]


async def tui_drop(binary, env, endpoint, workspace, thread, root, fixture, evidence):
    relay_path = endpoint.with_name("relay.sock")
    sessions = []  # Per TUI connection: the methods it sent.
    live = []

    async def relay(downstream):
        sent = []
        sessions.append(sent)
        live.append(downstream)
        upstream = await connect(endpoint)

        async def pump(source, sink, record):
            with contextlib.suppress(ConnectionClosed):
                async for raw in source:
                    if record is not None:
                        method = json.loads(raw).get("method")
                        if method and len(record) < 12:
                            record.append(method)
                    await sink.send(raw)
        tasks = [asyncio.create_task(pump(downstream, upstream, sent)),
                 asyncio.create_task(pump(upstream, downstream, None))]
        await asyncio.wait(tasks, return_when=asyncio.FIRST_COMPLETED)
        for task in tasks:
            task.cancel()
        await upstream.close()

    async def serve():
        return await unix_serve(relay, path=str(relay_path), max_size=None, compression=None)

    async def drop(server):
        # The relay (agentd's role) dies: its listener and the TUI's connection go away.
        server.close()
        for connection in list(live):
            await connection.close()
        live.clear()
        relay_path.unlink(missing_ok=True)

    server = await serve()
    terminal = Terminal([binary, "--remote", f"unix://{relay_path}", "--no-alt-screen", "resume",
                         thread, "TERMINAL_THIRD"], env, workspace, root / "c7-tui.bin")
    result = evidence["tui_after_drop"] = {}
    try:
        await until(lambda: terminal.contains("ACK_TERMINAL_THIRD_COMPLETE"), "TUI turn", timeout=40)
        result["first_connection_methods"] = sessions[0]
        await settle(0.5)

        # Idle drop, 5 s.
        mark = len(terminal.output)
        await drop(server)
        await settle(5.0)
        result["idle_drop"] = {"exited": terminal.process.poll() is not None,
                               "screen_during_outage": screen(terminal, mark)}
        server = await serve()
        count = len(sessions)
        back = await until(lambda: len(sessions) > count, "reconnect", timeout=30)
        result["idle_drop"]["reconnected"] = bool(back)
        await settle(1.0)
        result["idle_drop"]["reconnect_methods"] = sessions[-1]
        mark = len(terminal.output)
        terminal.write("FINAL_NATIVE")
        await settle(0.3)
        terminal.write("\r")
        ok = await until(lambda: terminal.contains("ACK_FINAL_NATIVE_COMPLETE", mark), "turn after reconnect", timeout=30)
        result["idle_drop"]["native_turn_after_reconnect"] = bool(ok)

        # Drop while a turn streams; the turn finishes while no TUI is connected.
        await settle(0.5)
        mark = len(terminal.output)
        terminal.write("SLOW_CANCEL")
        await settle(0.3)
        terminal.write("\r")
        await asyncio.to_thread(fixture.slow_started.wait, 20)
        await drop(server)
        fixture.release.set()
        await settle(3.0)
        server = await serve()
        count = len(sessions)
        await until(lambda: len(sessions) > count, "reconnect after mid-turn drop", timeout=30)
        shown = await until(lambda: terminal.contains("ACK_SLOW_CANCEL_COMPLETE", mark), "turn shown", timeout=20)
        result["mid_turn_drop"] = {"reconnected": True,
                                   "completed_turn_shown_after_reconnect": bool(shown),
                                   "reconnect_methods": sessions[-1]}

        # Long outage: 60 s with nothing listening.
        await settle(0.5)
        mark = len(terminal.output)
        await drop(server)
        await settle(60.0)
        result["long_outage_60s"] = {"exited": terminal.process.poll() is not None,
                                     "exit_code": terminal.process.returncode,
                                     "screen_during_outage": screen(terminal, mark)}
        if terminal.process.poll() is None:
            server = await serve()
            count = len(sessions)
            back = await until(lambda: len(sessions) > count, "reconnect after 60 s", timeout=40)
            result["long_outage_60s"]["reconnected"] = bool(back)
            server.close()
    except Exception as error:
        result["error"] = f"{type(error).__name__}: {error}"
        result["screen_tail"] = screen(terminal, max(0, len(terminal.output) - 4000))
        raise
    finally:
        await terminal.close()


async def probe(binary, output):
    evidence = {"codex": subprocess.run([binary, "--version"], capture_output=True,
                                        text=True).stdout.strip()}
    with ModelFixture() as fixture, tempfile.TemporaryDirectory(prefix="xm-", dir="/tmp") as tmp:
        root = Path(tmp)
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
        env = {k: v for k, v in os.environ.items() if not k.startswith(("CODEX_", "OPENAI_"))}
        env.update(HOME=str(root), CODEX_HOME=str(home))
        endpoint = root / "backend.sock"
        log = open(output / "c7-backend.log", "w")
        backend = subprocess.Popen([binary, "app-server", "--listen", f"unix://{endpoint}"],
                                   env=env, cwd=workspace, stdin=subprocess.DEVNULL, stdout=log,
                                   stderr=log, start_new_session=True)
        try:
            await until(endpoint.exists, "socket ready")
            thread = await multiclient(endpoint, workspace, evidence)
            await tui_drop(binary, env, endpoint, workspace, thread, output, fixture, evidence)
            evidence["status"] = "completed"
        except Exception as error:
            evidence["status"] = "failed"
            evidence["error"] = f"{type(error).__name__}: {error}"
        finally:
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
    evidence = asyncio.run(probe(args.codex, args.output))
    (args.output / "c7.json").write_text(json.dumps(evidence, indent=2))
    print(json.dumps(evidence, indent=2, ensure_ascii=False))
    sys.exit(0 if evidence.get("status") == "completed" else 1)


if __name__ == "__main__":
    main()

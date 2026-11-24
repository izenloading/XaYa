#!/usr/bin/env python3
"""O1-O7: plain Claude TUI in a PTY XaYa owns, with Mods, instead of `--bg` + attach.

Uses the real installed Claude, a loopback model fixture and an isolated configuration; no account.
Each check records what was actually observed; ownership survivors are evidence, not failures.
"""

import argparse
import asyncio
import contextlib
import json
import os
from pathlib import Path
import secrets
import shlex
import shutil
import subprocess
import sys
import time
import traceback
import uuid

from acceptance import match_boundaries
from claude_bridge import ClaudeBridge
from claude_fixture import ClaudeFixture, has_native_prompt
from codex_dual import Terminal, until
from owned_pty import describe, ownership, snapshot, terminate_owned
from run import isolated_env

CHECKS = ("O1", "O2", "O3", "O4", "O5", "O6", "O7")
LATE_AFTER = 20

TOOL = """import json, os, pathlib, sys, time
root = pathlib.Path(sys.argv[1])
pid = os.getpid()
read, write = os.pipe()
if os.fork() == 0:
    os.setsid()
    if os.fork() == 0:
        os.close(read)
        os.write(write, str(os.getpid()).encode())
        os.close(write)
        time.sleep(%(late)d)
        (root / f"late-grandchild-{pid}").write_text("late")
        os._exit(0)
    os._exit(0)
os.close(write)
grandchild = int(os.read(read, 32))
os.wait()
(root / f"tool-{pid}.json").write_text(json.dumps({"pid": pid, "grandchild": grandchild,
    "marker_in_env": "XAYA_OWNER" in os.environ, "started": time.time()}))
time.sleep(%(late)d)
(root / f"late-tool-{pid}").write_text("late")
""" % {"late": LATE_AFTER}


def main_turns(bridge, since=0):
    turns = []
    for start in bridge.events[since:]:
        if start.get("type") != "turn.start" or start["data"].get("agentId"):
            continue
        end = next((e for e in bridge.events if e.get("type") == "turn.complete"
                    and e["data"].get("turnId") == start["data"]["turnId"]), None)
        if end:
            turns.append({"id": start["data"]["turnId"], "start": start["received_at"], "end": end["received_at"]})
    return turns


async def probe(binary, root, checks):
    env = isolated_env(root)
    workspace = root / "workspace"
    workspace.mkdir()
    script = root / "tool.py"
    script.write_text(TOOL)
    tool_command = shlex.join([sys.executable, str(script), str(root)])
    marker = "XAYA_OWNER=" + secrets.token_hex(16)
    requested = str(uuid.uuid4())
    terminals = []
    owned = []
    with ClaudeBridge() as bridge, ClaudeFixture(workspace, tool_command) as fixture:
        local_env = dict(env, ANTHROPIC_BASE_URL=fixture.base_url, CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC="1",
                         DISABLE_AUTOUPDATER="1", XAYA_OWNER=marker.split("=", 1)[1])
        config = Path(env["CLAUDE_CONFIG_DIR"])
        (config / ".claude.json").write_text(json.dumps({"hasCompletedOnboarding": True, "theme": "dark",
            "autoUpdates": False, "customApiKeyResponses": {"approved": [env["ANTHROPIC_API_KEY"]], "rejected": []},
            "projects": {str(workspace): {"hasTrustDialogAccepted": True, "hasCompletedProjectOnboarding": True}}}))
        plugin = root / "plugin"
        (plugin / ".claude-plugin").mkdir(parents=True)
        (plugin / "hooks").mkdir()
        (plugin / ".claude-plugin/plugin.json").write_text('{"name":"xaya-owned-probe","version":"0.1.0"}')
        (plugin / "hooks/hooks.json").write_text('{"modules":["./register.js"]}')
        shutil.copyfile(Path(__file__).with_name("claude_mod.js"), plugin / "hooks/register.js")
        (plugin / "hooks/config.js").write_text(f"export const endpoint={json.dumps(bridge.endpoint)};\n"
                                                f"export const token={json.dumps(bridge.token)};\n")
        settings = root / "settings.json"
        settings.write_text(json.dumps({"permissions": {"defaultMode": "manual"},
            "hooks": {"PermissionRequest": [{"hooks": [{"type": "http", "url": bridge.endpoint + "/hooks",
                "headers": {"X-Probe-Token": bridge.token}, "timeout": 25}]}]}}))
        base = [binary, "--model", "claude-sonnet-4-6", "--tools", "Write,Bash,TaskStop", "--permission-mode",
                "manual", "--setting-sources", "user", "--strict-mcp-config", "--plugin-dir", str(plugin),
                "--settings", str(settings)]

        def launch(extra, name):
            terminal = Terminal([*base, *extra, "--debug-file", str(root / f"{name}-debug.log")],
                                local_env, workspace, root / f"{name}.bin")
            terminals.append(terminal)
            return terminal

        async def identity(index):
            return await until(lambda: ([e for e in bridge.events if e.get("type") == "session.start"]
                                        [index:index + 1] or [None])[0], f"Mods session.start #{index + 1}", timeout=30)

        async def turn(text):
            start = await until(lambda: next((e for e in bridge.events if e.get("type") == "turn.start"
                and e["data"].get("text") == text), None), text + " start", timeout=40)
            end = await until(lambda: next((e for e in bridge.events if e.get("type") == "turn.complete"
                and e["data"].get("turnId") == start["data"]["turnId"]), None), text + " complete", timeout=40)
            return start, end

        async def typed(terminal, text):
            terminal.write(text)
            await asyncio.sleep(0.3)
            terminal.write("\r")
            return await turn(text)

        async def command(action):
            identifier = bridge.enqueue(action)
            ack = await until(lambda: bridge.acks.get(identifier), "Mods ack " + action["type"], timeout=40)
            if not ack.get("ok"):
                raise RuntimeError(f"Mods command failed: {ack}")
            return ack

        async def background_tool(text, seen):
            await command({"type": "send", "text": text})
            pending = await until(lambda: next((p for p in bridge.permissions if p["state"] == "pending"
                and "Local disposable delayed writer" in json.dumps(p["event"])), None), "Bash permission")
            bridge.answer(pending["id"], "allow")
            report = await until(lambda: next((p for p in root.glob("tool-*.json") if p not in seen), None),
                                 "background tool started")
            seen.add(report)
            tool = json.loads(report.read_text())
            use = await until(lambda: next((e for e in reversed(bridge.events) if e.get("type") == "tool.start"
                and e["data"].get("tool") == "Bash"), None), "Bash tool.start")
            done = await until(lambda: next((e for e in bridge.events if e.get("type") == "tool.complete"
                and e["data"].get("toolUseId") == use["data"]["tool_use_id"]), None), "Bash tool.complete")
            tool["task_id"] = ((done["data"].get("result") or {}).get("result") or {}).get("backgroundTaskId")
            return tool

        def owned_entries(pids):
            entries = snapshot(marker)
            return [e for e in entries if e.pid in pids], entries

        async def cleanup(pids, label):
            survivors, _ = owned_entries(pids)
            result = terminate_owned(survivors, marker)
            await until(lambda: not owned_entries(pids)[0], label + " owned processes gone", timeout=10)
            return result

        try:
            # O1: explicit native identity is honoured in a plain TUI (unlike --bg).
            terminal = launch(["--session-id", requested], "owned-tui")
            root_pid = terminal.process.pid
            start_identity = await identity(0)
            await asyncio.sleep(0.5)
            await typed(terminal, "XAYA_CLAUDE_OWNED_BOOT")
            boot = next(r for r in fixture.requests if has_native_prompt(r, "XAYA_CLAUDE_OWNED_BOOT"))
            model_session = json.loads(boot["metadata"]["user_id"]).get("session_id")
            checks["O1"] = {"status": "passed" if start_identity["sessionId"] == requested == model_session
                            else "failed", "evidence": {"requested": requested, "mods": start_identity["sessionId"],
                            "model_request": model_session, "pty_root_pid": root_pid}}

            # O2: Mods streaming text, program send and native input in the same owned process.
            start, _ = await turn("XAYA_CLAUDE_OWNED_BOOT")
            chunks = [e for e in bridge.events if e.get("type") == "step.chunk"
                      and e["data"].get("turnId") == start["data"]["turnId"] and e["data"]["chunk"].get("kind") == "text"]
            finished = next(o for o in fixture.observations if o["marker"] == "XAYA_CLAUDE_OWNED_BOOT")["finished_at"]
            await command({"type": "send", "text": "XAYA_CLAUDE_PROGRAM_SEND"})
            await turn("XAYA_CLAUDE_PROGRAM_SEND")
            await until(lambda: terminal.contains("ACK_XAYA_CLAUDE_PROGRAM_SEND_COMPLETE"), "program send visible")
            await asyncio.sleep(0.3)
            await typed(terminal, "XAYA_CLAUDE_NATIVE_NEXT")
            await asyncio.sleep(0.5)
            o2 = {"streamed_before_model_finished": any(e["received_at"] < finished for e in chunks),
                  "exact_text": "".join(e["data"]["chunk"]["text"] for e in chunks) == "ACK_XAYA_CLAUDE_OWNED_BOOT_COMPLETE",
                  "program_send_visible_in_tui": True, "native_input_after_program_send": True}
            checks["O2"] = {"status": "passed" if all(o2.values()) else "failed", "evidence": o2}

            # O3: OSC 7501 stays available beside Mods in a plain TUI (bg workers disable it).
            turns = main_turns(bridge)
            try:
                checks["O3"] = {"status": "passed", "evidence": {"matches": match_boundaries(
                    list(terminal.osc.cycles), turns), "turns": len(turns)}}
            except ValueError as error:
                checks["O3"] = {"status": "failed", "evidence": {"error": str(error),
                    "osc_reports": len(terminal.osc.reports), "turns": len(turns)}}
            (root / "osc.json").write_text(json.dumps({"reports": terminal.osc.reports,
                "cycles": terminal.osc.cycles, "turns": turns}, indent=2))

            # O4: a background Bash and its reparented grandchild are attributable to the owned PTY.
            seen = set()
            # The fixture holds the post-tool reply open, so the snapshot sees the task while it runs.
            tool = await background_tool("XAYA_CLAUDE_CANCEL_TOOL", seen)
            owned += [tool["pid"], tool["grandchild"]]
            live, entries = owned_entries({tool["pid"], tool["grandchild"]})
            view = ownership(entries, root_pid)
            o4 = {"task_id": tool["task_id"], "marker_in_tool_env": tool["marker_in_env"],
                  "tool_in_tree": tool["pid"] in view["tree"], "tool_marked": tool["pid"] in view["marked"],
                  "grandchild_marked": tool["grandchild"] in view["marked"],
                  "grandchild_reparented_outside_tree": tool["grandchild"] in view["reparented"],
                  "unmarked_descendants": describe(entries, view["unmarked_descendants"]),
                  "processes": describe(entries, {tool["pid"], tool["grandchild"]})}
            required = ("task_id", "marker_in_tool_env", "tool_in_tree", "tool_marked", "grandchild_marked")
            checks["O4"] = {"status": "passed" if all(o4[k] for k in required) else "failed", "evidence": o4}

            # O5: what TaskStop actually leaves behind, then identity-checked cleanup by the owner.
            stopped = await command({"type": "stop-task", "taskId": tool["task_id"]})
            await asyncio.sleep(3)
            after_stop, _ = owned_entries({tool["pid"], tool["grandchild"]})
            fixture.holds["XAYA_CLAUDE_CANCEL_TOOL"].set()
            await turn("XAYA_CLAUDE_CANCEL_TOOL")
            cleaned = await cleanup({tool["pid"], tool["grandchild"]}, "TaskStop")
            await asyncio.sleep(max(0, tool["started"] + LATE_AFTER + 1 - time.time()))
            late = sorted(p.name for p in root.glob(f"late-*-{tool['pid']}"))
            notices = [e["data"]["text"] for e in bridge.events if e.get("type") == "turn.start"
                       and tool["task_id"] in str(e["data"].get("text", ""))]
            o5 = {"taskstop_result": stopped.get("result"), "survivors_after_taskstop": describe(after_stop,
                  {e.pid for e in after_stop}), "owner_cleanup": cleaned, "late_writes": late,
                  "task_notifications": notices}
            checks["O5"] = {"status": "passed" if not late and tool["pid"] not in {e.pid for e in after_stop}
                            else "failed", "evidence": o5}

            # O6: losing the PTY owner (master closed) — which owned processes outlive it.
            second = await background_tool("XAYA_CLAUDE_CANCEL_TOOL", seen)
            owned += [second["pid"], second["grandchild"]]
            await asyncio.sleep(1)
            asyncio.get_running_loop().remove_reader(terminal.master)
            os.close(terminal.master)
            exited = True
            try:
                await asyncio.to_thread(terminal.process.wait, 10)
            except subprocess.TimeoutExpired:
                exited = False
            terminals.remove(terminal)
            terminal.log.close()
            after_hup, entries = owned_entries({second["pid"], second["grandchild"]})
            leftover = ownership(entries, root_pid)
            cleaned = await cleanup({second["pid"], second["grandchild"]} | leftover["marked"], "PTY loss")
            await asyncio.sleep(max(0, second["started"] + LATE_AFTER + 1 - time.time()))
            late = sorted(p.name for p in root.glob(f"late-*-{second['pid']}"))
            o6 = {"claude_exited_on_hangup": exited, "survivors_after_hangup": describe(entries,
                  {e.pid for e in after_hup}), "other_marked_survivors": sorted(leftover["marked"] -
                  {second["pid"], second["grandchild"]}), "owner_cleanup": cleaned, "late_writes": late}
            checks["O6"] = {"status": "passed" if exited and not late else "failed", "evidence": o6}

            # O7: an explicit --resume in a fresh owned PTY continues the same native conversation.
            resumed = launch(["--resume", requested], "owned-resume")
            again = await identity(1)
            await asyncio.sleep(0.5)
            await typed(resumed, "XAYA_CLAUDE_RESUMED")
            request = next(r for r in reversed(fixture.requests) if has_native_prompt(r, "XAYA_CLAUDE_RESUMED"))
            o7 = {"mods_session_id": again["sessionId"],
                  "model_session_id": json.loads(request["metadata"]["user_id"]).get("session_id"),
                  "prior_context": all(m in json.dumps(request["messages"])
                                       for m in ("XAYA_CLAUDE_OWNED_BOOT", "XAYA_CLAUDE_NATIVE_NEXT"))}
            checks["O7"] = {"status": "passed" if o7["mods_session_id"] == requested == o7["model_session_id"]
                            and o7["prior_context"] else "failed", "evidence": o7}

            launches = [i for i, e in enumerate(bridge.events) if e.get("type") == "session.start"]
            for begin, end in zip(launches, launches[1:] + [len(bridge.events)]):
                sequence = [e["sequence"] for e in bridge.events[begin:end]]
                if sequence != list(range(1, len(sequence) + 1)) or any(e.get("missed") for e in bridge.events[begin:end]):
                    checks["O2"]["status"] = "failed"
                    checks["O2"]["evidence"]["events_contiguous"] = False
            checks["O2"]["evidence"].setdefault("events_contiguous", True)
            if fixture.errors:
                raise RuntimeError(str(fixture.errors))
        finally:
            for terminal in terminals:
                with contextlib.suppress(Exception):
                    await terminal.close()
            with contextlib.suppress(Exception):
                leftover, _ = owned_entries(set(owned))
                terminate_owned(leftover, marker)
            (root / "events.json").write_text(json.dumps(bridge.events, indent=2))
            (root / "permissions.json").write_text(json.dumps(bridge.permissions, indent=2))
            (root / "acks.json").write_text(json.dumps(bridge.acks, indent=2))
            (root / "model-requests.json").write_text(json.dumps(fixture.requests, indent=2))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True, help="New directory outside any Git checkout")
    parser.add_argument("--claude-bin", default="claude")
    args = parser.parse_args()
    root = args.output.resolve()
    root.mkdir(parents=True, exist_ok=False)
    root.chmod(0o700)
    if subprocess.run(["git", "-C", str(root), "rev-parse"], capture_output=True).returncode == 0:
        parser.error("--output must be outside any Git checkout")
    binary = shutil.which(args.claude_bin)
    if binary is None:
        parser.error("Claude executable not found")
    report = {"date": time.strftime("%Y-%m-%d"), "checks": {key: {"status": "not_run"} for key in CHECKS},
              "claude_version": subprocess.run([binary, "--version"], capture_output=True, text=True).stdout.strip()}
    try:
        asyncio.run(probe(binary, root, report["checks"]))
    except Exception as error:
        report["error"] = f"{type(error).__name__}: {error}"
        (root / "error.log").write_text(traceback.format_exc())
    (root / "report.json").write_text(json.dumps(report, indent=2))
    print(json.dumps({k: v["status"] for k, v in report["checks"].items()} | {"error": report.get("error")}))
    sys.exit(0 if all(v["status"] == "passed" for v in report["checks"].values()) else 1)


if __name__ == "__main__":
    main()

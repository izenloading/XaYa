"""A2-A5 real background worker probes with injected Mods and HTTP hooks."""

import asyncio
import hashlib
import json
import os
from pathlib import Path
import shlex
import shutil
import sys
import time

from claude_background import process_alive, run_cli, session_id
from claude_bridge import ClaudeBridge
from claude_fixture import ClaudeFixture, has_native_prompt
from codex_dual import Terminal, until
from acceptance import match_boundaries


async def daemon_reply(binary, env, workspace, config, root, job_id, text):
    """Probe-only private protocol: validate a socket derived from this isolated config."""
    status = await run_cli(binary, env, workspace, "daemon", "status")
    (root / "mods-daemon-status.txt").write_text(status)
    digest = hashlib.sha256(str(config.resolve()).encode()).hexdigest()[:8]
    directory = Path("/tmp") / f"cc-daemon-{os.getuid()}" / digest
    endpoint = directory / "control.sock"
    if str(directory) not in status or endpoint.stat().st_uid != os.getuid():
        raise RuntimeError("Cannot verify the isolated Claude daemon endpoint")
    auth = (config / "daemon/control.key").read_text().strip()
    async with asyncio.timeout(8):
        reader, writer = await asyncio.open_unix_connection(str(endpoint))
        try:
            writer.write((json.dumps({"proto": 1, "op": "reply", "short": job_id,
                "text": text, "nextTurn": True, "auth": auth}) + "\n").encode())
            await writer.drain()
            result = json.loads(await reader.readline())
            if not result.get("ok"):
                raise RuntimeError("Isolated daemon rejected semantic reply")
            return result
        finally:
            writer.close()
            await writer.wait_closed()


async def mods_probe(binary, env, root, checks):
    workspace = root / "mods-workspace"
    workspace.mkdir()
    delayed = root / "delayed-writer.py"
    delayed.write_text(
        "import json, os, pathlib, time\n"
        f"root=pathlib.Path({str(root)!r})\n"
        "(root/'tool-pid.json').write_text(json.dumps({'pid':os.getpid(),'started':time.monotonic()}))\n"
        "time.sleep(15)\n(root/'tool-finished').write_text('late write')\n")
    tool_command = shlex.join([sys.executable, str(delayed)])
    terminal = None
    launched = False
    job_id = worker_pid = tool_pid = None
    with ClaudeBridge() as bridge, ClaudeFixture(workspace, tool_command) as fixture:
        local_env = dict(env, ANTHROPIC_BASE_URL=fixture.base_url,
            CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC="1", DISABLE_AUTOUPDATER="1")
        config = Path(env["CLAUDE_CONFIG_DIR"])
        (config / ".claude.json").write_text(json.dumps({"hasCompletedOnboarding": True,
            "theme": "dark", "autoUpdates": False, "customApiKeyResponses": {
                "approved": [env["ANTHROPIC_API_KEY"]], "rejected": []},
            "projects": {str(workspace): {"hasTrustDialogAccepted": True, "hasCompletedProjectOnboarding": True}}}))
        plugin = root / "mods-plugin"
        (plugin / ".claude-plugin").mkdir(parents=True)
        (plugin / "hooks").mkdir()
        (plugin / ".claude-plugin/plugin.json").write_text(json.dumps({"name": "xaya-p0", "version": "0.1.0"}))
        (plugin / "hooks/hooks.json").write_text(json.dumps({"modules": ["./register.js"]}))
        shutil.copyfile(Path(__file__).with_name("claude_mod.js"), plugin / "hooks/register.js")
        (plugin / "hooks/config.js").write_text(
            f"export const endpoint = {json.dumps(bridge.endpoint)};\nexport const token = {json.dumps(bridge.token)};\n")
        (plugin / "hooks/config.js").chmod(0o600)
        settings = {"permissions": {"defaultMode": "manual"}, "env": {
            key: local_env[key] for key in ("ANTHROPIC_API_KEY", "ANTHROPIC_BASE_URL",
                "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "DISABLE_AUTOUPDATER")},
            "hooks": {"PermissionRequest": [{"hooks": [{"type": "http", "url": bridge.endpoint + "/hooks",
                "headers": {"X-Probe-Token": bridge.token}, "timeout": 25}]}]}}
        (config / "settings.json").write_text(json.dumps(settings))

        async def command(action, allow_drop=False):
            identifier = bridge.enqueue(action)
            ack = await until(lambda: bridge.acks.get(identifier), f"Mods ACK {action['type']}")
            if not ack.get("ok"):
                raise RuntimeError(f"Mods command failed: {ack}")
            if action["type"] == "send" and not allow_drop and (ack.get("result") or {}).get("drop"):
                raise RuntimeError(f"Mods prompt was dropped: {ack}")
            return ack

        async def turn(marker):
            start = await until(lambda: next((e for e in bridge.events if e.get("type") == "turn.start"
                and marker in str(e.get("data", {}).get("text", ""))), None), f"{marker} turn start")
            end = await until(lambda: next((e for e in bridge.events if e.get("type") == "turn.complete"
                and e["data"].get("turnId") == start["data"]["turnId"]), None), f"{marker} turn complete")
            return start, end

        async def permission(marker):
            return await until(lambda: next((p for p in bridge.permissions
                if marker in json.dumps(p["event"])), None), marker + " permission")

        try:
            launched = True
            output = await run_cli(binary, local_env, workspace, "--model", "claude-sonnet-4-6",
                "--tools", "Write,Bash,TaskStop", "--permission-mode", "manual", "--setting-sources", "user",
                "--strict-mcp-config", "--plugin-dir", str(plugin), "--bg", "--name", "xaya-mods",
                "--settings", str(config / "settings.json"), "--debug-file", str(root / "mods-debug.log"),
                "XAYA_CLAUDE_MOD_BOOT")
            (root / "mods-launch.txt").write_text(output)
            async with asyncio.timeout(20):
                while not worker_pid:
                    entries = json.loads(await run_cli(binary, local_env, workspace, "agents", "--json", "--all"))
                    entry = next((e for e in entries if e.get("cwd") == str(workspace)), None)
                    if entry:
                        job_id, worker_pid = entry.get("id"), entry.get("pid")
                        native_id = session_id(entry)
                    if not worker_pid:
                        await asyncio.sleep(0.2)
            terminal = Terminal([binary, "attach", job_id], local_env, workspace, root / "mods-tui.bin")
            identity = await until(lambda: next((e for e in bridge.events if e.get("type") == "session.start"), None),
                                   "Mods session.start", timeout=25)
            if identity["sessionId"] != native_id:
                raise RuntimeError("Mods native session ID disagrees with agents listing")
            await turn("XAYA_CLAUDE_MOD_BOOT")
            await until(lambda: terminal.contains("ACK_XAYA_CLAUDE_MOD_BOOT_COMPLETE"), "Mods attach ready")
            start, _ = await turn("XAYA_CLAUDE_MOD_BOOT")
            chunks = [e for e in bridge.events if e.get("type") == "step.chunk"
                and e["data"].get("turnId") == start["data"]["turnId"] and e["data"]["chunk"].get("kind") == "text"]
            observation = next(o for o in fixture.observations if o["marker"] == "XAYA_CLAUDE_MOD_BOOT")
            if not chunks or not any(e["received_at"] < observation["finished_at"] for e in chunks):
                raise RuntimeError("No live Mods text before model stream completion")
            if "".join(e["data"]["chunk"]["text"] for e in chunks) != "ACK_XAYA_CLAUDE_MOD_BOOT_COMPLETE":
                raise RuntimeError("Mods text did not match actual model stream")
            checks["A2"] = {"status": "passed", "evidence": {"native_session_id": native_id,
                "worker_pid": worker_pid, "streamed_before_model_finished": True, "exact_text": True}}
            a1 = checks["A1"]
            if a1["status"] == "not_run":
                a1.update(status="partial", evidence={"specified_uuid_criterion": "not_met_by_bg"})
            a1.setdefault("evidence", {})["mods_native_binding_verified"] = True
            a1["evidence"]["mods_binding_session_id"] = native_id

            await command({"type": "send", "text": "XAYA_CLAUDE_PROGRAM_SEND"})
            await turn("XAYA_CLAUDE_PROGRAM_SEND")
            await until(lambda: terminal.contains("ACK_XAYA_CLAUDE_PROGRAM_SEND_COMPLETE"), "program output in TUI")
            await asyncio.sleep(0.3)
            terminal.write("XAYA_CLAUDE_NATIVE_NEXT")
            await asyncio.sleep(0.3)
            terminal.write("\r")
            await turn("XAYA_CLAUDE_NATIVE_NEXT")
            await asyncio.sleep(0.3)
            native_turns = []
            for marker in ("XAYA_CLAUDE_PROGRAM_SEND", "XAYA_CLAUDE_NATIVE_NEXT"):
                begin, end = await turn(marker)
                native_turns.append({"id": begin["data"]["turnId"], "start": begin["received_at"], "end": end["received_at"]})
            e1 = checks["E1"]
            if e1["status"] == "not_run":
                e1.update(status="partial", evidence={})
            try:
                e1["evidence"]["claude_background"] = {"status": "passed",
                    "matches": match_boundaries(terminal.osc, native_turns)}
            except ValueError as error:
                e1["evidence"]["claude_background"] = {"status": "unsupported" if not terminal.osc.reports else "failed",
                    "error": str(error), "mode": "bg+attach", "osc_reports": len(terminal.osc.reports),
                    "native_turns": len(native_turns), "fallback": "Use Mods turn boundaries for managed sessions"}
            (root / "claude-osc.json").write_text(json.dumps({"reports": terminal.osc.reports,
                "cycles": terminal.osc.cycles, "native_turns": native_turns}, indent=2))
            checks["A4"] = {"status": "partial", "evidence": {"program_send_in_same_worker": True,
                "native_input_after_program_send": True, "native_esc_behavior": "not_run"}}

            approvals = []
            checks["A3"] = {"status": "partial", "evidence": approvals}
            for marker, decision in (("XAYA_CLAUDE_ALLOW", "allow"), ("XAYA_CLAUDE_DENY", "deny")):
                await command({"type": "send", "text": marker})
                pending = await permission(marker)
                if not bridge.answer(pending["id"], decision):
                    raise RuntimeError("Permission expired before controller answer")
                await turn(marker)
                exists = (workspace / (marker + ".txt")).exists()
                if exists != (decision == "allow"):
                    raise RuntimeError("Permission decision did not match filesystem outcome")
                approvals.append({"decision": decision, "file_exists": exists})

            marker = "XAYA_CLAUDE_NATIVE_PERMISSION"
            offset = len(terminal.output)
            await command({"type": "send", "text": marker})
            pending = await permission(marker)
            await until(lambda: terminal.contains("Do you want", offset) or terminal.contains("Yes, allow", offset),
                        "native permission prompt")
            terminal.write("\r")
            await turn(marker)
            if not (workspace / (marker + ".txt")).exists():
                raise RuntimeError("Native permission did not allow the write")
            if bridge.answer(pending["id"], "deny"):
                raise RuntimeError("Late permission answer was accepted after native completion")
            approvals.append({"native_answer": True, "expired_after_turn": True, "late_reply_rejected": True})
            checks["A3"]["status"] = "passed"

            await command({"type": "send", "text": "XAYA_CLAUDE_SLOW"})
            active = await until(lambda: next((e for e in bridge.events if e.get("type") == "turn.start"
                and e["data"].get("text") == "XAYA_CLAUDE_SLOW"), None), "cancel active turn")
            await until(lambda: any(e.get("type") == "step.chunk" and e["data"].get("turnId") ==
                active["data"]["turnId"] for e in bridge.events), "cancel after streaming delta")
            await command({"type": "abort", "turnId": active["data"]["turnId"]})
            _, ended = await turn("XAYA_CLAUDE_SLOW")
            if not ended["data"].get("isAborted"):
                raise RuntimeError("turn.abort did not produce isAborted")
            fixture.holds["XAYA_CLAUDE_SLOW"].set()
            checks["A5"] = {"status": "partial", "evidence": {"stream_abort": True}}

            await command({"type": "send", "text": "XAYA_CLAUDE_CANCEL_TOOL"})
            pending = await permission("Local disposable delayed writer")
            bridge.answer(pending["id"], "allow")
            await until(lambda: (root / "tool-pid.json").exists(), "delayed writer started")
            child = json.loads((root / "tool-pid.json").read_text())
            tool_pid = child["pid"]
            use = await until(lambda: next((e for e in bridge.events if e.get("type") == "tool.start"
                and e["data"].get("tool") == "Bash"), None), "native Bash tool ID")
            result = await until(lambda: next((e for e in bridge.events if e.get("type") == "tool.complete"
                and e["data"].get("toolUseId") == use["data"]["tool_use_id"]), None), "background Bash result")
            task_id = result["data"]["result"]["result"].get("backgroundTaskId")
            if not task_id:
                raise RuntimeError("Native backgroundTaskId missing; refusing to guess from PID")
            tool_turn = next(e for e in bridge.events if e.get("type") == "turn.start" and
                             e["data"].get("text") == "XAYA_CLAUDE_CANCEL_TOOL")
            await command({"type": "abort", "turnId": tool_turn["data"]["turnId"]})
            _, ended = await turn("XAYA_CLAUDE_CANCEL_TOOL")
            if not ended["data"].get("isAborted"):
                raise RuntimeError("Tool turn did not abort")
            stopped = await command({"type": "stop-task", "taskId": task_id})
            if (stopped.get("result") or {}).get("isError") or (stopped.get("result") or {}).get("deny"):
                raise RuntimeError("TaskStop was rejected")
            fixture.holds["XAYA_CLAUDE_CANCEL_TOOL"].set()
            await until(lambda: not process_alive(tool_pid), "TaskStop child exits", timeout=6)
            await asyncio.sleep(max(0, child["started"] + 15.5 - time.monotonic()))
            if (root / "tool-finished").exists():
                raise RuntimeError("Cancelled child performed its delayed write")
            await command({"type": "send", "text": "XAYA_CLAUDE_AFTER_STOP"})
            await turn("XAYA_CLAUDE_AFTER_STOP")
            checks["A5"] = {"status": "passed", "evidence": {"stream_abort": True,
                "task_id_from_native_result": task_id, "child_exited": True, "late_write_absent": True,
                "same_worker_continued": True}}

            terminal.write("XAYA_CLAUDE_NATIVE_ESC")
            await asyncio.sleep(0.3)
            terminal.write("\r")
            esc_start = await until(lambda: next((e for e in bridge.events if e.get("type") == "turn.start"
                and e["data"].get("text") == "XAYA_CLAUDE_NATIVE_ESC"), None), "native Esc active turn")
            await until(lambda: any(e.get("type") == "step.chunk" and e["data"].get("turnId") ==
                esc_start["data"]["turnId"] for e in bridge.events), "native Esc streaming delta")
            terminal.write("\x1b")
            _, esc_end = await turn("XAYA_CLAUDE_NATIVE_ESC")
            if not esc_end["data"].get("isAborted"):
                raise RuntimeError("Native Esc did not abort the stream")
            fixture.holds["XAYA_CLAUDE_NATIVE_ESC"].set()
            dropped = await command({"type": "send", "text": "XAYA_CLAUDE_REFUSED_AFTER_ESC"}, allow_drop=True)
            reason = (dropped.get("result") or {}).get("drop", "")
            if "stopped" not in reason:
                raise RuntimeError(f"Unexpected native Esc prompt-submit behavior: {dropped}")
            await asyncio.sleep(0.3)
            if any(has_native_prompt(r, "XAYA_CLAUDE_REFUSED_AFTER_ESC") for r in fixture.requests):
                raise RuntimeError("Dropped prompt was nevertheless sent to the model")
            checks["A4"]["evidence"].update(native_esc_behavior=reason, dropped_prompt_not_executed=True)
            await daemon_reply(binary, local_env, workspace, config, root, job_id, "XAYA_CLAUDE_ESC_RECOVERY")
            await turn("XAYA_CLAUDE_ESC_RECOVERY")
            await command({"type": "send", "text": "XAYA_CLAUDE_AFTER_RECOVERY"})
            await turn("XAYA_CLAUDE_AFTER_RECOVERY")
            entries = json.loads(await run_cli(binary, local_env, workspace, "agents", "--json", "--all"))
            current = next((e for e in entries if e.get("id") == job_id), None)
            if not current or session_id(current) != native_id or current.get("pid") != worker_pid:
                raise RuntimeError("Recovery changed the native worker identity")
            final_request = next((r for r in reversed(fixture.requests)
                if has_native_prompt(r, "XAYA_CLAUDE_AFTER_RECOVERY")), None)
            if not final_request or json.loads(final_request["metadata"]["user_id"])["session_id"] != native_id:
                raise RuntimeError("Recovery model request has a different native ID")
            checks["A4"]["status"] = "passed"
            checks["A4"]["evidence"].update(private_daemon_reply=True, private_protocol=1,
                same_worker_after_recovery=True, prompt_submit_after_recovery=True)
            if fixture.errors:
                raise RuntimeError(str(fixture.errors))
            if {e.get("sessionId") for e in bridge.events} != {native_id}:
                raise RuntimeError("Mods channel mixed native session identities")
            sequences = [e["sequence"] for e in bridge.events]
            if sequences != list(range(1, len(sequences) + 1)) or any(e.get("missed") for e in bridge.events):
                raise RuntimeError("Mods event sequence has gaps or reordering")
            checks["A2"]["evidence"]["events_contiguous"] = True
        finally:
            try:
                try:
                    if terminal:
                        await terminal.close()
                finally:
                    if launched:
                        try:
                            if job_id:
                                await run_cli(binary, local_env, workspace, "stop", job_id)
                        finally:
                            cleanup = await run_cli(binary, local_env, workspace, "daemon", "stop", "--any")
                            (root / "mods-cleanup.txt").write_text(cleanup)
                            for pid in (worker_pid, tool_pid):
                                if pid:
                                    await until(lambda: not process_alive(pid), "probe worker/tool cleanup", timeout=10)
            finally:
                (root / "mods-events.json").write_text(json.dumps(bridge.events, indent=2))
                (root / "mods-permissions.json").write_text(json.dumps(bridge.permissions, indent=2))
                (root / "mods-acks.json").write_text(json.dumps(bridge.acks, indent=2))
                (root / "mods-model-requests.json").write_text(json.dumps(fixture.requests, indent=2))
                (root / "mods-model-observations.json").write_text(json.dumps(fixture.observations, indent=2))

"""A1 foundations using public background/agents/attach/stop commands."""

import asyncio
import json
import os
from pathlib import Path
import subprocess
import uuid

from claude_fixture import ClaudeFixture, has_native_prompt
from codex_dual import Terminal, until


async def run_cli(binary, env, cwd, *args, timeout=30):
    result = await asyncio.to_thread(subprocess.run, [binary, *args], env=env, cwd=cwd,
                                    capture_output=True, text=True, timeout=timeout)
    if result.returncode:
        raise RuntimeError(f"Claude {' '.join(args)}: {result.stderr or result.stdout}")
    return result.stdout


def session_id(entry):
    return entry.get("sessionId") or entry.get("session_id")


def process_alive(pid):
    try:
        os.kill(pid, 0)
        return True
    except ProcessLookupError:
        return False


async def claude_probe(binary, env, root, checks):
    workspace = root / "claude-workspace"
    workspace.mkdir()
    terminal = None
    launched = False
    job_id = None
    requested_id = str(uuid.uuid4())
    detail = {"requested_session_id": requested_id}
    checks["A1"] = {"status": "partial", "evidence": detail}
    with ClaudeFixture() as fixture:
        local_env = dict(env, ANTHROPIC_BASE_URL=fixture.base_url,
                         CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC="1", DISABLE_AUTOUPDATER="1")
        config = Path(env["CLAUDE_CONFIG_DIR"])
        (config / ".claude.json").write_text(json.dumps({
            "hasCompletedOnboarding": True, "theme": "dark", "autoUpdates": False,
            "customApiKeyResponses": {"approved": [env["ANTHROPIC_API_KEY"]], "rejected": []},
            "projects": {str(workspace): {"hasTrustDialogAccepted": True,
                "hasCompletedProjectOnboarding": True}}}))
        (config / "settings.json").write_text(json.dumps({"permissions": {"defaultMode": "manual"},
            "env": {key: local_env[key] for key in ("ANTHROPIC_API_KEY", "ANTHROPIC_BASE_URL",
                "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "DISABLE_AUTOUPDATER")}}))
        base = ["--model", "claude-sonnet-4-6", "--tools", "", "--permission-mode", "manual",
                "--setting-sources", "user", "--strict-mcp-config"]

        async def listing():
            value = json.loads(await run_cli(binary, local_env, workspace, "agents", "--json", "--all"))
            (root / "claude-agents.json").write_text(json.dumps(value, indent=2))
            return value

        try:
            baseline = await run_cli(binary, local_env, workspace, *base, "-p", "XAYA_CLAUDE_BASELINE",
                "--output-format", "stream-json", "--include-partial-messages", "--verbose")
            (root / "claude-baseline.jsonl").write_text(baseline)
            events = [json.loads(line) for line in baseline.splitlines() if line.strip()]
            if not any(e.get("type") == "result" and not e.get("is_error") and
                       "ACK_XAYA_CLAUDE_BASELINE_COMPLETE" in e.get("result", "") for e in events):
                raise RuntimeError("Claude headless baseline did not complete")
            detail["headless_baseline"] = True
            launched = True
            output = await run_cli(binary, local_env, workspace, *base, "--bg", "--session-id", requested_id,
                                   "--name", "xaya-fixture", "XAYA_CLAUDE_BACKGROUND")
            (root / "claude-background.txt").write_text(output)
            detail["background_output"] = output
            entries = await listing()
            entries = entries if isinstance(entries, list) else entries.get("agents", [])
            matching = [e for e in entries if session_id(e) and e.get("cwd") == str(workspace)]
            if not matching:
                matching = [e for e in entries if session_id(e)]
            if len(matching) != 1:
                raise RuntimeError(f"Expected one isolated background session, got {entries}")
            entry = matching[0]
            native_id = session_id(entry)
            job_id = entry.get("id") or entry.get("shortId") or native_id
            async with asyncio.timeout(20):
                while not isinstance(entry.get("pid"), int):
                    await asyncio.sleep(0.2)
                    entries = await listing()
                    entries = entries if isinstance(entries, list) else entries.get("agents", [])
                    entry = next((e for e in entries if session_id(e) == native_id), {})
            os.kill(entry["pid"], 0)
            detail.update(native_session_id=native_id, job_id=job_id,
                          requested_id_honored=native_id == requested_id, worker_pid=entry.get("pid"))
            terminal = Terminal([binary, "attach", job_id], local_env, workspace, root / "claude-attach.bin")
            await until(lambda: terminal.contains("ACK_XAYA_CLAUDE_BACKGROUND_COMPLETE"),
                        "background response visible after attach", timeout=40)
            terminal.write("XAYA_CLAUDE_ATTACHED")
            await asyncio.sleep(0.3)
            terminal.write("\r")
            await until(lambda: terminal.contains("ACK_XAYA_CLAUDE_ATTACHED_COMPLETE"),
                        "attached native input completes", timeout=40)
            terminal.write("\x1a")
            await until(lambda: terminal.process.poll() is not None, "Ctrl+Z detaches terminal")
            await terminal.close()
            terminal = None
            entries = await listing()
            entries = entries if isinstance(entries, list) else entries.get("agents", [])
            current = next((e for e in entries if session_id(e) == native_id), None)
            if current is None:
                raise RuntimeError("Background session disappeared after detach")
            if current.get("pid") != entry["pid"]:
                raise RuntimeError("Worker PID changed after detach")
            os.kill(current["pid"], 0)
            terminal = Terminal([binary, "attach", job_id], local_env, workspace, root / "claude-reattach.bin")
            await until(lambda: terminal.contains("ACK_XAYA_CLAUDE_ATTACHED_COMPLETE"),
                        "reattach keeps same conversation", timeout=40)
            terminal.write("XAYA_CLAUDE_REATTACHED")
            await asyncio.sleep(0.3)
            terminal.write("\r")
            await until(lambda: terminal.contains("ACK_XAYA_CLAUDE_REATTACHED_COMPLETE"),
                        "reattached native input completes", timeout=40)
            entries = await listing()
            entries = entries if isinstance(entries, list) else entries.get("agents", [])
            current = next((e for e in entries if session_id(e) == native_id), None)
            if not current or current.get("pid") != entry["pid"]:
                raise RuntimeError("Reattach changed the native worker identity")
            final_request = next((request for request in reversed(fixture.requests)
                if has_native_prompt(request, "XAYA_CLAUDE_REATTACHED")), None)
            if not final_request or not all(marker in json.dumps(final_request["messages"])
                for marker in ("XAYA_CLAUDE_BACKGROUND", "XAYA_CLAUDE_ATTACHED")):
                raise RuntimeError("Reattached request lost prior conversation context")
            metadata = json.loads(final_request["metadata"]["user_id"])
            if metadata.get("session_id") != native_id:
                raise RuntimeError("Model request belongs to a different native session")
            detail.update(attach_completed=True, native_input_completed=True, detached_and_reattached=True,
                          worker_pid_preserved=True, reattached_input_completed=True,
                          previous_context_preserved=True, native_id_verified_in_model_request=True)
            if fixture.errors:
                raise RuntimeError(str(fixture.errors))
            # The design's requested-UUID criterion is not met if --bg owns the ID.
            checks["A1"] = {"status": "passed" if native_id == requested_id else "partial", "evidence": detail}
        finally:
            try:
                try:
                    if terminal:
                        await terminal.close()
                finally:
                    if launched:
                        try:
                            if job_id:
                                stopped = await run_cli(binary, local_env, workspace, "stop", job_id)
                                (root / "claude-stop.txt").write_text(stopped)
                        finally:
                            cleanup = await run_cli(binary, local_env, workspace, "daemon", "stop", "--any")
                            (root / "claude-cleanup.txt").write_text(cleanup)
                            if detail.get("worker_pid"):
                                await until(lambda: not process_alive(detail["worker_pid"]),
                                            "isolated Claude worker exits after cleanup", timeout=10)
                                detail["worker_stopped_after_cleanup"] = True
            finally:
                (root / "claude-model-requests.json").write_text(json.dumps(fixture.requests, indent=2))

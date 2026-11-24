#!/usr/bin/env python3
"""Real Claude TUI in an XaYa-owned PTY + Rust service, against a loopback model; no account."""
import argparse
import asyncio
import json
import os
import signal
from pathlib import Path
import shutil
import subprocess
import tempfile
import shlex
import sys
import threading
import time

from claude_owned_pty import TOOL, LATE_AFTER
from claude_fixture import ClaudeFixture, has_native_prompt
from codex_dual import Terminal, until
from run import isolated_env


async def exercise(xaya, claude, root):
    env = isolated_env(root)
    workspace = root / "workspace"
    workspace.mkdir()
    for args in (("init", "-q"), ("config", "user.name", "XaYa acceptance"),
                 ("config", "user.email", "acceptance@xaya.invalid")):
        subprocess.run(["git", *args], cwd=workspace, check=True, capture_output=True)
    (workspace / "source").write_text("base\n")
    subprocess.run(["git", "add", "source"], cwd=workspace, check=True)
    subprocess.run(["git", "-c", "core.hooksPath=/dev/null", "commit", "-qm", "base"], cwd=workspace, check=True)
    subprocess.run(["git", "branch", "claude-target"], cwd=workspace, check=True)
    # Native bg workers require an already isolated checkout for editing Git projects.
    repository = workspace
    workspace = root / "task-workspace"
    subprocess.run(["git", "worktree", "add", "-q", "-b", "claude-task", str(workspace)],
                   cwd=repository, check=True)
    terminals, snapshots, events, calls = [], [], [], []
    watcher = reader = None
    errors = []
    tools = root / "tools"
    tools.mkdir()
    script = root / "tool.py"
    script.write_text(TOOL)
    # The tool shell and a /bin/sleep it starts are platform programs: on macOS 27 neither shows
    # its environment, so end-tools must find them through the shell's own registration.
    shell_pids = tools / "shell.pids"
    tool_command = (f'/bin/sleep 600 & echo "$$ $!" > {shlex.quote(str(shell_pids))}; '
                    + shlex.join([sys.executable, str(script), str(tools)]))
    unrelated = None
    with ClaudeFixture(workspace, tool_command) as fixture, \
            tempfile.TemporaryDirectory(prefix="xc-", dir="/tmp") as runtime:
        env.update(XAYA_AGENT_RUNTIME_DIR=str(Path(runtime) / "run"),
                   ANTHROPIC_BASE_URL=fixture.base_url, ANTHROPIC_MODEL="claude-sonnet-4-6",
                   CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC="1", DISABLE_AUTOUPDATER="1")
        config = Path(env["CLAUDE_CONFIG_DIR"])
        (config / ".claude.json").write_text(json.dumps({"hasCompletedOnboarding": True,
            "theme": "dark", "autoUpdates": False, "customApiKeyResponses": {
                "approved": [env["ANTHROPIC_API_KEY"]], "rejected": []},
            "projects": {str(workspace): {"hasTrustDialogAccepted": True, "hasCompletedProjectOnboarding": True}}}))
        (config / "settings.json").write_text(json.dumps({"permissions": {"defaultMode": "manual"},
            "env": {key: env[key] for key in ("ANTHROPIC_API_KEY", "ANTHROPIC_BASE_URL", "ANTHROPIC_MODEL",
                "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "DISABLE_AUTOUPDATER")}}))

        def cli(*args, success=True):
            result = subprocess.run([xaya, *args], env=env, cwd=workspace,
                                    capture_output=True, text=True, timeout=45)
            calls.append({"argv": list(args), "code": result.returncode,
                          "stdout": result.stdout, "stderr": result.stderr})
            if success:
                if result.returncode:
                    raise RuntimeError(f"XaYa {args}: {result.stderr}")
                return json.loads(result.stdout)
            assert result.returncode != 0, result.stdout
            return result

        def snapshot(identifier):
            return cli("agent", "session", "show", identifier)

        async def ready(identifier):
            def check():
                s = snapshot(identifier)
                if s["state"] in ("interrupted", "failed"):
                    raise RuntimeError(s)
                return s if s["state"] == "ready" else None
            return await until(check, "Claude ready", timeout=45)

        try:
            # An unrelated process in the same checkout, without the launch marker, must survive.
            unrelated = subprocess.Popen(["/bin/sleep", "600"], cwd=workspace, start_new_session=True)
            started = cli("run", "--engine", "claude", "--claude", claude,
                          "--detach", "--json", "XAYA_CLAUDE_PRODUCT_FIRST")
            identifier = started["id"]
            watcher = subprocess.Popen([xaya, "agent", "session", "watch", identifier], env=env,
                cwd=workspace, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
            def read():
                try:
                    for line in watcher.stdout:
                        events.append(json.loads(line))
                except Exception as error:
                    errors.append(str(error))
            reader = threading.Thread(target=read, daemon=True)
            reader.start()
            first = await ready(identifier)
            snapshots.append(first)
            assert first["engine"] == "claude" and first["sandbox"] == "manual", first
            assert first["native_thread_id"] and first["backend_pid"], first
            assert first["native_job_id"] is None, "no vendor background job is involved"
            terminal = Terminal([xaya, "attach", identifier], env, workspace, root / "first-tui.bin")
            terminals.append(terminal)
            await until(lambda: terminal.contains("ACK_XAYA_CLAUDE_PRODUCT_FIRST_COMPLETE"), "native first history", timeout=40)
            assert snapshot(identifier)["attached"] is True
            cli("attach", identifier, success=False)
            terminal.write("XAYA_CLAUDE_PRODUCT_NATIVE")
            await asyncio.sleep(0.3)
            terminal.write("\r")
            await until(lambda: terminal.contains("ACK_XAYA_CLAUDE_PRODUCT_NATIVE_COMPLETE"), "native turn", timeout=40)
            await ready(identifier)
            terminal.write("\x1a")
            await until(lambda: terminal.process.poll() is not None, "Ctrl+Z detach", timeout=15)
            await terminal.close()
            terminals.remove(terminal)
            detached = await until(lambda: (s if not (s := snapshot(identifier))["attached"] else None), "lease release")
            assert detached["backend_pid"] == first["backend_pid"], detached
            terminal = Terminal([xaya, "attach", identifier], env, workspace, root / "reattached-tui.bin")
            terminals.append(terminal)
            await until(lambda: terminal.contains("ACK_XAYA_CLAUDE_PRODUCT_NATIVE_COMPLETE"), "native reattach history", timeout=40)
            terminal.write("XAYA_CLAUDE_ALLOW")
            await asyncio.sleep(0.3)
            terminal.write("\r")
            await until(lambda: snapshot(identifier)["state"] == "blocked", "native permission state", timeout=40)
            def receipt(command):
                r = cli("agent", "session", "command", identifier, command)
                return r if r["status"] != "pending" else None
            def approval():
                detail = cli("agent", "session", "approvals", identifier)
                return detail["approvals"][0] if detail["approvals"] else None
            native_first = await until(approval, "panel approval", timeout=20)
            assert native_first["controllable"] and native_first["details"]["tool_name"] == "Write", native_first
            await until(lambda: terminal.contains("Create file") and terminal.contains("1. Yes"), "native permission UI")
            # The first menu paint can precede activation of its input layer.
            await asyncio.sleep(0.5)
            terminal.write("1\r")
            await until(lambda: (workspace / "XAYA_CLAUDE_ALLOW.txt").exists(), "native approved file", timeout=30)
            await ready(identifier)
            assert (workspace / "XAYA_CLAUDE_ALLOW.txt").read_text() == "XAYA_CLAUDE_ALLOW\n"
            # The native answer came first: the panel request expired and a late answer is refused.
            assert approval() is None
            cli("agent", "session", "respond", identifier, native_first["id"], "decline",
                "--command-id", "claude-late-answer", success=False)
            # Panel answers: decline, then accept, each returned once to the waiting hook.
            panel_answers = {}
            for marker, decision in (("XAYA_CLAUDE_DENY", "decline"), ("XAYA_CLAUDE_NATIVE_PERMISSION", "accept")):
                offset = len(terminal.output)
                terminal.write(marker)
                await asyncio.sleep(0.3)
                terminal.write("\r")
                pending = await until(approval, f"{marker} panel approval", timeout=40)
                await until(lambda: terminal.contains("1. Yes", offset), f"{marker} native prompt shown too")
                command = f"claude-panel-{decision}"
                cli("agent", "session", "respond", identifier, pending["id"], decision, "--command-id", command)
                answered = await until(lambda: receipt(command), f"{marker} panel receipt", timeout=20)
                assert answered["status"] == "succeeded", answered
                await ready(identifier)
                created = (workspace / (marker + ".txt")).exists()
                assert created == (decision == "accept"), (marker, decision, created)
                assert approval() is None
                panel_answers[decision] = {"receipt": answered, "file_created": created}
            def streaming_turn():
                current = snapshot(identifier)
                turn = current.get("turn_id")
                return turn if turn and any(e.get("kind") == "text_delta" and
                    e.get("data", {}).get("turn_id") == turn for e in events) else None
            # Program send: delivered only by the native turn that carries exactly this text.
            cli("agent", "session", "send", identifier, "XAYA_CLAUDE_PROGRAM_SEND",
                "--command-id", "claude-send-once")
            delivered = await until(lambda: receipt("claude-send-once"), "program send receipt", timeout=30)
            assert delivered["status"] == "succeeded" and delivered["turn_id"], delivered
            await until(lambda: terminal.contains("ACK_XAYA_CLAUDE_PROGRAM_SEND_COMPLETE"),
                        "program message in the native TUI", timeout=40)
            await ready(identifier)
            assert cli("agent", "session", "send", identifier, "XAYA_CLAUDE_PROGRAM_SEND",
                       "--command-id", "claude-send-once") == delivered
            assert sum(has_native_prompt(r, "XAYA_CLAUDE_PROGRAM_SEND") and
                       "ACK_XAYA_CLAUDE_PROGRAM_SEND" not in json.dumps(r) for r in fixture.requests) == 1
            # After a native Esc, Claude drops programmatic prompts: an explicit failure, no resend.
            terminal.write("XAYA_CLAUDE_NATIVE_ESC")
            await asyncio.sleep(0.3)
            terminal.write("\r")
            esc_turn = await until(streaming_turn, "native Esc stream", timeout=30)
            terminal.write("\x1b")
            await until(lambda: any(e.get("kind") == "turn_completed" and
                e.get("data", {}).get("turn_id") == esc_turn and
                e.get("data", {}).get("status") == "interrupted" for e in events),
                "native Esc aborted turn", timeout=15)
            fixture.holds["XAYA_CLAUDE_NATIVE_ESC"].set()
            await ready(identifier)
            cli("agent", "session", "send", identifier, "XAYA_CLAUDE_AFTER_ESC",
                "--command-id", "claude-send-after-esc")
            dropped = await until(lambda: receipt("claude-send-after-esc"), "post-Esc send receipt", timeout=30)
            await asyncio.sleep(1)
            esc_reached_model = any(has_native_prompt(r, "XAYA_CLAUDE_AFTER_ESC") for r in fixture.requests)
            assert (dropped["status"] == "failed") != esc_reached_model, (dropped, esc_reached_model)
            send_after_esc = {"receipt": dropped, "reached_model": esc_reached_model}
            await ready(identifier)
            # A real public Mods abort, with an explicit ID and an independently observed end.
            terminal.write("XAYA_CLAUDE_SLOW")
            await asyncio.sleep(0.3)
            terminal.write("\r")
            cancel_turn = await until(streaming_turn, "native stream before controller cancellation", timeout=30)
            cancelled = cli("agent", "session", "cancel", identifier, "--turn-id", cancel_turn,
                            "--command-id", "claude-cancel-once")
            assert cancelled["status"] in ("pending", "succeeded"), cancelled
            def cancellation_receipt():
                receipt = cli("agent", "session", "command", identifier, "claude-cancel-once")
                return receipt if receipt["status"] != "pending" else None
            cancelled = await until(cancellation_receipt, "real Mods cancellation acknowledgement", timeout=15)
            assert cancelled["status"] == "succeeded" and cancelled["turn_id"] == cancel_turn, cancelled
            await until(lambda: any(e.get("kind") == "turn_completed" and
                e.get("data", {}).get("turn_id") == cancel_turn and
                e.get("data", {}).get("status") == "interrupted" for e in events),
                "matching native aborted turn", timeout=15)
            assert cli("agent", "session", "cancel", identifier, "--turn-id", cancel_turn,
                       "--command-id", "claude-cancel-once") == cancelled
            cli("agent", "session", "cancel", identifier, "--turn-id", cancel_turn,
                "--command-id", "claude-cancel-stale", success=False)
            fixture.holds["XAYA_CLAUDE_SLOW"].set()
            await ready(identifier)
            terminal.write("XAYA_CLAUDE_AFTER_CONTROL_CANCEL")
            await asyncio.sleep(0.3)
            terminal.write("\r")
            await until(lambda: terminal.contains("ACK_XAYA_CLAUDE_AFTER_CONTROL_CANCEL_COMPLETE"),
                        "native input after controller cancellation", timeout=30)
            await ready(identifier)
            # A real backgrounded Bash tool whose grandchild setsid()s out of every process tree.
            offset = len(terminal.output)
            terminal.write("XAYA_CLAUDE_CANCEL_TOOL")
            await asyncio.sleep(0.3)
            terminal.write("\r")
            await until(lambda: terminal.contains("1. Yes", offset), "native Bash permission UI", timeout=40)
            await asyncio.sleep(0.5)
            terminal.write("1\r")
            report = await until(lambda: next(iter(tools.glob("tool-*.json")), None),
                                 "background tool started", timeout=30)
            tool = json.loads(report.read_text())
            assert tool["marker_in_env"], tool
            shell_tool = [int(pid) for pid in shell_pids.read_text().split()]
            assert len(shell_tool) == 2, shell_tool
            for pid in (tool["pid"], tool["grandchild"], *shell_tool):
                os.kill(pid, 0)
            # End the tools on their own: refused while their turn runs, then only the
            # backgrounded Bash and its setsid() grandchild end; Claude keeps running.
            busy = subprocess.run([xaya, "agent", "session", "end-tools", identifier], env=env,
                                  cwd=workspace, capture_output=True, text=True, timeout=15)
            assert busy.returncode != 0, busy.stdout
            fixture.holds["XAYA_CLAUDE_CANCEL_TOOL"].set()
            await ready(identifier)
            ended = cli("agent", "session", "end-tools", identifier)
            snapshots.append(ended)
            assert ended["ended"] >= 4, ended
            for pid in (tool["pid"], tool["grandchild"], *shell_tool):
                try:
                    os.kill(pid, 0)
                    raise AssertionError(f"tool process {pid} survived end-tools")
                except ProcessLookupError:
                    pass
            os.kill(first["backend_pid"], 0)
            assert unrelated.poll() is None, "an unmarked process in the checkout was touched"
            terminal.write("XAYA_CLAUDE_AFTER_END_TOOLS")
            await asyncio.sleep(0.3)
            terminal.write("\r")
            await until(lambda: terminal.contains("ACK_XAYA_CLAUDE_AFTER_END_TOOLS_COMPLETE"),
                        "native input after end-tools", timeout=30)
            await ready(identifier)
            terminal.write("\x1a")
            await until(lambda: terminal.process.poll() is not None, "final detach")
            await terminal.close()
            terminals.remove(terminal)
            stopped = cli("agent", "session", "stop", identifier)
            snapshots.append(stopped)
            assert stopped["state"] == "stopped", stopped
            # Stop ended Claude and the reparented grandchild that TaskStop would leave alive.
            for pid in (first["backend_pid"], tool["pid"], tool["grandchild"]):
                try:
                    os.kill(pid, 0)
                    raise AssertionError(f"owned process {pid} survived stop")
                except ProcessLookupError:
                    pass
            assert unrelated.poll() is None, "an unmarked process in the checkout was touched"
            checkpoint = cli("checkpoint", "create", identifier, "--json")
            assert checkpoint["status"] == "ready", checkpoint
            checked = cli("check", "run", checkpoint["id"], "--", "/usr/bin/true")
            assert checked["status"] == "succeeded", checked
            preview = cli("land", "preview", checkpoint["id"], "--check-id", checked["id"], "--target", "claude-target")
            assert preview["status"] == "ready", preview
            verified = cli("land", "verify", preview["id"])
            assert verified["integration_verified"] is True, verified
            landed = cli("land", "run", preview["id"])
            assert landed["status"] == "landed", landed
            landed_file = subprocess.run(["git", "show", "claude-target:XAYA_CLAUDE_ALLOW.txt"],
                cwd=workspace, capture_output=True, text=True, check=True).stdout
            assert landed_file == "XAYA_CLAUDE_ALLOW\n", landed_file
            await asyncio.sleep(max(0, tool["started"] + LATE_AFTER + 1 - time.time()))
            assert not list(tools.glob("late-*")), "a background tool wrote after stop"
            cli("agent", "stop", "--json")
            cli("agent", "start", "--json")
            restored = snapshot(identifier)
            snapshots.append(restored)
            assert restored["native_thread_id"] == first["native_thread_id"]
            assert restored["state"] == "stopped", restored
            # Every owned process is gone, so attach resumes the same conversation in a new host.
            terminal = Terminal([xaya, "attach", identifier], env, workspace, root / "resumed-tui.bin")
            terminals.append(terminal)
            resumed = await ready(identifier)
            snapshots.append(resumed)
            assert resumed["native_thread_id"] == first["native_thread_id"], resumed
            assert resumed["backend_pid"] not in (None, first["backend_pid"]), resumed
            await until(lambda: snapshot(identifier)["attached"], "resumed attach lease", timeout=20)
            await asyncio.sleep(1)
            terminal.write("XAYA_CLAUDE_PRODUCT_RESUMED")
            await asyncio.sleep(0.3)
            terminal.write("\r")
            await until(lambda: terminal.contains("ACK_XAYA_CLAUDE_PRODUCT_RESUMED_COMPLETE"),
                        "native turn after resume", timeout=40)
            request = next(r for r in reversed(fixture.requests) if has_native_prompt(r, "XAYA_CLAUDE_PRODUCT_RESUMED"))
            assert json.loads(request["metadata"]["user_id"])["session_id"] == first["native_thread_id"]
            history = json.dumps(request["messages"])
            assert all(m in history for m in ("XAYA_CLAUDE_PRODUCT_FIRST", "XAYA_CLAUDE_ALLOW")), history
            await ready(identifier)
            terminal.write("\x1a")
            await until(lambda: terminal.process.poll() is not None, "resumed detach", timeout=15)
            await terminal.close()
            terminals.remove(terminal)
            ended = cli("agent", "session", "stop", identifier)
            snapshots.append(ended)
            assert ended["state"] == "stopped", ended
            try:
                os.kill(resumed["backend_pid"], 0)
                raise AssertionError("resumed Claude survived stop")
            except ProcessLookupError:
                pass
            assert cli("checkpoint", "create", identifier, "--json")["status"] == "ready"
            await until(lambda: any(e.get("kind") == "text_delta" for e in events), "live Mods text")
            text = "".join(e["data"]["text"] for e in events if e.get("kind") == "text_delta")
            assert "ACK_XAYA_CLAUDE_PRODUCT_NATIVE_COMPLETE" in text, text
            request = next(r for r in reversed(fixture.requests) if has_native_prompt(r, "XAYA_CLAUDE_PRODUCT_NATIVE"))
            metadata = json.loads(request["metadata"]["user_id"])
            assert metadata["session_id"] == first["native_thread_id"]
            assert "XAYA_CLAUDE_PRODUCT_FIRST" in json.dumps(request["messages"])
            offline = cli("run", "--engine", "claude", "--claude", claude,
                          "--detach", "--json", "XAYA_CLAUDE_OFFLINE_FIRST")
            offline_live = await ready(offline["id"])
            terminal = Terminal([xaya, "attach", offline["id"]], env, workspace, root / "offline-tui.bin")
            terminals.append(terminal)
            await until(lambda: terminal.contains("ACK_XAYA_CLAUDE_OFFLINE_FIRST_COMPLETE"), "offline session history", timeout=40)
            # SIGKILL only the exact disposable XaYa service, leaving the native worker/TUI alive.
            status = cli("agent", "status", "--json")
            os.kill(status["pid"], signal.SIGKILL)
            terminal.write("XAYA_CLAUDE_OFFLINE_MUST_NOT_SEND")
            await asyncio.sleep(0.3)
            terminal.write("\r")
            await until(lambda: terminal.contains("XaYa input gate is closed or unavailable"),
                        "explicit disconnected input rejection", timeout=20)
            assert not any(has_native_prompt(r, "XAYA_CLAUDE_OFFLINE_MUST_NOT_SEND") for r in fixture.requests)
            terminal.write("\x1a")
            await until(lambda: terminal.process.poll() is not None, "offline detach", timeout=15)
            await terminal.close()
            terminals.remove(terminal)
            # The owned program outlived the service; the new service rejoins it through the
            # endpoint its PTY host kept, from one Mods snapshot, without relaunching it.
            cli("agent", "start", "--json")
            os.kill(offline_live["backend_pid"], 0)
            restarted = await ready(offline["id"])
            snapshots.append(restarted)
            assert restarted["backend_pid"] == offline_live["backend_pid"], restarted
            assert restarted["native_thread_id"] == offline_live["native_thread_id"], restarted
            terminal = Terminal([xaya, "attach", offline["id"]], env, workspace, root / "rejoined-tui.bin")
            terminals.append(terminal)
            await until(lambda: snapshot(offline["id"])["attached"], "rejoined attach lease", timeout=20)
            await asyncio.sleep(1)
            # Claude puts input a hook dropped back into its input box; never send it along.
            assert terminal.contains("XAYA_CLAUDE_OFFLINE_MUST_NOT_SEND")
            terminal.write("\x15")
            await asyncio.sleep(0.3)
            terminal.write("XAYA_CLAUDE_REJOINED")
            await asyncio.sleep(0.3)
            terminal.write("\r")
            await until(lambda: terminal.contains("ACK_XAYA_CLAUDE_REJOINED_COMPLETE"),
                        "native turn after the service rejoined", timeout=40)
            request = next(r for r in reversed(fixture.requests) if has_native_prompt(r, "XAYA_CLAUDE_REJOINED"))
            assert json.loads(request["metadata"]["user_id"])["session_id"] == offline_live["native_thread_id"]
            assert not any("XAYA_CLAUDE_OFFLINE_MUST_NOT_SEND" in json.dumps(r.get("messages", []))
                           for r in fixture.requests), "refused input must never reach the model"
            await ready(offline["id"])
            terminal.write("\x1a")
            await until(lambda: terminal.process.poll() is not None, "rejoined detach", timeout=15)
            await terminal.close()
            terminals.remove(terminal)
            recovered = cli("agent", "session", "stop", offline["id"])
            snapshots.append(recovered)
            assert recovered["state"] == "stopped", recovered
            try:
                os.kill(offline_live["backend_pid"], 0)
                raise AssertionError("Claude survived the recovered stop")
            except ProcessLookupError:
                pass
            assert cli("checkpoint", "create", offline["id"], "--json")["status"] == "ready"
            assert not fixture.errors and not errors, (fixture.errors, errors)
            return {"status": "passed", "native_thread_id": first["native_thread_id"],
                    "owned_pty": True, "manual_permissions": True,
                    "background_tool_and_setsid_grandchild_ended_by_stop": True,
                    "end_tools_keeps_claude_and_ends_background_tool": True,
                    "no_late_background_write": True, "unmarked_process_untouched": True,
                    "service_crash_then_explicit_stop_recovers_checkout": True,
                    "service_crash_rejoins_live_tui_without_relaunch": True,
                    "native_turn_and_reattach": True, "exclusive_attach_lease": True,
                    "live_mods_text": True, "native_permission_and_write": True,
                    "turn_bound_controller_cancellation": True,
                    "native_input_after_controller_cancellation": True,
                    "stopped_checkout_checkpoint_check_land": True,
                    "service_disconnect_rejects_native_input": True,
                    "persistent_identity": True,
                    "explicit_resume_same_native_session_new_owned_host": True,
                    "native_answer_expires_panel_approval": True,
                    "panel_answers": panel_answers,
                    "program_send_delivered_by_its_own_turn": True,
                    "program_send_deduplicated": True,
                    "send_after_native_esc": send_after_esc}
        finally:
            try:
                for terminal in terminals:
                    await terminal.close()
                if watcher is not None:
                    watcher.terminate()
                    watcher.wait(timeout=5)
                    if reader:
                        reader.join(timeout=2)
                    watcher.stdout.close()
                    watcher.stderr.close()
                cli("agent", "stop", "--json")
            finally:
                fixture.holds["XAYA_CLAUDE_CANCEL_TOOL"].set()
                if unrelated is not None:
                    unrelated.kill()
                    unrelated.wait()
                (root / "sessions.json").write_text(json.dumps(snapshots, indent=2))
                (root / "events.json").write_text(json.dumps(events, indent=2))
                (root / "cli.json").write_text(json.dumps(calls, indent=2))
                (root / "model-requests.json").write_text(json.dumps(fixture.requests, indent=2))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--xaya", type=Path, default=Path("target/debug/xaya"))
    parser.add_argument("--claude", default=shutil.which("claude"))
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()
    if not args.claude:
        parser.error("Claude CLI is required")
    root = args.output.resolve()
    if subprocess.run(["git", "-C", str(root.parent), "rev-parse", "--git-dir"], capture_output=True).returncode == 0:
        parser.error("output must be outside a git checkout")
    root.mkdir()
    try:
        result = asyncio.run(exercise(str(args.xaya.resolve()), str(Path(args.claude).resolve()), root))
    except BaseException as error:
        (root / "report.json").write_text(json.dumps({"status": "failed", "error": str(error)}, indent=2))
        raise
    (root / "report.json").write_text(json.dumps(result, indent=2))
    print(json.dumps(result, indent=2))


if __name__ == "__main__":
    main()

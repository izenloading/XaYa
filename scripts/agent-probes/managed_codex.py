#!/usr/bin/env python3
"""Exercise the Rust-managed Codex TUI against a local model, without a real account."""

import argparse
import asyncio
import json
import os
from pathlib import Path
import shlex
import shutil
import signal
import subprocess
import sys
import tempfile
import threading

from codex_dual import Terminal, until
from model_fixture import ModelFixture
from run import isolated_env


def service_pid(run_dir):
    """The one process holding the service's persistent lock."""
    holders = subprocess.run(["lsof", "-t", str(run_dir / "agentd.lock")],
                             capture_output=True, text=True).stdout.split()
    if len(set(holders)) != 1:
        raise RuntimeError(f"Expected one service process, found {holders}")
    return int(holders[0])


def alive(pid):
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    return True


def prepare_landing(workspace, root, env):
    """Create source and target histories entirely inside the disposable probe directory."""
    def git(cwd, *args):
        return subprocess.run(["git", "-C", str(cwd), *args], env=env, check=True,
                              capture_output=True, text=True, timeout=15).stdout.strip()

    git(workspace, "init", "-q", "-b", "main")
    git(workspace, "config", "user.name", "XaYa acceptance")
    git(workspace, "config", "user.email", "fixture@example.invalid")
    (workspace / ".gitignore").write_text("local-cache/\n")
    (workspace / ".xaya").mkdir()
    # This exact plan runs once on the source and once on each integrated candidate.
    argv = [sys.executable, "-c",
            "from pathlib import Path; "
            "assert Path('accepted.txt').read_text() == 'XAYA_FILE_ACCEPT\\n'; "
            "print('XAYA_ACCEPTED_CONTENT_OK')"]
    integration = [sys.executable, "-c",
                   "from pathlib import Path; "
                   "p = Path('target-only.txt'); "
                   "assert not p.exists() or p.read_text() == 'TARGET_ONLY\\n'; "
                   "print('XAYA_INTEGRATED_TARGET_OK' if p.exists() else 'XAYA_SOURCE_OK')"]
    (workspace / ".xaya/project.toml").write_text(
        '[verify]\ntimeout = "30s"\nsteps = [\n'
        f'  {{ name = "content", argv = {json.dumps(argv)} }},\n'
        f'  {{ name = "target", argv = {json.dumps(integration)} }},\n]\n')
    git(workspace, "add", ".")
    git(workspace, "commit", "-qm", "acceptance base")
    git(workspace, "switch", "-qc", "task")
    target = root / "target"
    git(workspace, "worktree", "add", "--quiet", str(target), "main")
    (target / "target-only.txt").write_text("TARGET_ONLY\n")
    git(target, "add", "target-only.txt")
    git(target, "commit", "-qm", "target-only history")
    git(target, "branch", "unattached-target")
    (target / "local-cache").mkdir()
    (target / "local-cache/keep").write_text("KEEP_IGNORED\n")
    # Deliberately preserve a user's staged change throughout the full native workflow.
    (workspace / "user-staged.txt").write_text("USER_STAGED\n")
    git(workspace, "add", "user-staged.txt")
    return git


def exercise_landing(cli, workspace, root, session_id, git, evidence, source_head, source_index):
    """CLI acceptance of checkpoints, trusted plans, both landing paths and durable retries."""
    def record(label, *args):
        value = cli(*args, timeout=60)
        evidence.append({"stage": label, "value": value})
        # Keep the successful prefix even if a later command or assertion fails.
        (root / "landing.json").write_text(json.dumps(evidence, indent=2))
        return value

    assert git(workspace, "rev-parse", "HEAD") == source_head
    assert (workspace / ".git/index").read_bytes() == source_index
    cp = record("checkpoint", "checkpoint", "create", session_id,
                "--command-id", "acceptance-checkpoint", "--json")
    assert cp["status"] == "ready", cp
    assert git(workspace, "show", f"{cp['commit']}:accepted.txt") == "XAYA_FILE_ACCEPT"
    info = record("project", "project", "show")
    assert info["trusted"] is False, info
    record("trust", "project", "trust", "--hash", info["config_hash"])
    check = record("source-check", "check", "project", cp["id"],
                   "--command-id", "acceptance-check", "--config-hash", info["config_hash"])
    assert check["status"] == "succeeded" and not check["stale"], check
    assert len(check["steps"]) == 2 and all(s["status"] == "succeeded" for s in check["steps"]), check
    source_log = record("source-log", "check", "log", check["id"])
    assert "XAYA_ACCEPTED_CONTENT_OK" in source_log["text"], source_log
    assert "XAYA_SOURCE_OK" in source_log["text"], source_log
    landings = []
    for branch in ("main", "unattached-target"):
        old = git(workspace, "rev-parse", branch)
        preview = record(f"{branch}-preview", "land", "preview", cp["id"],
                         "--check-id", check["id"], "--target", branch,
                         "--command-id", f"acceptance-{branch}")
        assert preview["status"] == "ready" and not preview["stale"], preview
        candidate = preview["candidate_commit"]
        assert git(workspace, "show", "-s", "--format=%P", candidate) == old
        recheck = record(f"{branch}-recheck", "land", "recheck", preview["id"])
        assert recheck["valid"], recheck
        verified = record(f"{branch}-verify", "land", "verify", preview["id"])
        assert verified["integration_verified"], verified
        integrated = record(f"{branch}-integration", "check", "show", verified["integration_check_id"])
        assert integrated["integration_preview_id"] == preview["id"], integrated
        assert integrated["commit"] == candidate and integrated["tree"] == preview["candidate_tree"]
        assert integrated["status"] == "succeeded" and not integrated["stale"], integrated
        assert integrated["cwd"] != str(workspace) and not Path(integrated["cwd"]).exists(), integrated
        assert not integrated["integration_cleanup_pending"], integrated
        log = record(f"{branch}-log", "check", "log", integrated["id"])
        assert "XAYA_INTEGRATED_TARGET_OK" in log["text"], log
        # Restart between verification and landing to exercise durable candidate bindings.
        record(f"{branch}-stop", "stop", "--json")
        record(f"{branch}-start", "start", "--json")
        landed = record(f"{branch}-land", "land", "run", preview["id"])
        assert landed["status"] == "landed" and landed["landing_outcome"] == "applied", landed
        assert git(workspace, "rev-parse", branch) == candidate
        reconciled = record(f"{branch}-reconcile", "land", "reconcile", preview["id"])
        assert reconciled["landing_outcome"] == "applied", reconciled
        record(f"{branch}-restart-stop", "stop", "--json")
        record(f"{branch}-restart-start", "start", "--json")
        duplicate = record(f"{branch}-retry", "land", "run", preview["id"])
        assert duplicate["id"] == preview["id"] and duplicate["status"] == "landed", duplicate
        assert git(workspace, "rev-parse", branch) == candidate
        landings.append(landed)
    target = root / "target"
    assert (target / "accepted.txt").read_text() == "XAYA_FILE_ACCEPT\n"
    assert (target / "user-staged.txt").read_text() == "USER_STAGED\n"
    assert (target / "local-cache/keep").read_text() == "KEEP_IGNORED\n"
    assert git(target, "status", "--porcelain") == ""
    assert git(target, "write-tree") == landings[0]["candidate_tree"]
    assert git(workspace, "rev-parse", "HEAD") == source_head
    assert git(workspace, "symbolic-ref", "--short", "HEAD") == "task"
    assert (workspace / ".git/index").read_bytes() == source_index
    restored_cp = record("restored-checkpoint", "checkpoint", "show", cp["id"])
    assert restored_cp["commit"] == cp["commit"]
    restored_check = record("restored-check", "check", "show", check["id"])
    assert restored_check["status"] == "succeeded" and not restored_check["stale"], restored_check
    return {"checkpoint_and_trusted_plan": True, "integrated_candidate_verified": True,
            "checked_out_and_unattached_targets_landed": True, "landing_retry_after_restart": True,
            "source_head_and_staged_index_preserved": True, "ignored_target_file_preserved": True,
            "integration_checkout_cleaned": True}



def tool_processes(token):
    result = subprocess.run(["pgrep", "-f", token], capture_output=True, text=True)
    return [int(pid) for pid in result.stdout.split() if int(pid) != os.getpid()]

async def exercise(xaya, codex, root):
    env = isolated_env(root)
    workspace = root / "workspace"
    workspace.mkdir()
    git = prepare_landing(workspace, root, env)
    source_head = git(workspace, "rev-parse", "HEAD")
    source_index = (workspace / ".git/index").read_bytes()
    terminals = []
    records = []
    commands = []
    events = []
    watch_errors = []
    landing_evidence = []
    watcher = None
    watch_thread = None
    # A sandboxed command that keeps running after its exec call returns and leaves a
    # setsid() grandchild; both are found by the unique token in their argv.
    tool_token = "XAYA_TOOL_TOKEN_" + os.urandom(8).hex()
    tool_script = ("import os, time\nif os.fork() == 0:\n    os.setsid()\n    time.sleep(90)\n"
                   "    os._exit(0)\ntime.sleep(90)\n")
    fixture = ModelFixture(commands={"TOOL_MARKER": shlex.join(
        [sys.executable, "-c", tool_script, tool_token])})
    with tempfile.TemporaryDirectory(prefix="xl-", dir="/tmp") as runtime, fixture:
        env["XAYA_AGENT_RUNTIME_DIR"] = str(Path(runtime) / "run")
        (Path(env["CODEX_HOME"]) / "config.toml").write_text(
            'model="gpt-5.4"\nmodel_provider="probe"\ncheck_for_update_on_startup=false\n'
            'approval_policy="on-request"\nsandbox_mode="read-only"\n'
            '[model_providers.probe]\nname="Local fixture"\n'
            f'base_url="{fixture.base_url}"\nwire_api="responses"\nrequires_openai_auth=false\n'
            f'[projects.{json.dumps(str(workspace))}]\ntrust_level="trusted"\n'
            '[analytics]\nenabled=false\n[feedback]\nenabled=false\n')

        def cli(*args, timeout=8):
            result = subprocess.run([xaya, "agent", *args], env=env, cwd=workspace,
                                    check=True, capture_output=True, text=True, timeout=timeout)
            return json.loads(result.stdout)

        def snapshot():
            sessions = cli("session", "list", "--json")
            if len(sessions) != 1:
                raise RuntimeError(f"Expected one managed session, got {sessions}")
            return sessions[0]

        async def acknowledged(session_id, command_id, status="succeeded"):
            receipt = await until(lambda: (r if (r := cli("session", "command", session_id, command_id))["status"] != "pending" else None),
                                  f"command acknowledgement {command_id}")
            commands.append(receipt)
            assert receipt["status"] == status, receipt
            return receipt

        try:
            terminal = Terminal([xaya, "run", "--codex", codex, "-C", str(workspace),
                                 "TERMINAL_FIRST"], env, workspace, root / "first-tui.bin")
            terminals.append(terminal)
            await until(lambda: terminal.contains("ACK_TERMINAL_FIRST_COMPLETE"),
                        "managed native first turn", timeout=40)
            first = await until(lambda: (s if (s := snapshot())["native_thread_id"]
                                        and s["state"] == "ready" else None), "bound ready session")
            records.append(first)
            await terminal.close()
            terminals.remove(terminal)
            detached = await until(lambda: (s if not (s := snapshot())["attached"] else None),
                                   "native detach")
            assert detached["backend_pid"] == first["backend_pid"]
            records.append(detached)
            terminal = Terminal([xaya, "attach", first["id"]], env, workspace, root / "resumed-tui.bin")
            terminals.append(terminal)
            await until(lambda: terminal.contains("ACK_TERMINAL_FIRST_COMPLETE"), "native resumed history")
            terminal.write("TERMINAL_THIRD")
            await asyncio.sleep(0.3)
            terminal.write("\r")
            await until(lambda: terminal.contains("ACK_TERMINAL_THIRD_COMPLETE"), "native resumed input")
            resumed = await until(lambda: (s if (s := snapshot())["state"] == "ready" else None),
                                  "resumed ready session")
            assert resumed["native_thread_id"] == first["native_thread_id"]
            assert resumed["backend_pid"] == first["backend_pid"]
            records.append(resumed)
            watch_log = open(root / "watch.log", "w")
            watcher = subprocess.Popen([xaya, "agent", "session", "watch", first["id"]],
                                       env=env, cwd=workspace, stdout=subprocess.PIPE,
                                       stderr=watch_log, text=True)

            def read_events():
                try:
                    for line in watcher.stdout:
                        events.append(json.loads(line))
                except Exception as error:
                    watch_errors.append(str(error))

            watch_thread = threading.Thread(target=read_events, daemon=True)
            watch_thread.start()
            await until(lambda: events, "subscription snapshot")
            commands.append(cli("session", "send", first["id"], "INJECT_SECOND", "--command-id", "product-send"))
            await acknowledged(first["id"], "product-send")
            await until(lambda: terminal.contains("ACK_INJECT_SECOND_COMPLETE"), "controller output in native TUI")
            await until(lambda: snapshot()["state"] == "ready", "controller turn completed")
            # Retrying the same command ID retrieves its receipt instead of starting another turn.
            duplicate = cli("session", "send", first["id"], "INJECT_SECOND", "--command-id", "product-send")
            assert duplicate["status"] == "succeeded"
            await until(lambda: "ACK_INJECT_SECOND_COMPLETE" in "".join(
                event["data"]["text"] for event in events[1:] if event.get("kind") == "text_delta"),
                "normalized streaming deltas")
            commands.append(cli("session", "send", first["id"], "COMMAND_DENY", "--command-id", "product-deny-turn"))
            await until(lambda: snapshot()["state"] == "blocked", "controller approval waiting")
            pending = cli("session", "approvals", first["id"])
            approval = pending["approvals"][0]
            decision = "decline" if "decline" in approval["details"].get("availableDecisions", []) else "cancel"
            commands.append(cli("session", "respond", first["id"], approval["id"], decision, "--command-id", "product-deny"))
            await acknowledged(first["id"], "product-deny")
            if decision == "decline":
                await until(lambda: terminal.contains("ACK_COMMAND_DENY_COMPLETE"), "controller approval resolved in TUI")
            await until(lambda: snapshot()["state"] == "ready", "denied turn completed")
            commands.append(cli("session", "respond", first["id"], approval["id"], "accept", "--command-id", "product-stale"))
            await acknowledged(first["id"], "product-stale", "failed")
            commands.append(cli("session", "send", first["id"], "FILE_ACCEPT", "--command-id", "product-file-turn"))
            await until(lambda: snapshot()["state"] == "blocked", "file approval waiting")
            pending = cli("session", "approvals", first["id"])
            file_approval = pending["approvals"][0]
            assert file_approval["method"] == "item/fileChange/requestApproval", file_approval
            commands.append(cli("session", "respond", first["id"], file_approval["id"], "accept", "--command-id", "product-file-accept"))
            await acknowledged(first["id"], "product-file-accept")
            await until(lambda: terminal.contains("ACK_FILE_ACCEPT_COMPLETE"), "file approval result in native TUI")
            await until(lambda: snapshot()["state"] == "ready", "file turn finished")
            assert (workspace / "accepted.txt").read_text() == "XAYA_FILE_ACCEPT\n"
            commands.append(cli("session", "send", first["id"], "SLOW_CANCEL", "--command-id", "product-slow"))
            await until(fixture.slow_started.is_set, "managed streaming response")
            await acknowledged(first["id"], "product-slow")
            commands.append(cli("session", "cancel", first["id"], "--command-id", "product-cancel"))
            await acknowledged(first["id"], "product-cancel")
            await until(lambda: snapshot()["state"] == "ready", "interrupted managed turn")
            fixture.release.set()
            terminal.write("FINAL_NATIVE")
            await asyncio.sleep(0.3)
            terminal.write("\r")
            await until(lambda: terminal.contains("ACK_FINAL_NATIVE_COMPLETE"), "native input after controller cancellation")
            await until(lambda: snapshot()["state"] == "ready", "final native turn")
            terminal.write("COMMAND_ACCEPT")
            await asyncio.sleep(0.3)
            terminal.write("\r")
            await until(lambda: terminal.contains("Would you like to run"), "native approval prompt")
            blocked = await until(lambda: (s if (s := snapshot())["state"] == "blocked" else None),
                                  "managed approval state")
            records.append(blocked)
            # A pending approval must still be answerable after replacing the native terminal.
            await terminal.close()
            terminals.remove(terminal)
            await until(lambda: not snapshot()["attached"], "detach during approval")
            terminal = Terminal([xaya, "attach", first["id"]], env, workspace, root / "approval-tui.bin")
            terminals.append(terminal)
            # The title can be rendered during resume before the menu/input layer is ready.
            await until(lambda: terminal.contains("Press enter to confirm"), "resumed native approval menu")
            await asyncio.sleep(0.3)
            terminal.write("\r")
            await until(lambda: terminal.contains("ACK_COMMAND_ACCEPT_COMPLETE"), "native approval result")
            finished = await until(lambda: (s if (s := snapshot())["state"] == "ready" else None),
                                   "resolved managed approval")
            records.append(finished)
            await terminal.close()
            terminals.remove(terminal)
            cli("stop", "--json")
            cli("start", "--json")
            restored = snapshot()
            records.append(restored)
            assert restored["id"] == first["id"] and restored["state"] == "stopped"
            assert restored["native_thread_id"] == first["native_thread_id"]
            assert restored["backend_pid"] is None
            assert cli("session", "command", first["id"], "product-send")["status"] == "succeeded"
            fresh_snapshot = cli("session", "approvals", first["id"])
            # The stored stream continues across a restart, and holds every event watched so far.
            assert fresh_snapshot["epoch"] == events[0]["epoch"], (fresh_snapshot, events[0])
            assert fresh_snapshot["last_seq"] >= events[-1]["seq"]
            stored = subprocess.run([xaya, "agent", "session", "history", first["id"]], env=env,
                                    cwd=workspace, check=True, capture_output=True, text=True,
                                    timeout=15).stdout.splitlines()
            stored = {event["seq"]: event for event in map(json.loads, stored)}
            missing = [event["seq"] for event in events[1:] if stored.get(event["seq"]) != event]
            assert not missing, f"watched events missing from stored history: {missing}"
            terminal = Terminal([xaya, "attach", first["id"]], env, workspace, root / "restarted-tui.bin")
            terminals.append(terminal)
            await until(lambda: terminal.contains("ACK_COMMAND_ACCEPT_COMPLETE"), "persistent native history")
            await asyncio.sleep(0.3)
            terminal.write("AFTER_RESTART")
            await asyncio.sleep(0.3)
            terminal.write("\r")
            await until(lambda: terminal.contains("ACK_AFTER_RESTART_COMPLETE"), "native turn after service restart")
            resumed = await until(lambda: (s if (s := snapshot())["state"] == "ready" else None), "restarted session ready")
            assert resumed["native_thread_id"] == first["native_thread_id"]
            assert resumed["backend_pid"] != first["backend_pid"]
            records.append(resumed)
            await terminal.close()
            terminals.remove(terminal)
            await until(lambda: not snapshot()["attached"], "detach before service crash")
            # End-tools: a background exec tool and its setsid() grandchild end after their
            # turn; the backend keeps running and serves the next turn.
            commands.append(cli("session", "send", first["id"], "TOOL_MARKER background",
                                "--command-id", "tool-turn"))
            await until(lambda: len(tool_processes(tool_token)) == 2, "background exec tool", timeout=30)
            await until(lambda: (s if (s := snapshot())["state"] == "ready" else None),
                        "tool turn finished", timeout=60)
            assert len(tool_processes(tool_token)) == 2, "the tool outlives its exec call and turn"
            ended = cli("session", "end-tools", first["id"])
            records.append(ended)
            assert ended["ended"] >= 2, ended
            await until(lambda: not tool_processes(tool_token), "tools ended", timeout=10)
            assert alive(snapshot()["backend_pid"]), "the backend keeps running"
            # P2: killing the service mid-turn must not end the turn; the next service adopts the
            # hosted backend on a new connection instead of starting a second writer.
            fixture.slow_started.clear()
            fixture.release.clear()
            commands.append(cli("session", "send", first["id"], "SLOW_CANCEL survives crash",
                                "--command-id", "crash-turn"))
            await until(fixture.slow_started.is_set, "model stream before service crash")
            await acknowledged(first["id"], "crash-turn")
            crashed = snapshot()
            assert crashed["state"] == "working", crashed
            killed = service_pid(Path(env["XAYA_AGENT_RUNTIME_DIR"]))
            os.kill(killed, signal.SIGKILL)
            await until(lambda: not alive(killed), "service killed")
            assert alive(crashed["backend_pid"]), "backend must outlive the service"
            cli("start", "--json")
            adopted = await until(lambda: (s if (s := snapshot())["state"] == "working" else None),
                                  "adopted running turn")
            assert adopted["backend_pid"] == crashed["backend_pid"], adopted
            assert adopted["turn_id"] == crashed["turn_id"], adopted
            records.append(adopted)
            fixture.release.set()
            await until(lambda: snapshot()["state"] == "ready", "adopted turn completed")
            terminal = Terminal([xaya, "attach", first["id"]], env, workspace, root / "adopted-tui.bin")
            terminals.append(terminal)
            await until(lambda: terminal.contains("ACK_SLOW_CANCEL_COMPLETE"), "adopted turn in native history")
            await asyncio.sleep(0.3)
            mark = len(terminal.output)
            terminal.write("TERMINAL_FIRST after adoption")
            await asyncio.sleep(0.3)
            terminal.write("\r")
            await until(lambda: terminal.contains("ACK_TERMINAL_FIRST_COMPLETE", since=mark),
                        "native turn on the adopted backend")
            resumed = await until(lambda: (s if (s := snapshot())["state"] == "ready" else None),
                                  "adopted session ready")
            assert resumed["backend_pid"] == crashed["backend_pid"]
            records.append(resumed)
            # A crash with the native TUI attached: nothing here touches the service, so only
            # `xaya attach` can start it again; the TUI reconnects to the same backend by itself.
            run_dir = Path(env["XAYA_AGENT_RUNTIME_DIR"])
            killed = service_pid(run_dir)
            os.kill(killed, signal.SIGKILL)
            await until(lambda: not alive(killed), "service killed with the TUI attached")
            await until(lambda: terminal.contains("Reconnecting", since=mark), "TUI reconnecting")

            def restarted():
                try:
                    return (pid := service_pid(run_dir)) != killed and pid
                except RuntimeError:
                    return None
            replacement = await until(restarted, "service restarted by attach", timeout=30)
            rejoined = await until(lambda: (s if (s := snapshot())["attached"]
                                            and s["state"] == "ready" else None),
                                   "TUI reconnected", timeout=40)
            assert rejoined["backend_pid"] == crashed["backend_pid"], rejoined
            assert terminal.process.poll() is None, "the TUI kept running"
            records.append(rejoined)
            await asyncio.sleep(0.5)
            mark = len(terminal.output)
            terminal.write("FINAL_NATIVE")
            await asyncio.sleep(0.3)
            terminal.write("\r")
            await until(lambda: terminal.contains("ACK_FINAL_NATIVE_COMPLETE", since=mark),
                        "native turn after the attach-restarted service", timeout=40)
            await until(lambda: snapshot()["state"] == "ready", "ready after reconnect")
            reconnect = {"killed_service": killed, "restarted_by_attach": replacement,
                         "backend_pid": rejoined["backend_pid"]}
            final_backend_pid = resumed["backend_pid"]
            stopped = cli("session", "stop", first["id"])
            assert stopped["state"] == "stopped" and stopped["backend_pid"] is None
            records.append(stopped)
            # The stopped session takes its attached TUI with it instead of leaving it reconnecting.
            await until(lambda: terminal.process.poll() is not None, "attached TUI ended with stop",
                        timeout=15)
            try:
                os.kill(final_backend_pid, 0)
            except ProcessLookupError:
                pass
            else:
                raise RuntimeError("Managed backend is still running after stop")
            assert cli("status", "--json")["active_sessions"] == 0
            landing = exercise_landing(cli, workspace, root, first["id"], git, landing_evidence,
                                       source_head, source_index)
            if fixture.errors:
                raise RuntimeError(f"Local model fixture errors: {fixture.errors}")
            if watch_errors:
                raise RuntimeError(f"Event reader errors: {watch_errors}")
            baseline = events[0]
            for index, event in enumerate(events[1:], 1):
                assert event["epoch"] == baseline["epoch"]
                assert event["seq"] == baseline["last_seq"] + index
            assert any(event.get("kind") == "turn_completed" and event["data"]["status"] == "interrupted"
                       for event in events[1:])
            return {"status": "passed", "landing": landing, "native_thread_id": first["native_thread_id"],
                    "same_backend_after_detach": True,
                    "event_history_survives_restart": True,
                    "end_tools_ends_background_exec_and_keeps_backend": True, "native_input_after_reattach": True,
                    "native_approval_after_reattach": True,
                    "controller_send_and_streaming": True, "controller_approval_and_stale_rejection": True,
                    "controller_file_approval_write_verified": True,
                    "persistent_identity_and_receipt": True, "epoch_changes_after_restart": True,
                    "explicit_resume_same_thread": True,
                    "controller_cancel_and_native_input": True, "event_sequence_contiguous": True,
                    "turn_survives_service_crash": True, "adopted_backend_native_input": True,
                    "attach_restarts_crashed_service": reconnect,
                    "tui_reconnects_and_ends_with_stop": True,
                    "backend_reaped_after_stop": True}
        finally:
            try:
                for terminal in terminals:
                    await terminal.close()
            finally:
                try:
                    try:
                        if watcher is not None:
                            if watcher.poll() is None:
                                watcher.terminate()
                            watcher.wait(timeout=5)
                            if watch_thread:
                                watch_thread.join(timeout=2)
                            watcher.stdout.close()
                            watch_log.close()
                    finally:
                        cli("stop", "--json")
                finally:
                    (root / "sessions.json").write_text(json.dumps(records, indent=2))
                    (root / "model-requests.json").write_text(json.dumps(fixture.requests, indent=2))
                    (root / "commands.json").write_text(json.dumps(commands, indent=2))
                    (root / "events.json").write_text(json.dumps(events, indent=2))
                    (root / "landing.json").write_text(json.dumps(landing_evidence, indent=2))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--xaya", type=Path, default=Path("target/debug/xaya"))
    parser.add_argument("--codex", default=shutil.which("codex"))
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()
    if not args.codex:
        parser.error("Codex CLI is required")
    root = args.output.resolve()
    if subprocess.run(["git", "-C", str(root.parent), "rev-parse", "--git-dir"],
                      capture_output=True).returncode == 0:
        parser.error("output must be outside a git checkout")
    root.mkdir()
    try:
        result = asyncio.run(exercise(str(args.xaya.resolve()), str(Path(args.codex).resolve()), root))
    except BaseException as error:
        (root / "report.json").write_text(json.dumps({"status":"failed", "error":str(error)}, indent=2))
        raise
    (root / "report.json").write_text(json.dumps(result, indent=2))
    print(json.dumps(result, indent=2))


if __name__ == "__main__":
    main()

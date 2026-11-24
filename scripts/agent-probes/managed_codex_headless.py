#!/usr/bin/env python3
"""Queue a Codex task that starts without a native TUI, against a local model, then attach.

The service starts the Codex backend, initializes it, starts the main thread and submits the
task as the first message itself. A native TUI attached afterwards resumes that same thread and
shows the first turn's answer. Uses the real Codex CLI, an isolated configuration and a loopback
model; no real account.
"""

import argparse
import asyncio
import json
from pathlib import Path
import shutil
import subprocess
import tempfile

from codex_dual import Terminal, until
from model_fixture import ModelFixture
from run import isolated_env


async def exercise(xaya, codex, root):
    env = isolated_env(root)
    workspace = root / "workspace"
    workspace.mkdir()

    def git(*args):
        return subprocess.run(["git", *args], cwd=workspace, env=env, check=True,
                              capture_output=True, text=True).stdout.strip()

    git("init", "-q")
    git("config", "user.name", "XaYa probe")
    git("config", "user.email", "probe@example.invalid")
    (workspace / "README.md").write_text("probe\n")
    git("add", ".")
    git("-c", "core.hooksPath=/dev/null", "commit", "-qm", "base")
    fixture = ModelFixture()
    terminals = []
    records = {}
    with tempfile.TemporaryDirectory(prefix="xh-", dir="/tmp") as runtime, fixture:
        env["XAYA_AGENT_RUNTIME_DIR"] = str(Path(runtime) / "run")
        (Path(env["CODEX_HOME"]) / "config.toml").write_text(
            'model="gpt-5.4"\nmodel_provider="probe"\ncheck_for_update_on_startup=false\n'
            'approval_policy="on-request"\nsandbox_mode="read-only"\n'
            '[model_providers.probe]\nname="Local fixture"\n'
            f'base_url="{fixture.base_url}"\nwire_api="responses"\nrequires_openai_auth=false\n'
            f'[projects.{json.dumps(str(workspace))}]\ntrust_level="trusted"\n'
            '[analytics]\nenabled=false\n[feedback]\nenabled=false\n')

        def cli(*args, timeout=15):
            result = subprocess.run([xaya, "agent", *args], env=env, cwd=workspace,
                                    check=True, capture_output=True, text=True, timeout=timeout)
            return json.loads(result.stdout)

        def attempt(*args):
            try:
                return cli(*args)
            except subprocess.CalledProcessError:
                return None  # Not recorded yet: the service starts the thread first.

        try:
            task = cli("task", "new", "--project", ".", "--title", "Headless Codex",
                       "--goal", "TERMINAL_FIRST from the queue", "--criterion", "answers")
            records["queued"] = cli("task", "enqueue", task["id"], "--request-id", "headless",
                                    "--engine", "codex", "--codex", codex)
            dispatched = await until(
                lambda: (t if (t := cli("task", "show", task["id"]))["task"]["queue"]["state"]
                         == "dispatched" else None), "the queue to dispatch Codex", timeout=60)
            session_id = dispatched["task"]["queue"]["session_id"]
            try:
                receipt = await until(
                    lambda: (r if (r := attempt("session", "command", session_id,
                                                "xaya-first-message"))
                             and r["status"] != "pending" else None),
                    "the first message", timeout=60)
            except BaseException:
                records["session_on_timeout"] = attempt("session", "show", session_id)
                records["sessions_on_timeout"] = attempt("session", "list", "--json")
                records["task_on_timeout"] = attempt("task", "show", task["id"])
                raise
            records["first_message"] = receipt
            assert receipt["status"] == "succeeded" and receipt["turn_id"], receipt
            ready = await until(
                lambda: (s if (s := cli("session", "show", session_id))["state"] == "ready"
                         and s["turn_id"] is None else None), "the first turn to end", timeout=60)
            records["after_first_turn"] = ready
            assert ready["native_thread_id"] and not ready["attached"], ready
            prompts = [json.dumps(request.get("input", [])) for request in fixture.requests]
            assert sum("TERMINAL_FIRST from the queue" in prompt for prompt in prompts) >= 1, prompts
            # A native TUI attached now resumes the thread the service started.
            terminal = Terminal([xaya, "attach", session_id], env, workspace, root / "attach-tui.bin")
            terminals.append(terminal)
            await until(lambda: terminal.contains("ACK_TERMINAL_FIRST_COMPLETE"),
                        "the first turn in the attached TUI", timeout=60)
            attached = await until(
                lambda: (s if (s := cli("session", "show", session_id))["attached"] else None),
                "the attached TUI")
            records["attached"] = attached
            assert attached["native_thread_id"] == ready["native_thread_id"], attached
            await terminal.close()
            terminals.remove(terminal)
            stopped = cli("session", "stop", session_id, timeout=30)
            records["stopped"] = stopped
            assert stopped["state"] == "stopped", stopped
            # Each request carries the whole conversation (Codex adds its own environment context
            # as another user message): a message sent twice would appear twice in one request.
            sent = max(sum("TERMINAL_FIRST from the queue" in json.dumps(item)
                           for item in request.get("input", []) if item.get("role") == "user")
                       for request in fixture.requests)
            assert sent == 1, f"the first message appears {sent} times"
            return {"status": "passed", "first_message_sent_once": True,
                    "model_requests": len(fixture.requests),
                    "attached_tui_resumed_service_thread": True,
                    "thread": ready["native_thread_id"], "turn": receipt["turn_id"]}
        finally:
            try:
                for terminal in terminals:
                    await terminal.close()
            finally:
                try:
                    cli("stop", "--json")
                finally:
                    (root / "records.json").write_text(json.dumps(records, indent=2))
                    (root / "model-requests.json").write_text(json.dumps(fixture.requests, indent=2))


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
        result = asyncio.run(exercise(str(args.xaya.resolve()),
                                      str(Path(args.codex).resolve()), root))
    except BaseException as error:
        (root / "report.json").write_text(json.dumps({"status": "failed", "error": str(error)},
                                                     indent=2))
        raise
    (root / "report.json").write_text(json.dumps(result, indent=2))
    print(json.dumps(result, indent=2))


if __name__ == "__main__":
    main()
